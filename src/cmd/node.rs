//! `node add` — stage a behavior or boundary node (with typed ports and
//! optional nesting) into the changeset. Nothing hits the server.

use hydrate_wire::models::node::Kind;

use super::context::require_workdir;
use crate::cli::{NodeAddArgs, NodeKind, NodeMvArgs, NodeRmArgs, NodeSetArgs};
use crate::error::CliError;
use crate::output::OutputMode;
use crate::staging::{
    parse_port_spec, Changeset, NodeAdded, NodeEdit, NodeReparented, NodeSpec, NodeUpdated,
    PortSpec,
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
        is_external: args.external,
        external_kind: args.external_kind.as_deref(),
        verifications: args.verifications.clone(),
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
        // Scalars: a blank value is "untouched", mirroring `--description ""`.
        user_kind: blank_to_none(args.user_kind.as_deref()),
        path_prefix: blank_to_none(args.path_prefix.as_deref()),
        // --external / --no-external toggle is_external; neither = untouched.
        is_external: external_flag(args.external, args.no_external),
        external_kind: blank_to_none(args.external_kind.as_deref()),
        verifications: list_field(&args.verifications, args.clear_verifications),
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
    let constraints = list_field(constraints, clear_constraints);
    (description, constraints)
}

/// A blank/whitespace scalar flag is "untouched" (`None`), mirroring how
/// `--description ""` is treated everywhere. Pure, so it's unit-testable.
fn blank_to_none(value: Option<&str>) -> Option<String> {
    value.filter(|s| !s.trim().is_empty()).map(str::to_string)
}

/// Map the `--external` / `--no-external` pair to `is_external`: `--external` →
/// `Some(true)`, `--no-external` → `Some(false)`, neither → `None` (untouched).
/// clap `conflicts_with` rules out both at once. Pure, so it's unit-testable.
fn external_flag(external: bool, no_external: bool) -> Option<bool> {
    if external {
        Some(true)
    } else if no_external {
        Some(false)
    } else {
        None
    }
}

/// Map a repeatable list flag + its `--clear-*` companion to key-presence intent:
/// clear → `Some([])`; any values → `Some(list)` with blanks dropped; neither →
/// `None` (untouched). Shared by constraints and verifications.
fn list_field(values: &[String], clear: bool) -> Option<Vec<String>> {
    if clear {
        Some(Vec::new())
    } else if values.is_empty() {
        None
    } else {
        Some(
            values
                .iter()
                .filter(|v| !v.trim().is_empty())
                .cloned()
                .collect(),
        )
    }
}

