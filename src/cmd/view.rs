//! The complete, identity-free projection of a node for the read surfaces
//! (`show`, `walk`).
//!
//! A read renders the **whole** node — its `description`, `constraints`,
//! `verifications`, and the boundary/external scalars — never a skeleton. A
//! node's `description` is its prompt, so a read that hides it defeats the point
//! of building from the living spec; the projection here is what both read verbs
//! use so neither can silently drop a field.
//!
//! Identity stays out by design: nodes and ports are addressed by their dotted
//! path and name, not by their server UUIDs, and the placeholder layout
//! `position` is never surfaced. Everything else the API carries is projected.

use std::collections::HashMap;

use hydrate_wire::models::{self, WireEdge, WireNode, WirePort};
use serde::Serialize;
use uuid::Uuid;

use crate::error::CliError;

/// A port in a read view: its name and type (as the author declared them) plus
/// any per-port spec fields the API carries. Unnamed ports render as
/// `<unnamed>`. No id, no owning-node id — this is an inspection surface.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PortView {
    pub name: Option<String>,
    pub r#type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contract_name: Option<String>,
}

impl PortView {
    /// The compact `name:type` label used in the human, single-line port list.
    pub fn label(&self) -> String {
        let name = self.name.as_deref().unwrap_or("<unnamed>");
        match &self.r#type {
            Some(t) => format!("{name}:{t}"),
            None => name.to_string(),
        }
    }
}

/// A verification in a read view: the check's text, its optional type tag, and
/// who authored it (`user` or `agent`). The verification's UUID is identity, so
/// it stays out — the text is the spec content an agent needs.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VerificationView {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    pub author: String,
}

/// The whole node, projected for reading. Every content field the API carries is
/// present; the small booleans (`is_external`, `is_test_node`) always appear so
/// the reader can tell "false" from "the field was dropped". Optional scalars and
/// empty collections are omitted so a plain node stays uncluttered.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NodeView {
    pub path: String,
    pub kind: String,
    /// Codegen language carried by a node — set via `--language`, in practice on
    /// a boundary. Omitted when unset so a languageless node never emits a bogus
    /// or null value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// The node's description — its prompt. Always present (may be empty).
    pub description: String,
    pub status: String,
    pub is_external: bool,
    pub is_test_node: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path_prefix: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub documentation_url: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub constraints: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub source_decisions: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub verifications: Vec<VerificationView>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<PortView>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<PortView>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub config: Vec<PortView>,
}

/// An edge in a read view: source and target as dotted `node.port` paths.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EdgeView {
    pub from: String,
    pub to: String,
}

/// A wire node kind as its stable lowercase token.
pub fn kind_str(kind: models::wire_node::Kind) -> &'static str {
    match kind {
        models::wire_node::Kind::Behavior => "behavior",
        models::wire_node::Kind::Boundary => "boundary",
        models::wire_node::Kind::State => "state",
        models::wire_node::Kind::Io => "io",
        models::wire_node::Kind::Interface => "interface",
    }
}

/// An author enum as its stable lowercase token.
fn author_str(author: models::wire_verification::Author) -> &'static str {
    match author {
        models::wire_verification::Author::User => "user",
        models::wire_verification::Author::Agent => "agent",
    }
}

/// Project one wire node into its complete [`NodeView`] at the given dotted
/// `path`. The caller owns path reconstruction (it differs between a whole-graph
/// tree and a scoped slice); this projects every content field faithfully.
pub fn node_view(node: &WireNode, path: String) -> NodeView {
    let d = &node.data;
    NodeView {
        path,
        kind: kind_str(node.kind).to_string(),
        // `language` is a double option on the wire (present / null / absent);
        // flatten so only a real value surfaces.
        language: d.language.clone().flatten(),
        description: d.description.clone(),
        status: d.status.clone(),
        is_external: d.is_external,
        is_test_node: d.is_test_node,
        user_kind: d.user_kind.clone().flatten(),
        path_prefix: d.path_prefix.clone().flatten(),
        external_kind: d.external_kind.clone().flatten(),
        protocol: d.protocol.clone().flatten(),
        documentation_url: d.documentation_url.clone().flatten(),
        constraints: d.constraints.clone().unwrap_or_default(),
        source_decisions: d.source_decisions.clone().flatten().unwrap_or_default(),
        verifications: d
            .verifications
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|v| VerificationView {
                text: v.text.clone(),
                r#type: v.r#type.clone().flatten(),
                author: author_str(v.author).to_string(),
            })
            .collect(),
        inputs: port_views(d.inputs.as_deref()),
        outputs: port_views(d.outputs.as_deref()),
        config: port_views(d.config.as_deref()),
    }
}

