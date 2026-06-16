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
use crate::state::{Index, Stage};

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
    /// The node's description (the spec/prompt). `None` omits it (server default).
    pub description: Option<&'a str>,
    /// Plain-text constraints; empty omits the field.
    pub constraints: Vec<String>,
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
///
/// Resolution consults the stage first, then the pulled [`Index`] (the committed
/// live graph) when one is present — so an edge can reference a node staged in
/// this session OR one committed in an earlier one. The index is read-only; only
/// the stage is ever written back.
pub struct Changeset {
    stage: Stage,
    index: Option<Index>,
}

impl Changeset {
    /// A changeset over `stage` that also resolves against a pulled [`Index`] of
    /// the live graph (when present), so committed nodes/ports are addressable by
    /// their dotted path. Pass `None` for the within-session flow (no pull).
    pub fn with_index(stage: Stage, index: Option<Index>) -> Changeset {
        Changeset { stage, index }
    }

    pub fn into_stage(self) -> Stage {
        self.stage
    }

    /// Top-level node paths from the pulled index (those with no `.` in the
    /// path) — what `clear` deletes to wipe the branch (cascade removes the
    /// rest). Empty when nothing has been pulled.
    pub fn top_level_node_paths(&self) -> Vec<String> {
        let Some(index) = &self.index else {
            return Vec::new();
        };
        let mut paths: Vec<String> = index
            .entries
            .keys()
            .filter_map(|k| k.strip_prefix("node:"))
            .filter(|p| !p.contains('.'))
            .map(str::to_string)
            .collect();
        paths.sort();
        paths
    }

