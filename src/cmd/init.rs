//! `init` — plant a small, marked pointer block into this directory's
//! `AGENTS.md` so a coding agent that reads that file discovers the hydrate
//! workflow. The block only points at `hydrate guide` (the versioned source of
//! truth for the loop), so the user's file never drifts out of date against the
//! commands.
//!
//! Pure local file operation: no network, no bound working copy required. It is
//! idempotent — the block is delimited by markers, so re-running replaces it in
//! place and never duplicates it or clobbers the user's other content.

use std::fs;
use std::path::{Path, PathBuf};

use super::context;
use crate::error::CliError;
use crate::output::OutputMode;

/// Opening marker of the managed block. Its presence identifies a prior `init`.
const START: &str = "<!-- hydrate:start -->";
/// Closing marker of the managed block.
const END: &str = "<!-- hydrate:end -->";

/// The exact block written into `AGENTS.md`. It defers to `hydrate guide` rather
/// than inlining the loop, so it cannot go stale as the commands evolve.
const BLOCK: &str = "\
<!-- hydrate:start -->
## Architecture context (hydrate)

This project uses hydrate for its living architecture spec. Run `hydrate guide` for the
workflow, then follow it: `hydrate walk` for context before editing, author decisions as
you build, and `hydrate validate` before every commit.
<!-- hydrate:end -->";

/// The file `init` manages, in the current directory.
const AGENTS_FILE: &str = "AGENTS.md";

/// What `init` did to the file — reported to the user, distinct per case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// The file was absent and was created holding just the block.
    Created,
    /// The file existed without a block; the block was appended.
    Appended,
    /// The file already had a block; it was replaced in place.
    Updated,
}

impl Outcome {
    /// Stable machine token for `--json`.
    fn token(self) -> &'static str {
        match self {
            Outcome::Created => "created",
            Outcome::Appended => "appended",
            Outcome::Updated => "updated",
        }
    }
}

pub fn run(mode: OutputMode) -> Result<(), CliError> {
    let path = context::cwd()?.join(AGENTS_FILE);
    let outcome = apply(&path)?;
    println!("{}", render(outcome, &path, mode));
    Ok(())
}

/// Write the pointer block into `path`, choosing create/append/update from the
/// file's current state. Fails loud on any IO error rather than proceeding as
/// if it had written the block.
fn apply(path: &Path) -> Result<Outcome, CliError> {
    // If anything is already at `path`, refuse to follow a symlink: writing
    // through it would clobber an unintended target the user never named.
    // `symlink_metadata` does NOT follow the link, so a broken or valid link is
    // detected the same way; a stat error means there is nothing to preserve.
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(CliError::InitRefused(format!(
                "{} is a symlink; refusing to write through it",
                path.display()
            )));
        }
        Ok(_) => {}
        // No entry at `path` (or the parent isn't a directory): there is nothing
        // to preserve, so create the file holding just the block. If the location
        // is genuinely unwritable, `write` surfaces that loudly.
        Err(_) => {
            write(path, &format!("{BLOCK}\n"))?;
            return Ok(Outcome::Created);
        }
    }

    let existing = fs::read_to_string(path)
        .map_err(|e| CliError::Other(format!("could not read {}: {e}", path.display())))?;

    let (contents, outcome) = match classify(&existing)? {
        // A single well-formed block: splice the new block over exactly the old
        // one (markers included), leaving every byte around it untouched.
        MarkerState::OneBlock { start, end } => {
            let stop = end + END.len();
            let mut next = String::with_capacity(existing.len());
            next.push_str(&existing[..start]);
            next.push_str(BLOCK);
            next.push_str(&existing[stop..]);
            (next, Outcome::Updated)
        }
        // No block yet: append it, guaranteeing a blank line before it so it
        // never runs into the user's existing content.
        MarkerState::None => {
            let mut next = existing;
            if !next.ends_with('\n') {
                next.push('\n');
            }
            next.push('\n');
            next.push_str(BLOCK);
            next.push('\n');
            (next, Outcome::Appended)
        }
    };

    write(path, &contents)?;
    Ok(outcome)
}