fn port_views(ports: Option<&[WirePort]>) -> Vec<PortView> {
    ports
        .unwrap_or_default()
        .iter()
        .map(|p| PortView {
            name: p.name.clone(),
            r#type: p.r#type.clone(),
            description: p.description.clone(),
            external: p.external,
            contract_name: p.contract_name.clone().flatten(),
        })
        .collect()
}

impl NodeView {
    /// Render the node as an indented human block. `indent_level` is the node's
    /// depth (0 = top of the view); its heading sits one level in and its detail
    /// two, so a nested tree reads as a tree. Every projected field is laid out
    /// readably — a read is never brief by default.
    pub fn human(&self, indent_level: usize) -> String {
        let head = "  ".repeat(indent_level + 1);
        let body = "  ".repeat(indent_level + 2);
        // A dotted path always has at least one segment (names are non-empty), so
        // `rsplit` yields the leaf; there is no fallback case to handle.
        let leaf = self
            .path
            .rsplit('.')
            .next()
            .expect("a node path always has at least one segment");
        let language = self
            .language
            .as_deref()
            .map(|l| format!("  ({l})"))
            .unwrap_or_default();
        let mut out = format!("{head}{leaf}  [{}]{language}", self.kind);

        if !self.description.is_empty() {
            out.push_str(&format!("\n{body}desc: {}", self.description));
        }
        if !self.inputs.is_empty() {
            out.push_str(&format!("\n{body}in:  {}", join_ports(&self.inputs)));
        }
        if !self.outputs.is_empty() {
            out.push_str(&format!("\n{body}out: {}", join_ports(&self.outputs)));
        }
        if !self.config.is_empty() {
            out.push_str(&format!("\n{body}config: {}", join_ports(&self.config)));
        }
        if let Some(k) = &self.user_kind {
            out.push_str(&format!("\n{body}user-kind: {k}"));
        }
        if let Some(p) = &self.path_prefix {
            out.push_str(&format!("\n{body}path-prefix: {p}"));
        }
        if self.is_external {
            out.push_str(&format!("\n{body}external"));
            if let Some(k) = &self.external_kind {
                out.push_str(&format!(" ({k})"));
            }
        }
        if let Some(p) = &self.protocol {
            out.push_str(&format!("\n{body}protocol: {p}"));
        }
        if let Some(u) = &self.documentation_url {
            out.push_str(&format!("\n{body}doc-url: {u}"));
        }
        if self.is_test_node {
            out.push_str(&format!("\n{body}test-node"));
        }
        if !self.constraints.is_empty() {
            out.push_str(&format!("\n{body}constraints:"));
            for c in &self.constraints {
                out.push_str(&format!("\n{body}  - {c}"));
            }
        }
        if !self.verifications.is_empty() {
            out.push_str(&format!("\n{body}verifications:"));
            for v in &self.verifications {
                let ty = v
                    .r#type
                    .as_deref()
                    .map(|t| format!(" [{t}]"))
                    .unwrap_or_default();
                out.push_str(&format!("\n{body}  - ({}){ty} {}", v.author, v.text));
            }
        }
        if !self.source_decisions.is_empty() {
            out.push_str(&format!("\n{body}source-decisions:"));
            for s in &self.source_decisions {
                out.push_str(&format!("\n{body}  - {s}"));
            }
        }
        out
    }
}

