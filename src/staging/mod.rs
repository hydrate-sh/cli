//! The authoring engine: turn named, path-addressed authoring commands into the
//! typed wire deltas that `commit` will POST, entirely client-side.
//!
//! Identity is minted here. Every new node and port gets a locally-generated
//! UUID at stage time; the author only ever types names. A `name → UUID` alias
//! table (persisted in the stage) lets later commands in the same session
//! address those freshly-minted entities by their dotted path
//! (`Api.Rater.raw`) without the author ever seeing a UUID or a port handle.
//!
//! This layer holds **no** copy of the server's type rules — it lowers names to
//! handles and checks only what is purely local (slug shape, required port
//! types, and not staging the same name twice). The server remains the sole
//! authority on the graph's validity; a bad batch is rejected at commit, loudly.

use hydrate_wire::models;
use serde::Serialize;
use uuid::Uuid;

use crate::error::CliError;
use crate::state::Stage;

/// Which side of a node a port lives on. An edge's source is an output; its
/// target is an input — so the side is implied by how a port path is used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    In,
    Out,
}

impl Side {
    fn as_str(self) -> &'static str {
        match self {
            Side::In => "in",
            Side::Out => "out",
        }
    }

    fn opposite(self) -> Side {
        match self {
            Side::In => Side::Out,
            Side::Out => Side::In,
        }
    }
}

/// A typed port to declare on a node (`name:type`, type required).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortSpec {
    pub name: String,
    pub r#type: String,
}

/// A node to stage. `parent` is the dotted path of an already-staged boundary
/// (or `None` for a top-level node).
#[derive(Debug, Clone)]
pub struct NodeSpec<'a> {
    pub kind: models::node::Kind,
    pub name: &'a str,
    pub parent: Option<&'a str>,
    pub inputs: Vec<PortSpec>,
    pub outputs: Vec<PortSpec>,
    pub user_kind: Option<&'a str>,
    pub path_prefix: Option<&'a str>,
}

/// What `add_node` recorded, for the caller to render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeAdded {
    pub path: String,
    pub kind: &'static str,
    pub inputs: usize,
    pub outputs: usize,
}

/// The wire kind as its stable lowercase token (`behavior` / `boundary`).
fn kind_str(kind: models::node::Kind) -> &'static str {
    match kind {
        models::node::Kind::Behavior => "behavior",
        models::node::Kind::Boundary => "boundary",
    }
}

/// An in-memory view over the on-disk [`Stage`] that can resolve dotted paths
/// and append authoring deltas. Load it from a stage, mutate, then write it back.
pub struct Changeset {
    stage: Stage,
}

impl Changeset {
    pub fn from_stage(stage: Stage) -> Changeset {
        Changeset { stage }
    }

    pub fn into_stage(self) -> Stage {
        self.stage
    }

    /// The staged deltas so far (used by the rejection tests; `status`/`diff`
    /// read the persisted stage directly).
    #[cfg(test)]
    fn deltas(&self) -> &[serde_json::Value] {
        &self.stage.deltas
    }