    /// Resolve an alias key (`node:…`/`port:…`) against the stage first, then the
    /// pulled index. Freshly-staged identity wins over committed identity for the
    /// same path — but `add_node` forbids staging a path that already exists in
    /// either, so in practice the two never disagree.
    fn lookup(&self, key: &str) -> Option<Uuid> {
        self.stage
            .aliases
            .get(key)
            .copied()
            .or_else(|| self.index.as_ref().and_then(|i| i.get(key)))
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
        // A name that already exists on the branch (in the pulled index) would be
        // rejected by the server at commit; catch it locally with a clearer message
        // so the author isn't surprised by a late server rejection.
        if self
            .index
            .as_ref()
            .is_some_and(|i| i.get(&node_key(&path)).is_some())
        {
            return Err(CliError::InvalidArgument(format!(
                "a node '{path}' already exists on the branch; choose another name"
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
            // Description (the prompt) and constraints: omit when absent OR empty
            // so the server applies its own defaults rather than us forcing "".
            // Filtering here (not just in the diff preview) keeps the staged
            // delta, the `diff` preview, and the committed value identical — an
            // empty `--description ""` is "no description", everywhere.
            description: spec
                .description
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            constraints: {
                let kept: Vec<String> = spec
                    .constraints
                    .iter()
                    .filter(|c| !c.trim().is_empty())
                    .cloned()
                    .collect();
                if kept.is_empty() {
                    None
                } else {
                    Some(kept)
                }
            },
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

    /// Resolve a dotted node path to its UUID — staged this session or committed
    /// (pulled into the index) — or fail loud. A node already staged for deletion
    /// is rejected so you can't, e.g., reparent under a node you're removing in
    /// the same batch.
    fn resolve_node(&self, path: &str) -> Result<Uuid, CliError> {
        let id = self.lookup(&node_key(path)).ok_or_else(|| {
            CliError::InvalidArgument(format!(
                "unknown node '{path}'; stage it, or `hydrate pull` if it's already on the branch"
            ))
        })?;
        if self.staged_deletions().contains(&id) {
            return Err(CliError::InvalidArgument(format!(
                "node '{path}' is staged for deletion; can't reference it in the same changeset"
            )));
        }
        Ok(id)
    }

    /// The set of node UUIDs already staged for deletion this session. Derived
    /// from the staged `delete_node` deltas — no separate on-disk state, so the
    /// stage format is unchanged and a re-run can't double-stage a deletion.
    ///
    /// The `nodeId` is always a well-formed UUID because we serialized it here
    /// (`remove_node`); the `.ok()` skip is only for a foreign/corrupt staged
    /// delta, which is independently caught loud at `summarize` and `lower`, so
    /// this never quietly hides a real deletion. Note: this tombstones NODES,
    /// not edge endpoints — an `edge add` onto a node staged for deletion isn't
    /// blocked locally (the index carries no port→node map), but the server
    /// cascade removes that edge with the node, so it's a no-op, not corruption.
    fn staged_deletions(&self) -> std::collections::HashSet<Uuid> {
        self.stage
            .deltas
            .iter()
            .filter(|v| v.get("type").and_then(serde_json::Value::as_str) == Some("delete_node"))
            .filter_map(|v| v.get("nodeId").and_then(serde_json::Value::as_str))
            .filter_map(|s| Uuid::parse_str(s).ok())
            .collect()
    }

    /// Stage a partial edit of the node at `path` (resolved against the
    /// stage ∪ pulled index). `UpdateNodeData` is key-presence partial: only the
    /// fields present in `after` change, the rest are left untouched — so we send
    /// just what was set, never echoing the node's other data. At least one of
    /// `description`/`constraints` must be `Some`, else there's nothing to do.
    pub fn update_node(
        &mut self,
        path: &str,
        description: Option<&str>,
        constraints: Option<Vec<String>>,
    ) -> Result<NodeUpdated, CliError> {
        if description.is_none() && constraints.is_none() {
            return Err(CliError::InvalidArgument(
                "nothing to set — pass --description and/or --constraint".to_string(),
            ));
        }
        let id = self.resolve_node(path)?;
        let after = models::NodeData {
            description: description.map(str::to_string),
            constraints: constraints.clone(),
            ..Default::default()
        };
        let delta = models::UpdateNodeDataDelta::new(
            after,
            id,
            models::update_node_data_delta::Type::UpdateNodeData,
        );
        self.push(&delta)?;
        Ok(NodeUpdated {
            path: path.to_string(),
            description: description.map(str::to_string),
            constraints,
        })
    }

    /// Stage a cascade-deletion of the node at `path` (resolved against the
    /// stage ∪ pulled index). The server cascade removes its descendant subtree
    /// and incident edges; we record only the node id. Fails loud on an unknown
    /// path or a double-delete.
    pub fn remove_node(&mut self, path: &str) -> Result<NodeRemoved, CliError> {
        let id = self.lookup(&node_key(path)).ok_or_else(|| {
            CliError::InvalidArgument(format!(
                "unknown node '{path}'; nothing to remove (run `hydrate pull` if it's on the branch)"
            ))
        })?;
        if self.staged_deletions().contains(&id) {
            return Err(CliError::InvalidArgument(format!(
                "node '{path}' is already staged for deletion"
            )));
        }
        let delta = models::DeleteNodeDelta::new(id, models::delete_node_delta::Type::DeleteNode);
        self.push(&delta)?;
        Ok(NodeRemoved {
            path: path.to_string(),
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
        if let Some(id) = self.lookup(&port_key(node_path, side, port)) {
            return Ok(id);
        }
        // Give a precise diagnostic when the port exists, just on the other side
        // (e.g. an input used as `--from`) rather than the misleading "unknown".
        // Check the same stage∪index domain so the hint fires for committed ports
        // too, not only freshly-staged ones.
        if self
            .lookup(&port_key(node_path, side.opposite(), port))
            .is_some()
        {
            return Err(CliError::InvalidArgument(format!(
                "'{path}' is an {} port; an edge runs from an output (--from) to an input (--to)",
                side.opposite().as_str()
            )));
        }
        Err(CliError::InvalidArgument(format!(
            "unknown {} port '{path}'; stage the node that owns it, or `hydrate pull` if it's already on the branch",
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

/// What `remove_node` recorded, for the caller to render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRemoved {
    pub path: String,
}

/// What `update_node` recorded, for the caller to render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeUpdated {
    pub path: String,
    pub description: Option<String>,
    /// `None` = untouched, `Some([])` = cleared, `Some(vec)` = set.
    pub constraints: Option<Vec<String>>,
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

/// Lower the staged changeset into the typed delta batch `commit` POSTs.
///
/// Nodes are ordered before edges: an edge's handles reference ports created by
/// the `add_node` deltas, so the nodes must be applied first within the batch.
/// A delta this version did not author (an unknown `type`) is a loud error — we
/// refuse to commit something we cannot order or vouch for.
pub fn lower(stage: &Stage) -> Result<Vec<models::V1DeltasBodyDeltasInner>, CliError> {
    use models::V1DeltasBodyDeltasInner as Inner;
    // Ordered buckets: create nodes, then wire edges, then deletions last. An
    // edge's handles reference ports created by the add_node deltas, so nodes
    // must precede edges. Deletions go last for unrelated tear-down within one
    // batch. NOTE: clearing a name and re-adding the SAME name is NOT a
    // one-commit operation — `add_node` rejects a path still present in the
    // pulled index, so you `clear` + `commit`, then `pull` and rebuild in a
    // second commit. Each `type` tag is dispatched into its CONCRETE struct
    // (never the internally-tagged enum, which would eat the tag) — see
    // `staged_node_delta_is_commit_ready`.
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    let mut updates = Vec::new();
    let mut deletes = Vec::new();
    for value in &stage.deltas {
        let kind = value
            .get("type")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| CliError::State("a staged delta is missing its type".to_string()))?;
        match kind {
            "add_node" => nodes.push(Inner::AddNode(Box::new(parse_delta(value)?))),
            "add_edge" => edges.push(Inner::AddEdge(Box::new(parse_delta(value)?))),
            "update_node_data" => {
                updates.push(Inner::UpdateNodeData(Box::new(parse_delta(value)?)))
            }
            "delete_node" => deletes.push(Inner::DeleteNode(Box::new(parse_delta(value)?))),
            other => {
                return Err(CliError::State(format!(
                    "cannot commit an unsupported staged delta '{other}'"
                )))
            }
        }
    }
    // nodes → edges → updates → deletes: a node exists before it's wired or
    // edited, and tear-down comes last.
    nodes.append(&mut edges);
    nodes.append(&mut updates);
    nodes.append(&mut deletes);
    Ok(nodes)
}

/// Build a resolution [`Index`] from a pulled branch graph: every committed node
/// and every named port, keyed by the same `node:`/`port:` scheme the stage's
/// alias table uses, so `resolve_node`/`resolve_port` fall through to committed
/// identity with one key.
///
/// Each node's dotted path is reconstructed from the flat `parent_id` chain. A
/// missing parent or a `parent_id` cycle is corruption in the server's response
/// — surfaced loudly, never silently dropping a node from the index (which would
/// make its path quietly unresolvable, the exact mis-resolution we refuse). An
/// unnamed committed port is skipped: it cannot be addressed by a dotted path, so
/// there is nothing to resolve — it is not droppable identity the author can name.
pub fn index_from_graph(graph: &models::GraphResponse) -> Result<Index, CliError> {
    let by_id: std::collections::HashMap<Uuid, &models::WireNode> =
        graph.nodes.iter().map(|n| (n.id, n)).collect();

    let mut entries = std::collections::BTreeMap::new();
    let mut node_info = std::collections::BTreeMap::new();
    for node in &graph.nodes {
        let path = node_path(node, &by_id)?;
        if entries.insert(node_key(&path), node.id).is_some() {
            return Err(CliError::State(format!(
                "the branch graph reports two nodes at path '{path}'"
            )));
        }
        insert_ports(&mut entries, &path, Side::In, node.data.inputs.as_deref())?;
        insert_ports(&mut entries, &path, Side::Out, node.data.outputs.as_deref())?;
        // Per-node kind + ports — what `node set` patches and `boundary flatten`
        // checks. Keyed by the server's node id.
        node_info.insert(
            node.id,
            crate::state::NodeInfo {
                kind: node_kind_str(node.kind).to_string(),
                inputs: port_infos(node.data.inputs.as_deref()),
                outputs: port_infos(node.data.outputs.as_deref()),
            },
        );
    }

    // Edge map: (source_handle, target_handle) → edge id, for `edge rm`. An edge
    // missing a handle, or two edges between the same ports, is corruption —
    // surfaced loudly, never silently dropped (which would make `edge rm` claim
    // "no such edge" for an edge that exists).
    let mut edges = std::collections::BTreeMap::new();
    for edge in &graph.edges {
        let (Some(src), Some(tgt)) = (edge.source_handle, edge.target_handle) else {
            return Err(CliError::State(
                "the branch graph has an edge missing a port handle".to_string(),
            ));
        };
        if edges
            .insert(crate::state::edge_lookup_key(src, tgt), edge.id)
            .is_some()
        {
            return Err(CliError::State(
                "the branch graph reports two edges between the same ports".to_string(),
            ));
        }
    }

    Ok(Index {
        version: graph.branch.version,
        entries,
        node_info,
        edges,
    })
}

/// A wire node kind as its stable token (`behavior` / `boundary`).
fn node_kind_str(kind: models::wire_node::Kind) -> &'static str {
    match kind {
        models::wire_node::Kind::Behavior => "behavior",
        models::wire_node::Kind::Boundary => "boundary",
    }
}

/// Project a wire port list into the index's [`PortInfo`]. Unnamed ports are
/// skipped (not path-addressable, so `node set` can't target them; the server
/// keeps them as-is when the surrounding node is updated).
fn port_infos(ports: Option<&[models::WirePort]>) -> Vec<crate::state::PortInfo> {
    ports
        .unwrap_or_default()
        .iter()
        .filter_map(|p| {
            Some(crate::state::PortInfo {
                id: p.id,
                name: p.name.clone()?,
                r#type: p.r#type.clone().unwrap_or_default(),
            })
        })
        .collect()
}

/// Reconstruct a node's dotted path by walking the `parent_id` chain to the root.
fn node_path(
    node: &models::WireNode,
    by_id: &std::collections::HashMap<Uuid, &models::WireNode>,
) -> Result<String, CliError> {
    let mut parts = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut current = node;
    loop {
        if !seen.insert(current.id) {
            return Err(CliError::State(
                "the branch graph has a parent_id cycle".to_string(),
            ));
        }
        // `.` is the path separator and names must be non-empty: a server name
        // that breaks either rule would build a path that silently shadows or
        // collides with a real nesting, mis-resolving to the wrong UUID. The
        // local authoring slug rules forbid both, so this only fires on a graph
        // authored elsewhere — surface it loudly rather than index a bad path.
        let name = &current.data.name;
        if name.is_empty() || name.contains('.') {
            return Err(CliError::State(format!(
                "the branch graph has a node name that can't be path-addressed: {name:?}"
            )));
        }
        parts.push(name.clone());
        match current.parent_id {
            None => break,
            Some(parent_id) => {
                current = by_id.get(&parent_id).copied().ok_or_else(|| {
                    CliError::State(format!(
                        "the branch graph node '{}' references a missing parent {parent_id}",
                        current.data.name
                    ))
                })?;
            }
        }
    }
    parts.reverse();
    Ok(parts.join("."))
}

/// Add one side's named ports to the index under `port:<node_path>:<side>:<name>`.
fn insert_ports(
    entries: &mut std::collections::BTreeMap<String, Uuid>,
    node_path: &str,
    side: Side,
    ports: Option<&[models::WirePort]>,
) -> Result<(), CliError> {
    for port in ports.unwrap_or_default() {
        // Unnamed ports aren't path-addressable; nothing to resolve, so skip.
        let Some(name) = port.name.as_deref() else {
            continue;
        };
        if entries
            .insert(port_key(node_path, side, name), port.id)
            .is_some()
        {
            return Err(CliError::State(format!(
                "the branch graph reports two {} ports named '{name}' on '{node_path}'",
                side.as_str()
            )));
        }
    }
    Ok(())
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
        /// The node's description (the spec/prompt), if one was staged.
        description: Option<String>,
        /// Plain-text constraints staged on the node.
        constraints: Vec<String>,
    },
    Edge {
        from: String,
        to: String,
    },
    /// A staged partial edit of a node's data (only the set fields change).
    /// `constraints`: `None` = untouched, `Some([])` = cleared, `Some(vec)` = set.
    UpdateNode {
        path: String,
        description: Option<String>,
        constraints: Option<Vec<String>>,
    },
    /// A staged node deletion (cascades the subtree server-side).
    DeleteNode {
        path: String,
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
    pub updates: usize,
    pub deletes: usize,
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
/// Load the stage and the pulled index from `base`, then summarize. The single
/// entry point `status`/`diff` use, so the index is always threaded into
/// rendering — a committed edge handle resolves to its dotted path rather than
/// reading as "not staged". Keeping this here (not inline in each command) means
/// the wiring can't silently regress to summarizing the stage alone.
pub fn summarize_workdir(base: &std::path::Path) -> Result<StageSummary, CliError> {
    let stage = Stage::load(base)?;
    let index = Index::load(base)?;
    summarize(&stage, index.as_ref())
}

pub fn summarize(stage: &Stage, index: Option<&Index>) -> Result<StageSummary, CliError> {
    // Build the UUID → path maps from BOTH the stage aliases and the pulled
    // index: a cross-commit edge's handle is a COMMITTED port UUID that lives
    // only in the index, so rendering it (status/diff) needs the index too —
    // otherwise it reads as "references a port that is not staged".
    let node_paths = reverse_paths(stage, index, "node:", render_node_path);
    let port_paths = reverse_paths(stage, index, "port:", render_port_path);

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
                    description: data.description.filter(|s| !s.is_empty()),
                    constraints: data.constraints.unwrap_or_default(),
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
            "update_node_data" => {
                let d: models::UpdateNodeDataDelta = parse_delta(value)?;
                let path = node_paths.get(&d.node_id).cloned().ok_or_else(|| {
                    CliError::State(
                        "a staged edit targets a node that is neither staged nor pulled"
                            .to_string(),
                    )
                })?;
                summary.updates += 1;
                summary.ops.push(OpSummary::UpdateNode {
                    path,
                    description: d.after.description.filter(|s| !s.is_empty()),
                    // Keep the Option so the preview distinguishes "cleared"
                    // (Some([])) from "untouched" (None) — they are different edits.
                    constraints: d.after.constraints,
                });
            }
            "delete_node" => {
                let d: models::DeleteNodeDelta = parse_delta(value)?;
                // Reverse-map the node id to its dotted path so `status`/`diff`
                // show WHAT is being deleted, never a UUID. A delete whose target
                // is in neither the stage nor the pulled index is corruption —
                // surfaced loudly, never rendered as a bare id.
                let path = node_paths.get(&d.node_id).cloned().ok_or_else(|| {
                    CliError::State(
                        "a staged deletion targets a node that is neither staged nor pulled"
                            .to_string(),
                    )
                })?;
                summary.deletes += 1;
                summary.ops.push(OpSummary::DeleteNode { path });
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

/// Build a UUID → display-string map from the alias keys with the given prefix,
/// over the stage aliases AND the pulled index entries (committed identity).
fn reverse_paths(
    stage: &Stage,
    index: Option<&Index>,
    prefix: &str,
    render: fn(&str) -> String,
) -> std::collections::HashMap<Uuid, String> {
    stage
        .aliases
        .iter()
        .chain(index.into_iter().flat_map(|i| i.entries.iter()))
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
        CliError::State(
            "a staged edge references a port that is neither staged nor in the pulled index"
                .to_string(),
        )
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
            description: None,
            constraints: vec![],
        }
    }

    fn empty() -> Changeset {
        Changeset::with_index(Stage::empty(), None)
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

    #[test]
    fn add_node_stages_description_and_constraints() {
        let mut cs = empty();
        cs.add_node(&NodeSpec {
            description: Some("Scores a hotdog 0-10"),
            constraints: vec!["latency < 50ms".to_string()],
            ..behavior("Rater", None)
        })
        .unwrap();
        let d: models::AddNodeDelta =
            serde_json::from_value(cs.into_stage().deltas.remove(0)).unwrap();
        let data = d.node.data.unwrap();
        assert_eq!(data.description.as_deref(), Some("Scores a hotdog 0-10"));
        assert_eq!(data.constraints, Some(vec!["latency < 50ms".to_string()]));
    }

    #[test]
    fn add_node_omits_absent_description_and_constraints() {
        // No --description / --constraint → the fields are omitted (None), so the
        // server applies its own defaults rather than us forcing "".
        let mut cs = empty();
        cs.add_node(&behavior("Plain", None)).unwrap();
        let d: models::AddNodeDelta =
            serde_json::from_value(cs.into_stage().deltas.remove(0)).unwrap();
        let data = d.node.data.unwrap();
        assert_eq!(data.description, None);
        assert_eq!(data.constraints, None);
    }

    #[test]
    fn add_node_treats_empty_description_and_blank_constraints_as_absent() {
        // `--description ""` and a blank `--constraint "  "` must collapse to None
        // in the DELTA (not just the preview) — otherwise an empty string would
        // ride to the server and override its default, while `diff` showed
        // nothing. Staged == previewed == committed.
        let mut cs = empty();
        cs.add_node(&NodeSpec {
            description: Some(""),
            constraints: vec!["   ".to_string(), "real".to_string()],
            ..behavior("Edge", None)
        })
        .unwrap();
        let stage = cs.into_stage();
        let d: models::AddNodeDelta = serde_json::from_value(stage.deltas[0].clone()).unwrap();
        let data = d.node.data.unwrap();
        assert_eq!(data.description, None, "empty description must be omitted");
        // The blank constraint is dropped; only the real one survives.
        assert_eq!(data.constraints, Some(vec!["real".to_string()]));

        // And the preview agrees (no description shown).
        let summary = summarize(&stage, None).unwrap();
        let desc = summary.ops.iter().find_map(|op| match op {
            OpSummary::Node { description, .. } => Some(description.clone()),
            _ => None,
        });
        assert_eq!(desc, Some(None));
    }

    #[test]
    fn summarize_surfaces_description_and_constraints() {
        let mut cs = empty();
        cs.add_node(&NodeSpec {
            description: Some("the prompt"),
            constraints: vec!["c1".to_string(), "c2".to_string()],
            ..behavior("Rater", None)
        })
        .unwrap();
        let summary = summarize(&cs.into_stage(), None).unwrap();
        let (desc, cons) = summary
            .ops
            .iter()
            .find_map(|op| match op {
                OpSummary::Node {
                    description,
                    constraints,
                    ..
                } => Some((description.clone(), constraints.clone())),
                _ => None,
            })
            .unwrap();
        assert_eq!(desc.as_deref(), Some("the prompt"));
        assert_eq!(cons, vec!["c1".to_string(), "c2".to_string()]);
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

        let summary = summarize(&stage, None).unwrap();
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
        let err = summarize(&stage, None).unwrap_err();
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
        let err = summarize(&stage, None).unwrap_err();
        assert!(matches!(err, CliError::State(_)), "got {err:?}");
    }

    #[test]
    fn summarize_counts_unknown_delta_types_as_other() {
        // `reparent_node` is a real delta kind this version doesn't itemize yet —
        // a good stand-in for "forward-compat unknown" (delete_node is now
        // itemized, so it would no longer land in `other`).
        let mut stage = Stage::empty();
        stage
            .deltas
            .push(serde_json::json!({"type": "reparent_node", "nodeId": Uuid::new_v4()}));
        let summary = summarize(&stage, None).unwrap();
        assert_eq!((summary.nodes, summary.edges, summary.other), (0, 0, 1));
        assert_eq!(summary.total(), 1);
    }

    // ---- lower (stage -> commit batch) ----

    fn raw_edge() -> serde_json::Value {
        serde_json::json!({
            "type": "add_edge",
            "edge": {
                "id": Uuid::new_v4(),
                "sourceHandle": Uuid::new_v4(),
                "targetHandle": Uuid::new_v4(),
            }
        })
    }

    fn raw_node() -> serde_json::Value {
        serde_json::json!({
            "type": "add_node",
            "node": { "id": Uuid::new_v4(), "kind": "behavior", "parent_id": null }
        })
    }

    #[test]
    fn lower_orders_nodes_before_edges() {
        use models::V1DeltasBodyDeltasInner as Inner;
        // Stage them edge-first; lower must still emit the node first so the
        // edge's handles resolve when the batch is applied.
        let mut stage = Stage::empty();
        stage.deltas.push(raw_edge());
        stage.deltas.push(raw_node());
        let lowered = lower(&stage).unwrap();
        assert_eq!(lowered.len(), 2);
        assert!(
            matches!(lowered[0], Inner::AddNode(_)),
            "node must be first"
        );
        assert!(matches!(lowered[1], Inner::AddEdge(_)), "edge must be last");
    }

    #[test]
    fn lower_rejects_an_unsupported_delta() {
        let mut stage = Stage::empty();
        stage
            .deltas
            .push(serde_json::json!({"type": "flatten_boundary", "id": Uuid::new_v4()}));
        let err = lower(&stage).unwrap_err();
        assert!(matches!(err, CliError::State(_)), "got {err:?}");
        assert!(err.to_string().contains("unsupported"), "{err}");
    }

    // ---- index_from_graph + resolution fallthrough ----

    /// A pulled graph: top-level boundary `Api` with a nested behavior
    /// `Api.Rater` that has an output port `score:Rating`. Built by deserializing
    /// the wire JSON so the test also exercises the real `GraphResponse` shape.
    fn pulled_graph(api: Uuid, rater: Uuid, score: Uuid, version: i32) -> models::GraphResponse {
        serde_json::from_value(serde_json::json!({
            "branch": { "id": Uuid::from_u128(0xB), "version": version },
            "project_id": Uuid::from_u128(0xA),
            "version": version.to_string(),
            "edges": [],
            "nodes": [
                {
                    "id": api, "kind": "boundary", "parent_id": null,
                    "position": { "x": 0.0, "y": 0.0 },
                    "data": { "name": "Api", "description": "", "status": "idle",
                              "isTestNode": false, "is_external": false }
                },
                {
                    "id": rater, "kind": "behavior", "parent_id": api,
                    "position": { "x": 0.0, "y": 0.0 },
                    "data": { "name": "Rater", "description": "", "status": "idle",
                              "isTestNode": false, "is_external": false,
                              "inputs": [ { "id": Uuid::from_u128(0x4ABC), "name": "raw", "type": "Patty" } ],
                              "outputs": [ { "id": score, "name": "score", "type": "Rating" } ] }
                }
            ]
        }))
        .unwrap()
    }

    #[test]
    fn index_from_graph_reconstructs_nested_paths_and_ports() {
        let (api, rater, score) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        let index = index_from_graph(&pulled_graph(api, rater, score, 5)).unwrap();
        assert_eq!(index.version, 5);
        // The boundary, the nested behavior (dotted), and its output port.
        assert_eq!(index.get("node:Api"), Some(api));
        assert_eq!(index.get("node:Api.Rater"), Some(rater));
        assert_eq!(index.get("port:Api.Rater:out:score"), Some(score));
    }

    /// A single node with arbitrary parent_id/name/ports, for the corruption
    /// tests — built as JSON so each variant only sets what it cares about.
    fn graph_with_nodes(nodes: serde_json::Value) -> models::GraphResponse {
        serde_json::from_value(serde_json::json!({
            "branch": { "id": Uuid::from_u128(0xB), "version": 1 },
            "project_id": Uuid::from_u128(0xA),
            "version": "1",
            "edges": [],
            "nodes": nodes,
        }))
        .unwrap()
    }

    fn node_json(
        id: Uuid,
        name: &str,
        parent: Option<Uuid>,
        ports: serde_json::Value,
    ) -> serde_json::Value {
        serde_json::json!({
            "id": id, "kind": "behavior", "parent_id": parent,
            "position": { "x": 0.0, "y": 0.0 },
            "data": serde_json::json!({
                "name": name, "description": "", "status": "idle",
                "isTestNode": false, "is_external": false,
            }).as_object().map(|o| {
                let mut o = o.clone();
                if !ports.is_null() { o.insert("outputs".to_string(), ports); }
                serde_json::Value::Object(o)
            }).unwrap()
        })
    }

    #[test]
    fn index_from_graph_records_node_kind_and_ports() {
        let (api, rater, score) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        let index = index_from_graph(&pulled_graph(api, rater, score, 5)).unwrap();
        // Kind is captured (for boundary-only checks like `flatten`).
        assert_eq!(index.node_info(&api).unwrap().kind, "boundary");
        let rater_info = index.node_info(&rater).unwrap();
        assert_eq!(rater_info.kind, "behavior");
        // The output port is recorded with its id, name, and type — what
        // `node set` resends to preserve the UUID.
        let out = &rater_info.outputs[0];
        assert_eq!(
            (out.id, out.name.as_str(), out.r#type.as_str()),
            (score, "score", "Rating")
        );
        assert_eq!(rater_info.inputs[0].name, "raw");
    }

    #[test]
    fn index_from_graph_builds_the_edge_map() {
        let (src, tgt, eid) = (
            Uuid::from_u128(0x50),
            Uuid::from_u128(0x70),
            Uuid::from_u128(0xED),
        );
        let graph: models::GraphResponse = serde_json::from_value(serde_json::json!({
            "branch": { "id": Uuid::from_u128(0xB), "version": 2 },
            "project_id": Uuid::from_u128(0xA), "version": "2",
            "nodes": [
                { "id": Uuid::from_u128(1), "kind": "behavior", "parent_id": null,
                  "position": {"x":0.0,"y":0.0},
                  "data": {"name":"A","description":"","status":"idle","isTestNode":false,"is_external":false,
                           "outputs":[{"id":src,"name":"o","type":"T"}]} },
                { "id": Uuid::from_u128(2), "kind": "behavior", "parent_id": null,
                  "position": {"x":0.0,"y":0.0},
                  "data": {"name":"B","description":"","status":"idle","isTestNode":false,"is_external":false,
                           "inputs":[{"id":tgt,"name":"i","type":"T"}]} },
            ],
            "edges": [ { "id": eid, "source": Uuid::from_u128(1), "target": Uuid::from_u128(2),
                         "sourceHandle": src, "targetHandle": tgt } ],
        })).unwrap();
        let index = index_from_graph(&graph).unwrap();
        assert_eq!(index.edge_id(src, tgt), Some(eid));
        assert_eq!(index.edge_id(tgt, src), None); // direction matters
    }

    #[test]
    fn index_from_graph_fails_loud_on_an_edge_missing_a_handle() {
        let graph: models::GraphResponse = serde_json::from_value(serde_json::json!({
            "branch": { "id": Uuid::from_u128(0xB), "version": 1 },
            "project_id": Uuid::from_u128(0xA), "version": "1",
            "nodes": [ { "id": Uuid::from_u128(1), "kind": "behavior", "parent_id": null,
                "position": {"x":0.0,"y":0.0},
                "data": {"name":"A","description":"","status":"idle","isTestNode":false,"is_external":false} } ],
            "edges": [ { "id": Uuid::from_u128(0xED), "source": Uuid::from_u128(1), "target": Uuid::from_u128(1),
                         "sourceHandle": null, "targetHandle": null } ],
        })).unwrap();
        let err = index_from_graph(&graph).unwrap_err();
        assert!(matches!(err, CliError::State(_)), "got {err:?}");
        assert!(err.to_string().contains("missing a port handle"), "{err}");
    }

    #[test]
    fn index_from_graph_fails_loud_on_a_parent_id_cycle() {
        // Two nodes whose parent_id point at each other — the path walk never
        // reaches a root. Must fail loud, not loop or drop the nodes silently.
        let (a, b) = (Uuid::from_u128(1), Uuid::from_u128(2));
        let graph = graph_with_nodes(serde_json::json!([
            node_json(a, "A", Some(b), serde_json::Value::Null),
            node_json(b, "B", Some(a), serde_json::Value::Null),
        ]));
        let err = index_from_graph(&graph).unwrap_err();
        assert!(matches!(err, CliError::State(_)), "got {err:?}");
        assert!(err.to_string().contains("cycle"), "{err}");
    }

    #[test]
    fn index_from_graph_fails_loud_on_a_duplicate_path() {
        // Two top-level nodes with the same name resolve to the same dotted
        // path — ambiguous, so the index build must refuse rather than let one
        // silently shadow the other.
        let graph = graph_with_nodes(serde_json::json!([
            node_json(Uuid::from_u128(1), "Dup", None, serde_json::Value::Null),
            node_json(Uuid::from_u128(2), "Dup", None, serde_json::Value::Null),
        ]));
        let err = index_from_graph(&graph).unwrap_err();
        assert!(matches!(err, CliError::State(_)), "got {err:?}");
        assert!(err.to_string().contains("two nodes at path"), "{err}");
    }

    #[test]
    fn index_from_graph_fails_loud_on_a_duplicate_port_name() {
        let graph = graph_with_nodes(serde_json::json!([node_json(
            Uuid::from_u128(1),
            "N",
            None,
            serde_json::json!([
                { "id": Uuid::from_u128(7), "name": "p", "type": "T" },
                { "id": Uuid::from_u128(8), "name": "p", "type": "T" },
            ]),
        )]));
        let err = index_from_graph(&graph).unwrap_err();
        assert!(matches!(err, CliError::State(_)), "got {err:?}");
        assert!(err.to_string().contains("two out ports named 'p'"), "{err}");
    }

    #[test]
    fn index_from_graph_rejects_a_dotted_or_empty_node_name() {
        // A committed name carrying the path separator can't be addressed; loud.
        for bad in ["Api.Rater", ""] {
            let graph = graph_with_nodes(serde_json::json!([node_json(
                Uuid::from_u128(1),
                bad,
                None,
                serde_json::Value::Null
            )]));
            let err = index_from_graph(&graph).unwrap_err();
            assert!(
                matches!(err, CliError::State(_)),
                "for {bad:?}: got {err:?}"
            );
            assert!(
                err.to_string().contains("path-addressed"),
                "for {bad:?}: {err}"
            );
        }
    }

    #[test]
    fn index_from_graph_skips_an_unnamed_port_without_erroring() {
        // An unnamed committed port has no addressable key, so it's skipped —
        // the build still succeeds and indexes the node.
        let graph = graph_with_nodes(serde_json::json!([node_json(
            Uuid::from_u128(1),
            "N",
            None,
            serde_json::json!([{ "id": Uuid::from_u128(7), "type": "T" }]),
        )]));
        let index = index_from_graph(&graph).unwrap();
        assert_eq!(index.get("node:N"), Some(Uuid::from_u128(1)));
        // No port entry was fabricated for the nameless port.
        assert!(
            index.entries.keys().all(|k| !k.starts_with("port:")),
            "{:?}",
            index.entries
        );
    }

    #[test]
    fn index_from_graph_fails_loud_on_missing_parent() {
        // A node whose parent_id points at a node not present in the response is
        // corruption — the path can't be built, so the whole pull fails loud
        // rather than silently dropping the node from the index.
        let graph: models::GraphResponse = serde_json::from_value(serde_json::json!({
            "branch": { "id": Uuid::from_u128(0xB), "version": 1 },
            "project_id": Uuid::from_u128(0xA),
            "version": "1",
            "edges": [],
            "nodes": [ {
                "id": Uuid::from_u128(2), "kind": "behavior",
                "parent_id": Uuid::from_u128(0xDEAD),
                "position": { "x": 0.0, "y": 0.0 },
                "data": { "name": "Orphan", "description": "", "status": "idle",
                          "isTestNode": false, "is_external": false }
            } ]
        }))
        .unwrap();
        let err = index_from_graph(&graph).unwrap_err();
        assert!(matches!(err, CliError::State(_)), "got {err:?}");
        assert!(err.to_string().contains("missing parent"), "{err}");
    }

    /// The live repro, in miniature: an edge whose SOURCE is a committed port
    /// (resolved via the pulled index) and whose TARGET is a freshly-staged port
    /// must resolve and stage — the exact thing that was impossible before.
    #[test]
    fn edge_resolves_a_committed_source_against_the_pulled_index() {
        let (api, rater, score) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        let index = index_from_graph(&pulled_graph(api, rater, score, 5)).unwrap();

        let mut cs = Changeset::with_index(Stage::empty(), Some(index));
        // Stage a brand-new sink with an input of the matching type.
        cs.add_node(&NodeSpec {
            inputs: vec![port("rating", "Rating")],
            ..behavior("Sink", None)
        })
        .unwrap();
        // Committed `Api.Rater.score` -> freshly-staged `Sink.rating`.
        let added = cs.add_edge("Api.Rater.score", "Sink.rating").unwrap();
        assert_eq!(added.from, "Api.Rater.score");

        let stage = cs.into_stage();
        let edge = stage
            .deltas
            .iter()
            .find(|v| v["type"] == "add_edge")
            .unwrap()
            .clone();
        let d: models::AddEdgeDelta = serde_json::from_value(edge).unwrap();
        // The source handle is the COMMITTED port UUID from the index.
        assert_eq!(d.edge.source_handle, Some(Some(score)));
        assert_eq!(
            d.edge.target_handle,
            Some(Some(stage.aliases["port:Sink:in:rating"]))
        );
    }

    #[test]
    fn summarize_renders_a_cross_commit_edge_via_the_index() {
        let (api, rater, score) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        let index = index_from_graph(&pulled_graph(api, rater, score, 5)).unwrap();
        let mut cs = Changeset::with_index(Stage::empty(), Some(index.clone()));
        cs.add_node(&NodeSpec {
            inputs: vec![port("rating", "Rating")],
            ..behavior("Sink", None)
        })
        .unwrap();
        cs.add_edge("Api.Rater.score", "Sink.rating").unwrap();
        let stage = cs.into_stage();

        // The bug: the edge's source handle is a COMMITTED port UUID, absent
        // from the stage aliases, so rendering without the index fails loud.
        let err = summarize(&stage, None).unwrap_err();
        assert!(matches!(err, CliError::State(_)), "got {err:?}");

        // The fix: with the index, the committed source renders by its path.
        let summary = summarize(&stage, Some(&index)).unwrap();
        let edge = summary
            .ops
            .iter()
            .find_map(|op| match op {
                OpSummary::Edge { from, to } => Some((from.clone(), to.clone())),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            edge,
            ("Api.Rater.score".to_string(), "Sink.rating".to_string())
        );
    }

    #[test]
    fn summarize_renders_a_committed_target_via_the_index() {
        // The symmetric case: a freshly-staged SOURCE wired into a COMMITTED
        // input port (`Api.Rater.raw`, in the index only). The target handle
        // must also resolve through the index, not just the source.
        let (api, rater, score) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        let index = index_from_graph(&pulled_graph(api, rater, score, 5)).unwrap();
        let mut cs = Changeset::with_index(Stage::empty(), Some(index.clone()));
        cs.add_node(&NodeSpec {
            outputs: vec![port("patty", "Patty")],
            ..behavior("Grill", None)
        })
        .unwrap();
        cs.add_edge("Grill.patty", "Api.Rater.raw").unwrap();
        let stage = cs.into_stage();

        let summary = summarize(&stage, Some(&index)).unwrap();
        let edge = summary
            .ops
            .iter()
            .find_map(|op| match op {
                OpSummary::Edge { from, to } => Some((from.clone(), to.clone())),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            edge,
            ("Grill.patty".to_string(), "Api.Rater.raw".to_string())
        );
    }

    #[test]
    fn summarize_workdir_loads_and_threads_the_index_from_disk() {
        // Guards the status/diff wiring: a cross-commit edge persisted to a
        // workdir must render via summarize_workdir, which has to load AND pass
        // the index. If the index weren't threaded, handle_path would fail loud.
        let tmp = tempfile::TempDir::new().unwrap();
        let (api, rater, score) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        let index = index_from_graph(&pulled_graph(api, rater, score, 5)).unwrap();
        let mut cs = Changeset::with_index(Stage::empty(), Some(index.clone()));
        cs.add_node(&NodeSpec {
            inputs: vec![port("rating", "Rating")],
            ..behavior("Sink", None)
        })
        .unwrap();
        cs.add_edge("Api.Rater.score", "Sink.rating").unwrap();
        cs.into_stage().save(tmp.path()).unwrap();
        index.save(tmp.path()).unwrap();

        let summary = summarize_workdir(tmp.path()).unwrap();
        assert!(summary.ops.iter().any(|op| matches!(
            op,
            OpSummary::Edge { from, to } if from == "Api.Rater.score" && to == "Sink.rating"
        )));
    }

    #[test]
    fn node_add_parents_under_a_committed_boundary() {
        let (api, rater, score) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        let index = index_from_graph(&pulled_graph(api, rater, score, 5)).unwrap();

        let mut cs = Changeset::with_index(Stage::empty(), Some(index));
        // Parent path `Api` exists only in the pulled index, not the stage.
        let added = cs.add_node(&behavior("Logger", Some("Api"))).unwrap();
        assert_eq!(added.path, "Api.Logger");
        let stage = cs.into_stage();
        let d: models::AddNodeDelta = serde_json::from_value(stage.deltas[0].clone()).unwrap();
        // The new node's parent_id is the committed Api UUID.
        assert_eq!(d.node.parent_id, Some(Some(api)));
    }

    #[test]
    fn add_node_rejects_a_name_already_on_the_branch() {
        let (api, rater, score) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        let index = index_from_graph(&pulled_graph(api, rater, score, 5)).unwrap();
        let mut cs = Changeset::with_index(Stage::empty(), Some(index));
        // `Api` already exists on the branch.
        let err = cs
            .add_node(&NodeSpec {
                kind: Kind::Boundary,
                ..behavior("Api", None)
            })
            .unwrap_err();
        assert!(matches!(err, CliError::InvalidArgument(_)), "got {err:?}");
        assert!(
            err.to_string().contains("already exists on the branch"),
            "{err}"
        );
        assert!(cs.deltas().is_empty());
    }

    #[test]
    fn unknown_port_without_a_pull_points_at_pull() {
        // No index: referencing a committed port can't resolve, and the error
        // tells the author to pull rather than leaving them stuck.
        let mut cs = empty();
        cs.add_node(&NodeSpec {
            inputs: vec![port("rating", "Rating")],
            ..behavior("Sink", None)
        })
        .unwrap();
        let err = cs.add_edge("Api.Rater.score", "Sink.rating").unwrap_err();
        assert!(err.to_string().contains("hydrate pull"), "{err}");
    }

    // ---- node rm / delete ----

    #[test]
    fn remove_node_stages_a_delete_for_a_committed_node() {
        let (api, rater, score) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        let index = index_from_graph(&pulled_graph(api, rater, score, 5)).unwrap();
        let mut cs = Changeset::with_index(Stage::empty(), Some(index));
        let removed = cs.remove_node("Api.Rater").unwrap();
        assert_eq!(removed.path, "Api.Rater");
        let stage = cs.into_stage();
        let d: models::DeleteNodeDelta = serde_json::from_value(stage.deltas[0].clone()).unwrap();
        // The delete targets the COMMITTED node's UUID from the index.
        assert_eq!(d.node_id, rater);
    }

    #[test]
    fn remove_node_unknown_path_fails_loud() {
        let mut cs = empty();
        let err = cs.remove_node("Ghost").unwrap_err();
        assert!(matches!(err, CliError::InvalidArgument(_)), "got {err:?}");
        assert!(err.to_string().contains("Ghost"), "{err}");
    }

    #[test]
    fn remove_node_rejects_a_double_delete() {
        let mut cs = empty();
        cs.add_node(&behavior("Rater", None)).unwrap();
        cs.remove_node("Rater").unwrap();
        let err = cs.remove_node("Rater").unwrap_err();
        assert!(
            err.to_string().contains("already staged for deletion"),
            "{err}"
        );
    }

    #[test]
    fn resolve_node_rejects_a_node_staged_for_deletion() {
        // Can't reparent a new node under a boundary you're removing this batch.
        let mut cs = empty();
        cs.add_node(&NodeSpec {
            kind: Kind::Boundary,
            ..behavior("Api", None)
        })
        .unwrap();
        cs.remove_node("Api").unwrap();
        let err = cs.add_node(&behavior("Rater", Some("Api"))).unwrap_err();
        assert!(err.to_string().contains("staged for deletion"), "{err}");
    }

    #[test]
    fn delete_node_delta_is_commit_ready() {
        // Reconstructs into the concrete DeleteNodeDelta via the type tag — the
        // same round-trip guarantee commit relies on (never the tagged enum).
        let mut cs = empty();
        cs.add_node(&behavior("Rater", None)).unwrap();
        cs.remove_node("Rater").unwrap();
        let value = cs
            .into_stage()
            .deltas
            .into_iter()
            .find(|v| v["type"] == "delete_node")
            .unwrap();
        let d: models::DeleteNodeDelta = serde_json::from_value(value).unwrap();
        assert_eq!(d.r#type, models::delete_node_delta::Type::DeleteNode);
    }

    #[test]
    fn lower_orders_deletes_after_adds() {
        use models::V1DeltasBodyDeltasInner as Inner;
        let mut cs = empty();
        cs.add_node(&behavior("Keep", None)).unwrap();
        cs.add_node(&behavior("Gone", None)).unwrap();
        cs.remove_node("Gone").unwrap();
        let lowered = lower(&cs.into_stage()).unwrap();
        // Two add_node then the delete_node last.
        assert!(matches!(lowered[0], Inner::AddNode(_)));
        assert!(matches!(lowered[1], Inner::AddNode(_)));
        assert!(
            matches!(lowered[2], Inner::DeleteNode(_)),
            "delete must be last"
        );
    }

    #[test]
    fn summarize_renders_a_deletion_by_path_not_uuid() {
        let (api, rater, score) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        let index = index_from_graph(&pulled_graph(api, rater, score, 5)).unwrap();
        let mut cs = Changeset::with_index(Stage::empty(), Some(index.clone()));
        cs.remove_node("Api.Rater").unwrap();
        let summary = summarize(&cs.into_stage(), Some(&index)).unwrap();
        assert_eq!(summary.deletes, 1);
        let path = summary.ops.iter().find_map(|op| match op {
            OpSummary::DeleteNode { path } => Some(path.clone()),
            _ => None,
        });
        assert_eq!(path.as_deref(), Some("Api.Rater"));
        // The committed UUID never surfaces.
        assert!(!format!("{summary:?}").contains(&rater.to_string()));
    }

    #[test]
    fn top_level_node_paths_lists_only_roots_from_the_index() {
        let (api, rater, score) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        let index = index_from_graph(&pulled_graph(api, rater, score, 5)).unwrap();
        let cs = Changeset::with_index(Stage::empty(), Some(index));
        // `Api` is top-level; `Api.Rater` is nested → excluded.
        assert_eq!(cs.top_level_node_paths(), vec!["Api".to_string()]);
    }

    #[test]
    fn top_level_node_paths_empty_without_a_pull() {
        assert!(empty().top_level_node_paths().is_empty());
    }

    #[test]
    fn summarize_fails_loud_on_a_deletion_targeting_an_unknown_node() {
        // A staged delete whose node id is in neither the stage nor the index is
        // corruption — never rendered as a bare id, surfaced loudly.
        let mut stage = Stage::empty();
        stage.deltas.push(serde_json::json!({
            "type": "delete_node",
            "nodeId": Uuid::new_v4(),
        }));
        let err = summarize(&stage, None).unwrap_err();
        assert!(matches!(err, CliError::State(_)), "got {err:?}");
    }

    #[test]
    fn clearing_all_top_level_paths_stages_a_delete_per_root() {
        // The composition `clear` performs: enumerate top-level paths, remove
        // each. Two roots (Api, Store) → two delete_node deltas for their ids.
        let api = Uuid::from_u128(0xA1);
        let store = Uuid::from_u128(0x57);
        let graph: models::GraphResponse = serde_json::from_value(serde_json::json!({
            "branch": { "id": Uuid::from_u128(0xB), "version": 1 },
            "project_id": Uuid::from_u128(0xC),
            "version": "1",
            "edges": [],
            "nodes": [
                { "id": api, "kind": "boundary", "parent_id": null,
                  "position": {"x":0.0,"y":0.0},
                  "data": {"name":"Api","description":"","status":"idle","isTestNode":false,"is_external":false} },
                { "id": store, "kind": "boundary", "parent_id": null,
                  "position": {"x":0.0,"y":0.0},
                  "data": {"name":"Store","description":"","status":"idle","isTestNode":false,"is_external":false} },
            ],
        }))
        .unwrap();
        let index = index_from_graph(&graph).unwrap();
        let mut cs = Changeset::with_index(Stage::empty(), Some(index));
        for path in cs.top_level_node_paths() {
            cs.remove_node(&path).unwrap();
        }
        let stage = cs.into_stage();
        let deleted: std::collections::HashSet<Uuid> = stage
            .deltas
            .iter()
            .filter(|v| v["type"] == "delete_node")
            .map(|v| Uuid::parse_str(v["nodeId"].as_str().unwrap()).unwrap())
            .collect();
        assert_eq!(
            deleted,
            std::collections::HashSet::from([api, store]),
            "clear must stage a delete for each top-level root"
        );
    }

    // ---- node set / update ----

    #[test]
    fn update_node_stages_a_partial_edit_for_a_committed_node() {
        let (api, rater, score) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        let index = index_from_graph(&pulled_graph(api, rater, score, 5)).unwrap();
        let mut cs = Changeset::with_index(Stage::empty(), Some(index));
        cs.update_node("Api.Rater", Some("new prompt"), None)
            .unwrap();
        let d: models::UpdateNodeDataDelta =
            serde_json::from_value(cs.into_stage().deltas[0].clone()).unwrap();
        assert_eq!(d.node_id, rater);
        // Only the description is present; other fields are left untouched (None).
        assert_eq!(d.after.description.as_deref(), Some("new prompt"));
        assert_eq!(d.after.constraints, None);
        assert_eq!(d.after.name, None);
    }

    #[test]
    fn update_node_clear_constraints_sets_an_empty_list() {
        let mut cs = empty();
        cs.add_node(&behavior("Rater", None)).unwrap();
        cs.update_node("Rater", None, Some(vec![])).unwrap();
        let d: models::UpdateNodeDataDelta = serde_json::from_value(
            cs.into_stage()
                .deltas
                .into_iter()
                .find(|v| v["type"] == "update_node_data")
                .unwrap(),
        )
        .unwrap();
        // Present-but-empty = "clear", distinct from absent (untouched).
        assert_eq!(d.after.constraints, Some(vec![]));
        assert_eq!(d.after.description, None);
    }

    #[test]
    fn update_node_with_nothing_to_set_fails_loud() {
        let mut cs = empty();
        cs.add_node(&behavior("Rater", None)).unwrap();
        let err = cs.update_node("Rater", None, None).unwrap_err();
        assert!(matches!(err, CliError::InvalidArgument(_)), "got {err:?}");
        assert!(err.to_string().contains("nothing to set"), "{err}");
    }

    #[test]
    fn update_node_unknown_path_fails_loud() {
        let mut cs = empty();
        let err = cs.update_node("Ghost", Some("x"), None).unwrap_err();
        assert!(err.to_string().contains("Ghost"), "{err}");
    }

    #[test]
    fn update_node_delta_is_commit_ready_and_lowers_after_adds_before_deletes() {
        use models::V1DeltasBodyDeltasInner as Inner;
        let mut cs = empty();
        cs.add_node(&behavior("Rater", None)).unwrap();
        cs.add_node(&behavior("Gone", None)).unwrap();
        cs.update_node("Rater", Some("edited"), None).unwrap();
        cs.remove_node("Gone").unwrap();
        let lowered = lower(&cs.into_stage()).unwrap();
        // adds (2) → update (1) → delete (1)
        assert!(matches!(lowered[0], Inner::AddNode(_)));
        assert!(matches!(lowered[1], Inner::AddNode(_)));
        assert!(
            matches!(lowered[2], Inner::UpdateNodeData(_)),
            "update after adds"
        );
        assert!(matches!(lowered[3], Inner::DeleteNode(_)), "delete last");
    }

    #[test]
    fn summarize_renders_an_update_by_path_not_uuid() {
        let (api, rater, score) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        let index = index_from_graph(&pulled_graph(api, rater, score, 5)).unwrap();
        let mut cs = Changeset::with_index(Stage::empty(), Some(index.clone()));
        cs.update_node("Api.Rater", Some("edited"), Some(vec!["c".to_string()]))
            .unwrap();
        let summary = summarize(&cs.into_stage(), Some(&index)).unwrap();
        assert_eq!(summary.updates, 1);
        let found = summary.ops.iter().find_map(|op| match op {
            OpSummary::UpdateNode {
                path,
                description,
                constraints,
            } => Some((path.clone(), description.clone(), constraints.clone())),
            _ => None,
        });
        assert_eq!(
            found,
            Some((
                "Api.Rater".to_string(),
                Some("edited".to_string()),
                Some(vec!["c".to_string()])
            ))
        );
        assert!(!format!("{summary:?}").contains(&rater.to_string()));
    }

    #[test]
    fn summarize_fails_loud_on_an_update_targeting_an_unknown_node() {
        let mut stage = Stage::empty();
        stage.deltas.push(serde_json::json!({
            "type": "update_node_data",
            "nodeId": Uuid::new_v4(),
            "after": { "description": "x" },
        }));
        let err = summarize(&stage, None).unwrap_err();
        assert!(matches!(err, CliError::State(_)), "got {err:?}");
    }

    #[test]
    fn update_node_rejects_a_node_staged_for_deletion() {
        let mut cs = empty();
        cs.add_node(&behavior("Rater", None)).unwrap();
        cs.remove_node("Rater").unwrap();
        let err = cs.update_node("Rater", Some("x"), None).unwrap_err();
        assert!(err.to_string().contains("staged for deletion"), "{err}");
    }

    #[test]
    fn lower_a_real_staged_graph_round_trips_into_the_batch() {
        let mut cs = graph_with_two_ports();
        cs.add_edge("Maker.dog", "Rater.raw").unwrap();
        let lowered = lower(&cs.into_stage()).unwrap();
        assert_eq!(lowered.len(), 3);
        // Serializing the batch produces valid wire JSON (the POST body): both
        // nodes first (in order), the edge last.
        let json = serde_json::to_value(&lowered).unwrap();
        assert_eq!(json[0]["type"], "add_node");
        assert_eq!(json[1]["type"], "add_node");
        assert_eq!(json[2]["type"], "add_edge");
    }
}