fn join_ports(ports: &[PortView]) -> String {
    ports
        .iter()
        .map(PortView::label)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Build a `port UUID → owner label` map over a labeled node set, where each
/// label is the `node-path.port-name` an edge endpoint on that port renders as.
/// Used to translate a scoped slice's edges back to dotted paths.
pub fn port_labels(nodes: &[(String, &WireNode)]) -> HashMap<Uuid, String> {
    let mut labels = HashMap::new();
    for (path, node) in nodes {
        for side in [
            node.data.inputs.as_deref(),
            node.data.outputs.as_deref(),
            node.data.config.as_deref(),
        ] {
            for p in side.unwrap_or_default() {
                let name = p.name.as_deref().unwrap_or("<unnamed>");
                labels.insert(p.id, format!("{path}.{name}"));
            }
        }
    }
    labels
}

/// Translate a slice's edges into dotted `from → to` [`EdgeView`]s using a
/// [`port_labels`] map. An edge missing a handle, or naming a port not in the
/// slice, is corruption in the server's response — surfaced loudly, never a
/// silently-dropped connection.
pub fn resolve_edges(
    labels: &HashMap<Uuid, String>,
    edges: &[WireEdge],
) -> Result<Vec<EdgeView>, CliError> {
    let mut out = Vec::with_capacity(edges.len());
    for edge in edges {
        let (Some(src), Some(tgt)) = (edge.source_handle, edge.target_handle) else {
            return Err(CliError::State(
                "the graph has an edge missing a port handle".to_string(),
            ));
        };
        let from = labels.get(&src).ok_or_else(|| {
            CliError::State(format!(
                "the graph has an edge to an unknown port handle {src}"
            ))
        })?;
        let to = labels.get(&tgt).ok_or_else(|| {
            CliError::State(format!(
                "the graph has an edge to an unknown port handle {tgt}"
            ))
        })?;
        out.push(EdgeView {
            from: from.clone(),
            to: to.clone(),
        });
    }
    out.sort_by(|a, b| (a.from.as_str(), a.to.as_str()).cmp(&(b.from.as_str(), b.to.as_str())));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hydrate_wire::models::{Position, WireNodeData, WireVerification};

    /// A behavior node carrying the full content matrix, to prove nothing is
    /// dropped by the projection.
    fn rich_node() -> WireNode {
        let mut data = WireNodeData::new(
            "Rate a hot dog on a 0-10 scale.".to_string(),
            false,
            false,
            "Rater".to_string(),
            "draft".to_string(),
        );
        data.constraints = Some(vec!["deterministic".to_string()]);
        data.verifications = Some(vec![WireVerification {
            author: models::wire_verification::Author::Agent,
            id: Uuid::from_u128(0x5),
            text: "score is within 0..=10".to_string(),
            r#type: Some(Some("property".to_string())),
        }]);
        data.inputs = Some(vec![WirePort {
            description: Some("the raw dog".to_string()),
            id: Uuid::from_u128(0xF0),
            name: Some("raw".to_string()),
            r#type: Some("HotDog".to_string()),
            external: None,
            contract_name: None,
        }]);
        WireNode {
            data: Box::new(data),
            id: Uuid::from_u128(0x12),
            kind: models::wire_node::Kind::Behavior,
            parent_id: None,
            position: Box::new(Position::new(0.0, 0.0)),
        }
    }

    #[test]
    fn node_view_projects_the_whole_node_into_json() {
        // The projection must carry description, constraints, and verifications —
        // the fields the old skeleton dropped — verbatim.
        let v = node_view(&rich_node(), "Api.Rater".to_string());
        let json = serde_json::to_value(&v).unwrap();
        assert_eq!(json["path"], "Api.Rater");
        assert_eq!(json["kind"], "behavior");
        assert_eq!(json["description"], "Rate a hot dog on a 0-10 scale.");
        assert_eq!(json["status"], "draft");
        assert_eq!(json["constraints"][0], "deterministic");
        assert_eq!(json["verifications"][0]["text"], "score is within 0..=10");
        assert_eq!(json["verifications"][0]["type"], "property");
        assert_eq!(json["verifications"][0]["author"], "agent");
        assert_eq!(json["inputs"][0]["name"], "raw");
        assert_eq!(json["inputs"][0]["type"], "HotDog");
        assert_eq!(json["inputs"][0]["description"], "the raw dog");
        // is_external is a bool that always appears, so false ≠ dropped.
        assert_eq!(json["is_external"], false);
    }

    #[test]
    fn node_view_human_lays_out_every_projected_field() {
        // The human block must surface the same content the JSON does — the two
        // outputs carry the same information.
        let v = node_view(&rich_node(), "Api.Rater".to_string());
        let human = v.human(0);
        assert!(human.contains("Rater  [behavior]"), "{human}");
        assert!(human.contains("desc: Rate a hot dog"), "{human}");
        assert!(human.contains("raw:HotDog"), "{human}");
        assert!(human.contains("constraints:"), "{human}");
        assert!(human.contains("- deterministic"), "{human}");
        assert!(human.contains("verifications:"), "{human}");
        assert!(
            human.contains("(agent) [property] score is within 0..=10"),
            "{human}"
        );
    }

    #[test]
    fn languageless_node_omits_the_language_field() {
        // A node with no language must not emit a bogus or null value in JSON, and
        // must not print the `]  (` language annotation in the human block.
        let v = node_view(&rich_node(), "Api.Rater".to_string());
        let json = serde_json::to_string(&v).unwrap();
        assert!(!json.contains("language"), "{json}");
        assert!(!v.human(0).contains("]  ("), "{}", v.human(0));
    }

    #[test]
    fn indent_level_grows_the_block_indentation() {
        // A deeper node is indented further — the tree reads as a tree.
        let v = node_view(&rich_node(), "Api.Rater".to_string());
        let shallow = v.human(0);
        let deep = v.human(2);
        let lead = |s: &str| s.len() - s.trim_start().len();
        let head = |s: &str| lead(s.lines().find(|l| l.contains("Rater  [")).unwrap());
        assert!(
            head(&deep) > head(&shallow),
            "deep={deep}\nshallow={shallow}"
        );
    }

    #[test]
    fn resolve_edges_translates_handles_to_dotted_paths() {
        let node = rich_node();
        let labels = port_labels(&[("Api.Rater".to_string(), &node)]);
        // A self-edge on the one known handle resolves to its dotted label.
        let edge = WireEdge {
            id: Uuid::from_u128(0xED),
            source: Uuid::from_u128(0x12),
            source_handle: Some(Uuid::from_u128(0xF0)),
            target: Uuid::from_u128(0x12),
            target_handle: Some(Uuid::from_u128(0xF0)),
        };
        let edges = resolve_edges(&labels, &[edge]).unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].from, "Api.Rater.raw");
        assert_eq!(edges[0].to, "Api.Rater.raw");
    }

    #[test]
    fn resolve_edges_fails_loud_on_an_unknown_handle() {
        let labels = HashMap::new();
        let edge = WireEdge {
            id: Uuid::from_u128(0xED),
            source: Uuid::from_u128(0x1),
            source_handle: Some(Uuid::from_u128(0xBEEF)),
            target: Uuid::from_u128(0x1),
            target_handle: Some(Uuid::from_u128(0xBEEF)),
        };
        let err = resolve_edges(&labels, &[edge]).unwrap_err();
        assert!(matches!(err, CliError::State(_)), "got {err:?}");
    }

    #[test]
    fn resolve_edges_fails_loud_on_a_missing_handle() {
        let labels = HashMap::new();
        let edge = WireEdge {
            id: Uuid::from_u128(0xED),
            source: Uuid::from_u128(0x1),
            source_handle: None,
            target: Uuid::from_u128(0x1),
            target_handle: Some(Uuid::from_u128(0xF0)),
        };
        let err = resolve_edges(&labels, &[edge]).unwrap_err();
        assert!(matches!(err, CliError::State(_)), "got {err:?}");
        assert!(err.to_string().contains("missing a port handle"), "{err}");
    }
}
