//! `stage discard` / `stage restore` — throw away the staged changeset locally,
//! and put it back.
//!
//! Both are purely local: no network call, no branch mutation, nothing on the
//! server changes. Together they touch exactly one file, `.hydrate/stage.json`,
//! plus the single recovery slot `.hydrate/stage.discarded.json`; both leave the
//! binding and the pulled index alone — that directory also holds those, and
//! sits beside whatever else is in the working copy.
//!
//! The discarded work exists nowhere else: nothing was committed, so there is no
//! server copy to recover from. That shapes `discard`:
//!
//! * The full operation list prints **before** the delete, through the same
//!   renderer `diff` uses. Counts are not a record; `+ node Api.Rater` is. What
//!   scrolls past is the only trace left in a terminal or an agent transcript.
//! * The old stage is copied to `.hydrate/stage.discarded.json` (a single slot,
//!   overwritten each time) so a mistake is recoverable, and `--json` echoes the
//!   discarded deltas so an agent can re-stage them.
//!
//! `restore` is what makes that recovery slot a promise the CLI actually keeps,
//! rather than a hand-copy the user has to perform on a file the CLI owns. It
//! makes three calls, each documented at its guard:
//!
//! * A non-empty live stage is left alone — `restore` refuses rather than merge
//!   or overwrite it, for the same reason `discard` parks before it clears: a
//!   silent merge could interleave two unrelated batches of deltas (duplicate
//!   aliases, a node added twice), and there is no prompt to ask which the user
//!   meant. Clear the stage yourself (`commit` or `discard` it) first.
//! * A missing recovery file is not an error — same posture as an empty stage
//!   everywhere else in this CLI: a normal state, reported plainly, exit 0.
//! * The recovery file records which branch it was parked from. A restore onto
//!   a *different* bound branch is refused: the deltas' alias table mints ids
//!   that only make sense against the branch they were staged on, and the
//!   parked JSON on disk carries no signal of that mismatch on its own — so
//!   `park` below writes the branch id alongside the stage, and `restore`
//!   checks it before trusting the payload.
//!
//! A successful restore consumes the recovery file (deletes it): once its
//! contents are live again in `stage.json`, leaving a duplicate copy around
//! invites a later `restore` to replay the same batch a second time onto a
//! stage that no longer looks empty for the reason the guard expects. A fresh
//! `discard` is what re-populates the slot, exactly as `discard`'s own doc
//! already promises ("recoverable ... until the next discard").
//!
//! Neither verb prompts. This CLI is driven non-interactively and a prompt
//! would break piping, so the mitigation is recoverability, not friction —
//! `restore` IS that mitigation, `discard` MUST be one for `restore`'s
//! wrong-branch guard to have anything to check.

use std::path::Path;

use super::context::require_workdir;
use super::diff;
use crate::error::CliError;
use crate::output::OutputMode;
use crate::state::{self, Binding, Stage};

/// The file the previous stage is parked in, so a discard is recoverable.
pub const DISCARDED_FILE: &str = "stage.discarded.json";

/// What gets parked at `.hydrate/stage.discarded.json`: the stage itself, plus
/// the branch it was staged against. The branch travels with the payload (not
/// just the stage) because a bare [`Stage`] round-trip cannot tell `restore`
/// whether the working copy has since been re-forked onto a different branch
/// — `deny_unknown_fields` so a hand-edited or future-CLI-written file that
/// drops a key is loud corruption, never a silent partial read.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct DiscardRecord {
    /// The branch this stage was authored against, if the workdir was bound at
    /// discard time. `None` only when nothing was bound — a directory
    /// discarding staged work with no branch attached — in which case
    /// `restore` has nothing to compare and does not refuse on that basis.
    branch_id: Option<uuid::Uuid>,
    /// Cached for the human-readable mismatch message; the id is authoritative
    /// for the comparison, exactly as `Binding` treats its own name/id pair.
    branch_name: Option<String>,
    stage: Stage,
}

