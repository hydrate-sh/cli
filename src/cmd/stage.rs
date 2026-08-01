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
    println!("{}", render(&stage, binding.as_ref(), mode)?);
    if !stage.deltas.is_empty() {
        park(&base, &stage)?;
        Stage::empty().save(&base)?;
    }
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

/// Render the discard report. Returned rather than printed so both modes are
/// directly testable, and built from the same [`OpSummary`] projection `status`
/// and `diff` use — including its nouns — so the three cannot drift on what a
/// stage contains.
///
/// [`OpSummary`]: crate::staging::OpSummary
fn render(stage: &Stage, binding: Option<&Binding>, mode: OutputMode) -> Result<String, CliError> {
    let branch = binding.map(|b| b.branch_name.as_str());
    let summary = crate::staging::summarize(stage, None)?;

    if stage.deltas.is_empty() {
        return Ok(match mode {
            OutputMode::Json => serde_json::json!({
                "discarded": 0,
                "deltas": [],
            })
            .to_string(),
            OutputMode::Human => match branch {
                Some(b) => format!("Nothing staged on branch '{b}'; nothing to discard."),
                None => "Nothing staged; nothing to discard.".to_string(),
            },
        });
    }

    Ok(match mode {
        OutputMode::Json => serde_json::json!({
            "discarded": stage.deltas.len(),
            "recovery_file": format!(".hydrate/{DISCARDED_FILE}"),
            // The deltas verbatim, so an agent can re-stage without reading the
            // recovery file off disk.
            "deltas": stage.deltas,
            "summary": {
                "nodes": summary.nodes, "edges": summary.edges,
                "updates": summary.updates, "deletes": summary.deletes,
                "other": summary.other, "total": summary.total(),
            },
        })
        .to_string(),
        OutputMode::Human => {
            let mut out = String::new();
            // The ops first: this listing is the record of what was thrown away.
            for op in &summary.ops {
                out.push_str(&diff::op_line(op));
                out.push('\n');
            }
            let counts = super::status::staged_counts(&summary);
            match branch {
                Some(b) => out.push_str(&format!(
                    "Discarded {} on branch '{b}': {counts}.",
                    super::status::plural(stage.deltas.len(), "staged operation")
                )),
                None => out.push_str(&format!(
                    "Discarded {}: {counts}.",
                    super::status::plural(stage.deltas.len(), "staged operation")
                )),
            }
            out.push_str(&format!(
                "\nRecoverable from .hydrate/{DISCARDED_FILE} until the next discard."
            ));
            out
        }
    })
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
        let out = render(&Stage::empty(), Some(&binding()), OutputMode::Human).unwrap();
        assert!(out.contains("Nothing staged"), "{out}");
        assert!(out.contains("nothing to discard"), "{out}");

        let json = render(&Stage::empty(), Some(&binding()), OutputMode::Json).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["discarded"], 0, "{json}");
    }

    #[test]
    fn human_output_lists_the_operations_not_just_counts() {
        // The listing is the ONLY record of the discarded work: nothing was
        // committed, so no server copy exists. "1 node" does not tell you what
        // you lost; `+ behavior Rater` does.
        let out = render(&staged(), Some(&binding()), OutputMode::Human).unwrap();
        assert!(
            out.contains("Rater"),
            "op list missing the node name:\n{out}"
        );
        assert!(
            out.contains("Score it."),
            "op list dropped the authored description, which exists nowhere else:\n{out}"
        );
        assert!(out.contains("demo"), "{out}");
    }

    #[test]
    fn human_output_uses_the_same_nouns_as_status() {
        // `status`, `diff` and this verb read one projection; if they disagree
        // on what a stage contains, one of them is lying about the same file.
        let stage = staged();
        let summary = crate::staging::summarize(&stage, None).unwrap();
        let counts = super::super::status::staged_counts(&summary);
        let out = render(&stage, Some(&binding()), OutputMode::Human).unwrap();
        assert!(
            out.contains(&counts),
            "discard counts {counts:?} not found in:\n{out}"
        );
    }

    #[test]
    fn json_echoes_the_deltas_so_an_agent_can_restage() {
        let json = render(&staged(), Some(&binding()), OutputMode::Json).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["discarded"], 1, "{json}");
        let deltas = v["deltas"].as_array().expect("deltas array");
        assert_eq!(deltas.len(), 1, "{json}");
        assert_eq!(deltas[0]["type"], "add_node", "{json}");
        // The authored description must survive into the echo, or the recovery
        // path loses the same thing the human listing exists to preserve.
        assert_eq!(
            deltas[0]["node"]["data"]["description"], "Score it.",
            "{json}"
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
        let human = render(&staged(), Some(&binding()), OutputMode::Human).unwrap();
        assert!(human.contains(DISCARDED_FILE), "{human}");
        let json = render(&staged(), Some(&binding()), OutputMode::Json).unwrap();
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