    /// Validate, mint identity for, and stage a node (plus its ports).
    pub fn add_node(&mut self, spec: &NodeSpec) -> Result<NodeAdded, CliError> {
        validate_slug(spec.name, "node name")?;

        // Boundary-only flags must not appear on a behavior node — that is a
        // command-grammar mistake we can catch locally and clearly.
        if spec.kind == models::node::Kind::Behavior
            && (spec.user_kind.is_some() || spec.path_prefix.is_some())
        {
            return Err(CliError::InvalidArgument(
                "--user-kind/--path-prefix apply only to --kind boundary".to_string(),
            ));
        }

        // Resolve the parent (if any) within the staged set, and compute this
        // node's full dotted path.
        let parent_id = match spec.parent {
            Some(parent) => Some(Some(self.resolve_node(parent)?)),
            None => Some(None),
        };
        let path = match spec.parent {
            Some(parent) => format!("{parent}.{}", spec.name),
            None => spec.name.to_string(),
        };
        if self.stage.aliases.contains_key(&node_key(&path)) {
            return Err(CliError::InvalidArgument(format!(
                "a node '{path}' is already staged; choose another name"
            )));
        }

        let inputs = self.mint_ports(&path, Side::In, &spec.inputs)?;
        let outputs = self.mint_ports(&path, Side::Out, &spec.outputs)?;

        let node_id = Uuid::new_v4();
        let data = models::NodeData {
            name: Some(spec.name.to_string()),
            inputs: Some(inputs.deltas),
            outputs: Some(outputs.deltas),
            user_kind: spec.user_kind.map(|k| Some(k.to_string())),
            path_prefix: spec.path_prefix.map(|p| Some(p.to_string())),
            ..Default::default()
        };
        let node = models::Node {
            data: Some(Box::new(data)),
            id: node_id,
            kind: spec.kind,
            parent_id,
        };
        let delta = models::AddNodeDelta::new(node, models::add_node_delta::Type::AddNode);
        self.push(&delta)?;

        // Record identity only after the delta is built, so a serialization
        // failure cannot leave a dangling alias.
        self.stage.aliases.insert(node_key(&path), node_id);
        for (spec, id) in spec.inputs.iter().zip(inputs.ids) {
            self.stage
                .aliases
                .insert(port_key(&path, Side::In, &spec.name), id);
        }
        for (spec, id) in spec.outputs.iter().zip(outputs.ids) {
            self.stage
                .aliases
                .insert(port_key(&path, Side::Out, &spec.name), id);
        }

        Ok(NodeAdded {
            path,
            kind: kind_str(spec.kind),
            inputs: spec.inputs.len(),
            outputs: spec.outputs.len(),
        })
    }

    /// Resolve a dotted node path to its staged UUID, or fail loud.
    fn resolve_node(&self, path: &str) -> Result<Uuid, CliError> {
        self.stage
            .aliases
            .get(&node_key(path))
            .copied()
            .ok_or_else(|| {
                CliError::InvalidArgument(format!(
                    "unknown node '{path}'; stage it before referencing it"
                ))
            })
    }

    /// Validate a port set (slugs, types, no duplicate name on the same side)
    /// and mint a UUID for each, returning the wire ports and their ids in order.
    fn mint_ports(
        &self,
        node_path: &str,
        side: Side,
        specs: &[PortSpec],
    ) -> Result<MintedPorts, CliError> {
        let mut deltas = Vec::with_capacity(specs.len());
        let mut ids = Vec::with_capacity(specs.len());
        let mut seen = std::collections::BTreeSet::new();
        for port in specs {
            validate_slug(&port.name, "port name")?;
            validate_type(&port.r#type)?;
            if !seen.insert(port.name.as_str()) {
                return Err(CliError::InvalidArgument(format!(
                    "duplicate {} port '{}' on '{node_path}'",
                    side.as_str(),
                    port.name
                )));
            }
            let id = Uuid::new_v4();
            ids.push(id);
            deltas.push(models::Port {
                description: None,
                id,
                name: Some(port.name.clone()),
                r#type: Some(port.r#type.clone()),
            });
        }
        Ok(MintedPorts { deltas, ids })
    }

    /// Validate, mint identity for, and stage an edge between two staged ports.
    /// `from` is an output-port path, `to` an input-port path (the side is
    /// implied by position), each `node.port` over the staged set.
    pub fn add_edge(&mut self, from: &str, to: &str) -> Result<EdgeAdded, CliError> {
        let source = self.resolve_port(from, Side::Out)?;
        let target = self.resolve_port(to, Side::In)?;

        let key = edge_key(source, target);
        if self.stage.aliases.contains_key(&key) {
            return Err(CliError::InvalidArgument(format!(
                "an edge from '{from}' to '{to}' is already staged"
            )));
        }

        let edge_id = Uuid::new_v4();
        let edge = models::Edge {
            id: edge_id,
            source_handle: Some(Some(source)),
            target_handle: Some(Some(target)),
        };
        let delta = models::AddEdgeDelta::new(edge, models::add_edge_delta::Type::AddEdge);
        self.push(&delta)?;
        self.stage.aliases.insert(key, edge_id);

        Ok(EdgeAdded {
            from: from.to_string(),
            to: to.to_string(),
        })
    }

    /// Resolve a `node.port` path to a staged port UUID on the given side.
    fn resolve_port(&self, path: &str, side: Side) -> Result<Uuid, CliError> {
        let (node_path, port) = path.rsplit_once('.').ok_or_else(|| {
            CliError::InvalidArgument(format!(
                "'{path}' is not a port path; write it as node.port (e.g. Rater.raw)"
            ))
        })?;
        if let Some(id) = self.stage.aliases.get(&port_key(node_path, side, port)) {
            return Ok(*id);
        }
        // Give a precise diagnostic when the port exists, just on the other side
        // (e.g. an input used as `--from`) rather than the misleading "unknown".
        if self
            .stage
            .aliases
            .contains_key(&port_key(node_path, side.opposite(), port))
        {
            return Err(CliError::InvalidArgument(format!(
                "'{path}' is an {} port; an edge runs from an output (--from) to an input (--to)",
                side.opposite().as_str()
            )));
        }
        Err(CliError::InvalidArgument(format!(
            "unknown {} port '{path}'; stage the node that owns it first",
            side.as_str()
        )))
    }

    /// Serialize a delta to JSON and append it to the staged batch.
    fn push<D: Serialize>(&mut self, delta: &D) -> Result<(), CliError> {
        let value = serde_json::to_value(delta)
            .map_err(|e| CliError::Other(format!("could not encode the staged delta: {e}")))?;
        self.stage.deltas.push(value);
        Ok(())
    }
}

/// What `add_edge` recorded, for the caller to render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeAdded {
    pub from: String,
    pub to: String,
}

