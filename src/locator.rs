//! Turn the server's opaque ids in a coherence report into the dotted paths you
//! authored with.
//!
//! The server reports a finding against a node / port / edge **id**, because ids
//! are what its coherence engine carries. An id is not something you can act on:
//! `input port ae97175b-… has no incoming edge` names nothing you typed and
//! nothing you can look up without another round-trip. With a hundred findings
//! that is a hundred lookups.
//!
//! Everything needed to translate is already on disk:
//!
//!   * the pulled index — `node:<path>` and `port:<path>:<side>:<name>` keys,
//!     plus `node_info` (the only place **config** ports appear) and an edge
//!     table keyed `"<src_port>:<tgt_port>"`;
//!   * the stage's alias table — the same `node:` / `port:` keys for work you
//!     have staged but not committed, **and** `edge:<src>:<tgt>` for staged
//!     edges.
//!
//! Labels are rendered with [`crate::staging::render_port_path`], the same
//! function `status` and `diff` use, so a port reads `Api.Rater.key` here and
//! everywhere else — one spelling, and the one the authoring verbs accept.
//!
//! Nothing is ever guessed. An id we cannot place keeps its raw form, and a
//! partially-resolved edge keeps the raw id of the side that is missing, so the
//! caller can say the local view is behind the branch rather than presenting a
//! confident half-truth.

use std::collections::BTreeMap;

use uuid::Uuid;

use crate::staging::render_port_path;
use crate::state::{Index, Stage};

/// A resolved locator: what to display, and whether every id in it was placed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Resolved {
    /// The display label, e.g. `Api.Rater.key` or `A.out -> B.in`.
    pub(crate) label: String,
    /// False when part of the label is still a raw id (a dangling edge whose
    /// far endpoint no longer exists locally). Callers surface this rather than
    /// letting a half-resolved label read as a complete answer.
    pub(crate) complete: bool,
}

/// Reverse lookup from server id to the path you authored with.
pub(crate) struct Locators {
    /// id → label.
    by_id: BTreeMap<Uuid, String>,
    /// edge id → (source port id, target port id).
    edges: BTreeMap<Uuid, (Uuid, Uuid)>,
}

impl Locators {
    /// Build from the pulled index and the current stage.
    ///
    /// The stage is applied *after* the index so staged intent wins over a
    /// stale pulled view of the same id.
    pub(crate) fn new(index: Option<&Index>, stage: &Stage) -> Locators {
        let mut by_id = BTreeMap::new();
        let mut edges = BTreeMap::new();

        if let Some(index) = index {
            let mut node_paths: BTreeMap<Uuid, String> = BTreeMap::new();
            for (key, id) in &index.entries {
                match classify(key) {
                    Some(Key::Node(path)) => {
                        node_paths.insert(*id, path.to_string());
                        by_id.insert(*id, path.to_string());
                    }
                    Some(Key::Port(rest)) => {
                        by_id.insert(*id, render_port_path(rest));
                    }
                    Some(Key::Edge(rest)) => {
                        if let Some(pair) = parse_edge_endpoints(rest) {
                            edges.insert(*id, pair);
                        }
                    }
                    None => {}
                }
            }
            // `entries` carries only inputs and outputs — CONFIG ports appear
            // nowhere but `node_info`. Without this pass a finding against a
            // config port degrades to a raw id and then advises a `pull`, which
            // could never have helped.
            for (node_id, info) in &index.node_info {
                let Some(node_path) = node_paths.get(node_id) else {
                    continue;
                };
                for port in info.inputs.iter().chain(&info.outputs).chain(&info.config) {
                    by_id
                        .entry(port.id)
                        .or_insert_with(|| format!("{node_path}.{}", port.name));
                }
            }
            for (endpoints, edge_id) in &index.edges {
                if let Some(pair) = parse_edge_endpoints(endpoints) {
                    edges.insert(*edge_id, pair);
                }
            }
        }

        for (key, id) in &stage.aliases {
            match classify(key) {
                Some(Key::Node(path)) => {
                    by_id.insert(*id, path.to_string());
                }
                Some(Key::Port(rest)) => {
                    by_id.insert(*id, render_port_path(rest));
                }
                // Staged edges are the primary case for `validate`, which
                // dry-runs the stage — missing this scheme meant a finding about
                // an edge you just staged could never resolve.
                Some(Key::Edge(rest)) => {
                    if let Some(pair) = parse_edge_endpoints(rest) {
                        edges.insert(*id, pair);
                    }
                }
                None => {}
            }
        }

        Locators { by_id, edges }
    }

