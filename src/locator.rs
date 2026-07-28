//! Turn the server's opaque ids in a coherence report into the dotted paths you
//! authored with.
//!
//! The server reports a finding against a node / port / edge **id**, because ids
//! are what its coherence engine carries. An id is not something you can act on:
//! `input port ae97175b-… has no incoming edge` names nothing you typed and
//! nothing you can look up without another round-trip. With a hundred findings
//! that is a hundred lookups.
//!
//! Everything needed to translate is already on disk. The pulled index maps
//! `node:<path>` and `port:<path>:<side>:<name>` to ids, and the stage's alias
//! table uses the identical key scheme for work you have staged but not
//! committed — so findings about brand-new ports resolve too. This module
//! inverts both into id → label, and inverts the index's edge table so an edge
//! id resolves to the two ports it joins.
//!
//! Unresolvable ids keep their raw form. An id we cannot place means the local
//! view is stale or the finding is about something we have never seen, and
//! quietly dropping it would be worse than showing it — so the caller is
//! expected to say so (see `validate`'s stale-index hint).

use std::collections::BTreeMap;

use uuid::Uuid;

use crate::state::{Index, Stage};

/// Reverse lookup from server id to the path you authored with.
pub(crate) struct Locators {
    /// id → label, e.g. `cachetools.Cache.clear` or
    /// `cachetools.Cache.contains:in:key`.
    by_id: BTreeMap<Uuid, String>,
    /// edge id → (source port id, target port id), from the index's
    /// `"<src>:<tgt>" → edge_id` table.
    edges: BTreeMap<Uuid, (Uuid, Uuid)>,
}

impl Locators {
    /// Build from the pulled index and the current stage.
    ///
    /// The stage is applied *after* the index so a staged re-mint of the same
    /// path wins — the stage is the newer intent.
    pub(crate) fn new(index: Option<&Index>, stage: &Stage) -> Locators {
        let mut by_id = BTreeMap::new();
        let mut edges = BTreeMap::new();

        if let Some(index) = index {
            for (key, id) in &index.entries {
                if let Some(label) = label_for(key) {
                    by_id.insert(*id, label);
                }
            }
            for (endpoints, edge_id) in &index.edges {
                if let Some((src, tgt)) = parse_edge_key(endpoints) {
                    edges.insert(*edge_id, (src, tgt));
                }
            }
        }
        for (key, id) in &stage.aliases {
            if let Some(label) = label_for(key) {
                by_id.insert(*id, label);
            }
        }

        Locators { by_id, edges }
    }

    /// The label for `id`, or `None` when it isn't in the local view.
    pub(crate) fn label(&self, id: &Uuid) -> Option<&str> {
        self.by_id.get(id).map(String::as_str)
    }

    /// Resolve a finding's locator string. An edge id renders as the two ports it
    /// joins (`<src> -> <tgt>`), which is what makes a dangling-wire or
    /// type-mismatch finding actionable. Returns `None` when nothing resolves,
    /// so the caller can keep the raw id and flag the gap.
    pub(crate) fn resolve(&self, raw: &str) -> Option<String> {
        let id = Uuid::parse_str(raw).ok()?;
        if let Some(direct) = self.label(&id) {
            return Some(direct.to_string());
        }
        let (src, tgt) = self.edges.get(&id)?;
        // A half-resolved edge is still worth showing: a dangling wire is
        // precisely the case where one endpoint no longer exists, so falling
        // back to the raw id for the missing side is the informative rendering.
        let src = self.label(src).unwrap_or("<unknown>");
        let tgt = self.label(tgt).unwrap_or("<unknown>");
        Some(format!("{src} -> {tgt}"))
    }

    /// Replace every id inside `text` that we can place with its label, leaving
    /// the rest untouched. The server's message embeds the same id as the
    /// locator, so without this the path appears beside a message that still
    /// reads in raw ids.
    pub(crate) fn rewrite(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let bytes = text.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            // A UUID is 36 chars; only attempt a parse where one could start.
            if i + UUID_LEN <= bytes.len() {
                let candidate = &text[i..i + UUID_LEN];
                if let Some(label) = self.resolve(candidate) {
                    out.push_str(&label);
                    i += UUID_LEN;
                    continue;
                }
            }
            // Not a resolvable id here — copy one char and advance. Stepping by
            // the character (not the byte) keeps multi-byte text intact.
            let ch = text[i..]
                .chars()
                .next()
                .expect("index is on a char boundary");
            out.push(ch);
            i += ch.len_utf8();
        }
        out
    }
}

/// Canonical hyphenated UUID length.
const UUID_LEN: usize = 36;

/// `node:<path>` → `<path>`; `port:<path>:<side>:<name>` → `<path>:<side>:<name>`.
/// Anything else is a key scheme this build doesn't know — skipped rather than
/// guessed at.
fn label_for(key: &str) -> Option<String> {
    if let Some(rest) = key.strip_prefix("node:") {
        return Some(rest.to_string());
    }
    if let Some(rest) = key.strip_prefix("port:") {
        return Some(rest.to_string());
    }
    None
}