struct MintedPorts {
    deltas: Vec<models::Port>,
    ids: Vec<Uuid>,
}

fn node_key(path: &str) -> String {
    format!("node:{path}")
}

fn port_key(node_path: &str, side: Side, name: &str) -> String {
    format!("port:{node_path}:{}:{name}", side.as_str())
}

fn edge_key(source: Uuid, target: Uuid) -> String {
    format!("edge:{source}:{target}")
}

/// Strict slug: one or more of letters, digits, `-`, `_`. The dot is reserved as
/// the path separator, so it is not allowed inside a name.
pub fn validate_slug(value: &str, what: &str) -> Result<(), CliError> {
    if value.is_empty() {
        return Err(CliError::InvalidArgument(format!(
            "{what} must not be empty"
        )));
    }
    if let Some(bad) = value
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || *c == '-' || *c == '_'))
    {
        return Err(CliError::InvalidArgument(format!(
            "invalid {what} '{value}': '{bad}' is not allowed — use letters, digits, '-', or '_'"
        )));
    }
    Ok(())
}

/// A port type is any non-empty trimmed string; it is matched case-sensitively
/// by the server, so we do not normalize it, only reject the empty case.
pub fn validate_type(value: &str) -> Result<(), CliError> {
    if value.trim().is_empty() {
        return Err(CliError::InvalidArgument(
            "port type must not be empty (use 'any' for an untyped port)".to_string(),
        ));
    }
    Ok(())
}

/// Parse a `name:type` port flag. Both parts are required; the type may contain
/// anything but whitespace-only.
pub fn parse_port_spec(raw: &str) -> Result<PortSpec, CliError> {
    let (name, r#type) = raw.split_once(':').ok_or_else(|| {
        CliError::InvalidArgument(format!(
            "port '{raw}' must be written name:type (e.g. raw:HotDog)"
        ))
    })?;
    validate_slug(name, "port name")?;
    validate_type(r#type)?;
    Ok(PortSpec {
        name: name.to_string(),
        // Canonicalize by trimming surrounding whitespace (the type is matched
        // exactly by the server, so internal characters and case are preserved);
        // this prevents an invisible-to-the-eye `" HotDog"` from being rejected
        // at commit with no way to see why.
        r#type: r#type.trim().to_string(),
    })
}

/// A `name:type` pair, e.g. an input port `raw:HotDog`.
pub type NamedType = (String, String);

