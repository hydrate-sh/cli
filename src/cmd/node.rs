//! `node add` — stage a behavior or boundary node (with typed ports and
//! optional nesting) into the changeset. Nothing hits the server.

use hydrate_wire::models::node::Kind;

use super::context::require_workdir;
use crate::cli::{NodeAddArgs, NodeKind};
use crate::error::CliError;
use crate::output::OutputMode;
use crate::staging::{parse_port_spec, Changeset, NodeAdded, NodeSpec, PortSpec};
use crate::state::Stage;

pub fn add(args: NodeAddArgs, mode: OutputMode) -> Result<(), CliError> {
    let base = require_workdir()?;

    // Parse the typed-port flags up front so a malformed `name:type` fails
    // before we touch any staged state.
    let inputs = parse_ports(&args.inputs)?;
    let outputs = parse_ports(&args.outputs)?;

    let mut changeset = Changeset::from_stage(Stage::load(&base)?);
    let added = changeset.add_node(&NodeSpec {
        kind: map_kind(args.kind),
        name: &args.name,
        parent: args.parent.as_deref(),
        inputs,
        outputs,
        user_kind: args.user_kind.as_deref(),
        path_prefix: args.path_prefix.as_deref(),
    })?;
    changeset.into_stage().save(&base)?;

    println!("{}", render(&added, mode));
    Ok(())
}

fn parse_ports(raw: &[String]) -> Result<Vec<PortSpec>, CliError> {
    raw.iter().map(|s| parse_port_spec(s)).collect()
}

fn map_kind(kind: NodeKind) -> Kind {
    match kind {
        NodeKind::Behavior => Kind::Behavior,
        NodeKind::Boundary => Kind::Boundary,
    }
}

fn render(added: &NodeAdded, mode: OutputMode) -> String {
    match mode {
        OutputMode::Json => serde_json::json!({
            "staged": {
                "node": added.path,
                "inputs": added.inputs,
                "outputs": added.outputs,
            }
        })
        .to_string(),
        OutputMode::Human => format!(
            "Staged node '{}' ({}, {}).",
            added.path,
            plural(added.inputs, "input"),
            plural(added.outputs, "output"),
        ),
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

    #[test]
    fn parse_ports_collects_each_spec() {
        let ports = parse_ports(&["raw:HotDog".to_string(), "n:Count".to_string()]).unwrap();
        assert_eq!(ports.len(), 2);
        assert_eq!(ports[0].name, "raw");
        assert_eq!(ports[1].r#type, "Count");
    }

    #[test]
    fn parse_ports_propagates_a_bad_spec() {
        let err = parse_ports(&["raw:HotDog".to_string(), "oops".to_string()]).unwrap_err();
        assert!(matches!(err, CliError::InvalidArgument(_)), "got {err:?}");
    }

    #[test]
    fn human_render_pluralizes() {
        let one = render(
            &NodeAdded {
                path: "Rater".to_string(),
                inputs: 1,
                outputs: 2,
            },
            OutputMode::Human,
        );
        assert!(one.contains("Rater"), "{one}");
        assert!(one.contains("1 input"), "{one}");
        assert!(one.contains("2 outputs"), "{one}");
    }

    #[test]
    fn json_render_carries_path_and_counts() {
        let out = render(
            &NodeAdded {
                path: "Api.Rater".to_string(),
                inputs: 1,
                outputs: 0,
            },
            OutputMode::Json,
        );
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["staged"]["node"], "Api.Rater");
        assert_eq!(v["staged"]["inputs"], 1);
        assert_eq!(v["staged"]["outputs"], 0);
    }
}
