//! `edge add` — stage a typed edge between two staged ports, addressed by their
//! dotted `node.port` paths. Nothing hits the server.

use super::context::require_workdir;
use crate::cli::{EdgeAddArgs, EdgeRmArgs};
use crate::error::CliError;
use crate::output::OutputMode;
use crate::staging::{Changeset, EdgeAdded, EdgeRemoved};
use crate::state::{Index, Stage};

pub fn add(args: EdgeAddArgs, mode: OutputMode) -> Result<(), CliError> {
    let base = require_workdir()?;
    let mut changeset = Changeset::with_index(Stage::load(&base)?, Index::load(&base)?);
    let added = changeset.add_edge(&args.from, &args.to)?;
    changeset.into_stage().save(&base)?;

    println!("{}", render(&added, mode));
    Ok(())
}

pub fn rm(args: EdgeRmArgs, mode: OutputMode) -> Result<(), CliError> {
    let base = require_workdir()?;
    let mut changeset = Changeset::with_index(Stage::load(&base)?, Index::load(&base)?);
    let removed = changeset.remove_edge(&args.from, &args.to)?;
    changeset.into_stage().save(&base)?;

    println!("{}", render_removed(&removed, mode));
    Ok(())
}

fn render_removed(removed: &EdgeRemoved, mode: OutputMode) -> String {
    match mode {
        OutputMode::Json => serde_json::json!({
            "staged": { "remove_edge": { "from": removed.from, "to": removed.to } }
        })
        .to_string(),
        OutputMode::Human => format!(
            "Staged removal of edge '{}' -> '{}'.",
            removed.from, removed.to
        ),
    }
}

fn render(added: &EdgeAdded, mode: OutputMode) -> String {
    match mode {
        OutputMode::Json => serde_json::json!({
            "staged": { "edge": { "from": added.from, "to": added.to } }
        })
        .to_string(),
        OutputMode::Human => format!("Staged edge '{}' -> '{}'.", added.from, added.to),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_render_shows_both_endpoints() {
        let out = render(
            &EdgeAdded {
                from: "Maker.dog".to_string(),
                to: "Rater.raw".to_string(),
            },
            OutputMode::Human,
        );
        // Exact match so a from/to swap or format change is caught.
        assert_eq!(out, "Staged edge 'Maker.dog' -> 'Rater.raw'.");
    }

    #[test]
    fn json_render_carries_from_and_to() {
        let out = render(
            &EdgeAdded {
                from: "Maker.dog".to_string(),
                to: "Rater.raw".to_string(),
            },
            OutputMode::Json,
        );
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["staged"]["edge"]["from"], "Maker.dog");
        assert_eq!(v["staged"]["edge"]["to"], "Rater.raw");
    }
}
