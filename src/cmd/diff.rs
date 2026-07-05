//! `diff` — show the staged operations in detail, with every entity rendered by
//! its dotted path (never a UUID).

use super::context::require_workdir;
use crate::error::CliError;
use crate::output::OutputMode;
use crate::staging::{summarize_workdir, NamedType, OpSummary, StageSummary};

pub fn run(mode: OutputMode) -> Result<(), CliError> {
    let base = require_workdir()?;
    let summary = summarize_workdir(&base)?;
    println!("{}", render(&summary, mode));
    Ok(())
}

fn render(summary: &StageSummary, mode: OutputMode) -> String {
    match mode {
        OutputMode::Json => {
            let ops: Vec<serde_json::Value> = summary.ops.iter().map(op_json).collect();
            serde_json::json!({ "ops": ops }).to_string()
        }
        OutputMode::Human => {
            if summary.is_empty() {
                return "Nothing staged.".to_string();
            }
            summary
                .ops
                .iter()
                .map(op_line)
                .collect::<Vec<_>>()
                .join("\n")
        }
    }
}

fn op_line(op: &OpSummary) -> String {
    match op {
        OpSummary::Node {
            kind,
            path,
            inputs,
            outputs,
            description,
            constraints,
            verifications,
            external,
            protocol,
            doc_url,
            is_test_node,
            config,
        } => {
            let mut line = format!("+ {kind} {path}");
            if *external {
                line.push_str(" (external)");
            }
            if *is_test_node {
                line.push_str(" (test)");
            }
            let mut parts = Vec::new();
            if !inputs.is_empty() {
                parts.push(format!("in: {}", join_ports(inputs)));
            }
            if !outputs.is_empty() {
                parts.push(format!("out: {}", join_ports(outputs)));
            }
            if !config.is_empty() {
                parts.push(format!("config: {}", join_ports(config)));
            }
            if !parts.is_empty() {
                line.push_str(&format!(" ({})", parts.join("; ")));
            }
            // Show the spec content so it's verifiable in the terminal, not only
            // after a commit + editor round-trip.
            if let Some(d) = description {
                line.push_str(&format!("\n    description: {d}"));
            }
            for c in constraints {
                line.push_str(&format!("\n    constraint: {c}"));
            }
            for v in verifications {
                line.push_str(&format!("\n    verification: {v}"));
            }
            if let Some(p) = protocol {
                line.push_str(&format!("\n    protocol: {p}"));
            }
            if let Some(d) = doc_url {
                line.push_str(&format!("\n    doc-url: {d}"));
            }
            line
        }
        OpSummary::Edge { from, to } => format!("+ edge {from} -> {to}"),
        OpSummary::UpdateNode {
            path,
            name,
            description,
            constraints,
            inputs,
            outputs,
            user_kind,
            path_prefix,
            language,
            external,
            external_kind,
            protocol,
            doc_url,
            is_test_node,
            verifications,
            config,
        } => {
            let mut line = format!("~ node {path}");
            if let Some(n) = name {
                line.push_str(&format!("\n    rename -> {n}"));
            }
            // description: Some("") = cleared, Some(v) = set, None = untouched.
            match description {
                Some(d) if d.is_empty() => line.push_str("\n    description: (cleared)"),
                Some(d) => line.push_str(&format!("\n    description: {d}")),
                None => {}
            }
            // Distinguish cleared (Some([])) from untouched (None).
            match constraints {
                Some(cs) if cs.is_empty() => line.push_str("\n    constraints: (cleared)"),
                Some(cs) => {
                    for c in cs {
                        line.push_str(&format!("\n    constraint: {c}"));
                    }
                }
                None => {}
            }
            match verifications {
                Some(vs) if vs.is_empty() => line.push_str("\n    verifications: (cleared)"),
                Some(vs) => {
                    for v in vs {
                        line.push_str(&format!("\n    verification: {v}"));
                    }
                }
                None => {}
            }
            line.push_str(&scalar_line(user_kind, "user-kind"));
            line.push_str(&scalar_line(path_prefix, "path-prefix"));
            line.push_str(&scalar_line(language, "language"));
            line.push_str(&scalar_line(external_kind, "external-kind"));
            line.push_str(&scalar_line(protocol, "protocol"));
            line.push_str(&scalar_line(doc_url, "doc-url"));
            if let Some(ext) = external {
                line.push_str(&format!("\n    external: {ext}"));
            }
            if let Some(t) = is_test_node {
                line.push_str(&format!("\n    test-node: {t}"));
            }
            if let Some(ps) = inputs {
                line.push_str(&format!("\n    inputs -> {}", join_ports(ps)));
            }
            if let Some(ps) = outputs {
                line.push_str(&format!("\n    outputs -> {}", join_ports(ps)));
            }
            if let Some(ps) = config {
                line.push_str(&format!("\n    config -> {}", join_ports(ps)));
            }
            line
        }
        OpSummary::Reparent { path, new_parent } => {
            format!(
                "~ move {path} -> {}",
                new_parent.as_deref().unwrap_or("(top level)")
            )
        }
        OpSummary::Flatten { path } => format!("~ flatten {path}"),
        OpSummary::DeleteEdge { from, to } => format!("- edge {from} -> {to}"),
        OpSummary::DeleteNode { path } => format!("- node {path}"),
        OpSummary::Other { kind } => format!("+ {kind}"),
    }
}