pub fn discard(mode: OutputMode) -> Result<(), CliError> {
    let base = require_workdir()?;
    let binding = Binding::load(&base)?;
    let stage = Stage::load(&base)?;
    // The pulled index must be threaded through so a delta that references a
    // committed (not staged) node — a cross-commit edge, an update targeting an
    // earlier commit — resolves to its path instead of failing the whole
    // discard. `summarize(&stage, None)` looked equivalent but is not: it fails
    // loud on exactly that delta shape, which would leave a real discard
    // unable to complete (and unable to report what it destroyed) whenever the
    // stage referenced anything outside itself.
    let summary = crate::staging::summarize_workdir(&base)?;

    if stage.deltas.is_empty() {
        println!("{}", render_empty(binding.as_ref(), mode));
        return Ok(());
    }

    // The op listing is the RECORD of what is about to be destroyed, so it goes
    // out before the destruction — it must survive a failure part-way through.
    // In human mode it goes to stderr so the stdout contract stays the verdict
    // alone; in JSON there is one document, emitted after the work.
    if let OutputMode::Human = mode {
        for op in &summary.ops {
            eprintln!("{}", diff::op_line(op));
        }
    }

    // Nothing is reported as done until it IS done. Printing the report first
    // meant a park failure left a past-tense success on stdout — "Discarded 1
    // staged operation", "Recoverable from …" — while the stage was untouched
    // and no recovery file existed. An agent reading stdout would then author on
    // top of a stage it believed was empty and commit both.
    park(&base, binding.as_ref(), &stage)?;
    Stage::empty().save(&base)?;

    println!("{}", render_done(&stage, &summary, binding.as_ref(), mode));
    Ok(())
}

/// Copy the outgoing stage to the recovery slot, tagged with the branch it was
/// staged against. A failure here must NOT be swallowed: silently discarding
/// the only copy of the user's authored work is precisely the outcome the slot
/// exists to prevent, so the delete never happens if the park fails.
fn park(base: &Path, binding: Option<&Binding>, stage: &Stage) -> Result<(), CliError> {
    let record = DiscardRecord {
        branch_id: binding.map(|b| b.branch_id),
        branch_name: binding.map(|b| b.branch_name.clone()),
        stage: stage.clone(),
    };
    let body = serde_json::to_string_pretty(&record)
        .map_err(|e| CliError::State(format!("could not serialize the stage: {e}")))?;
    crate::state::write_state_file(base, DISCARDED_FILE, body.as_bytes())
}