    /// The label for `id`, or `None` when it isn't in the local view.
    fn label(&self, id: &Uuid) -> Option<&str> {
        self.by_id.get(id).map(String::as_str)
    }

    /// Resolve a finding's locator. An edge renders as the two ports it joins,
    /// which is what makes a dangling-wire or type-mismatch finding actionable.
    ///
    /// Returns `None` when nothing can be placed — including an edge whose two
    /// endpoints are both unknown, since `<unknown> -> <unknown>` carries no
    /// information *and* would suppress the stale-view warning.
    pub(crate) fn resolve(&self, raw: &str) -> Option<Resolved> {
        let id = Uuid::parse_str(raw).ok()?;
        if let Some(direct) = self.label(&id) {
            return Some(Resolved {
                label: direct.to_string(),
                complete: true,
            });
        }
        let (src, tgt) = self.edges.get(&id)?;
        let src_label = self.label(src);
        let tgt_label = self.label(tgt);
        if src_label.is_none() && tgt_label.is_none() {
            return None;
        }
        // Keep the RAW id for the side we can't place. A dangling wire is
        // exactly the case where one endpoint is gone, and the id is the only
        // thing left that correlates with the server.
        let src_text = src_label
            .map(str::to_string)
            .unwrap_or_else(|| src.to_string());
        let tgt_text = tgt_label
            .map(str::to_string)
            .unwrap_or_else(|| tgt.to_string());
        Some(Resolved {
            label: format!("{src_text} -> {tgt_text}"),
            complete: src_label.is_some() && tgt_label.is_some(),
        })
    }

    /// Replace every id inside `text` that we can place with its label, leaving
    /// the rest untouched. The server embeds the same id in the message as in
    /// the locator, so without this the path would appear beside prose that
    /// still reads in raw ids.
    pub(crate) fn rewrite(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let bytes = text.as_bytes();
        let mut cursor = 0usize;
        // Drive from char_indices so every offset examined is a real char
        // boundary; `str::get` then makes the WINDOW END safe too. Slicing a
        // `&str` at a non-boundary panics, and a 36-BYTE window can end inside a
        // multi-byte char — which crashed the whole command on any message
        // carrying non-ASCII (a port type, a description, a typographic dash).
        for (i, ch) in text.char_indices() {
            if i < cursor {
                continue;
            }
            if looks_like_uuid(bytes, i) {
                if let Some(candidate) = text.get(i..i + UUID_LEN) {
                    if let Some(resolved) = self.resolve(candidate) {
                        out.push_str(&resolved.label);
                        cursor = i + UUID_LEN;
                        continue;
                    }
                }
            }
            out.push(ch);
            cursor = i + ch.len_utf8();
        }
        out
    }
}

/// Canonical hyphenated UUID length.
const UUID_LEN: usize = 36;

/// Cheap shape test before paying for a full parse: a canonical UUID is ASCII
/// with hyphens at fixed offsets. Parsing at every offset without this is ~25x
/// slower on large messages, for four byte comparisons.
fn looks_like_uuid(bytes: &[u8], i: usize) -> bool {
    if i + UUID_LEN > bytes.len() {
        return false;
    }
    bytes[i + 8] == b'-' && bytes[i + 13] == b'-' && bytes[i + 18] == b'-' && bytes[i + 23] == b'-'
}