/// The hydrate-marker state of an existing `AGENTS.md`, as far as `init` cares.
enum MarkerState {
    /// No markers at all — safe to append the block.
    None,
    /// Exactly one well-formed block: one start before one end. Byte offsets of
    /// the start marker and the start of the end marker, for in-place splicing.
    OneBlock { start: usize, end: usize },
}

/// Classify the file's hydrate-marker state, refusing loudly on any malformed or
/// ambiguous arrangement rather than silently splicing out the user's content.
///
/// Only two shapes are safe to act on: no markers (append) and exactly one
/// start-before-one-end (replace in place). A lone start, a lone end, an end
/// before a start, or duplicate markers of either kind are ambiguous — acting on
/// them could delete arbitrary user text or append a duplicate block — so they
/// are rejected with a distinct error and the file is left untouched.
fn classify(existing: &str) -> Result<MarkerState, CliError> {
    let starts = existing.match_indices(START).count();
    let ends = existing.match_indices(END).count();

    match (starts, ends) {
        (0, 0) => Ok(MarkerState::None),
        (1, 1) => {
            // Unwraps are safe: exactly one of each marker exists.
            let start = existing.find(START).unwrap();
            let end = existing.find(END).unwrap();
            if start < end {
                Ok(MarkerState::OneBlock { start, end })
            } else {
                Err(malformed())
            }
        }
        _ => Err(malformed()),
    }
}

/// The distinct refusal raised for any malformed/ambiguous marker state.
fn malformed() -> CliError {
    CliError::InitRefused(format!(
        "{AGENTS_FILE} has a malformed or ambiguous hydrate block (the `{START}` / `{END}` \
         markers). Refusing to edit it to avoid losing your content — fix the markers by hand \
         and re-run."
    ))
}

/// Write `contents` to `path` atomically: write a sibling temp file in the same
/// directory, then `rename` it over `path`. A crash or full disk mid-write leaves
/// the previous `AGENTS.md` intact rather than a half-written or truncated one.
/// The temp file is a sibling so the rename stays within one filesystem (where it
/// is atomic) and replaces any existing target in a single step. Any IO failure
/// maps to a loud, distinct error.
fn write(path: &Path, contents: &str) -> Result<(), CliError> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, contents)
        .map_err(|e| CliError::Other(format!("could not write {}: {e}", tmp.display())))?;
    fs::rename(&tmp, path).map_err(|e| {
        // Best-effort cleanup; the rename error is the one that matters.
        let _ = fs::remove_file(&tmp);
        CliError::Other(format!("could not write {}: {e}", path.display()))
    })
}

