//! `edge add` — stage a typed edge between two staged ports, addressed by their
//! dotted `node.port` paths. Nothing hits the server.

use super::context::require_workdir;
use crate::cli::EdgeAddArgs;
use crate::error::CliError;
use crate::output::OutputMode;
use crate::staging::{Changeset, EdgeAdded};
use crate::state::Stage;

pub fn add(args: EdgeAddArgs, mode: OutputMode) -> Result<(), CliError> {
    let base = require_workdir()?;
    let mut changeset = Changeset::from_stage(Stage::load(&base)?);
    let added = changeset.add_edge(&args.from, &args.to)?;
    changeset.into_stage().save(&base)?;

    println!("{}", render(&added, mode));
    Ok(())
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
        assert!(out.contains("Maker.dog"), "{out}");
        assert!(out.contains("Rater.raw"), "{out}");
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
