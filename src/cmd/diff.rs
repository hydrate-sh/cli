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
        } => {
            let mut line = format!("+ {kind} {path}");
            let mut parts = Vec::new();
            if !inputs.is_empty() {
                parts.push(format!("in: {}", join_ports(inputs)));
            }
            if !outputs.is_empty() {
                parts.push(format!("out: {}", join_ports(outputs)));
            }
            if !parts.is_empty() {
                line.push_str(&format!(" ({})", parts.join("; ")));
            }
            line
        }
        OpSummary::Edge { from, to } => format!("+ edge {from} -> {to}"),
        OpSummary::Other { kind } => format!("+ {kind}"),
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
        } => serde_json::json!({
            "op": "add_node",
            "kind": kind,
            "node": path,
            "inputs": ports_json(inputs),
            "outputs": ports_json(outputs),
        }),
        OpSummary::Edge { from, to } => serde_json::json!({
            "op": "add_edge",
            "from": from,
            "to": to,
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
    fn human_renders_edge_by_paths() {
        let out = render(&summary(vec![edge_op()]), OutputMode::Human);
        assert_eq!(out, "+ edge Maker.dog -> Api.Rater.raw");
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
