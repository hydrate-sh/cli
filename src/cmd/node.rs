//! `node add` — stage a behavior or boundary node (with typed ports and
//! optional nesting) into the changeset. Nothing hits the server.

use hydrate_wire::models::node::Kind;

use super::context::require_workdir;
use crate::cli::{NodeAddArgs, NodeKind, NodeRmArgs, NodeSetArgs};
use crate::error::CliError;
use crate::output::OutputMode;
use crate::staging::{
    parse_port_spec, Changeset, NodeAdded, NodeEdit, NodeSpec, NodeUpdated, PortSpec,
};
use crate::state::{Index, Stage};

pub fn add(args: NodeAddArgs, mode: OutputMode) -> Result<(), CliError> {
    let base = require_workdir()?;

    // Parse the typed-port flags up front so a malformed `name:type` fails
    // before we touch any staged state.
    let inputs = parse_ports(&args.inputs)?;
    let outputs = parse_ports(&args.outputs)?;

    let mut changeset = Changeset::with_index(Stage::load(&base)?, Index::load(&base)?);
    let added = changeset.add_node(&NodeSpec {
        kind: map_kind(args.kind),
        name: &args.name,
        parent: args.parent.as_deref(),
        inputs,
        outputs,
        user_kind: args.user_kind.as_deref(),
        path_prefix: args.path_prefix.as_deref(),
        description: args.description.as_deref(),
        constraints: args.constraints.clone(),
    })?;
    changeset.into_stage().save(&base)?;

    println!("{}", render(&added, mode));
    Ok(())
}

fn parse_ports(raw: &[String]) -> Result<Vec<PortSpec>, CliError> {
    raw.iter().map(|s| parse_port_spec(s)).collect()
}

pub fn rm(args: NodeRmArgs, mode: OutputMode) -> Result<(), CliError> {
    let base = require_workdir()?;
    let mut changeset = Changeset::with_index(Stage::load(&base)?, Index::load(&base)?);
    let mut removed = Vec::with_capacity(args.paths.len());
    for path in &args.paths {
        // Stage each, in order; a bad path fails loud and stops before writing
        // (the changeset isn't persisted until all paths resolve).
        removed.push(changeset.remove_node(path)?.path);
    }
    changeset.into_stage().save(&base)?;

    println!("{}", render_removed(&removed, mode));
    Ok(())
}

pub fn set(args: NodeSetArgs, mode: OutputMode) -> Result<(), CliError> {
    let base = require_workdir()?;
    let mut changeset = Changeset::with_index(Stage::load(&base)?, Index::load(&base)?);
    let (description, constraints) = set_fields(
        args.description.as_deref(),
        &args.constraints,
        args.clear_constraints,
    );
    let edit = NodeEdit {
        // A node has no "clear name" semantics, so an empty/blank `--name` is
        // garbage, not an untouched field — pass it through and let
        // `validate_slug` reject it loudly rather than silently dropping it.
        name: args.name.clone(),
        description,
        constraints,
        add_in: parse_ports(&args.add_in)?,
        add_out: parse_ports(&args.add_out)?,
        rm_in: args.rm_in.clone(),
        rm_out: args.rm_out.clone(),
        retype_in: parse_ports(&args.retype_in)?,
        retype_out: parse_ports(&args.retype_out)?,
    };
    let updated = changeset.update_node(&args.path, &edit)?;
    changeset.into_stage().save(&base)?;

    println!("{}", render_updated(&updated, mode));
    Ok(())
}

/// Map the `set` flags to key-presence intent (pure, so it's unit-testable):
/// `--clear-constraints` → `Some([])` (clear); any `--constraint` → `Some(list)`
/// with blank entries dropped; neither → `None` (untouched). `--description ""`
/// → `None` for that field, consistent with `node add`'s empty filtering.
fn set_fields(
    description: Option<&str>,
    constraints: &[String],
    clear_constraints: bool,
) -> (Option<String>, Option<Vec<String>>) {
    let description = description
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string);
    let constraints = if clear_constraints {
        Some(Vec::new())
    } else if constraints.is_empty() {
        None
    } else {
        Some(
            constraints
                .iter()
                .filter(|c| !c.trim().is_empty())
                .cloned()
                .collect(),
        )
    };
    (description, constraints)
}

fn render_updated(u: &NodeUpdated, mode: OutputMode) -> String {
    match mode {
        OutputMode::Json => serde_json::json!({
            "staged": {
                "set": u.path,
                "name": u.name,
                "description": u.description,
                "constraints": u.constraints,
                "ports_changed": u.ports_changed,
            }
        })
        .to_string(),
        OutputMode::Human => {
            let mut fields = Vec::new();
            if u.name.is_some() {
                fields.push("name".to_string());
            }
            if u.description.is_some() {
                fields.push("description".to_string());
            }
            match &u.constraints {
                Some(cs) if cs.is_empty() => fields.push("constraints cleared".to_string()),
                Some(_) => fields.push("constraints".to_string()),
                None => {}
            }
            if u.ports_changed {
                fields.push("ports".to_string());
            }
            format!("Staged edit of '{}' ({}).", u.path, fields.join(" + "))
        }
    }
}