fn render_updated(u: &NodeUpdated, mode: OutputMode) -> String {
    match mode {
        OutputMode::Json => serde_json::json!({
            "staged": {
                "set": u.path,
                "name": u.name,
                "description": u.description,
                "constraints": u.constraints,
                "user_kind": u.user_kind,
                "path_prefix": u.path_prefix,
                "external": u.is_external,
                "external_kind": u.external_kind,
                "verifications": u.verifications,
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
            match &u.verifications {
                Some(vs) if vs.is_empty() => fields.push("verifications cleared".to_string()),
                Some(_) => fields.push("verifications".to_string()),
                None => {}
            }
            if u.user_kind.is_some() {
                fields.push("user-kind".to_string());
            }
            if u.path_prefix.is_some() {
                fields.push("path-prefix".to_string());
            }
            if u.is_external.is_some() {
                fields.push("external".to_string());
            }
            if u.external_kind.is_some() {
                fields.push("external-kind".to_string());
            }
            if u.ports_changed {
                fields.push("ports".to_string());
            }
            format!("Staged edit of '{}' ({}).", u.path, fields.join(" + "))
        }
    }
}

pub fn mv(args: NodeMvArgs, mode: OutputMode) -> Result<(), CliError> {
    let base = require_workdir()?;
    // Exactly one destination: a parent path, or --top.
    let new_parent = match (args.parent.as_deref(), args.top) {
        (Some(p), false) => Some(p),
        (None, true) => None,
        (None, false) => {
            return Err(CliError::InvalidArgument(
                "specify a destination: --parent <path> or --top".to_string(),
            ))
        }
        (Some(_), true) => unreachable!("clap conflicts_with prevents both"),
    };
    let mut changeset = Changeset::with_index(Stage::load(&base)?, Index::load(&base)?);
    let moved = changeset.reparent_node(&args.path, new_parent)?;
    changeset.into_stage().save(&base)?;

    println!("{}", render_moved(&moved, mode));
    Ok(())
}

fn render_moved(m: &NodeReparented, mode: OutputMode) -> String {
    match mode {
        OutputMode::Json => serde_json::json!({
            "staged": { "move": m.path, "parent": m.new_parent }
        })
        .to_string(),
        OutputMode::Human => format!(
            "Staged move of '{}' to {}.",
            m.path,
            m.new_parent.as_deref().unwrap_or("the top level")
        ),
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
    fn render_moved_human_and_json_for_parent_and_top_level() {
        let to_core = NodeReparented {
            path: "Api.Rater".to_string(),
            new_parent: Some("Core".to_string()),
        };
        assert_eq!(
            render_moved(&to_core, OutputMode::Human),
            "Staged move of 'Api.Rater' to Core."
        );
        let v: serde_json::Value =
            serde_json::from_str(&render_moved(&to_core, OutputMode::Json)).unwrap();
        assert_eq!(v["staged"]["move"], "Api.Rater");
        assert_eq!(v["staged"]["parent"], "Core");

        let to_top = NodeReparented {
            path: "Api.Rater".to_string(),
            new_parent: None,
        };
        assert_eq!(
            render_moved(&to_top, OutputMode::Human),
            "Staged move of 'Api.Rater' to the top level."
        );
        let v: serde_json::Value =
            serde_json::from_str(&render_moved(&to_top, OutputMode::Json)).unwrap();
        assert!(v["staged"]["parent"].is_null(), "{v}");
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
                user_kind: None,
                path_prefix: None,
                is_external: None,
                external_kind: None,
                verifications: None,
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
                user_kind: None,
                path_prefix: None,
                is_external: None,
                external_kind: None,
                verifications: None,
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
                user_kind: None,
                path_prefix: None,
                is_external: None,
                external_kind: None,
                verifications: None,
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
                user_kind: None,
                path_prefix: None,
                is_external: None,
                external_kind: None,
                verifications: None,
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
                user_kind: None,
                path_prefix: None,
                is_external: None,
                external_kind: None,
                verifications: None,
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
    fn blank_to_none_filters_whitespace() {
        assert_eq!(blank_to_none(None), None);
        assert_eq!(blank_to_none(Some("  ")), None);
        assert_eq!(
            blank_to_none(Some("subsystem")),
            Some("subsystem".to_string())
        );
    }

    #[test]
    fn external_flag_maps_the_toggle() {
        assert_eq!(external_flag(false, false), None); // untouched
        assert_eq!(external_flag(true, false), Some(true)); // --external
        assert_eq!(external_flag(false, true), Some(false)); // --no-external
    }

    #[test]
    fn list_field_maps_clear_values_and_none() {
        assert_eq!(list_field(&[], false), None); // untouched
        assert_eq!(list_field(&[], true), Some(vec![])); // cleared
        assert_eq!(
            list_field(&["a".to_string(), "  ".to_string()], false),
            Some(vec!["a".to_string()]) // blanks dropped
        );
    }

    #[test]
    fn render_updated_names_the_scalar_and_verification_fields() {
        let u = NodeUpdated {
            path: "Api.Rater".to_string(),
            name: None,
            description: None,
            constraints: None,
            user_kind: Some("subsystem".to_string()),
            path_prefix: Some("src/api/".to_string()),
            is_external: Some(true),
            external_kind: Some("rest-api".to_string()),
            verifications: Some(vec!["responds in 50ms".to_string()]),
            ports_changed: false,
        };
        let human = render_updated(&u, OutputMode::Human);
        for label in [
            "user-kind",
            "path-prefix",
            "external",
            "external-kind",
            "verifications",
        ] {
            assert!(human.contains(label), "{human} missing {label}");
        }
        let v: serde_json::Value =
            serde_json::from_str(&render_updated(&u, OutputMode::Json)).unwrap();
        assert_eq!(v["staged"]["user_kind"], "subsystem");
        assert_eq!(v["staged"]["path_prefix"], "src/api/");
        assert_eq!(v["staged"]["external"], true);
        assert_eq!(v["staged"]["external_kind"], "rest-api");
        assert_eq!(v["staged"]["verifications"][0], "responds in 50ms");
    }

    #[test]
    fn render_updated_distinguishes_cleared_verifications() {
        let u = NodeUpdated {
            path: "Api.Rater".to_string(),
            name: None,
            description: None,
            constraints: None,
            user_kind: None,
            path_prefix: None,
            is_external: None,
            external_kind: None,
            verifications: Some(vec![]),
            ports_changed: false,
        };
        assert!(render_updated(&u, OutputMode::Human).contains("verifications cleared"));
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