/// One staged delta, rendered for inspection with all identity translated back
/// to dotted paths — `status`/`diff` never surface a UUID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpSummary {
    Node {
        kind: &'static str,
        path: String,
        inputs: Vec<NamedType>,
        outputs: Vec<NamedType>,
    },
    Edge {
        from: String,
        to: String,
    },
    /// A delta kind this version does not render in detail (forward-compat).
    Other {
        kind: String,
    },
}

/// Counts plus a per-op view of the staged changeset.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StageSummary {
    pub nodes: usize,
    pub edges: usize,
    pub other: usize,
    pub ops: Vec<OpSummary>,
}

impl StageSummary {
    pub fn total(&self) -> usize {
        self.ops.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }
}

/// Summarize a staged changeset for `status`/`diff`, rendering minted UUIDs back
/// to the dotted paths the author used. A staged delta that cannot be parsed is
/// a loud error — corruption is surfaced, never silently skipped.
pub fn summarize(stage: &Stage) -> Result<StageSummary, CliError> {
    let node_paths = reverse_paths(stage, "node:", render_node_path);
    let port_paths = reverse_paths(stage, "port:", render_port_path);

    let mut summary = StageSummary::default();
    for value in &stage.deltas {
        let kind = value
            .get("type")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| CliError::State("a staged delta is missing its type".to_string()))?;
        match kind {
            "add_node" => {
                let d: models::AddNodeDelta = parse_delta(value)?;
                let data = d.node.data.unwrap_or_default();
                let path = node_paths
                    .get(&d.node.id)
                    .cloned()
                    .or_else(|| data.name.clone())
                    .ok_or_else(|| {
                        CliError::State("a staged node has no resolvable path".to_string())
                    })?;
                summary.nodes += 1;
                summary.ops.push(OpSummary::Node {
                    kind: kind_str(d.node.kind),
                    path,
                    inputs: named_types(data.inputs.as_deref()),
                    outputs: named_types(data.outputs.as_deref()),
                });
            }
            "add_edge" => {
                let d: models::AddEdgeDelta = parse_delta(value)?;
                summary.edges += 1;
                summary.ops.push(OpSummary::Edge {
                    from: handle_path(&port_paths, d.edge.source_handle)?,
                    to: handle_path(&port_paths, d.edge.target_handle)?,
                });
            }
            other => {
                summary.other += 1;
                summary.ops.push(OpSummary::Other {
                    kind: other.to_string(),
                });
            }
        }
    }
    Ok(summary)
}

fn parse_delta<D: serde::de::DeserializeOwned>(value: &serde_json::Value) -> Result<D, CliError> {
    D::deserialize(value)
        .map_err(|e| CliError::State(format!("a staged delta could not be read: {e}")))
}

/// Build a UUID → display-string map from the alias keys with the given prefix.
fn reverse_paths(
    stage: &Stage,
    prefix: &str,
    render: fn(&str) -> String,
) -> std::collections::HashMap<Uuid, String> {
    stage
        .aliases
        .iter()
        .filter_map(|(key, id)| key.strip_prefix(prefix).map(|rest| (*id, render(rest))))
        .collect()
}

/// `node:` alias bodies are already the dotted path.
fn render_node_path(rest: &str) -> String {
    rest.to_string()
}

/// `port:` alias bodies are `node_path:side:name`; render as `node_path.name`
/// (side is dropped — it is not part of the author-facing path).
fn render_port_path(rest: &str) -> String {
    let mut parts = rest.rsplitn(3, ':');
    let name = parts.next().unwrap_or("");
    let _side = parts.next();
    let node_path = parts.next().unwrap_or("");
    format!("{node_path}.{name}")
}

/// Resolve an edge handle UUID back to its port path. An absent handle or one
/// that points at a port the stage doesn't contain is corruption — surfaced
/// loudly, never rendered as a benign placeholder.
fn handle_path(
    map: &std::collections::HashMap<Uuid, String>,
    handle: Option<Option<Uuid>>,
) -> Result<String, CliError> {
    let id = handle
        .flatten()
        .ok_or_else(|| CliError::State("a staged edge is missing an endpoint".to_string()))?;
    map.get(&id).cloned().ok_or_else(|| {
        CliError::State("a staged edge references a port that is not staged".to_string())
    })
}