/// Render a double-option scalar edit line: cleared (`Some(None)`) reads
/// distinctly from set (`Some(Some)`); untouched (`None`) renders nothing.
fn scalar_line(field: &Option<Option<String>>, label: &str) -> String {
    match field {
        Some(None) => format!("\n    {label}: (cleared)"),
        Some(Some(v)) => format!("\n    {label}: {v}"),
        None => String::new(),
    }
}

fn join_ports(ports: &[NamedType]) -> String {
    ports
        .iter()
        .map(|(name, ty)| format!("{name}:{ty}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn op_json(op: &OpSummary) -> serde_json::Value {
    match op {
        OpSummary::Node {
            kind,
            path,
            inputs,
            outputs,
            description,
            constraints,
            verifications,
            external,
            protocol,
            doc_url,
            is_test_node,
            config,
        } => serde_json::json!({
            "op": "add_node",
            "kind": kind,
            "node": path,
            "inputs": ports_json(inputs),
            "outputs": ports_json(outputs),
            "config": ports_json(config),
            "description": description,
            "constraints": constraints,
            "verifications": verifications,
            "external": external,
            "protocol": protocol,
            "doc_url": doc_url,
            "test_node": is_test_node,
        }),
        OpSummary::Edge { from, to } => serde_json::json!({
            "op": "add_edge",
            "from": from,
            "to": to,
        }),
        OpSummary::UpdateNode {
            path,
            name,
            description,
            constraints,
            inputs,
            outputs,
            user_kind,
            path_prefix,
            language,
            external,
            external_kind,
            protocol,
            doc_url,
            is_test_node,
            verifications,
            config,
        } => {
            let mut obj = serde_json::json!({
                "op": "update_node_data",
                "node": path,
                "name": name,
                "description": description,
                "constraints": constraints,
                "inputs": inputs.as_ref().map(|p| ports_json(p)),
                "outputs": outputs.as_ref().map(|p| ports_json(p)),
                "config": config.as_ref().map(|p| ports_json(p)),
                "external": external,
                "test_node": is_test_node,
                "verifications": verifications,
            });
            // Double-option scalars: untouched → key omitted; cleared → null; set
            // → value. Keeping the distinction in JSON mirrors the human output.
            let map = obj.as_object_mut().expect("json object");
            for (key, field) in [
                ("user_kind", user_kind),
                ("path_prefix", path_prefix),
                ("language", language),
                ("external_kind", external_kind),
                ("protocol", protocol),
                ("doc_url", doc_url),
            ] {
                if let Some(inner) = field {
                    map.insert(
                        key.to_string(),
                        match inner {
                            Some(v) => serde_json::Value::String(v.clone()),
                            None => serde_json::Value::Null,
                        },
                    );
                }
            }
            obj
        }
        OpSummary::Reparent { path, new_parent } => serde_json::json!({
            "op": "reparent_node",
            "node": path,
            "parent": new_parent,
        }),
        OpSummary::Flatten { path } => serde_json::json!({
            "op": "flatten_boundary",
            "node": path,
        }),
        OpSummary::DeleteEdge { from, to } => serde_json::json!({
            "op": "delete_edge",
            "from": from,
            "to": to,
        }),
        OpSummary::DeleteNode { path } => serde_json::json!({
            "op": "delete_node",
            "node": path,
        }),
        OpSummary::Other { kind } => serde_json::json!({ "op": kind }),
    }
}

fn ports_json(ports: &[NamedType]) -> Vec<serde_json::Value> {
    ports
        .iter()
        .map(|(name, ty)| serde_json::json!({ "name": name, "type": ty }))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node_op() -> OpSummary {
        OpSummary::Node {
            kind: "behavior",
            path: "Api.Rater".to_string(),
            inputs: vec![("raw".to_string(), "HotDog".to_string())],
            outputs: vec![("score".to_string(), "Score".to_string())],
            description: None,
            constraints: vec![],
            verifications: vec![],
            external: false,
            protocol: None,
            doc_url: None,
            is_test_node: false,
            config: vec![],
        }
    }

    fn edge_op() -> OpSummary {
        OpSummary::Edge {
            from: "Maker.dog".to_string(),
            to: "Api.Rater.raw".to_string(),
        }
    }

    fn summary(ops: Vec<OpSummary>) -> StageSummary {
        StageSummary {
            nodes: 0,
            edges: 0,
            updates: 0,
            deletes: 0,
            other: 0,
            ops,
        }
    }

    #[test]
    fn human_renders_node_with_typed_ports() {
        let out = render(&summary(vec![node_op()]), OutputMode::Human);
        assert_eq!(
            out,
            "+ behavior Api.Rater (in: raw:HotDog; out: score:Score)"
        );
    }

    #[test]
    fn human_renders_description_and_constraints() {
        // The spec content must be verifiable in the terminal, not only after a
        // commit + editor round-trip.
        let op = OpSummary::Node {
            kind: "behavior",
            path: "Rater".to_string(),
            inputs: vec![],
            outputs: vec![],
            description: Some("scores a hotdog".to_string()),
            constraints: vec!["fast".to_string(), "stateless".to_string()],
            verifications: vec![],
            external: false,
            protocol: None,
            doc_url: None,
            is_test_node: false,
            config: vec![],
        };
        let out = render(&summary(vec![op]), OutputMode::Human);
        assert!(out.contains("description: scores a hotdog"), "{out}");
        assert!(out.contains("constraint: fast"), "{out}");
        assert!(out.contains("constraint: stateless"), "{out}");
    }

    #[test]
    fn human_omits_description_line_when_absent() {
        // node_op() has description: None, constraints: [] — neither line appears.
        let out = render(&summary(vec![node_op()]), OutputMode::Human);
        assert!(!out.contains("description:"), "{out}");
        assert!(!out.contains("constraint:"), "{out}");
    }

    #[test]
    fn json_node_carries_description_and_constraints() {
        let op = OpSummary::Node {
            kind: "behavior",
            path: "Rater".to_string(),
            inputs: vec![],
            outputs: vec![],
            description: Some("the prompt".to_string()),
            constraints: vec!["c1".to_string()],
            verifications: vec![],
            external: false,
            protocol: None,
            doc_url: None,
            is_test_node: false,
            config: vec![],
        };
        let out = render(&summary(vec![op]), OutputMode::Json);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["ops"][0]["description"], "the prompt");
        assert_eq!(v["ops"][0]["constraints"][0], "c1");
    }

    #[test]
    fn human_and_json_render_external_and_verifications() {
        let op = OpSummary::Node {
            kind: "behavior",
            path: "Db".to_string(),
            inputs: vec![],
            outputs: vec![],
            description: None,
            constraints: vec![],
            verifications: vec!["responds within 50ms".to_string()],
            external: true,
            protocol: None,
            doc_url: None,
            is_test_node: false,
            config: vec![],
        };
        let human = render(&summary(vec![op.clone()]), OutputMode::Human);
        assert!(human.contains("(external)"), "{human}");
        assert!(
            human.contains("verification: responds within 50ms"),
            "{human}"
        );

        let v: serde_json::Value =
            serde_json::from_str(&render(&summary(vec![op]), OutputMode::Json)).unwrap();
        assert_eq!(v["ops"][0]["external"], true);
        assert_eq!(v["ops"][0]["verifications"][0], "responds within 50ms");

        // Negative: a plain node renders neither marker.
        let plain = render(&summary(vec![node_op()]), OutputMode::Human);
        assert!(!plain.contains("(external)"), "{plain}");
        assert!(!plain.contains("verification:"), "{plain}");
    }

    #[test]
    fn human_and_json_render_update_with_rename_and_ports() {
        let op = OpSummary::UpdateNode {
            path: "Api.Rater".to_string(),
            name: Some("Scorer".to_string()),
            description: None,
            constraints: None,
            inputs: None,
            outputs: Some(vec![
                ("score".to_string(), "Rating".to_string()),
                ("extra".to_string(), "Blob".to_string()),
            ]),
            user_kind: None,
            path_prefix: None,
            language: None,
            external: None,
            external_kind: None,
            protocol: None,
            doc_url: None,
            is_test_node: None,
            config: None,
            verifications: None,
        };
        let human = render(&summary(vec![op.clone()]), OutputMode::Human);
        assert!(human.contains("~ node Api.Rater"), "{human}");
        assert!(human.contains("rename -> Scorer"), "{human}");
        assert!(
            human.contains("outputs -> score:Rating, extra:Blob"),
            "{human}"
        );

        let out = render(&summary(vec![op]), OutputMode::Json);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["ops"][0]["op"], "update_node_data");
        assert_eq!(v["ops"][0]["name"], "Scorer");
        assert_eq!(v["ops"][0]["outputs"][1]["name"], "extra");
        assert_eq!(v["ops"][0]["outputs"][1]["type"], "Blob");
    }

    #[test]
    fn human_renders_edge_by_paths() {
        let out = render(&summary(vec![edge_op()]), OutputMode::Human);
        assert_eq!(out, "+ edge Maker.dog -> Api.Rater.raw");
    }

    #[test]
    fn human_and_json_render_a_flatten_by_path() {
        let op = OpSummary::Flatten {
            path: "Api".to_string(),
        };
        assert_eq!(
            render(&summary(vec![op.clone()]), OutputMode::Human),
            "~ flatten Api"
        );
        let v: serde_json::Value =
            serde_json::from_str(&render(&summary(vec![op]), OutputMode::Json)).unwrap();
        assert_eq!(v["ops"][0]["op"], "flatten_boundary");
        assert_eq!(v["ops"][0]["node"], "Api");
    }

    #[test]
    fn human_and_json_render_a_reparent_by_path() {
        let to_core = OpSummary::Reparent {
            path: "Api.Rater".to_string(),
            new_parent: Some("Core".to_string()),
        };
        assert_eq!(
            render(&summary(vec![to_core.clone()]), OutputMode::Human),
            "~ move Api.Rater -> Core"
        );
        let v: serde_json::Value =
            serde_json::from_str(&render(&summary(vec![to_core]), OutputMode::Json)).unwrap();
        assert_eq!(v["ops"][0]["op"], "reparent_node");
        assert_eq!(v["ops"][0]["node"], "Api.Rater");
        assert_eq!(v["ops"][0]["parent"], "Core");

        // Top level renders distinctly (human label + JSON null).
        let to_top = OpSummary::Reparent {
            path: "Api.Rater".to_string(),
            new_parent: None,
        };
        assert_eq!(
            render(&summary(vec![to_top.clone()]), OutputMode::Human),
            "~ move Api.Rater -> (top level)"
        );
        let v: serde_json::Value =
            serde_json::from_str(&render(&summary(vec![to_top]), OutputMode::Json)).unwrap();
        assert!(v["ops"][0]["parent"].is_null(), "{v}");
    }

    #[test]
    fn human_and_json_render_update_scalar_and_verification_fields() {
        let op = OpSummary::UpdateNode {
            path: "Api.Rater".to_string(),
            name: None,
            description: None,
            constraints: None,
            inputs: None,
            outputs: None,
            user_kind: Some(Some("subsystem".to_string())),
            path_prefix: Some(Some("src/api/".to_string())),
            language: Some(Some("go".to_string())),
            external: Some(true),
            external_kind: Some(Some("rest-api".to_string())),
            protocol: None,
            doc_url: None,
            is_test_node: None,
            config: None,
            verifications: Some(vec!["responds in 50ms".to_string()]),
        };
        let human = render(&summary(vec![op.clone()]), OutputMode::Human);
        assert!(human.contains("user-kind: subsystem"), "{human}");
        assert!(human.contains("path-prefix: src/api/"), "{human}");
        assert!(human.contains("language: go"), "{human}");
        assert!(human.contains("external: true"), "{human}");
        assert!(human.contains("external-kind: rest-api"), "{human}");
        assert!(human.contains("verification: responds in 50ms"), "{human}");

        let v: serde_json::Value =
            serde_json::from_str(&render(&summary(vec![op]), OutputMode::Json)).unwrap();
        assert_eq!(v["ops"][0]["user_kind"], "subsystem");
        assert_eq!(v["ops"][0]["path_prefix"], "src/api/");
        assert_eq!(v["ops"][0]["language"], "go");
        assert_eq!(v["ops"][0]["external"], true);
        assert_eq!(v["ops"][0]["external_kind"], "rest-api");
        assert_eq!(v["ops"][0]["verifications"][0], "responds in 50ms");
    }

    #[test]
    fn render_update_distinguishes_cleared_set_and_untouched_scalars() {
        let op = OpSummary::UpdateNode {
            path: "Api.Rater".to_string(),
            name: None,
            description: None,
            constraints: None,
            inputs: None,
            outputs: None,
            user_kind: Some(None),
            path_prefix: None,
            language: None,
            external: None,
            external_kind: None,
            protocol: Some(Some("gRPC".to_string())),
            doc_url: None,
            is_test_node: Some(true),
            verifications: None,
            config: None,
        };
        let human = render(&summary(vec![op.clone()]), OutputMode::Human);
        assert!(human.contains("user-kind: (cleared)"), "{human}");
        assert!(human.contains("protocol: gRPC"), "{human}");
        assert!(human.contains("test-node: true"), "{human}");
        assert!(!human.contains("path-prefix"), "untouched omitted: {human}");

        let v: serde_json::Value =
            serde_json::from_str(&render(&summary(vec![op]), OutputMode::Json)).unwrap();
        // cleared → key present + null; set → value; untouched → key absent.
        assert!(
            v["ops"][0].get("user_kind").is_some(),
            "cleared key present"
        );
        assert!(v["ops"][0]["user_kind"].is_null(), "cleared → null");
        assert_eq!(v["ops"][0]["protocol"], "gRPC");
        assert!(
            v["ops"][0].get("path_prefix").is_none(),
            "untouched key absent"
        );
        assert_eq!(v["ops"][0]["test_node"], true);
    }

    #[test]
    fn render_shows_config_ports_on_add_and_update() {
        let add = OpSummary::Node {
            kind: "behavior",
            path: "Worker".to_string(),
            inputs: vec![],
            outputs: vec![],
            description: None,
            constraints: vec![],
            verifications: vec![],
            external: false,
            protocol: None,
            doc_url: None,
            is_test_node: false,
            config: vec![("region".to_string(), "String".to_string())],
        };
        let human = render(&summary(vec![add.clone()]), OutputMode::Human);
        assert!(human.contains("config: region:String"), "{human}");
        let v: serde_json::Value =
            serde_json::from_str(&render(&summary(vec![add]), OutputMode::Json)).unwrap();
        assert_eq!(v["ops"][0]["config"][0]["name"], "region");

        let upd = OpSummary::UpdateNode {
            path: "Worker".to_string(),
            name: None,
            description: None,
            constraints: None,
            inputs: None,
            outputs: None,
            user_kind: None,
            path_prefix: None,
            language: None,
            external: None,
            external_kind: None,
            protocol: None,
            doc_url: None,
            is_test_node: None,
            verifications: None,
            config: Some(vec![("region".to_string(), "String".to_string())]),
        };
        let human = render(&summary(vec![upd.clone()]), OutputMode::Human);
        assert!(human.contains("config -> region:String"), "{human}");
        let v: serde_json::Value =
            serde_json::from_str(&render(&summary(vec![upd]), OutputMode::Json)).unwrap();
        assert_eq!(v["ops"][0]["config"][0]["type"], "String");
    }

    #[test]
    fn render_node_add_shows_protocol_doc_and_test() {
        let op = OpSummary::Node {
            kind: "behavior",
            path: "Ext".to_string(),
            inputs: vec![],
            outputs: vec![],
            description: None,
            constraints: vec![],
            verifications: vec![],
            external: true,
            protocol: Some("gRPC".to_string()),
            doc_url: Some("https://x".to_string()),
            is_test_node: true,
            config: vec![],
        };
        let human = render(&summary(vec![op.clone()]), OutputMode::Human);
        assert!(
            human.contains("(external)") && human.contains("(test)"),
            "{human}"
        );
        assert!(human.contains("protocol: gRPC"), "{human}");
        assert!(human.contains("doc-url: https://x"), "{human}");
        let v: serde_json::Value =
            serde_json::from_str(&render(&summary(vec![op]), OutputMode::Json)).unwrap();
        assert_eq!(v["ops"][0]["protocol"], "gRPC");
        assert_eq!(v["ops"][0]["doc_url"], "https://x");
        assert_eq!(v["ops"][0]["test_node"], true);
    }

    #[test]
    fn json_distinguishes_cleared_verifications_from_untouched() {
        // Dual-output rule: the cleared-vs-untouched distinction must hold in JSON
        // too — Some([]) -> [] (cleared), None -> null (untouched).
        let cleared = OpSummary::UpdateNode {
            path: "Api.Rater".to_string(),
            name: None,
            description: None,
            constraints: None,
            inputs: None,
            outputs: None,
            user_kind: None,
            path_prefix: None,
            language: None,
            external: None,
            external_kind: None,
            protocol: None,
            doc_url: None,
            is_test_node: None,
            config: None,
            verifications: Some(vec![]),
        };
        let untouched = OpSummary::UpdateNode {
            path: "Api.Rater".to_string(),
            name: None,
            description: None,
            constraints: None,
            inputs: None,
            outputs: None,
            user_kind: None,
            path_prefix: None,
            language: None,
            external: None,
            external_kind: None,
            protocol: None,
            doc_url: None,
            is_test_node: None,
            config: None,
            verifications: None,
        };
        let v: serde_json::Value =
            serde_json::from_str(&render(&summary(vec![cleared]), OutputMode::Json)).unwrap();
        assert!(v["ops"][0]["verifications"].is_array(), "cleared -> []");
        assert_eq!(v["ops"][0]["verifications"].as_array().unwrap().len(), 0);
        let v: serde_json::Value =
            serde_json::from_str(&render(&summary(vec![untouched]), OutputMode::Json)).unwrap();
        assert!(v["ops"][0]["verifications"].is_null(), "untouched -> null");
    }

    #[test]
    fn human_renders_cleared_verifications_distinctly() {
        let op = OpSummary::UpdateNode {
            path: "Api.Rater".to_string(),
            name: None,
            description: None,
            constraints: None,
            inputs: None,
            outputs: None,
            user_kind: None,
            path_prefix: None,
            language: None,
            external: None,
            external_kind: None,
            protocol: None,
            doc_url: None,
            is_test_node: None,
            config: None,
            verifications: Some(vec![]),
        };
        assert!(render(&summary(vec![op]), OutputMode::Human).contains("verifications: (cleared)"));
    }

    #[test]
    fn human_and_json_render_an_edge_deletion_by_ports() {
        let op = OpSummary::DeleteEdge {
            from: "Maker.dog".to_string(),
            to: "Api.Rater.raw".to_string(),
        };
        let human = render(&summary(vec![op.clone()]), OutputMode::Human);
        assert_eq!(human, "- edge Maker.dog -> Api.Rater.raw");
        let out = render(&summary(vec![op]), OutputMode::Json);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["ops"][0]["op"], "delete_edge");
        assert_eq!(v["ops"][0]["from"], "Maker.dog");
        assert_eq!(v["ops"][0]["to"], "Api.Rater.raw");
    }

    #[test]
    fn human_reports_empty_stage() {
        assert_eq!(
            render(&summary(vec![]), OutputMode::Human),
            "Nothing staged."
        );
    }

    #[test]
    fn json_carries_each_op_in_order() {
        let out = render(&summary(vec![node_op(), edge_op()]), OutputMode::Json);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let ops = v["ops"].as_array().unwrap();
        assert_eq!(ops[0]["op"], "add_node");
        assert_eq!(ops[0]["node"], "Api.Rater");
        assert_eq!(ops[0]["inputs"][0]["name"], "raw");
        assert_eq!(ops[0]["inputs"][0]["type"], "HotDog");
        assert_eq!(ops[1]["op"], "add_edge");
        assert_eq!(ops[1]["from"], "Maker.dog");
        assert_eq!(ops[1]["to"], "Api.Rater.raw");
    }
}