/// Render the outcome for `mode`. Human = one sentence naming the action and the
/// path; JSON = `{init: {action, path}}` carrying the same two facts.
fn render(outcome: Outcome, path: &Path, mode: OutputMode) -> String {
    match mode {
        OutputMode::Json => serde_json::json!({
            "init": { "action": outcome.token(), "path": path.display().to_string() }
        })
        .to_string(),
        OutputMode::Human => {
            let path = path.display();
            match outcome {
                Outcome::Created => format!("Created {path} with the hydrate pointer."),
                Outcome::Appended => format!("Appended the hydrate pointer to {path}."),
                Outcome::Updated => format!("Updated the hydrate pointer in {path}."),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn agents_path(dir: &TempDir) -> PathBuf {
        dir.path().join(AGENTS_FILE)
    }

    #[test]
    fn creates_agents_md_when_absent() {
        let dir = TempDir::new().unwrap();
        let path = agents_path(&dir);
        assert!(!path.exists());

        let outcome = apply(&path).unwrap();

        assert_eq!(outcome, Outcome::Created);
        let written = fs::read_to_string(&path).unwrap();
        assert!(written.contains(START), "block start missing:\n{written}");
        assert!(written.contains(END), "block end missing:\n{written}");
        assert!(
            written.contains("hydrate guide"),
            "pointer must defer to the guide:\n{written}"
        );
    }

    #[test]
    fn appends_block_and_preserves_existing_content() {
        let dir = TempDir::new().unwrap();
        let path = agents_path(&dir);
        fs::write(&path, "# My project\n\nSome existing agent notes.\n").unwrap();

        let outcome = apply(&path).unwrap();

        assert_eq!(outcome, Outcome::Appended);
        let written = fs::read_to_string(&path).unwrap();
        // The user's content survives verbatim...
        assert!(
            written.contains("# My project"),
            "existing heading lost:\n{written}"
        );
        assert!(
            written.contains("Some existing agent notes."),
            "existing notes lost:\n{written}"
        );
        // ...and the block is there, after a blank-line separator.
        assert!(written.contains(BLOCK), "block not appended:\n{written}");
        assert!(
            written.contains("notes.\n\n<!-- hydrate:start -->"),
            "block should follow a blank line:\n{written}"
        );
    }

    #[test]
    fn updates_block_in_place_leaving_surrounding_content() {
        let dir = TempDir::new().unwrap();
        let path = agents_path(&dir);
        // A file with a STALE block between the user's own content on both sides.
        let stale = format!(
            "# Top matter\n\n{START}\nold stale text that must be replaced\n{END}\n\n## Footer note\n"
        );
        fs::write(&path, &stale).unwrap();

        let outcome = apply(&path).unwrap();

        assert_eq!(outcome, Outcome::Updated);
        let written = fs::read_to_string(&path).unwrap();
        assert!(
            !written.contains("old stale text"),
            "stale block content should be gone:\n{written}"
        );
        assert!(written.contains(BLOCK), "fresh block missing:\n{written}");
        // Surrounding content on both sides is untouched.
        assert!(
            written.contains("# Top matter"),
            "content before the block lost:\n{written}"
        );
        assert!(
            written.contains("## Footer note"),
            "content after the block lost:\n{written}"
        );
        // Exactly one block — no duplication.
        assert_eq!(
            written.matches(START).count(),
            1,
            "block duplicated:\n{written}"
        );
    }

    #[test]
    fn is_idempotent_running_twice_yields_identical_output() {
        let dir = TempDir::new().unwrap();
        let path = agents_path(&dir);
        fs::write(&path, "# Notes\n\nkeep me\n").unwrap();

        apply(&path).unwrap();
        let after_first = fs::read_to_string(&path).unwrap();
        let second = apply(&path).unwrap();
        let after_second = fs::read_to_string(&path).unwrap();

        // The second run recognizes its own block and rewrites it in place.
        assert_eq!(second, Outcome::Updated);
        assert_eq!(
            after_first, after_second,
            "init must be idempotent:\nfirst:\n{after_first}\nsecond:\n{after_second}"
        );
        assert_eq!(after_second.matches(START).count(), 1);
    }

    #[test]
    fn fails_loud_on_an_unwritable_path() {
        let dir = TempDir::new().unwrap();
        // A regular file stands where a directory would need to be, so the child
        // path can never be created — the write must surface an error, not a
        // silent success.
        let blocker = dir.path().join("not-a-dir");
        fs::write(&blocker, "x").unwrap();
        let path = blocker.join(AGENTS_FILE);

        let err = apply(&path).unwrap_err();

        match err {
            CliError::Other(msg) => assert!(
                msg.contains("could not write"),
                "error should name the write failure: {msg}"
            ),
            other => panic!("expected a loud IO error, got {other:?}"),
        }
    }

    /// Assert that `apply` refused to touch the file: it returns an
    /// `InitRefused` error and the file's bytes are exactly what they were.
    fn assert_refused_unchanged(path: &Path, before: &str) {
        let err = apply(path).unwrap_err();
        assert!(
            matches!(err, CliError::InitRefused(_)),
            "expected a loud InitRefused error, got {err:?}"
        );
        let after = fs::read_to_string(path).unwrap();
        assert_eq!(
            before, after,
            "file must be left byte-for-byte unchanged on refusal"
        );
    }

    #[test]
    fn lone_start_marker_refuses_and_leaves_file_untouched() {
        let dir = TempDir::new().unwrap();
        let path = agents_path(&dir);
        let before = format!("# Notes\n\n{START}\ndangling block, no end\n\nmore user text\n");
        fs::write(&path, &before).unwrap();
        assert_refused_unchanged(&path, &before);
    }

    #[test]
    fn lone_end_marker_refuses_and_leaves_file_untouched() {
        let dir = TempDir::new().unwrap();
        let path = agents_path(&dir);
        let before = format!("# Notes\n\nsome text\n{END}\ntrailing user text\n");
        fs::write(&path, &before).unwrap();
        assert_refused_unchanged(&path, &before);
    }

    #[test]
    fn end_before_start_refuses_and_leaves_file_untouched() {
        let dir = TempDir::new().unwrap();
        let path = agents_path(&dir);
        let before = format!("# Notes\n\n{END}\ninverted markers\n{START}\n");
        fs::write(&path, &before).unwrap();
        assert_refused_unchanged(&path, &before);
    }

    #[test]
    fn two_complete_blocks_refuse_and_leave_file_untouched() {
        let dir = TempDir::new().unwrap();
        let path = agents_path(&dir);
        let before = format!("{START}\nfirst\n{END}\n\nmiddle\n\n{START}\nsecond\n{END}\n");
        fs::write(&path, &before).unwrap();
        assert_refused_unchanged(&path, &before);
    }

    #[test]
    fn symlink_at_agents_md_refuses_rather_than_writing_through_it() {
        let dir = TempDir::new().unwrap();
        // A real target elsewhere, and AGENTS.md as a symlink pointing at it.
        let target = dir.path().join("real-target.md");
        let target_before = "# the real file behind the link\n";
        fs::write(&target, target_before).unwrap();
        let path = agents_path(&dir);
        std::os::unix::fs::symlink(&target, &path).unwrap();

        let err = apply(&path).unwrap_err();
        match err {
            CliError::InitRefused(msg) => {
                assert!(
                    msg.contains("symlink"),
                    "message should name the symlink: {msg}"
                );
            }
            other => panic!("expected a loud InitRefused error, got {other:?}"),
        }
        // The link's target is untouched — we did not write through it.
        assert_eq!(fs::read_to_string(&target).unwrap(), target_before);
    }

    #[test]
    fn update_in_place_preserves_surrounding_bytes_exactly() {
        // A stricter idempotency/preservation check: after replacing the block,
        // every byte outside the markers is identical to before.
        let dir = TempDir::new().unwrap();
        let path = agents_path(&dir);
        let before = format!("# Top\n\n{START}\nstale\n{END}\n\n## Bottom\n");
        fs::write(&path, &before).unwrap();

        apply(&path).unwrap();
        let after = fs::read_to_string(&path).unwrap();

        assert!(
            after.starts_with("# Top\n\n"),
            "prefix bytes changed:\n{after}"
        );
        assert!(
            after.ends_with("\n\n## Bottom\n"),
            "suffix bytes changed:\n{after}"
        );
    }

    #[test]
    fn render_human_names_the_action_and_path() {
        let path = Path::new("/tmp/proj/AGENTS.md");
        assert!(render(Outcome::Created, path, OutputMode::Human).contains("Created"));
        assert!(render(Outcome::Appended, path, OutputMode::Human).contains("Appended"));
        assert!(render(Outcome::Updated, path, OutputMode::Human).contains("Updated"));
        assert!(render(Outcome::Created, path, OutputMode::Human).contains("AGENTS.md"));
    }

    #[test]
    fn render_json_carries_action_and_path() {
        let path = Path::new("/tmp/proj/AGENTS.md");
        let out = render(Outcome::Appended, path, OutputMode::Json);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["init"]["action"], "appended");
        assert_eq!(v["init"]["path"], "/tmp/proj/AGENTS.md");
    }
}
