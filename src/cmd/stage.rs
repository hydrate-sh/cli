//! `stage discard` — throw away the staged changeset, locally.
//!
//! Purely local: no network call, no branch mutation, nothing on the server
//! changes. It touches exactly one file, `.hydrate/stage.json`, and leaves the
//! binding and the pulled index alone — that directory also holds the binding
//! and a large index, and sits beside whatever else is in the working copy.
//!
//! The discarded work exists nowhere else: nothing was committed, so there is no
//! server copy to recover from. Two consequences shape the design.
//!
//! * The full operation list prints **before** the delete, through the same
//!   renderer `diff` uses. Counts are not a record; `+ node Api.Rater` is. What
//!   scrolls past is the only trace left in a terminal or an agent transcript.
//! * The old stage is copied to `.hydrate/stage.discarded.json` (a single slot,
//!   overwritten each time) so a mistake is recoverable, and `--json` echoes the
//!   discarded deltas so an agent can re-stage them.
//!
//! There is no confirmation prompt. This CLI is driven non-interactively and a
//! prompt would break piping, so the mitigation is recoverability, not friction.

use std::path::Path;

use super::context::require_workdir;
use super::diff;
use crate::error::CliError;
use crate::output::OutputMode;
use crate::state::{Binding, Stage};

/// The file the previous stage is parked in, so a discard is recoverable.
pub const DISCARDED_FILE: &str = "stage.discarded.json";

pub fn discard(mode: OutputMode) -> Result<(), CliError> {
    let base = require_workdir()?;
    let binding = Binding::load(&base)?;
    let stage = Stage::load(&base)?;
    // Through `summarize_workdir`, which loads the pulled index — the same call
    // `status` and `diff` make. Summarizing without it fails on any delta whose
    // rendering needs a lookup, and a staged edge deletion is exactly that:
    //
    //     hydrate: a staged edge deletion targets an edge that isn't in the
    //     pulled index
    //
    // So `stage discard` could not discard the one stage a reader is most
    // likely to want gone — the deletion they made a moment ago — and left the
    // work staged. Found by using the verb, not by reading it.
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
    park(&base, &stage)?;
    Stage::empty().save(&base)?;

    println!("{}", render_done(&stage, &summary, binding.as_ref(), mode));
    Ok(())
}

/// Copy the outgoing stage to the recovery slot. A failure here must NOT be
/// swallowed: silently discarding the only copy of the user's authored work is
/// precisely the outcome the slot exists to prevent, so the delete never happens
/// if the park fails.
fn park(base: &Path, stage: &Stage) -> Result<(), CliError> {
    let body = serde_json::to_string_pretty(stage)
        .map_err(|e| CliError::State(format!("could not serialize the stage: {e}")))?;
    crate::state::write_state_file(base, DISCARDED_FILE, body.as_bytes())
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
        std::fs::write(base.join(".hydrate/index.json"), r#"{"version":1}"#).unwrap();
        std::fs::write(base.join(".env"), "HYD_API_KEY=secret").unwrap();

        let stage = Stage::load(base).unwrap();
        park(base, &stage).unwrap();
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
        park(base, &original).unwrap();

        let raw = std::fs::read_to_string(base.join(".hydrate").join(DISCARDED_FILE)).unwrap();
        let recovered: Stage = serde_json::from_str(&raw).expect("parked file is a valid stage");
        assert_eq!(recovered.deltas, original.deltas);
        assert_eq!(recovered.aliases, original.aliases);
    }
}