fn named_types(ports: Option<&[models::Port]>) -> Vec<NamedType> {
    ports
        .unwrap_or_default()
        .iter()
        .map(|p| {
            (
                p.name.clone().unwrap_or_default(),
                p.r#type.clone().unwrap_or_default(),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use hydrate_wire::models::node::Kind;

    fn port(name: &str, ty: &str) -> PortSpec {
        PortSpec {
            name: name.to_string(),
            r#type: ty.to_string(),
        }
    }

    fn behavior<'a>(name: &'a str, parent: Option<&'a str>) -> NodeSpec<'a> {
        NodeSpec {
            kind: Kind::Behavior,
            name,
            parent,
            inputs: vec![],
            outputs: vec![],
            user_kind: None,
            path_prefix: None,
        }
    }

    fn empty() -> Changeset {
        Changeset::from_stage(Stage::empty())
    }

    #[test]
    fn parse_port_spec_splits_name_and_type() {
        assert_eq!(
            parse_port_spec("raw:HotDog").unwrap(),
            port("raw", "HotDog")
        );
        // Only the first colon splits, so a type may itself contain a colon.
        assert_eq!(parse_port_spec("u:ns:Type").unwrap(), port("u", "ns:Type"));
    }

    #[test]
    fn parse_port_spec_requires_both_parts() {
        for bad in ["raw", "raw:", ":HotDog", "raw:   "] {
            assert!(
                matches!(parse_port_spec(bad), Err(CliError::InvalidArgument(_))),
                "expected rejection for {bad:?}"
            );
        }
    }

    #[test]
    fn slug_validation_rejects_dots_and_spaces() {
        assert!(validate_slug("Rater", "node name").is_ok());
        for bad in ["a.b", "a b", "", "a/b"] {
            assert!(validate_slug(bad, "node name").is_err(), "{bad:?}");
        }
    }

    #[test]
    fn add_node_stages_one_delta_and_aliases_the_path() {
        let mut cs = empty();
        // Same port name on BOTH sides: only the side qualifier keeps these two
        // handles distinct — drop it from `port_key` and this test fails.
        let added = cs
            .add_node(&NodeSpec {
                inputs: vec![port("raw", "HotDog")],
                outputs: vec![port("raw", "Score")],
                ..behavior("Rater", None)
            })
            .unwrap();
        assert_eq!(added.path, "Rater");
        assert_eq!(
            (added.kind, added.inputs, added.outputs),
            ("behavior", 1, 1)
        );
        assert_eq!(cs.deltas().len(), 1);
        let stage = cs.into_stage();
        assert!(stage.aliases.contains_key("node:Rater"));
        assert!(stage.aliases.contains_key("port:Rater:in:raw"));
        assert!(stage.aliases.contains_key("port:Rater:out:raw"));
        // The in/out handles for the same name are distinct UUIDs.
        assert_ne!(
            stage.aliases["port:Rater:in:raw"],
            stage.aliases["port:Rater:out:raw"]
        );
    }

    #[test]
    fn staged_node_delta_is_commit_ready() {
        // The staged JSON must carry the `add_node` discriminator and rebuild
        // into the concrete delta — this is the guarantee that `commit` can POST
        // it. (`commit` reconstructs `V1DeltasBodyDeltasInner` by dispatching on
        // the `type` tag into the concrete struct, NOT by deserializing the
        // internally-tagged enum directly: serde consumes the tag and the inner
        // struct's own required `type` field then reads as missing.)
        let mut cs = empty();
        cs.add_node(&NodeSpec {
            inputs: vec![port("raw", "HotDog")],
            ..behavior("Rater", None)
        })
        .unwrap();
        let value = cs.into_stage().deltas.remove(0);
        assert_eq!(value["type"], "add_node");
        let d: models::AddNodeDelta = serde_json::from_value(value).unwrap();
        assert_eq!(d.node.kind, Kind::Behavior);
        let data = d.node.data.unwrap();
        assert_eq!(data.name.as_deref(), Some("Rater"));
        let inputs = data.inputs.unwrap();
        assert_eq!(inputs[0].name.as_deref(), Some("raw"));
        assert_eq!(inputs[0].r#type.as_deref(), Some("HotDog"));
        // Top-level node carries an explicit null parent.
        assert_eq!(d.node.parent_id, Some(None));
    }

    #[test]
    fn nesting_resolves_parent_and_builds_dotted_path() {
        let mut cs = empty();
        cs.add_node(&NodeSpec {
            user_kind: Some("service"),
            ..NodeSpec {
                kind: Kind::Boundary,
                ..behavior("Api", None)
            }
        })
        .unwrap();
        let added = cs.add_node(&behavior("Rater", Some("Api"))).unwrap();
        assert_eq!(added.path, "Api.Rater");
        let stage = cs.into_stage();
        assert!(stage.aliases.contains_key("node:Api.Rater"));
        // The child's parent_id is the parent's minted UUID.
        let value = stage.deltas[1].clone();
        let d: models::AddNodeDelta = serde_json::from_value(value).unwrap();
        assert_eq!(d.node.parent_id, Some(Some(stage.aliases["node:Api"])));
    }

    #[test]
    fn unknown_parent_fails_loud() {
        let mut cs = empty();
        let err = cs.add_node(&behavior("Rater", Some("Ghost"))).unwrap_err();
        assert!(matches!(err, CliError::InvalidArgument(_)), "got {err:?}");
        assert!(err.to_string().contains("Ghost"), "{err}");
    }

    #[test]
    fn duplicate_node_name_in_scope_fails_loud() {
        let mut cs = empty();
        cs.add_node(&behavior("Rater", None)).unwrap();
        let err = cs.add_node(&behavior("Rater", None)).unwrap_err();
        assert!(matches!(err, CliError::InvalidArgument(_)), "got {err:?}");
        assert_eq!(err.kind(), "invalid_argument");
        assert!(err.to_string().contains("already staged"), "{err}");
        // Nothing was appended for the rejected node.
        assert_eq!(cs.deltas().len(), 1);
    }

    #[test]
    fn same_name_in_different_parents_is_allowed() {
        let mut cs = empty();
        cs.add_node(&NodeSpec {
            kind: Kind::Boundary,
            ..behavior("A", None)
        })
        .unwrap();
        cs.add_node(&NodeSpec {
            kind: Kind::Boundary,
            ..behavior("B", None)
        })
        .unwrap();
        cs.add_node(&behavior("Rater", Some("A"))).unwrap();
        // Same leaf name under a different boundary is a distinct path.
        cs.add_node(&behavior("Rater", Some("B"))).unwrap();
        let stage = cs.into_stage();
        assert!(stage.aliases.contains_key("node:A.Rater"));
        assert!(stage.aliases.contains_key("node:B.Rater"));
    }

    #[test]
    fn duplicate_port_on_same_side_fails_loud() {
        let mut cs = empty();
        let err = cs
            .add_node(&NodeSpec {
                inputs: vec![port("raw", "HotDog"), port("raw", "Other")],
                ..behavior("Rater", None)
            })
            .unwrap_err();
        assert!(matches!(err, CliError::InvalidArgument(_)), "got {err:?}");
        // The failed node left no partial state.
        assert!(cs.deltas().is_empty());
    }

    #[test]
    fn boundary_flags_on_behavior_are_rejected() {
        let mut cs = empty();
        let err = cs
            .add_node(&NodeSpec {
                user_kind: Some("service"),
                ..behavior("Rater", None)
            })
            .unwrap_err();
        assert!(matches!(err, CliError::InvalidArgument(_)), "got {err:?}");
        // The rejected node staged nothing.
        assert!(cs.deltas().is_empty());
    }

    #[test]
    fn missing_port_type_is_rejected_before_staging() {
        let mut cs = empty();
        let err = cs
            .add_node(&NodeSpec {
                inputs: vec![port("raw", "  ")],
                ..behavior("Rater", None)
            })
            .unwrap_err();
        assert!(matches!(err, CliError::InvalidArgument(_)), "got {err:?}");
        assert!(cs.deltas().is_empty());
    }

    #[test]
    fn parse_port_spec_trims_surrounding_type_whitespace() {
        // A stray space would otherwise stage `" HotDog"` and be rejected at
        // commit by the server's exact match, with no visible difference.
        assert_eq!(parse_port_spec("raw: HotDog ").unwrap().r#type, "HotDog");
        // Internal characters are preserved verbatim (case-sensitive exact match).
        assert_eq!(parse_port_spec("u:Hot Dog").unwrap().r#type, "Hot Dog");
    }

    #[test]
    fn boundary_node_delta_carries_user_kind_and_path_prefix() {
        let mut cs = empty();
        cs.add_node(&NodeSpec {
            kind: Kind::Boundary,
            user_kind: Some("service"),
            path_prefix: Some("/api"),
            ..behavior("Api", None)
        })
        .unwrap();
        let d: models::AddNodeDelta =
            serde_json::from_value(cs.into_stage().deltas.remove(0)).unwrap();
        let data = d.node.data.unwrap();
        assert_eq!(d.node.kind, Kind::Boundary);
        assert_eq!(data.user_kind, Some(Some("service".to_string())));
        assert_eq!(data.path_prefix, Some(Some("/api".to_string())));
    }

    // The on-disk surface `commit` depends on: a staged node must survive a real
    // save → Stage::load → reconstruct-the-delta round trip, not just live in
    // memory.
    #[test]
    fn staged_node_survives_disk_round_trip() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut cs = empty();
        cs.add_node(&NodeSpec {
            inputs: vec![port("raw", "HotDog")],
            ..behavior("Rater", None)
        })
        .unwrap();
        cs.into_stage().save(tmp.path()).unwrap();

        let reloaded = Stage::load(tmp.path()).unwrap();
        assert_eq!(reloaded.deltas.len(), 1);
        assert!(reloaded.aliases.contains_key("node:Rater"));
        let d: models::AddNodeDelta = serde_json::from_value(reloaded.deltas[0].clone()).unwrap();
        assert_eq!(d.node.data.unwrap().name.as_deref(), Some("Rater"));
    }

    // ---- edges ----

    /// Stage a tiny two-node graph with one output and one input port.
    fn graph_with_two_ports() -> Changeset {
        let mut cs = empty();
        cs.add_node(&NodeSpec {
            outputs: vec![port("dog", "HotDog")],
            ..behavior("Maker", None)
        })
        .unwrap();
        cs.add_node(&NodeSpec {
            inputs: vec![port("raw", "HotDog")],
            ..behavior("Rater", None)
        })
        .unwrap();
        cs
    }

    #[test]
    fn add_edge_resolves_ports_to_their_handles() {
        let mut cs = graph_with_two_ports();
        let added = cs.add_edge("Maker.dog", "Rater.raw").unwrap();
        assert_eq!(
            (added.from.as_str(), added.to.as_str()),
            ("Maker.dog", "Rater.raw")
        );
        let stage = cs.into_stage();
        // The staged edge delta carries the resolved port UUIDs as its handles —
        // the output port as the source, the input port as the target.
        let edge_delta = stage
            .deltas
            .iter()
            .find(|v| v["type"] == "add_edge")
            .unwrap()
            .clone();
        let d: models::AddEdgeDelta = serde_json::from_value(edge_delta).unwrap();
        assert_eq!(
            d.edge.source_handle,
            Some(Some(stage.aliases["port:Maker:out:dog"]))
        );
        assert_eq!(
            d.edge.target_handle,
            Some(Some(stage.aliases["port:Rater:in:raw"]))
        );
    }

    #[test]
    fn edge_to_unknown_port_fails_loud() {
        let mut cs = graph_with_two_ports();
        // `nope` is not a port on Rater at all.
        let err = cs.add_edge("Maker.dog", "Rater.nope").unwrap_err();
        assert!(matches!(err, CliError::InvalidArgument(_)), "got {err:?}");
        assert!(err.to_string().contains("Rater.nope"), "{err}");
    }

    #[test]
    fn edge_with_a_wrong_side_port_gets_a_precise_error() {
        let mut cs = graph_with_two_ports();
        // `Rater.raw` is an INPUT used as the source (`--from`, an output): the
        // port exists, so the error must say which side it is, not "unknown".
        let err = cs.add_edge("Rater.raw", "Maker.dog").unwrap_err();
        assert!(matches!(err, CliError::InvalidArgument(_)), "got {err:?}");
        let msg = err.to_string();
        assert!(msg.contains("is an in port"), "{msg}");
        assert!(!msg.contains("unknown"), "{msg}");
    }

    #[test]
    fn edge_with_non_port_path_fails_loud() {
        let mut cs = graph_with_two_ports();
        let err = cs.add_edge("Maker", "Rater.raw").unwrap_err();
        assert!(err.to_string().contains("node.port"), "{err}");
    }

    #[test]
    fn duplicate_edge_fails_loud() {
        let mut cs = graph_with_two_ports();
        cs.add_edge("Maker.dog", "Rater.raw").unwrap();
        let err = cs.add_edge("Maker.dog", "Rater.raw").unwrap_err();
        assert!(matches!(err, CliError::InvalidArgument(_)), "got {err:?}");
        assert!(err.to_string().contains("already staged"), "{err}");
    }

    // ---- summarize ----

    #[test]
    fn summarize_renders_paths_not_uuids() {
        let mut cs = graph_with_two_ports();
        cs.add_edge("Maker.dog", "Rater.raw").unwrap();
        let stage = cs.into_stage();
        // Every minted UUID, as it appears stringified — none may surface.
        let minted: Vec<String> = stage.aliases.values().map(Uuid::to_string).collect();

        let summary = summarize(&stage).unwrap();
        assert_eq!((summary.nodes, summary.edges, summary.total()), (2, 1, 3));

        // The edge op renders both endpoints by their dotted port paths.
        let edge = summary
            .ops
            .iter()
            .find_map(|op| match op {
                OpSummary::Edge { from, to } => Some((from.clone(), to.clone())),
                _ => None,
            })
            .unwrap();
        assert_eq!(edge, ("Maker.dog".to_string(), "Rater.raw".to_string()));

        // A node op carries its path + typed ports.
        let maker = summary
            .ops
            .iter()
            .find_map(|op| match op {
                OpSummary::Node { path, outputs, .. } if path == "Maker" => Some(outputs.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(maker, vec![("dog".to_string(), "HotDog".to_string())]);

        // Not one of the minted UUIDs appears anywhere in the rendered summary.
        let blob = format!("{summary:?}");
        for id in &minted {
            assert!(!blob.contains(id), "leaked UUID {id} in {blob}");
        }
    }

    #[test]
    fn summarize_flags_a_delta_missing_its_type() {
        let mut stage = Stage::empty();
        stage.deltas.push(serde_json::json!({"node": {}}));
        let err = summarize(&stage).unwrap_err();
        assert!(matches!(err, CliError::State(_)), "got {err:?}");
    }

    #[test]
    fn summarize_flags_an_edge_handle_with_no_staged_port() {
        // An edge whose handle points at a port the stage doesn't contain is
        // corruption — surfaced loudly, never rendered as a benign "?".
        let mut stage = Stage::empty();
        stage.deltas.push(serde_json::json!({
            "type": "add_edge",
            "edge": {
                "id": Uuid::new_v4(),
                "sourceHandle": Uuid::new_v4(),
                "targetHandle": Uuid::new_v4(),
            }
        }));
        let err = summarize(&stage).unwrap_err();
        assert!(matches!(err, CliError::State(_)), "got {err:?}");
    }

    #[test]
    fn summarize_counts_unknown_delta_types_as_other() {
        let mut stage = Stage::empty();
        stage
            .deltas
            .push(serde_json::json!({"type": "delete_node", "id": Uuid::new_v4()}));
        let summary = summarize(&stage).unwrap();
        assert_eq!((summary.nodes, summary.edges, summary.other), (0, 0, 1));
        assert_eq!(summary.total(), 1);
    }
}
