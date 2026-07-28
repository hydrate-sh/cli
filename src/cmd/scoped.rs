//! Shared plumbing for the SCOPED reads (`walk`, `show --depth`).
//!
//! A scoped read is addressed by node **id**, but the CLI addresses nodes by
//! dotted **path** — so the pulled index is what makes one possible without
//! first fetching the graph we are trying to avoid fetching.
//!
//! When the index can't answer, the caller falls back to the whole-graph read.
//! That is a real fallback, not a failure: asking for a slice should never
//! deny you your graph. It IS reported, because the whole point of a scoped
//! read is what crosses the wire, and silently fetching everything would look
//! identical to succeeding.

use std::path::Path;

use uuid::Uuid;

use crate::error::CliError;
use crate::state::Index;

/// What a read decided to do, before any network call.
///
/// Separated from the I/O so the decision is testable on its own. Without
/// this the dispatch is only reachable through a live request, which means
/// "did we actually stop fetching the whole graph?" — the entire point of a
/// scoped read — cannot be asserted anywhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Plan {
    /// Read just the slice rooted at this node.
    Scoped(Uuid),
    /// Fetch the whole branch and filter locally, for the stated reason.
    WholeGraph(Fallback),
}

/// Why a scoped read wasn't possible. Distinct variants because the remedies
/// differ: a pull fixes a missing index, and nothing fixes a typo but retyping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fallback {
    /// Not inside a working copy — `show` may be driven by `--project`.
    NoWorkingCopy,
    /// A working copy with no index yet: never pulled.
    NoIndex,
    /// An index that doesn't know this path: stale, or a typo.
    PathNotInIndex,
    /// The read targets a branch other than the one this copy is bound to, so
    /// the index — which records no branch identity — cannot be trusted for it.
    NotTheBoundBranch,
}

/// Decide how to read `path`, given the working copy root and whether the
/// request targets the bound branch.
///
/// `on_bound_branch` matters because [`Index`] stores only `node:<path>` → id
/// with no branch identity: it is implicitly the BOUND branch's. Resolving a
/// path through it and then reading a different branch can silently return a
/// different node, so that combination falls back instead.
pub(crate) fn plan(
    base: Option<&Path>,
    path: &str,
    on_bound_branch: bool,
) -> Result<Plan, CliError> {
    let Some(base) = base else {
        return Ok(Plan::WholeGraph(Fallback::NoWorkingCopy));
    };
    if !on_bound_branch {
        return Ok(Plan::WholeGraph(Fallback::NotTheBoundBranch));
    }
    let Some(index) = Index::load(base)? else {
        return Ok(Plan::WholeGraph(Fallback::NoIndex));
    };
    match index.entries.get(&format!("node:{path}")) {
        Some(id) => Ok(Plan::Scoped(*id)),
        None => Ok(Plan::WholeGraph(Fallback::PathNotInIndex)),
    }
}

/// The working-copy root, or `None` when this directory is not one. `show`
/// deliberately works outside a working copy (it takes `--project`), so a
/// missing root is an ordinary state rather than an error.
pub(crate) fn base_dir() -> Option<std::path::PathBuf> {
    super::context::cwd()
        .ok()
        .and_then(|c| crate::state::find_root(&c))
}

/// The note printed when a scoped read wasn't possible.
///
/// Names the actual cause. Telling someone to `hydrate pull` when they simply
/// mistyped a path sends them to the wrong fix, and the real error arrives a
/// moment later from the fallback read.
pub(crate) fn fallback_note(path: &str, why: Fallback) -> String {
    match why {
        Fallback::NoWorkingCopy => format!(
            "note: not in a working copy, so the whole branch was fetched and \
             filtered here to find '{path}'."
        ),
        Fallback::NoIndex => format!(
            "note: this working copy has no local index, so the whole branch \
             was fetched to find '{path}'. Run `hydrate pull` to read just the \
             slice next time."
        ),
        Fallback::PathNotInIndex => format!(
            "note: '{path}' is not in this working copy's index, so the whole \
             branch was fetched. If it exists on the branch, run `hydrate pull` \
             — the index may be behind."
        ),
        Fallback::NotTheBoundBranch => format!(
            "note: reading a branch other than the one bound here, so the local \
             index cannot resolve '{path}' and the whole branch was fetched."
        ),
    }
}