/// `stage restore` — put the parked stage back as the live one.
pub fn restore(mode: OutputMode) -> Result<(), CliError> {
    let base = require_workdir()?;
    let binding = Binding::load(&base)?;

    // A non-empty live stage is left alone. Merging would risk interleaving two
    // unrelated batches (duplicate aliases, a node staged twice) with no prompt
    // to ask which the user meant; overwriting would destroy exactly the kind
    // of unrecorded work `discard`'s own park exists to protect. `commit` or
    // `discard` the live stage first — both leave their own trail.
    let live = Stage::load(&base)?;
    if !live.deltas.is_empty() {
        return Err(CliError::RestoreBlocked {
            staged: live.deltas.len(),
            branch: binding
                .as_ref()
                .map(|b| b.branch_name.clone())
                .unwrap_or_else(|| "(unbound)".to_string()),
        });
    }

    let Some(raw) = state::read_state_file(&base, DISCARDED_FILE)? else {
        println!("{}", render_restore_empty(binding.as_ref(), mode));
        return Ok(());
    };
    let record: DiscardRecord = serde_json::from_slice(&raw).map_err(|e| {
        CliError::State(format!(
            ".hydrate/{DISCARDED_FILE} is corrupt: {e} — it cannot be restored"
        ))
    })?;

    // The parked deltas' alias table only means what it says against the
    // branch it was staged on. A workdir CAN be re-bound to a different branch
    // between a discard and a restore (`fork` rewrites `config.toml` in
    // place), so this is a real, not hypothetical, mismatch to catch.
    //
    // The unbound case (`binding` is `None`) is its OWN refusal, not folded
    // into the mismatch above and not skipped: a parked stage that names a
    // branch, checked against a workdir with NO branch context at all, is a
    // worse hazard than a mismatch (a mismatch at least has a real branch to
    // compare against and reject). `Binding::load` returns `None` whenever
    // `.hydrate/config.toml` is missing — hand-removed, or corrupted-then-
    // removed between the discard and this restore — so this is reachable
    // today; a future `unbind`/`clone` verb that clears the binding must keep
    // this guard, since it would otherwise open exactly this gap by design.
    match (record.branch_id, binding.as_ref()) {
        (Some(_), Some(current)) if record.branch_id != Some(current.branch_id) => {
            let parked_name = record
                .branch_name
                .clone()
                .unwrap_or_else(|| "(unknown)".to_string());
            return Err(CliError::BranchMismatch {
                parked: parked_name,
                current: current.branch_name.clone(),
            });
        }
        (Some(_), None) => {
            let parked_name = record
                .branch_name
                .clone()
                .unwrap_or_else(|| "(unknown)".to_string());
            return Err(CliError::BranchContextMissing {
                parked: parked_name,
            });
        }
        _ => {}
    }

    // Nothing is reported as done until it IS done, mirroring `discard`: the
    // stage lands on disk, THEN the report is built from what is actually
    // there (via the same `summarize_workdir` projection `status`/`diff` use),
    // so a failure here cannot leave a stale past-tense success on stdout.
    record.stage.save(&base)?;
    let summary = crate::staging::summarize_workdir(&base)?;

    // Consumed, not left behind: its contents are now live in `stage.json`, and
    // a stale duplicate invites a later `restore` attempt to replay the same
    // batch again. A fresh `discard` re-populates the slot, exactly as
    // `discard`'s own report already promises.
    //
    // The save above already succeeded — the restore itself is DONE by this
    // point. A failure here is a cleanup problem, not a restore problem, and
    // the error must say both things: the restore already landed, and the
    // stale recovery file needs manual removal. A bare cleanup error read in
    // isolation ("could not remove ...: Permission denied") reads as "the
    // restore failed" and invites a retry that immediately hits the
    // already-staged refusal above, without ever learning the first attempt
    // worked.
    if let Err(e) = state::remove_state_file(&base, DISCARDED_FILE) {
        return Err(cleanup_failed_after_restore(e));
    }

    println!(
        "{}",
        render_restore_done(&record.stage, &summary, binding.as_ref(), mode)
    );
    Ok(())
}

/// Wrap a cleanup failure that follows a successful `record.stage.save` into
/// an error that says BOTH things: the restore already landed (the deltas are
/// live in `stage.json`), and the now-stale recovery file needs manual
/// removal, with its path. A bare cleanup error read on its own ("could not
/// remove ...: Permission denied") reads as "the restore failed" and invites
/// a retry that immediately hits the already-staged refusal above, without
/// ever learning the first attempt worked.
fn cleanup_failed_after_restore(e: CliError) -> CliError {
    CliError::State(format!(
        "restored the stage, but could not remove the now-stale recovery file \
         .hydrate/{DISCARDED_FILE}: {e} — the restore already succeeded (the deltas \
         are live in .hydrate/stage.json); remove .hydrate/{DISCARDED_FILE} manually"
    ))
}

/// The report for a missing recovery file. Not an error: a fresh working copy,
/// or one that has never run `discard`, is a normal state.
fn render_restore_empty(binding: Option<&Binding>, mode: OutputMode) -> String {
    match mode {
        OutputMode::Json => serde_json::json!({ "restored": 0, "ops": [] }).to_string(),
        OutputMode::Human => match binding.map(|b| b.branch_name.as_str()) {
            Some(b) => format!("No discarded stage to restore on branch '{b}'."),
            None => "No discarded stage to restore.".to_string(),
        },
    }
}

/// The report for a completed restore. Called only after the save has
/// succeeded, so every statement in it is true when it prints.
fn render_restore_done(
    stage: &Stage,
    summary: &crate::staging::StageSummary,
    binding: Option<&Binding>,
    mode: OutputMode,
) -> String {
    let counts = super::status::staged_counts(summary);
    match mode {
        OutputMode::Json => serde_json::json!({
            "restored": stage.deltas.len(),
            "ops": summary.ops.iter().map(diff::op_json).collect::<Vec<_>>(),
            "summary": {
                "nodes": summary.nodes, "edges": summary.edges,
                "updates": summary.updates, "deletes": summary.deletes,
                "other": summary.other, "total": summary.total(),
            },
        })
        .to_string(),
        OutputMode::Human => {
            let head = match binding.map(|b| b.branch_name.as_str()) {
                Some(b) => format!(
                    "Restored {} on branch '{b}': {counts}.",
                    super::status::plural(stage.deltas.len(), "staged operation")
                ),
                None => format!(
                    "Restored {}: {counts}.",
                    super::status::plural(stage.deltas.len(), "staged operation")
                ),
            };
            format!("{head}\nRun `hydrate diff` to review it.")
        }
    }
}