fn render_removed(paths: &[String], mode: OutputMode) -> String {
    match mode {
        OutputMode::Json => serde_json::json!({ "staged": { "removed": paths } }).to_string(),
        OutputMode::Human => match paths {
            [one] => format!("Staged removal of '{one}'."),
            many => format!(
                "Staged removal of {} nodes: {}.",
                many.len(),
                many.join(", ")
            ),
        },
    }
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
                "kind": added.kind,
                "inputs": added.inputs,
                "outputs": added.outputs,
            }
        })
        .to_string(),
        OutputMode::Human => format!(
            "Staged {} node '{}' ({}, {}).",
            added.kind,
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
    fn human_render_pluralizes_precisely() {
        let one = render(
            &NodeAdded {
                path: "Rater".to_string(),
                kind: "behavior",
                inputs: 1,
                outputs: 2,
            },
            OutputMode::Human,
        );
        assert!(one.contains("behavior node 'Rater'"), "{one}");
        // Singular "1 input" must not be the prefix of a stray "1 inputs".
        assert!(one.contains("(1 input, 2 outputs)"), "{one}");
    }

    #[test]
    fn render_removed_human_singular_and_plural() {
        assert_eq!(
            render_removed(&["Api.Rater".to_string()], OutputMode::Human),
            "Staged removal of 'Api.Rater'."
        );
        let many = render_removed(&["Api".to_string(), "Store".to_string()], OutputMode::Human);
        assert!(many.contains("2 nodes"), "{many}");
        assert!(many.contains("Api, Store"), "{many}");
    }

    #[test]
    fn render_updated_names_the_rename_and_ports_fields() {
        let human = render_updated(
            &NodeUpdated {
                path: "Api.Rater".to_string(),
                name: Some("Scorer".to_string()),
                description: None,
                constraints: None,
                ports_changed: true,
            },
            OutputMode::Human,
        );
        assert!(human.contains("name + ports"), "{human}");

        let out = render_updated(
            &NodeUpdated {
                path: "Api.Rater".to_string(),
                name: Some("Scorer".to_string()),
                description: None,
                constraints: None,
                ports_changed: true,
            },
            OutputMode::Json,
        );
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["staged"]["name"], "Scorer");
        assert_eq!(v["staged"]["ports_changed"], true);
    }

    #[test]
    fn render_updated_names_the_set_fields() {
        let human = render_updated(
            &NodeUpdated {
                path: "Api.Rater".to_string(),
                name: None,
                description: Some("p".to_string()),
                constraints: Some(vec!["c".to_string()]),
                ports_changed: false,
            },
            OutputMode::Human,
        );
        assert!(human.contains("'Api.Rater'"), "{human}");
        assert!(human.contains("description + constraints"), "{human}");

        // Cleared constraints read distinctly from "set".
        let cleared = render_updated(
            &NodeUpdated {
                path: "Api.Rater".to_string(),
                name: None,
                description: None,
                constraints: Some(vec![]),
                ports_changed: false,
            },
            OutputMode::Human,
        );
        assert!(cleared.contains("constraints cleared"), "{cleared}");

        // JSON: untouched constraints render as null (not []), distinct from cleared.
        let out = render_updated(
            &NodeUpdated {
                path: "Api.Rater".to_string(),
                name: None,
                description: Some("p".to_string()),
                constraints: None,
                ports_changed: false,
            },
            OutputMode::Json,
        );
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["staged"]["set"], "Api.Rater");
        assert_eq!(v["staged"]["description"], "p");
        assert!(v["staged"]["constraints"].is_null(), "{out}");
    }

    #[test]
    fn set_fields_maps_flags_to_key_presence_intent() {
        // Empty/blank description → None (untouched), like `node add`.
        assert_eq!(set_fields(Some("  "), &[], false).0, None);
        assert_eq!(set_fields(Some("p"), &[], false).0, Some("p".to_string()));
        // No constraint flags → None (untouched).
        assert_eq!(set_fields(None, &[], false).1, None);
        // --clear-constraints → Some([]) (cleared).
        assert_eq!(set_fields(None, &[], true).1, Some(vec![]));
        // --constraint with a blank dropped → Some(non-empty list).
        assert_eq!(
            set_fields(None, &["a".to_string(), "  ".to_string()], false).1,
            Some(vec!["a".to_string()])
        );
    }

    #[test]
    fn render_removed_json_carries_the_paths() {
        let out = render_removed(&["Api".to_string(), "Store".to_string()], OutputMode::Json);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["staged"]["removed"][0], "Api");
        assert_eq!(v["staged"]["removed"][1], "Store");
    }

    #[test]
    fn json_render_carries_path_kind_and_counts() {
        let out = render(
            &NodeAdded {
                path: "Api.Rater".to_string(),
                kind: "boundary",
                inputs: 1,
                outputs: 0,
            },
            OutputMode::Json,
        );
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["staged"]["node"], "Api.Rater");
        assert_eq!(v["staged"]["kind"], "boundary");
        assert_eq!(v["staged"]["inputs"], 1);
        assert_eq!(v["staged"]["outputs"], 0);
    }
}