/// How to display a node the server could not give a dotted path.
///
/// The server returns such nodes deliberately — an unnamed node is legal while
/// designing — and reports why. Rendering the reason beats both a panic (the
/// path map is not total, so indexing it crashes on an ordinary graph) and a
/// fabricated path (which the caller would paste into the next command).
pub(crate) fn unaddressable_label(reason: &str) -> String {
    match reason {
        "empty_name" => "<unnamed — give it a name to address it>".to_string(),
        "reserved_separator" => "<name contains '.', which separates path segments>".to_string(),
        "ambiguous" => "<two nodes here share a name — rename one to address either>".to_string(),
        // An unrecognised reason is preserved rather than collapsed: a newer
        // server may report a cause this build predates, and hiding it would
        // leave the user with no way to learn what to fix.
        other => format!("<no path: {}>", sanitize(other)),
    }
}

/// Strip control characters from a server-supplied string before it reaches a
/// terminal.
///
/// Node names have no charset validation server-side, and they flow into the
/// dotted paths and reasons rendered here. Without this, a name carrying ANSI
/// escapes or a newline can forge report lines or repaint the screen — and the
/// realistic source isn't a hostile collaborator, it's an LLM naming nodes from
/// imported third-party content.
pub(crate) fn sanitize(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_control() {
                char::REPLACEMENT_CHARACTER
            } else {
                c
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_index(entries: &[(&str, Uuid)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let hy = dir.path().join(".hydrate");
        std::fs::create_dir_all(&hy).unwrap();
        let map: std::collections::BTreeMap<String, Uuid> = entries
            .iter()
            .map(|(k, v)| ((*k).to_string(), *v))
            .collect();
        let index = serde_json::json!({
            "version": 2, "entries": map, "node_info": {}, "edges": {},
        });
        std::fs::write(hy.join("index.json"), index.to_string()).unwrap();
        dir
    }

    #[test]
    fn no_working_copy_falls_back() {
        assert_eq!(
            plan(None, "Api", true).unwrap(),
            Plan::WholeGraph(Fallback::NoWorkingCopy),
        );
    }

    #[test]
    fn a_different_branch_falls_back_rather_than_trusting_the_index() {
        // The index records no branch identity, so resolving a path through it
        // and reading a DIFFERENT branch can return a different node entirely.
        let dir = write_index(&[("node:Api", Uuid::from_u128(1))]);
        assert_eq!(
            plan(Some(dir.path()), "Api", false).unwrap(),
            Plan::WholeGraph(Fallback::NotTheBoundBranch),
        );
    }

    #[test]
    fn a_working_copy_with_no_index_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".hydrate")).unwrap();
        assert_eq!(
            plan(Some(dir.path()), "Api", true).unwrap(),
            Plan::WholeGraph(Fallback::NoIndex),
        );
    }

    #[test]
    fn a_path_the_index_does_not_know_falls_back() {
        let dir = write_index(&[("node:Api", Uuid::from_u128(1))]);
        assert_eq!(
            plan(Some(dir.path()), "Nope", true).unwrap(),
            Plan::WholeGraph(Fallback::PathNotInIndex),
        );
    }

    #[test]
    fn a_known_path_on_the_bound_branch_is_read_scoped() {
        let id = Uuid::from_u128(7);
        let dir = write_index(&[("node:Api.Rater", id)]);
        assert_eq!(
            plan(Some(dir.path()), "Api.Rater", true).unwrap(),
            Plan::Scoped(id),
        );
    }

    #[test]
    fn every_fallback_reason_names_its_own_cause() {
        // A note that blames the wrong cause sends the user to the wrong fix.
        for (why, needle) in [
            (Fallback::NoWorkingCopy, "not in a working copy"),
            (Fallback::NoIndex, "no local index"),
            (Fallback::PathNotInIndex, "not in this working copy's index"),
            (Fallback::NotTheBoundBranch, "other than the one bound"),
        ] {
            let note = fallback_note("Api", why);
            assert!(note.contains(needle), "{why:?} -> {note}");
            assert!(note.contains("Api"), "{why:?} must name the path");
        }
    }

    #[test]
    fn every_unaddressable_reason_renders_something_actionable() {
        for (reason, needle) in [
            ("empty_name", "give it a name"),
            ("reserved_separator", "contains '.'"),
            ("ambiguous", "share a name"),
            ("something_new", "no path: something_new"),
        ] {
            let label = unaddressable_label(reason);
            assert!(label.contains(needle), "{reason} -> {label}");
        }
    }

    #[test]
    fn an_unknown_reason_cannot_repaint_the_terminal() {
        let label = unaddressable_label("\u{1b}[2Kforged");
        assert!(!label.contains('\u{1b}'), "{label:?}");
    }

    #[test]
    fn sanitize_strips_controls_but_keeps_ordinary_text() {
        assert_eq!(sanitize("Api.Rater"), "Api.Rater");
        assert_eq!(sanitize("a\nb"), "a\u{fffd}b");
        assert_eq!(sanitize("caf\u{e9}"), "caf\u{e9}");
    }
}