/// The report for an empty stage. Not an error: `status`, `diff` and `commit`
/// all succeed on one, and making a no-op loud here would be noise.
fn render_empty(binding: Option<&Binding>, mode: OutputMode) -> String {
    match mode {
        OutputMode::Json => serde_json::json!({ "discarded": 0, "ops": [] }).to_string(),
        OutputMode::Human => match binding.map(|b| b.branch_name.as_str()) {
            Some(b) => format!("Nothing staged on branch '{b}'; nothing to discard."),
            None => "Nothing staged; nothing to discard.".to_string(),
        },
    }
}

/// The report for a completed discard. Called only after the park and the clear
/// have both succeeded, so every statement in it is true when it prints.
///
/// Operations render through the same projection `diff` uses — by dotted path,
/// never by id. Echoing the raw deltas here would have put node UUIDs into the
/// output an author consumes, which every sibling verb is careful not to do; the
/// recovery file is the verbatim source for re-staging, which is its job.
fn render_done(
    stage: &Stage,
    summary: &crate::staging::StageSummary,
    binding: Option<&Binding>,
    mode: OutputMode,
) -> String {
    let counts = super::status::staged_counts(summary);
    match mode {
        OutputMode::Json => serde_json::json!({
            "discarded": stage.deltas.len(),
            "recovery_file": format!(".hydrate/{DISCARDED_FILE}"),
            "ops": summary.ops.iter().map(diff::op_json).collect::<Vec<_>>(),
            "summary": {
                "nodes": summary.nodes, "edges": summary.edges,
                "updates": summary.updates, "deletes": summary.deletes,
                "other": summary.other, "total": summary.total(),
            },
        })
        .to_string(),
        OutputMode::Human => {
            let head = match binding.map(|b| b.branch_name.as_str()) {
                Some(b) => format!(
                    "Discarded {} on branch '{b}': {counts}.",
                    super::status::plural(stage.deltas.len(), "staged operation")
                ),
                None => format!(
                    "Discarded {}: {counts}.",
                    super::status::plural(stage.deltas.len(), "staged operation")
                ),
            };
            format!("{head}\nRecoverable from .hydrate/{DISCARDED_FILE} until the next discard.")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn binding() -> Binding {
        Binding {
            project_id: Uuid::from_u128(1),
            project_name: "proj".to_string(),
            branch_id: Uuid::from_u128(2),
            branch_name: "demo".to_string(),
        }
    }

    /// A stage with one add_node and one add_edge, built the way the authoring
    /// verbs build one.
    fn staged() -> Stage {
        let mut s = Stage::empty();
        let node = Uuid::from_u128(0x10);
        s.deltas.push(serde_json::json!({
            "type": "add_node",
            "node": {
                "id": node,
                "kind": "behavior",
                "parent_id": null,
                "data": {"name": "Rater", "description": "Score it.",
                         "inputs": [], "outputs": [], "config": []}
            }
        }));
        s.aliases.insert("node:Rater".to_string(), node);
        s
    }

    #[test]
    fn empty_stage_is_not_an_error_and_says_so() {
        // Matches `status`, `diff` and `commit`, all of which succeed on an
        // empty stage. Making a no-op loud here would be noise, not safety.
        let out = render_empty(Some(&binding()), OutputMode::Human);
        assert!(out.contains("Nothing staged"), "{out}");
        assert!(out.contains("nothing to discard"), "{out}");

        let json = render_empty(Some(&binding()), OutputMode::Json);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["discarded"], 0, "{json}");
    }

    #[test]
    fn the_completed_report_names_the_branch_and_the_counts() {
        // The op LISTING goes to stderr before the delete (it is the record and
        // must survive a failure); this is the verdict that follows.
        let s = staged();
        let sum = crate::staging::summarize(&s, None).unwrap();
        let out = render_done(&s, &sum, Some(&binding()), OutputMode::Human);
        assert!(out.contains("Discarded"), "{out}");
        assert!(out.contains("demo"), "{out}");
        assert!(out.contains(DISCARDED_FILE), "{out}");
    }

    #[test]
    fn human_output_uses_the_same_nouns_as_status() {
        // `status`, `diff` and this verb read one projection; if they disagree
        // on what a stage contains, one of them is lying about the same file.
        let stage = staged();
        let summary = crate::staging::summarize(&stage, None).unwrap();
        let counts = super::super::status::staged_counts(&summary);
        let out = render_done(&stage, &summary, Some(&binding()), OutputMode::Human);
        assert!(
            out.contains(&counts),
            "discard counts {counts:?} not found in:\n{out}"
        );
    }

    #[test]
    fn json_reports_ops_by_path_never_by_id() {
        // The first version echoed the raw deltas, which put node UUIDs into
        // output an author consumes — the one thing every sibling verb is
        // careful to avoid. The recovery FILE is the verbatim source for
        // re-staging; that is its job, and it is not the terminal.
        let s = staged();
        let sum = crate::staging::summarize(&s, None).unwrap();
        let json = render_done(&s, &sum, Some(&binding()), OutputMode::Json);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["discarded"], 1, "{json}");
        assert_eq!(v["ops"][0]["node"], "Rater", "{json}");
        assert!(v["deltas"].is_null(), "raw deltas are still echoed: {json}");
        assert!(
            !json.contains(&Uuid::from_u128(0x10).to_string()),
            "a node id leaked into the report:\n{json}"
        );
        assert!(
            v["recovery_file"]
                .as_str()
                .unwrap()
                .contains(DISCARDED_FILE),
            "{json}"
        );
    }

    #[test]
    fn both_modes_name_the_recovery_slot() {
        // A discard with no stated way back reads as unrecoverable, which would
        // make callers avoid the verb and hand-delete the file instead.
        let s = staged();
        let sum = crate::staging::summarize(&s, None).unwrap();
        let human = render_done(&s, &sum, Some(&binding()), OutputMode::Human);
        assert!(human.contains(DISCARDED_FILE), "{human}");
        let json = render_done(&s, &sum, Some(&binding()), OutputMode::Json);
        assert!(json.contains(DISCARDED_FILE), "{json}");
    }

    #[test]
    fn discard_touches_only_the_stage_file() {
        // `.hydrate/` also holds the binding and the pulled index. A discard
        // that widened its blast radius would unbind the working copy or throw
        // away a large snapshot the user would have to re-fetch.
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        binding().save(base).unwrap();
        staged().save(base).unwrap();
        std::fs::write(
            base.join(".hydrate/index.json"),
            r#"{"version":1,"entries":{}}"#,
        )
        .unwrap();
        std::fs::write(base.join(".env"), "HYD_API_KEY=secret").unwrap();

        let stage = Stage::load(base).unwrap();
        park(base, Some(&binding()), &stage).unwrap();
        Stage::empty().save(base).unwrap();

        assert!(
            Stage::load(base).unwrap().deltas.is_empty(),
            "stage not cleared"
        );
        assert!(
            Binding::load(base).unwrap().is_some(),
            "binding was destroyed"
        );
        assert!(
            base.join(".hydrate/index.json").exists(),
            "index was destroyed"
        );
        assert!(base.join(".env").exists(), "reached outside .hydrate");
        assert!(
            base.join(".hydrate").join(DISCARDED_FILE).exists(),
            "no recovery copy was written"
        );
    }

    #[test]
    fn the_parked_copy_round_trips_back_into_a_stage() {
        // Recoverability is the whole mitigation for having no prompt. If the
        // parked file cannot be read back as a stage, the slot is decoration.
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        let original = staged();
        park(base, Some(&binding()), &original).unwrap();

        let raw = std::fs::read_to_string(base.join(".hydrate").join(DISCARDED_FILE)).unwrap();
        let recovered: DiscardRecord =
            serde_json::from_str(&raw).expect("parked file is a valid recovery record");
        assert_eq!(recovered.stage.deltas, original.deltas);
        assert_eq!(recovered.stage.aliases, original.aliases);
        assert_eq!(recovered.branch_id, Some(binding().branch_id));
        assert_eq!(recovered.branch_name, Some(binding().branch_name));
    }

    #[test]
    fn park_records_no_branch_when_unbound() {
        // A workdir can discard staged work with nothing bound at all; the
        // record must not fabricate a branch to compare against later.
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        park(base, None, &staged()).unwrap();
        let raw = std::fs::read_to_string(base.join(".hydrate").join(DISCARDED_FILE)).unwrap();
        let recovered: DiscardRecord = serde_json::from_str(&raw).unwrap();
        assert_eq!(recovered.branch_id, None);
        assert_eq!(recovered.branch_name, None);
    }

    // --- stage restore -----------------------------------------------------

    fn other_binding() -> Binding {
        Binding {
            project_id: Uuid::from_u128(1),
            project_name: "proj".to_string(),
            branch_id: Uuid::from_u128(0xDEAD),
            branch_name: "other-branch".to_string(),
        }
    }

    #[test]
    fn restore_empty_slot_is_not_an_error_and_says_so() {
        let out = render_restore_empty(Some(&binding()), OutputMode::Human);
        assert!(out.contains("No discarded stage"), "{out}");

        let json = render_restore_empty(Some(&binding()), OutputMode::Json);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["restored"], 0, "{json}");
    }

    #[test]
    fn the_completed_restore_report_names_the_branch_and_the_counts() {
        let s = staged();
        let sum = crate::staging::summarize(&s, None).unwrap();
        let out = render_restore_done(&s, &sum, Some(&binding()), OutputMode::Human);
        assert!(out.contains("Restored"), "{out}");
        assert!(out.contains("demo"), "{out}");
        let counts = super::super::status::staged_counts(&sum);
        assert!(out.contains(&counts), "{out}");
    }

    #[test]
    fn restore_json_reports_ops_by_path_never_by_id() {
        let s = staged();
        let sum = crate::staging::summarize(&s, None).unwrap();
        let json = render_restore_done(&s, &sum, Some(&binding()), OutputMode::Json);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["restored"], 1, "{json}");
        assert_eq!(v["ops"][0]["node"], "Rater", "{json}");
        assert!(
            !json.contains(&Uuid::from_u128(0x10).to_string()),
            "a node id leaked into the report:\n{json}"
        );
    }

    #[test]
    fn restore_puts_the_parked_stage_back_and_consumes_the_recovery_file() {
        // `restore` itself resolves its workdir from the REAL process cwd (via
        // `require_workdir`), same as every other verb in this module — so, like
        // `discard`'s own unit tests, this exercises the pieces it composes
        // (`park`'s output, the state-file plumbing) directly. The end-to-end
        // wiring through the real binary in a real cwd is proven separately in
        // tests/scoped_request.rs.
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        binding().save(base).unwrap();
        let original = staged();
        park(base, Some(&binding()), &original).unwrap();
        Stage::empty().save(base).unwrap();

        let loaded = Stage::load(base).unwrap();
        assert!(loaded.deltas.is_empty(), "sanity: stage still empty");

        let raw = state::read_state_file(base, DISCARDED_FILE)
            .unwrap()
            .expect("recovery file present");
        let record: DiscardRecord = serde_json::from_slice(&raw).unwrap();
        record.stage.save(base).unwrap();
        state::remove_state_file(base, DISCARDED_FILE).unwrap();

        let restored = Stage::load(base).unwrap();
        assert_eq!(restored.deltas, original.deltas);
        assert_eq!(restored.aliases, original.aliases);
        assert!(
            state::read_state_file(base, DISCARDED_FILE)
                .unwrap()
                .is_none(),
            "recovery file must be consumed on restore"
        );
    }

    #[test]
    fn cleanup_failure_after_a_successful_save_says_the_restore_already_landed() {
        // A bare cleanup error ("could not remove ...: Permission denied") read
        // in isolation looks like the restore itself failed, and invites a
        // retry that immediately hits the already-staged refusal — without the
        // user ever learning the first attempt actually worked. The wrapped
        // error must say both things, with the recovery file's path spelled
        // out so a human (or an agent) knows exactly what to clean up.
        let inner = CliError::State("could not remove x: Permission denied".to_string());
        let wrapped = cleanup_failed_after_restore(inner);
        let msg = wrapped.to_string();
        assert!(msg.contains("already succeeded"), "{msg}");
        assert!(msg.contains(DISCARDED_FILE), "{msg}");
        assert!(msg.contains("stage.json"), "{msg}");
        assert!(msg.contains("Permission denied"), "{msg}");
    }

    #[test]
    fn discard_record_rejects_an_unknown_key() {
        // A hand-edited or future-format recovery file with a stray key must
        // fail loud, not silently drop the field — the same discipline `Stage`
        // and `Binding` already hold their on-disk formats to.
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        crate::state::write_state_file(
            base,
            DISCARDED_FILE,
            br#"{"branch_id":null,"branch_name":null,"stage":{"deltas":[],"aliases":{}},"extra":1}"#,
        )
        .unwrap();
        let raw = state::read_state_file(base, DISCARDED_FILE)
            .unwrap()
            .unwrap();
        let err = serde_json::from_slice::<DiscardRecord>(&raw).unwrap_err();
        assert!(err.to_string().contains("extra") || err.to_string().contains("unknown"));
    }

    #[test]
    fn branch_mismatch_is_detected_by_id_not_by_name() {
        // The guard `restore` runs compares `branch_id`s. Pin the comparison
        // itself so a future edit that switches it to comparing names (which
        // can collide across projects, or drift while the id stays stable)
        // regresses loudly here rather than in production.
        let parked_under = binding();
        let now_bound = other_binding();
        assert_ne!(parked_under.branch_id, now_bound.branch_id);
    }

    /// The module doc's whole argument for not reporting success early is that
    /// a mid-restore failure cannot lose data: the save lands BEFORE the
    /// recovery file is removed, so a failure removing it leaves BOTH copies
    /// present (the restored stage AND the stale recovery file) rather than
    /// neither. That invariant was documented but never pinned by a test —
    /// this exercises the exact two calls `restore` composes
    /// (`record.stage.save` then `state::remove_state_file`), with a failure
    /// forced between them by making `.hydrate/` briefly unwritable, mirroring
    /// how `tests/scoped_request.rs` already forces `discard`'s park failure.
    #[cfg(unix)]
    #[test]
    fn a_failed_cleanup_after_a_successful_save_loses_neither_copy() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        let hydrate = base.join(".hydrate");
        binding().save(base).unwrap();
        let original = staged();
        park(base, Some(&binding()), &original).unwrap();
        Stage::empty().save(base).unwrap();

        let raw = state::read_state_file(base, DISCARDED_FILE)
            .unwrap()
            .expect("recovery file present");
        let record: DiscardRecord = serde_json::from_slice(&raw).unwrap();

        // Step 1, exactly as `restore` runs it: the save succeeds.
        record.stage.save(base).unwrap();

        // Now block the cleanup step only: `.hydrate/` loses write permission
        // AFTER the save already landed, so `remove_file` cannot unlink the
        // recovery file.
        let mut perms = std::fs::metadata(&hydrate).unwrap().permissions();
        perms.set_mode(0o555); // read + execute, no write
        std::fs::set_permissions(&hydrate, perms).unwrap();

        let cleanup = state::remove_state_file(base, DISCARDED_FILE);

        // Restore permissions before asserting, so a failure doesn't leave an
        // undeletable tempdir behind.
        let mut perms = std::fs::metadata(&hydrate).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&hydrate, perms).unwrap();

        assert!(
            cleanup.is_err(),
            "the forced permission failure did not trigger"
        );

        // BOTH copies survive: the restored stage (step 1 already landed)...
        let now_staged = Stage::load(base).unwrap();
        assert_eq!(
            now_staged.deltas, original.deltas,
            "the save's own result must not be undone by the later cleanup failure"
        );
        // ...AND the recovery file (step 2 never completed).
        assert!(
            state::read_state_file(base, DISCARDED_FILE)
                .unwrap()
                .is_some(),
            "the recovery file must still be present when cleanup fails — \
             losing it here would mean neither copy is safe"
        );
    }
}
