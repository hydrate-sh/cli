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

use std::path::PathBuf;

use uuid::Uuid;

use crate::error::CliError;
use crate::state::Index;

/// The working-copy root, or `None` when this directory is not one. `show`
/// deliberately works outside a working copy (it takes `--project`), so a
/// missing root is an ordinary state rather than an error.
pub(crate) fn base_dir() -> Option<PathBuf> {
    super::context::cwd()
        .ok()
        .and_then(|c| crate::state::find_root(&c))
}

/// Resolve `path` to the node id a scoped read needs, from the pulled index.
///
/// `None` means "cannot do a scoped read here" — no working copy, no index, or
/// the path is not in it (a stale index, or a typo).
pub(crate) fn scoped_target(base: &Option<PathBuf>, path: &str) -> Result<Option<Uuid>, CliError> {
    let Some(base) = base.as_ref() else {
        return Ok(None);
    };
    let Some(index) = Index::load(base)? else {
        return Ok(None);
    };
    Ok(index.entries.get(&format!("node:{path}")).copied())
}

/// The note printed when a scoped read wasn't possible and the whole branch
/// was fetched instead.
pub(crate) fn fallback_note(path: &str) -> String {
    format!(
        "note: no local index entry for '{path}', so the whole branch was \
         fetched and filtered here. Run `hydrate pull` in a bound working copy \
         to read just the slice."
    )
}

/// How to display a node the server could not give a dotted path.
///
/// The server returns such nodes deliberately — an unnamed node is legal while
/// designing — and reports why in `unaddressable`. Rendering the reason beats
/// both a panic (the map is not total, so indexing it is a crash waiting for
/// an unnamed node) and a fabricated path (which the caller would paste into
/// the next command).
pub(crate) fn unaddressable_label(reason: &str) -> String {
    match reason {
        "empty_name" => "<unnamed — give it a name to address it>".to_string(),
        "reserved_separator" => "<name contains '.', which separates path segments>".to_string(),
        "ambiguous" => "<two nodes here share a name — rename one to address either>".to_string(),
        other => format!("<no path: {other}>"),
    }
}