/// The index keys edges by `"<src_port_id>:<tgt_port_id>"`.
fn parse_edge_key(key: &str) -> Option<(Uuid, Uuid)> {
    let (src, tgt) = key.split_once(':')?;
    Some((Uuid::parse_str(src).ok()?, Uuid::parse_str(tgt).ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uuid(n: u8) -> Uuid {
        Uuid::from_bytes([n; 16])
    }

    fn index_with(entries: &[(&str, Uuid)], edges: &[(&str, Uuid)]) -> Index {
        Index {
            version: 1,
            entries: entries
                .iter()
                .map(|(k, v)| ((*k).to_string(), *v))
                .collect(),
            node_info: Default::default(),
            edges: edges.iter().map(|(k, v)| ((*k).to_string(), *v)).collect(),
        }
    }

    #[test]
    fn resolves_a_port_id_to_its_authored_path() {
        let port = uuid(1);
        let idx = index_with(&[("port:Api.Rater:in:key", port)], &[]);
        let loc = Locators::new(Some(&idx), &Stage::empty());
        assert_eq!(
            loc.resolve(&port.to_string()).as_deref(),
            Some("Api.Rater:in:key")
        );
    }

    #[test]
    fn resolves_a_node_id_to_its_dotted_path() {
        let node = uuid(2);
        let idx = index_with(&[("node:Api.Rater", node)], &[]);
        let loc = Locators::new(Some(&idx), &Stage::empty());
        assert_eq!(loc.resolve(&node.to_string()).as_deref(), Some("Api.Rater"));
    }

    #[test]
    fn resolves_an_edge_id_to_both_endpoints() {
        let (src, tgt, edge) = (uuid(3), uuid(4), uuid(5));
        let idx = index_with(
            &[("port:A:out:v", src), ("port:B:in:v", tgt)],
            &[(&format!("{src}:{tgt}"), edge)],
        );
        let loc = Locators::new(Some(&idx), &Stage::empty());
        assert_eq!(
            loc.resolve(&edge.to_string()).as_deref(),
            Some("A:out:v -> B:in:v"),
        );
    }

    #[test]
    fn a_dangling_edge_still_names_the_endpoint_that_survives() {
        // The whole point of a dangling-wire finding: one side is gone. The
        // surviving side must still be named, or the finding is unusable.
        let (src, missing, edge) = (uuid(6), uuid(7), uuid(8));
        let idx = index_with(
            &[("port:A:out:v", src)],
            &[(&format!("{src}:{missing}"), edge)],
        );
        let loc = Locators::new(Some(&idx), &Stage::empty());
        assert_eq!(
            loc.resolve(&edge.to_string()).as_deref(),
            Some("A:out:v -> <unknown>"),
        );
    }

    #[test]
    fn resolves_ids_that_exist_only_in_the_stage() {
        // A finding about a port staged but not yet committed is not in the
        // index at all — the alias table is the only place it appears.
        let staged_port = uuid(9);
        let mut stage = Stage::empty();
        stage
            .aliases
            .insert("port:Api.Fresh:in:x".to_string(), staged_port);
        let loc = Locators::new(None, &stage);
        assert_eq!(
            loc.resolve(&staged_port.to_string()).as_deref(),
            Some("Api.Fresh:in:x"),
        );
    }

    #[test]
    fn the_stage_wins_over_a_stale_index_for_the_same_id() {
        let id = uuid(10);
        let idx = index_with(&[("node:Old.Path", id)], &[]);
        let mut stage = Stage::empty();
        stage.aliases.insert("node:New.Path".to_string(), id);
        let loc = Locators::new(Some(&idx), &stage);
        assert_eq!(loc.resolve(&id.to_string()).as_deref(), Some("New.Path"));
    }

    #[test]
    fn an_unknown_id_does_not_resolve() {
        let loc = Locators::new(None, &Stage::empty());
        assert_eq!(loc.resolve(&uuid(11).to_string()), None);
    }

    #[test]
    fn a_non_uuid_locator_does_not_resolve() {
        let loc = Locators::new(None, &Stage::empty());
        assert_eq!(loc.resolve("not-a-uuid"), None);
    }

    #[test]
    fn rewrite_replaces_the_id_embedded_in_a_server_message() {
        let port = uuid(12);
        let idx = index_with(&[("port:Api.Rater:in:key", port)], &[]);
        let loc = Locators::new(Some(&idx), &Stage::empty());
        assert_eq!(
            loc.rewrite(&format!("input port {port} has no incoming edge")),
            "input port Api.Rater:in:key has no incoming edge",
        );
    }

    #[test]
    fn rewrite_leaves_an_unresolvable_id_alone() {
        let loc = Locators::new(None, &Stage::empty());
        let msg = format!("input port {} has no incoming edge", uuid(13));
        assert_eq!(loc.rewrite(&msg), msg);
    }

    #[test]
    fn rewrite_handles_multibyte_text_without_panicking() {
        // The scanner walks by char, not byte — a message carrying non-ASCII
        // must survive intact.
        let port = uuid(14);
        let idx = index_with(&[("port:Api:in:k", port)], &[]);
        let loc = Locators::new(Some(&idx), &Stage::empty());
        assert_eq!(
            loc.rewrite(&format!("café — port {port} → unfed")),
            "café — port Api:in:k → unfed",
        );
    }
}