/// The key schemes the index and the stage share. Kept as one exhaustive match
/// so a new scheme is a compile-time decision rather than a silent `None`.
enum Key<'a> {
    Node(&'a str),
    /// The remainder after `port:` — `<path>:<side>:<name>`.
    Port(&'a str),
    /// The remainder after `edge:` — `<src_id>:<tgt_id>`.
    Edge(&'a str),
}

/// Classify an index/stage key. An empty remainder is rejected: a label of `""`
/// would print as a blank locator AND count as successfully resolved, which is
/// the one state where the output is meaningless and the warning is suppressed.
fn classify(key: &str) -> Option<Key<'_>> {
    let (kind, rest) = key.split_once(':')?;
    if rest.is_empty() {
        return None;
    }
    match kind {
        "node" => Some(Key::Node(rest)),
        "port" => Some(Key::Port(rest)),
        "edge" => Some(Key::Edge(rest)),
        _ => None,
    }
}

/// `"<src_port_id>:<tgt_port_id>"` → the two ids.
fn parse_edge_endpoints(rest: &str) -> Option<(Uuid, Uuid)> {
    let (src, tgt) = rest.split_once(':')?;
    Some((Uuid::parse_str(src).ok()?, Uuid::parse_str(tgt).ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{NodeInfo, PortInfo};

    fn uuid(n: u8) -> Uuid {
        Uuid::from_bytes([n; 16])
    }

    fn index_with(entries: &[(&str, Uuid)], edges: &[(&str, Uuid)]) -> Index {
        Index {
            version: 2,
            entries: entries
                .iter()
                .map(|(k, v)| ((*k).to_string(), *v))
                .collect(),
            node_info: Default::default(),
            edges: edges.iter().map(|(k, v)| ((*k).to_string(), *v)).collect(),
        }
    }

    fn label_of(loc: &Locators, id: Uuid) -> Option<String> {
        loc.resolve(&id.to_string()).map(|r| r.label)
    }

    // ─── the panic this module used to have ─────────────────────────────────

    #[test]
    fn rewrite_survives_a_uuid_shaped_window_ending_inside_a_multibyte_char() {
        // The ONLY input that reaches the slice: it must pass the hyphen shape
        // check (positions 8/13/18/23) AND have a multi-byte char straddling
        // byte 36. `…abcdefghij` is 34 bytes, then `€` occupies 34..37, so the
        // 36-byte window ends inside it. Slicing there panics — and the cheaper
        // multibyte cases below never get that far, so this is the case that
        // actually pins the fix.
        let loc = Locators::new(None, &Stage::empty());
        let text = "abcdefgh-abcd-abcd-abcd-abcdefghij\u{20AC}";
        assert_eq!(text.len(), 37);
        assert!(!text.is_char_boundary(36), "fixture must straddle byte 36");
        assert_eq!(loc.rewrite(text), text);
    }

    #[test]
    fn rewrite_survives_a_window_ending_inside_a_multibyte_char() {
        // 35 ASCII then a 2-byte char: the 36-byte window from offset 0 ends
        // INSIDE the `é`. Slicing there panics, so this crashed `validate`
        // outright — no findings, no exit code, just a backtrace. It fires
        // before any lookup, so an empty local view does not protect you.
        let loc = Locators::new(None, &Stage::empty());
        let text = format!("{}\u{00e9} tail", "x".repeat(35));
        assert_eq!(loc.rewrite(&text), text);
    }

    #[test]
    fn rewrite_survives_a_real_type_mismatch_message_with_a_non_ascii_type() {
        // Port types are free author text. This is the server's actual
        // type_mismatch phrasing with a non-ASCII type.
        let loc = Locators::new(None, &Stage::empty());
        let text = format!(
            "edge {} connects a port of type 'µs' to one of type 'int'",
            uuid(1)
        );
        assert_eq!(loc.rewrite(&text), text);
    }

    #[test]
    fn rewrite_survives_dense_multibyte_text() {
        let loc = Locators::new(None, &Stage::empty());
        let text = "a€€€€€€€€€€€€";
        assert_eq!(loc.rewrite(text), text);
    }

    // ─── labels match the rest of the CLI ───────────────────────────────────

    #[test]
    fn a_port_renders_as_the_dotted_path_the_authoring_verbs_accept() {
        // `status`/`diff`/`show`/`walk` all render `Api.Rater.key`, and that is
        // what `edge add --to` takes. A third spelling would not be pasteable.
        let port = uuid(1);
        let idx = index_with(&[("port:Api.Rater:in:key", port)], &[]);
        let loc = Locators::new(Some(&idx), &Stage::empty());
        assert_eq!(label_of(&loc, port).as_deref(), Some("Api.Rater.key"));
    }

    #[test]
    fn resolves_a_node_id_to_its_dotted_path() {
        let node = uuid(2);
        let idx = index_with(&[("node:Api.Rater", node)], &[]);
        let loc = Locators::new(Some(&idx), &Stage::empty());
        assert_eq!(label_of(&loc, node).as_deref(), Some("Api.Rater"));
    }

    #[test]
    fn resolves_a_config_port_via_node_info() {
        // Config ports exist ONLY in node_info — the index's `entries` carries
        // inputs and outputs alone. Missing this made every config-port finding
        // unresolvable while advising a pull that could not help.
        let (node, cfg) = (uuid(3), uuid(4));
        let mut idx = index_with(&[("node:Api.Rater", node)], &[]);
        idx.node_info.insert(
            node,
            NodeInfo {
                kind: "behavior".to_string(),
                inputs: vec![],
                outputs: vec![],
                config: vec![PortInfo {
                    id: cfg,
                    name: "retries".to_string(),
                    r#type: "int".to_string(),
                    description: String::new(),
                }],
            },
        );
        let loc = Locators::new(Some(&idx), &Stage::empty());
        assert_eq!(label_of(&loc, cfg).as_deref(), Some("Api.Rater.retries"));
    }

    // ─── edges ──────────────────────────────────────────────────────────────

    #[test]
    fn resolves_an_edge_id_to_both_endpoints() {
        let (src, tgt, edge) = (uuid(5), uuid(6), uuid(7));
        let idx = index_with(
            &[("port:A:out:v", src), ("port:B:in:v", tgt)],
            &[(&format!("{src}:{tgt}"), edge)],
        );
        let loc = Locators::new(Some(&idx), &Stage::empty());
        let r = loc.resolve(&edge.to_string()).expect("edge should resolve");
        assert_eq!(r.label, "A.v -> B.v");
        assert!(r.complete);
    }

    #[test]
    fn a_dangling_edge_keeps_the_raw_id_of_the_side_it_cannot_place() {
        // The surviving side must be named, and the missing side must keep its
        // id — that id is the only thing left that correlates with the server.
        let (src, missing, edge) = (uuid(8), uuid(9), uuid(10));
        let idx = index_with(
            &[("port:A:out:v", src)],
            &[(&format!("{src}:{missing}"), edge)],
        );
        let loc = Locators::new(Some(&idx), &Stage::empty());
        let r = loc.resolve(&edge.to_string()).expect("edge should resolve");
        assert_eq!(r.label, format!("A.v -> {missing}"));
        assert!(!r.complete, "a half-resolved edge is not complete");
    }

    #[test]
    fn an_edge_with_both_endpoints_unknown_does_not_resolve() {
        // `<unknown> -> <unknown>` carries nothing AND would suppress the
        // stale-view warning — strictly worse than showing the raw id.
        let (src, tgt, edge) = (uuid(11), uuid(12), uuid(13));
        let idx = index_with(&[], &[(&format!("{src}:{tgt}"), edge)]);
        let loc = Locators::new(Some(&idx), &Stage::empty());
        assert_eq!(loc.resolve(&edge.to_string()), None);
    }

    #[test]
    fn resolves_an_edge_staged_but_not_committed() {
        // `validate` dry-runs the STAGE, so a staged edge is the primary case.
        // The stage keys these `edge:<src>:<tgt>`, a scheme distinct from the
        // index's edge table.
        let (src, tgt, edge) = (uuid(14), uuid(15), uuid(16));
        let mut stage = Stage::empty();
        stage.aliases.insert("port:A:out:v".to_string(), src);
        stage.aliases.insert("port:B:in:v".to_string(), tgt);
        stage.aliases.insert(format!("edge:{src}:{tgt}"), edge);
        let loc = Locators::new(None, &stage);
        assert_eq!(label_of(&loc, edge).as_deref(), Some("A.v -> B.v"));
    }

    // ─── stage / index interaction ──────────────────────────────────────────

    #[test]
    fn resolves_ids_that_exist_only_in_the_stage() {
        let staged_port = uuid(17);
        let mut stage = Stage::empty();
        stage
            .aliases
            .insert("port:Api.Fresh:in:x".to_string(), staged_port);
        let loc = Locators::new(None, &stage);
        assert_eq!(label_of(&loc, staged_port).as_deref(), Some("Api.Fresh.x"));
    }

    #[test]
    fn the_stage_wins_over_a_stale_index_for_the_same_id() {
        let id = uuid(18);
        let idx = index_with(&[("node:Old.Path", id)], &[]);
        let mut stage = Stage::empty();
        stage.aliases.insert("node:New.Path".to_string(), id);
        let loc = Locators::new(Some(&idx), &stage);
        assert_eq!(label_of(&loc, id).as_deref(), Some("New.Path"));
    }

    // ─── negatives ──────────────────────────────────────────────────────────

    #[test]
    fn an_unknown_id_does_not_resolve() {
        let loc = Locators::new(None, &Stage::empty());
        assert_eq!(loc.resolve(&uuid(19).to_string()), None);
    }

    #[test]
    fn a_non_uuid_locator_does_not_resolve() {
        let loc = Locators::new(None, &Stage::empty());
        assert_eq!(loc.resolve("not-a-uuid"), None);
    }

    #[test]
    fn an_empty_key_remainder_is_rejected_rather_than_labelled_blank() {
        // A truncated key would otherwise resolve to "", printing a blank
        // locator while counting as resolved.
        let id = uuid(20);
        let idx = index_with(&[("node:", id)], &[]);
        let loc = Locators::new(Some(&idx), &Stage::empty());
        assert_eq!(loc.resolve(&id.to_string()), None);
    }

    // ─── rewrite ────────────────────────────────────────────────────────────

    #[test]
    fn rewrite_replaces_the_id_embedded_in_a_server_message() {
        let port = uuid(21);
        let idx = index_with(&[("port:Api.Rater:in:key", port)], &[]);
        let loc = Locators::new(Some(&idx), &Stage::empty());
        assert_eq!(
            loc.rewrite(&format!("input port {port} has no incoming edge")),
            "input port Api.Rater.key has no incoming edge",
        );
    }

    #[test]
    fn rewrite_replaces_every_id_in_a_message_not_just_the_first() {
        let (a, b) = (uuid(22), uuid(23));
        let idx = index_with(&[("port:A:out:v", a), ("port:B:in:v", b)], &[]);
        let loc = Locators::new(Some(&idx), &Stage::empty());
        assert_eq!(loc.rewrite(&format!("{a} then {b}")), "A.v then B.v");
    }

    #[test]
    fn rewrite_leaves_an_unresolvable_id_alone() {
        let loc = Locators::new(None, &Stage::empty());
        let msg = format!("input port {} has no incoming edge", uuid(24));
        assert_eq!(loc.rewrite(&msg), msg);
    }

    #[test]
    fn rewrite_keeps_surrounding_multibyte_text_intact() {
        let port = uuid(25);
        let idx = index_with(&[("port:Api:in:k", port)], &[]);
        let loc = Locators::new(Some(&idx), &Stage::empty());
        assert_eq!(
            loc.rewrite(&format!("café — port {port} → unfed")),
            "café — port Api.k → unfed",
        );
    }
}
