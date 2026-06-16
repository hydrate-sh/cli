//! `status` — show the bound branch and a count of staged operations.

use super::context::require_workdir;
use crate::error::CliError;
use crate::output::OutputMode;
use crate::staging::{summarize_workdir, StageSummary};
use crate::state::Binding;

pub fn run(mode: OutputMode) -> Result<(), CliError> {
    let base = require_workdir()?;
    let binding = Binding::load(&base)?;
    let summary = summarize_workdir(&base)?;
    println!("{}", render(binding.as_ref(), &summary, mode));
    Ok(())
}

fn render(binding: Option<&Binding>, summary: &StageSummary, mode: OutputMode) -> String {
    match mode {
        OutputMode::Json => serde_json::json!({
            "branch": binding.map(|b| b.branch_name.clone()),
            "project": binding.map(|b| b.project_name.clone()),
            "staged": {
                "nodes": summary.nodes,
                "edges": summary.edges,
                "other": summary.other,
                "total": summary.total(),
            }
        })
        .to_string(),
        OutputMode::Human => {
            let mut out = match binding {
                Some(b) => format!(
                    "On branch '{}' (project '{}').",
                    b.branch_name, b.project_name
                ),
                None => "Not bound to a branch.".to_string(),
            };
            if summary.is_empty() {
                out.push_str("\nNothing staged.");
            } else {
                let mut parts = vec![plural(summary.nodes, "node"), plural(summary.edges, "edge")];
                // Only surfaces for a delta kind this version doesn't itemize;
                // shown so the displayed counts never silently undershoot total.
                if summary.other > 0 {
                    parts.push(plural(summary.other, "other op"));
                }
                out.push_str(&format!("\nStaged: {}.", parts.join(", ")));
            }
            out
        }
    }
}

fn plural(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("{n} {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::staging::OpSummary;
    use uuid::Uuid;

    fn binding() -> Binding {
        Binding {
            project_id: Uuid::from_u128(1),
            project_name: "hotdog-rater".to_string(),
            branch_id: Uuid::from_u128(2),
            branch_name: "spicy".to_string(),
        }
    }

    fn summary(nodes: usize, edges: usize) -> StageSummary {
        // `status` reads only the counts + `total()`; fill `ops` to the right
        // length with placeholders so `total()` matches.
        StageSummary {
            nodes,
            edges,
            other: 0,
            ops: vec![
                OpSummary::Other {
                    kind: String::new()
                };
                nodes + edges
            ],
        }
    }

    #[test]
    fn human_shows_branch_and_counts() {
        let out = render(Some(&binding()), &summary(2, 1), OutputMode::Human);
        assert!(out.contains("branch 'spicy'"), "{out}");
        assert!(out.contains("project 'hotdog-rater'"), "{out}");
        assert!(out.contains("2 nodes, 1 edge"), "{out}");
    }

    #[test]
    fn human_reports_empty_stage_and_no_binding() {
        let out = render(None, &summary(0, 0), OutputMode::Human);
        assert!(out.contains("Not bound"), "{out}");
        assert!(out.contains("Nothing staged"), "{out}");
    }

    #[test]
    fn json_carries_branch_and_counts() {
        let out = render(Some(&binding()), &summary(2, 1), OutputMode::Json);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["branch"], "spicy");
        assert_eq!(v["project"], "hotdog-rater");
        assert_eq!(v["staged"]["nodes"], 2);
        assert_eq!(v["staged"]["edges"], 1);
        assert_eq!(v["staged"]["total"], 3);
    }

    #[test]
    fn json_branch_is_null_when_unbound() {
        let out = render(None, &summary(0, 0), OutputMode::Json);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(v["branch"].is_null());
        assert!(v["project"].is_null());
        assert_eq!(v["staged"]["total"], 0);
    }

    // A stage of only not-itemized ops must not read as "0 nodes, 0 edges" while
    // claiming something is staged — the `other` count is surfaced in both modes.
    #[test]
    fn other_ops_are_surfaced_not_hidden() {
        let summary = StageSummary {
            nodes: 0,
            edges: 0,
            other: 2,
            ops: vec![
                OpSummary::Other {
                    kind: "delete_node".to_string()
                };
                2
            ],
        };
        let human = render(Some(&binding()), &summary, OutputMode::Human);
        assert!(human.contains("2 other ops"), "{human}");
        let v: serde_json::Value =
            serde_json::from_str(&render(Some(&binding()), &summary, OutputMode::Json)).unwrap();
        assert_eq!(v["staged"]["other"], 2);
        assert_eq!(v["staged"]["total"], 2);
    }
}
