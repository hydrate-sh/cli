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
    /// A third port channel (configuration), alongside inputs/outputs. Config
    /// ports are NOT edge endpoints — edge resolution only ever uses In/Out.
    Config,
}

impl Side {
    fn as_str(self) -> &'static str {
        match self {
            Side::In => "in",
            Side::Out => "out",
            Side::Config => "config",
        }
    }

    /// The opposite edge endpoint. Only meaningful for In/Out (the edge sides);
    /// `Config` never reaches edge resolution, so calling this on it is a bug.
    fn opposite(self) -> Side {
        match self {
            Side::In => Side::Out,
            Side::Out => Side::In,
            Side::Config => unreachable!("config ports are not edge endpoints"),
        }
    }
}

/// A typed port to declare on a node (`name:type`, type required).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortSpec {
    pub name: String,
    pub r#type: String,
}

/// The new full port list for one edited side (`None` = leave untouched) plus
/// the `(name, id)` pairs that were added, for alias recording.
type EditedSide = (Option<Vec<models::Port>>, Vec<(String, Uuid)>);

/// A node to stage. `parent` is the dotted path of an already-staged boundary
/// (or `None` for a top-level node).
#[derive(Debug, Clone)]
pub struct NodeSpec<'a> {
    pub kind: models::node::Kind,
    pub name: &'a str,
    pub parent: Option<&'a str>,
    pub inputs: Vec<PortSpec>,
    pub outputs: Vec<PortSpec>,
    /// Config ports (third channel; not edge endpoints).
    pub config: Vec<PortSpec>,
    pub user_kind: Option<&'a str>,
    pub path_prefix: Option<&'a str>,
    /// The node's description (the spec/prompt). `None` omits it (server default).
    pub description: Option<&'a str>,
    /// Plain-text constraints; empty omits the field.
    pub constraints: Vec<String>,
    /// Mark the node external (an outside system the graph depends on).
    pub is_external: bool,
    /// The external system's kind label (server requires it when `is_external`).
    pub external_kind: Option<&'a str>,
    /// Plain-text verifications (how the node is checked); empty omits the field.
    pub verifications: Vec<String>,
    /// External-system protocol (e.g. `gRPC`); `None`/blank omits it.
    pub protocol: Option<&'a str>,
    /// Documentation URL; `None`/blank omits it.
    pub doc_url: Option<&'a str>,
    /// Mark the node a test node.
    pub is_test_node: bool,
}

/// A partial edit to an existing node, for `node set`. Empty fields are left
/// untouched (key-presence). Port edits start from the node's current (pulled)
/// ports and preserve surviving port UUIDs.
#[derive(Debug, Clone, Default)]
pub struct NodeEdit {
    pub description: Option<String>,
    /// `None` = untouched, `Some([])` = cleared, `Some(vec)` = replace.
    pub constraints: Option<Vec<String>>,
    /// Rename the node.
    pub name: Option<String>,
    pub add_in: Vec<PortSpec>,
    pub add_out: Vec<PortSpec>,
    pub rm_in: Vec<String>,
    pub rm_out: Vec<String>,
    pub retype_in: Vec<PortSpec>,
    pub retype_out: Vec<PortSpec>,
    pub add_config: Vec<PortSpec>,
    pub rm_config: Vec<String>,
    pub retype_config: Vec<PortSpec>,
    /// Boundary classifier (`--user-kind`). `None` = untouched.
    pub user_kind: Option<String>,
    /// Boundary path prefix (`--path-prefix`). `None` = untouched.
    pub path_prefix: Option<String>,
    /// External marker toggle. `None` = untouched, `Some(true)` = `--external`,
    /// `Some(false)` = `--no-external`.
    pub is_external: Option<bool>,
    /// External system kind label (`--external-kind`). `None` = untouched.
    pub external_kind: Option<String>,
    /// `None` = untouched, `Some([])` = cleared, `Some(vec)` = replace.
    pub verifications: Option<Vec<String>>,
    /// External-system protocol (`--protocol`). `None` = untouched.
    pub protocol: Option<String>,
    /// Documentation URL (`--doc-url`). `None` = untouched.
    pub doc_url: Option<String>,
    /// Test-node marker toggle. `None` = untouched, `Some(true)` = `--test-node`,
    /// `Some(false)` = `--no-test-node`.
    pub is_test_node: Option<bool>,
    /// `--clear-*` companions: clear the corresponding scalar to null (or, for
    /// `description`, to empty). A clear flag overrides any value for the field.
    pub clear_description: bool,
    pub clear_user_kind: bool,
    pub clear_path_prefix: bool,
    pub clear_external_kind: bool,
    pub clear_protocol: bool,
    pub clear_doc_url: bool,
}

/// Resolve a value + its clear flag to the wire double-option field:
/// clear → `Some(None)` (null); a value → `Some(Some(v))`; neither → `None`
/// (untouched).
fn scalar_or_clear(value: &Option<String>, clear: bool) -> Option<Option<String>> {
    if clear {
        Some(None)
    } else {
        value.clone().map(Some)
    }
}

impl NodeEdit {
    fn touches_ports(&self) -> bool {
        !(self.add_in.is_empty()
            && self.add_out.is_empty()
            && self.rm_in.is_empty()
            && self.rm_out.is_empty()
            && self.retype_in.is_empty()
            && self.retype_out.is_empty()
            && self.add_config.is_empty()
            && self.rm_config.is_empty()
            && self.retype_config.is_empty())
    }

    fn is_empty(&self) -> bool {
        self.description.is_none()
            && self.constraints.is_none()
            && self.name.is_none()
            && self.user_kind.is_none()
            && self.path_prefix.is_none()
            && self.is_external.is_none()
            && self.external_kind.is_none()
            && self.verifications.is_none()
            && self.protocol.is_none()
            && self.doc_url.is_none()
            && self.is_test_node.is_none()
            && !self.clear_description
            && !self.clear_user_kind
            && !self.clear_path_prefix
            && !self.clear_external_kind
            && !self.clear_protocol
            && !self.clear_doc_url
            && !self.touches_ports()
    }
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
        let config = self.mint_ports(&path, Side::Config, &spec.config)?;

        let node_id = Uuid::new_v4();
        let data = models::NodeData {
            name: Some(spec.name.to_string()),
            inputs: Some(inputs.deltas),
            outputs: Some(outputs.deltas),
            config: Some(config.deltas),
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
            constraints: non_empty(&spec.constraints),
            // External marker + its kind label (server enforces external→kind).
            // A blank `--external-kind ""` is "no kind" (parity with --description),
            // so it collapses to None and the server's external→kind check fires
            // rather than us forwarding an empty label.
            is_external: if spec.is_external { Some(true) } else { None },
            is_test_node: if spec.is_test_node { Some(true) } else { None },
            external_kind: spec
                .external_kind
                .filter(|k| !k.trim().is_empty())
                .map(|k| Some(k.to_string())),
            protocol: spec
                .protocol
                .filter(|p| !p.trim().is_empty())
                .map(|p| Some(p.to_string())),
            documentation_url: spec
                .doc_url
                .filter(|d| !d.trim().is_empty())
                .map(|d| Some(d.to_string())),
            // Verifications: each non-blank text becomes a Verification with a
            // minted id and the default author.
            verifications: {
                let texts = non_empty(&spec.verifications);
                texts.map(|ts| {
                    ts.into_iter()
                        .map(|text| {
                            models::Verification::new(
                                models::verification::Author::default(),
                                Uuid::new_v4(),
                                text,
                            )
                        })
                        .collect()
                })
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

    /// Node ids already staged for flattening (the `nodeId` of every staged
    /// `flatten_boundary`). Mirrors `staged_deletions` so a boundary can't be
    /// flattened twice — the second flatten targets a node the first already
    /// dissolved, so without this guard it would silently stage a duplicate.
    fn staged_flattens(&self) -> std::collections::HashSet<Uuid> {
        self.stage
            .deltas
            .iter()
            .filter(|v| {
                v.get("type").and_then(serde_json::Value::as_str) == Some("flatten_boundary")
            })
            .filter_map(|v| v.get("nodeId").and_then(serde_json::Value::as_str))
            .filter_map(|s| Uuid::parse_str(s).ok())
            .collect()
    }

    /// Edge ids already staged for deletion (the `edgeId` of every staged
    /// `delete_edge`). Mirrors `staged_deletions` so a committed edge can't be
    /// queued for deletion twice — the index isn't mutated, so without this a
    /// second `edge rm` would silently push a duplicate `delete_edge`.
    fn staged_edge_deletions(&self) -> std::collections::HashSet<Uuid> {
        self.stage
            .deltas
            .iter()
            .filter(|v| v.get("type").and_then(serde_json::Value::as_str) == Some("delete_edge"))
            .filter_map(|v| v.get("edgeId").and_then(serde_json::Value::as_str))
            .filter_map(|s| Uuid::parse_str(s).ok())
            .collect()
    }

    /// The most recent full port list staged for `node_id` on `side`, if any
    /// `update_node_data` delta in the batch set that side. Each port edit
    /// resends the WHOLE side, so a later edit must build on the prior staged
    /// list — not the pulled snapshot — or it silently drops ports added/retyped
    /// by an earlier staged edit. Returns `None` if no staged edit touched the
    /// side (caller then falls back to the pulled snapshot). Carries `(name, id,
    /// type)`; an unnamed port keeps its id with an empty name.
    fn staged_side_ports(&self, node_id: Uuid, side: Side) -> Option<Vec<(String, Uuid, String)>> {
        let wire_key = match side {
            Side::In => "inputs",
            Side::Out => "outputs",
            Side::Config => "config",
        };
        let mut latest = None;
        for v in &self.stage.deltas {
            if v.get("type").and_then(serde_json::Value::as_str) != Some("update_node_data") {
                continue;
            }
            if v.get("nodeId").and_then(serde_json::Value::as_str)
                != Some(node_id.to_string().as_str())
            {
                continue;
            }
            // Key-presence: only a delta that actually set this side replaces the
            // baseline; one that edited the *other* side leaves it untouched.
            let Some(arr) = v
                .get("after")
                .and_then(|a| a.get(wire_key))
                .and_then(serde_json::Value::as_array)
            else {
                continue;
            };
            let ports = arr
                .iter()
                .filter_map(|p| {
                    let id = Uuid::parse_str(p.get("id")?.as_str()?).ok()?;
                    let name = p
                        .get("name")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let r#type = p
                        .get("type")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    Some((name, id, r#type))
                })
                .collect();
            latest = Some(ports);
        }
        latest
    }

    /// Stage a partial edit of the node at `path` (resolved against the
    /// stage ∪ pulled index). `UpdateNodeData` is key-presence partial: only the
    /// fields present in `after` change, the rest are left untouched — so we send
    /// just what was set, never echoing the node's other data. At least one of
    /// `description`/`constraints` must be `Some`, else there's nothing to do.
    pub fn update_node(&mut self, path: &str, edit: &NodeEdit) -> Result<NodeUpdated, CliError> {
        if edit.is_empty() {
            return Err(CliError::InvalidArgument(
                "nothing to set — pass a field flag (--description, --constraint, \
                 --name, --user-kind, --path-prefix, --external/--no-external, \
                 --external-kind, --protocol, --doc-url, --test-node/--no-test-node, \
                 --verification, or a --clear-* flag) or a port flag"
                    .to_string(),
            ));
        }
        let id = self.resolve_node(path)?;
        if let Some(new) = &edit.name {
            validate_slug(new, "node name")?;
        }
        // Port edits resend the FULL list for the touched side, starting from the
        // node's CURRENT ports (from the pulled snapshot) so surviving ports keep
        // their UUIDs — change a port id and you orphan every edge on it.
        let (inputs, in_added) = self.edited_side(id, path, Side::In, edit)?;
        let (outputs, out_added) = self.edited_side(id, path, Side::Out, edit)?;
        // Config ports are a third channel; they're not edge endpoints, so the
        // added-alias list is ignored (nothing wires to a config port).
        let (config, _config_added) = self.edited_side(id, path, Side::Config, edit)?;

        let after = models::NodeData {
            name: edit.name.clone(),
            // description has no null on the wire — `--clear-description` sets it
            // to empty; otherwise the value (or untouched).
            description: if edit.clear_description {
                Some(String::new())
            } else {
                edit.description.clone()
            },
            constraints: edit.constraints.clone(),
            inputs,
            outputs,
            config,
            // Boundary/external/doc scalars are key-presence double-option fields:
            // a value → Some(Some(v)); a `--clear-*` → Some(None) (null);
            // untouched → None.
            user_kind: scalar_or_clear(&edit.user_kind, edit.clear_user_kind),
            path_prefix: scalar_or_clear(&edit.path_prefix, edit.clear_path_prefix),
            is_external: edit.is_external,
            is_test_node: edit.is_test_node,
            external_kind: scalar_or_clear(&edit.external_kind, edit.clear_external_kind),
            protocol: scalar_or_clear(&edit.protocol, edit.clear_protocol),
            documentation_url: scalar_or_clear(&edit.doc_url, edit.clear_doc_url),
            // Verifications: None untouched, Some([]) clears, Some(vec) replaces —
            // each text becomes a Verification with a minted id + default author.
            verifications: edit.verifications.as_ref().map(|texts| {
                texts
                    .iter()
                    .map(|text| {
                        models::Verification::new(
                            models::verification::Author::default(),
                            Uuid::new_v4(),
                            text.clone(),
                        )
                    })
                    .collect()
            }),
            ..Default::default()
        };
        let delta = models::UpdateNodeDataDelta::new(
            after,
            id,
            models::update_node_data_delta::Type::UpdateNodeData,
        );
        self.push(&delta)?;
        // Record aliases for newly-added ports so they're wireable this session.
        for (name, port_id) in in_added {
            self.stage
                .aliases
                .insert(port_key(path, Side::In, &name), port_id);
        }
        for (name, port_id) in out_added {
            self.stage
                .aliases
                .insert(port_key(path, Side::Out, &name), port_id);
        }
        Ok(NodeUpdated {
            path: path.to_string(),
            name: edit.name.clone(),
            description: if edit.clear_description {
                Some(String::new())
            } else {
                edit.description.clone()
            },
            constraints: edit.constraints.clone(),
            user_kind: scalar_or_clear(&edit.user_kind, edit.clear_user_kind),
            path_prefix: scalar_or_clear(&edit.path_prefix, edit.clear_path_prefix),
            is_external: edit.is_external,
            external_kind: scalar_or_clear(&edit.external_kind, edit.clear_external_kind),
            protocol: scalar_or_clear(&edit.protocol, edit.clear_protocol),
            doc_url: scalar_or_clear(&edit.doc_url, edit.clear_doc_url),
            is_test_node: edit.is_test_node,
            verifications: edit.verifications.clone(),
            ports_changed: edit.touches_ports(),
        })
    }

    /// Compute the new full port list for `side` when the edit touches it, else
    /// `None` (leave it untouched, per key-presence). Surviving ports keep their
    /// pulled UUIDs; added ports mint new ones. Returns the wire ports plus the
    /// `(name, id)` pairs added, for alias recording. Fails loud on removing /
    /// retyping a port that isn't there, or adding one that already exists.
    fn edited_side(
        &self,
        node_id: Uuid,
        path: &str,
        side: Side,
        edit: &NodeEdit,
    ) -> Result<EditedSide, CliError> {
        let (add, rm, retype) = match side {
            Side::In => (&edit.add_in, &edit.rm_in, &edit.retype_in),
            Side::Out => (&edit.add_out, &edit.rm_out, &edit.retype_out),
            Side::Config => (&edit.add_config, &edit.rm_config, &edit.retype_config),
        };
        if add.is_empty() && rm.is_empty() && retype.is_empty() {
            return Ok((None, Vec::new()));
        }
        // Baseline = the most recent staged full list for this side (so a second
        // edit in the same working copy builds on the first), else the pulled
        // snapshot. Without the staged fold, a later same-side edit would resend
        // the pulled list and silently drop ports an earlier staged edit added.
        let mut ports: Vec<(String, Uuid, String)> = if let Some(staged) =
            self.staged_side_ports(node_id, side)
        {
            staged
        } else {
            let info = self
                    .index
                    .as_ref()
                    .and_then(|i| i.node_info(&node_id))
                    .ok_or_else(|| {
                        CliError::InvalidArgument(format!(
                            "can't edit ports of '{path}' — its current ports aren't pulled; run `hydrate pull`"
                        ))
                    })?;
            let current = match side {
                Side::In => &info.inputs,
                Side::Out => &info.outputs,
                Side::Config => &info.config,
            };
            current
                .iter()
                .map(|p| (p.name.clone(), p.id, p.r#type.clone()))
                .collect()
        };

        for name in rm {
            let before = ports.len();
            ports.retain(|(n, _, _)| n != name);
            if ports.len() == before {
                return Err(CliError::InvalidArgument(format!(
                    "'{path}' has no {} port '{name}' to remove",
                    side.as_str()
                )));
            }
        }
        for spec in retype {
            let port = ports
                .iter_mut()
                .find(|(n, _, _)| n == &spec.name)
                .ok_or_else(|| {
                    CliError::InvalidArgument(format!(
                        "'{path}' has no {} port '{}' to retype",
                        side.as_str(),
                        spec.name
                    ))
                })?;
            port.2 = spec.r#type.clone(); // keep id + name, change only the type
        }
        let mut added = Vec::new();
        for spec in add {
            if ports.iter().any(|(n, _, _)| n == &spec.name) {
                return Err(CliError::InvalidArgument(format!(
                    "'{path}' already has a {} port '{}'",
                    side.as_str(),
                    spec.name
                )));
            }
            let id = Uuid::new_v4();
            ports.push((spec.name.clone(), id, spec.r#type.clone()));
            added.push((spec.name.clone(), id));
        }
        let wire = ports
            .into_iter()
            .map(|(name, id, r#type)| models::Port {
                description: None,
                id,
                name: Some(name),
                r#type: Some(r#type),
            })
            .collect();
        Ok((Some(wire), added))
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

    /// Stage a flatten of the boundary at `path`: promotes its children to its
    /// parent and removes the boundary. Requires the node to be a pulled
    /// boundary — fails loud on a behavior, or when its kind isn't pulled.
    pub fn flatten_boundary(&mut self, path: &str) -> Result<BoundaryFlattened, CliError> {
        let id = self.resolve_node(path)?;
        let info = self
            .index
            .as_ref()
            .and_then(|i| i.node_info(&id))
            .ok_or_else(|| {
                CliError::InvalidArgument(format!(
                    "can't flatten '{path}' — its kind isn't pulled; run `hydrate pull`"
                ))
            })?;
        if info.kind != "boundary" {
            return Err(CliError::InvalidArgument(format!(
                "'{path}' is a {}, not a boundary — only boundaries can be flattened",
                info.kind
            )));
        }
        if self.staged_flattens().contains(&id) {
            return Err(CliError::InvalidArgument(format!(
                "boundary '{path}' is already staged for flatten"
            )));
        }
        let delta = models::FlattenBoundaryDelta::new(
            id,
            models::flatten_boundary_delta::Type::FlattenBoundary,
        );
        self.push(&delta)?;
        Ok(BoundaryFlattened {
            path: path.to_string(),
        })
    }

    /// Stage a reparent of the node at `path` under `new_parent` (a dotted path,
    /// or `None` = top level). Rejects moving a node under itself or one of its
    /// own descendants (a cycle) — the server enforces this too.
    pub fn reparent_node(
        &mut self,
        path: &str,
        new_parent: Option<&str>,
    ) -> Result<NodeReparented, CliError> {
        let id = self.resolve_node(path)?;
        let parent_id = match new_parent {
            None => None,
            Some(parent) => {
                if parent == path || parent.starts_with(&format!("{path}.")) {
                    return Err(CliError::InvalidArgument(format!(
                        "can't move '{path}' under '{parent}' — that's itself or a descendant"
                    )));
                }
                Some(self.resolve_node(parent)?)
            }
        };
        let delta = models::ReparentNodeDelta::new(
            id,
            parent_id,
            models::reparent_node_delta::Type::ReparentNode,
        );
        self.push(&delta)?;
        Ok(NodeReparented {
            path: path.to_string(),
            new_parent: new_parent.map(str::to_string),
        })
    }

    /// Stage the removal of the edge between two ports (addressed by their dotted
    /// `node.port` paths). If the edge was staged this session (not yet
    /// committed), drop that staged `add_edge` — it never reached the server. If
    /// it's a committed edge (in the pulled index), emit a `DeleteEdge`. Fail
    /// loud when there is no such edge.
    pub fn remove_edge(&mut self, from: &str, to: &str) -> Result<EdgeRemoved, CliError> {
        let source = self.resolve_port(from, Side::Out)?;
        let target = self.resolve_port(to, Side::In)?;
        let result = EdgeRemoved {
            from: from.to_string(),
            to: to.to_string(),
        };

        // 1) A staged-but-uncommitted edge: un-stage it (and its alias).
        let key = edge_key(source, target);
        if self.stage.aliases.remove(&key).is_some() {
            let src = source.to_string();
            let tgt = target.to_string();
            self.stage.deltas.retain(|v| {
                let is_this_edge = v.get("type").and_then(serde_json::Value::as_str)
                    == Some("add_edge")
                    && v.get("edge")
                        .and_then(|e| e.get("sourceHandle"))
                        .and_then(serde_json::Value::as_str)
                        == Some(src.as_str())
                    && v.get("edge")
                        .and_then(|e| e.get("targetHandle"))
                        .and_then(serde_json::Value::as_str)
                        == Some(tgt.as_str());
                !is_this_edge
            });
            return Ok(result);
        }

        // 2) A committed edge: resolve its id from the pulled index, emit delete.
        let edge_id = self
            .index
            .as_ref()
            .and_then(|i| i.edge_id(source, target))
            .ok_or_else(|| {
                CliError::InvalidArgument(format!(
                    "no edge from '{from}' to '{to}' (staged or on the branch); run `hydrate pull` if it's committed"
                ))
            })?;
        if self.staged_edge_deletions().contains(&edge_id) {
            return Err(CliError::InvalidArgument(format!(
                "edge from '{from}' to '{to}' is already staged for deletion"
            )));
        }
        let delta =
            models::DeleteEdgeDelta::new(edge_id, models::delete_edge_delta::Type::DeleteEdge);
        self.push(&delta)?;
        Ok(result)
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

/// What `remove_edge` recorded, for the caller to render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeRemoved {
    pub from: String,
    pub to: String,
}

/// What `remove_node` recorded, for the caller to render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRemoved {
    pub path: String,
}

/// What `reparent_node` recorded, for the caller to render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeReparented {
    pub path: String,
    /// The new parent path, or `None` for the top level.
    pub new_parent: Option<String>,
}

/// What `flatten_boundary` recorded, for the caller to render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundaryFlattened {
    pub path: String,
}

/// What `update_node` recorded, for the caller to render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeUpdated {
    pub path: String,
    pub name: Option<String>,
    /// `None` = untouched, `Some("")` = cleared, `Some(v)` = set.
    pub description: Option<String>,
    /// `None` = untouched, `Some([])` = cleared, `Some(vec)` = set.
    pub constraints: Option<Vec<String>>,
    /// Double-option scalars: `None` = untouched, `Some(None)` = cleared,
    /// `Some(Some(v))` = set.
    pub user_kind: Option<Option<String>>,
    pub path_prefix: Option<Option<String>>,
    pub is_external: Option<bool>,
    pub external_kind: Option<Option<String>>,
    pub protocol: Option<Option<String>>,
    pub doc_url: Option<Option<String>>,
    pub is_test_node: Option<bool>,
    /// `None` = untouched, `Some([])` = cleared, `Some(vec)` = set.
    pub verifications: Option<Vec<String>>,
    pub ports_changed: bool,
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

/// Drop blank/whitespace-only entries; `None` when nothing survives (so the
/// field is omitted from the delta and the server keeps its default).
fn non_empty(items: &[String]) -> Option<Vec<String>> {
    let kept: Vec<String> = items
        .iter()
        .filter(|c| !c.trim().is_empty())
        .cloned()
        .collect();
    if kept.is_empty() {
        None
    } else {
        Some(kept)
    }
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
            "reparent_node" => updates.push(Inner::ReparentNode(Box::new(parse_delta(value)?))),
            "flatten_boundary" => {
                updates.push(Inner::FlattenBoundary(Box::new(parse_delta(value)?)))
            }
            "delete_edge" => deletes.push(Inner::DeleteEdge(Box::new(parse_delta(value)?))),
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
                inputs: port_infos(node.data.inputs.as_deref())?,
                outputs: port_infos(node.data.outputs.as_deref())?,
                config: port_infos(node.data.config.as_deref())?,
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

/// Project a wire port list into the index's [`PortInfo`] — ALL ports, including
/// unnamed ones (recorded with an empty name). `node set` resends the full port
/// list to preserve UUIDs, so dropping a port here would delete it (and its
/// edges) on the next update. A port with no type is loud corruption: it can't
/// be faithfully resent, so fail rather than fabricate `""` and clobber the
/// server's type.
fn port_infos(ports: Option<&[models::WirePort]>) -> Result<Vec<crate::state::PortInfo>, CliError> {
    ports
        .unwrap_or_default()
        .iter()
        .map(|p| {
            let r#type = p.r#type.clone().ok_or_else(|| {
                CliError::State(format!(
                    "the branch graph has a port (id {}) with no type",
                    p.id
                ))
            })?;
            Ok(crate::state::PortInfo {
                id: p.id,
                name: p.name.clone().unwrap_or_default(),
                r#type,
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
        config: Vec<NamedType>,
        /// The node's description (the spec/prompt), if one was staged.
        description: Option<String>,
        /// Plain-text constraints staged on the node.
        constraints: Vec<String>,
        /// Plain-text verifications staged on the node.
        verifications: Vec<String>,
        /// Whether the node was marked external.
        external: bool,
        /// External-system protocol, if staged.
        protocol: Option<String>,
        /// Documentation URL, if staged.
        doc_url: Option<String>,
        /// Whether the node was marked a test node.
        is_test_node: bool,
    },
    Edge {
        from: String,
        to: String,
    },
    /// A staged partial edit of a node's data (only the set fields change).
    /// `constraints`: `None` = untouched, `Some([])` = cleared, `Some(vec)` = set.
    /// `inputs`/`outputs`: `Some(list)` = that side was replaced (full new list).
    UpdateNode {
        path: String,
        name: Option<String>,
        description: Option<String>,
        constraints: Option<Vec<String>>,
        inputs: Option<Vec<NamedType>>,
        outputs: Option<Vec<NamedType>>,
        config: Option<Vec<NamedType>>,
        /// Double-option scalars: `None` = untouched, `Some(None)` = cleared,
        /// `Some(Some(v))` = set.
        user_kind: Option<Option<String>>,
        path_prefix: Option<Option<String>>,
        external: Option<bool>,
        external_kind: Option<Option<String>>,
        protocol: Option<Option<String>>,
        doc_url: Option<Option<String>>,
        is_test_node: Option<bool>,
        verifications: Option<Vec<String>>,
    },
    /// A staged reparent: `new_parent` is `None` for the top level.
    Reparent {
        path: String,
        new_parent: Option<String>,
    },
    /// A staged boundary flatten (promotes children, removes the boundary).
    Flatten {
        path: String,
    },
    /// A staged edge deletion, rendered by its two port paths.
    DeleteEdge {
        from: String,
        to: String,
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
    // edge id → (source_handle, target_handle), inverted from the index's edge
    // map, so a `delete_edge` (which carries only the edge id) renders by ports.
    let edge_handles: std::collections::HashMap<Uuid, (Uuid, Uuid)> = index
        .map(|i| {
            i.edges
                .iter()
                .filter_map(|(k, eid)| {
                    let (s, t) = k.split_once(':')?;
                    Some((*eid, (Uuid::parse_str(s).ok()?, Uuid::parse_str(t).ok()?)))
                })
                .collect()
        })
        .unwrap_or_default();

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
                    config: named_types(data.config.as_deref()),
                    description: data.description.filter(|s| !s.is_empty()),
                    constraints: data.constraints.unwrap_or_default(),
                    verifications: data
                        .verifications
                        .unwrap_or_default()
                        .into_iter()
                        .map(|v| v.text)
                        .collect(),
                    external: data.is_external.unwrap_or(false),
                    protocol: data.protocol.flatten(),
                    doc_url: data.documentation_url.flatten(),
                    is_test_node: data.is_test_node.unwrap_or(false),
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
                    name: d.after.name,
                    description: d.after.description.filter(|s| !s.is_empty()),
                    // Keep the Option so the preview distinguishes "cleared"
                    // (Some([])) from "untouched" (None) — they are different edits.
                    constraints: d.after.constraints,
                    inputs: d.after.inputs.map(|p| named_types(Some(&p))),
                    outputs: d.after.outputs.map(|p| named_types(Some(&p))),
                    config: d.after.config.map(|p| named_types(Some(&p))),
                    // Keep the double-option so the preview distinguishes cleared
                    // (Some(None)) from untouched (None) from set (Some(Some(v))).
                    user_kind: d.after.user_kind,
                    path_prefix: d.after.path_prefix,
                    external: d.after.is_external,
                    external_kind: d.after.external_kind,
                    protocol: d.after.protocol,
                    doc_url: d.after.documentation_url,
                    is_test_node: d.after.is_test_node,
                    verifications: d
                        .after
                        .verifications
                        .map(|vs| vs.into_iter().map(|v| v.text).collect()),
                });
            }
            "reparent_node" => {
                let d: models::ReparentNodeDelta = parse_delta(value)?;
                let path = node_paths.get(&d.node_id).cloned().ok_or_else(|| {
                    CliError::State(
                        "a staged reparent targets a node that is neither staged nor pulled"
                            .to_string(),
                    )
                })?;
                let new_parent = match d.parent_id {
                    None => None,
                    Some(pid) => Some(node_paths.get(&pid).cloned().ok_or_else(|| {
                        CliError::State(
                            "a staged reparent targets a parent that is neither staged nor pulled"
                                .to_string(),
                        )
                    })?),
                };
                summary.updates += 1;
                summary.ops.push(OpSummary::Reparent { path, new_parent });
            }
            "flatten_boundary" => {
                let d: models::FlattenBoundaryDelta = parse_delta(value)?;
                let path = node_paths.get(&d.node_id).cloned().ok_or_else(|| {
                    CliError::State(
                        "a staged flatten targets a node that is neither staged nor pulled"
                            .to_string(),
                    )
                })?;
                summary.updates += 1;
                summary.ops.push(OpSummary::Flatten { path });
            }
            "delete_edge" => {
                let d: models::DeleteEdgeDelta = parse_delta(value)?;
                // Reverse the edge id to its two ports, then to their paths —
                // never a UUID. An id absent from the pulled edge map, or ports
                // absent from the path map, is corruption (surfaced loudly).
                let (src, tgt) = edge_handles.get(&d.edge_id).ok_or_else(|| {
                    CliError::State(
                        "a staged edge deletion targets an edge that isn't in the pulled index"
                            .to_string(),
                    )
                })?;
                let resolve = |id: &Uuid| {
                    port_paths.get(id).cloned().ok_or_else(|| {
                        CliError::State(
                            "a staged edge deletion references a port not in the index".to_string(),
                        )
                    })
                };
                summary.deletes += 1;
                summary.ops.push(OpSummary::DeleteEdge {
                    from: resolve(src)?,
                    to: resolve(tgt)?,
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
            config: vec![],
            user_kind: None,
            path_prefix: None,
            description: None,
            constraints: vec![],
            is_external: false,
            external_kind: None,
            verifications: vec![],
            protocol: None,
            doc_url: None,
            is_test_node: false,
        }
    }

    fn empty() -> Changeset {
        Changeset::with_index(Stage::empty(), None)
    }

    /// A scalar `node set` edit setting only the description.
    fn desc_edit(d: &str) -> NodeEdit {
        NodeEdit {
            description: Some(d.to_string()),
            ..Default::default()
        }
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
    fn add_node_stages_external_and_verifications() {
        let mut cs = empty();
        cs.add_node(&NodeSpec {
            is_external: true,
            external_kind: Some("postgres"),
            verifications: vec!["responds within 50ms".to_string(), "  ".to_string()],
            ..behavior("Db", None)
        })
        .unwrap();
        let d: models::AddNodeDelta =
            serde_json::from_value(cs.into_stage().deltas.remove(0)).unwrap();
        let data = d.node.data.unwrap();
        assert_eq!(data.is_external, Some(true));
        assert_eq!(data.external_kind, Some(Some("postgres".to_string())));
        // The blank verification is dropped; the real one becomes a Verification.
        let vs = data.verifications.unwrap();
        assert_eq!(vs.len(), 1);
        assert_eq!(vs[0].text, "responds within 50ms");
    }

    #[test]
    fn add_node_omits_external_and_verifications_when_absent() {
        let mut cs = empty();
        cs.add_node(&behavior("Plain", None)).unwrap();
        let d: models::AddNodeDelta =
            serde_json::from_value(cs.into_stage().deltas.remove(0)).unwrap();
        let data = d.node.data.unwrap();
        assert_eq!(data.is_external, None);
        assert_eq!(data.external_kind, None);
        assert_eq!(data.verifications, None);
    }

    #[test]
    fn add_node_drops_a_blank_external_kind() {
        // Parity with --description "": a blank kind is "no kind", not "".
        let mut cs = empty();
        cs.add_node(&NodeSpec {
            is_external: true,
            external_kind: Some("  "),
            ..behavior("Db", None)
        })
        .unwrap();
        let d: models::AddNodeDelta =
            serde_json::from_value(cs.into_stage().deltas.remove(0)).unwrap();
        let data = d.node.data.unwrap();
        assert_eq!(data.is_external, Some(true));
        assert_eq!(data.external_kind, None, "blank kind must collapse to None");
    }

    #[test]
    fn summarize_surfaces_external_and_verifications() {
        let mut cs = empty();
        cs.add_node(&NodeSpec {
            is_external: true,
            external_kind: Some("postgres"),
            verifications: vec!["responds within 50ms".to_string()],
            ..behavior("Db", None)
        })
        .unwrap();
        let summary = summarize(&cs.into_stage(), None).unwrap();
        let (external, verifications) = summary
            .ops
            .iter()
            .find_map(|op| match op {
                OpSummary::Node {
                    external,
                    verifications,
                    ..
                } => Some((*external, verifications.clone())),
                _ => None,
            })
            .unwrap();
        assert!(external, "is_external must map to external == true");
        assert_eq!(verifications, vec!["responds within 50ms".to_string()]);
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

    #[test]
    fn summarize_update_node_carries_rename_and_ports() {
        // The preview must reflect a rename and the resent port lists, by path.
        let (api, rater, score) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        let index = index_from_graph(&pulled_graph(api, rater, score, 5)).unwrap();
        let mut cs = Changeset::with_index(Stage::empty(), Some(index.clone()));
        cs.update_node(
            "Api.Rater",
            &NodeEdit {
                name: Some("Scorer".to_string()),
                add_out: vec![port("extra", "Blob")],
                ..Default::default()
            },
        )
        .unwrap();
        let summary = summarize(&cs.into_stage(), Some(&index)).unwrap();
        let (path, name, outputs) = summary
            .ops
            .iter()
            .find_map(|op| match op {
                OpSummary::UpdateNode {
                    path,
                    name,
                    outputs,
                    ..
                } => Some((path.clone(), name.clone(), outputs.clone())),
                _ => None,
            })
            .unwrap();
        assert_eq!(path, "Api.Rater"); // by path, never the UUID
        assert_eq!(name.as_deref(), Some("Scorer"));
        let outs = outputs.expect("the resent output list is present");
        assert!(outs.iter().any(|(n, t)| n == "extra" && t == "Blob"));
        assert!(outs.iter().any(|(n, _)| n == "score"));
    }

    #[test]
    fn summarize_update_node_carries_scalar_and_verification_fields() {
        // delta -> OpSummary must flatten the double-option scalars and extract
        // verification texts, so the preview shows them by value.
        let (api, rater, score) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        let index = index_from_graph(&pulled_graph(api, rater, score, 5)).unwrap();
        let mut cs = Changeset::with_index(Stage::empty(), Some(index.clone()));
        cs.update_node(
            "Api.Rater",
            &NodeEdit {
                user_kind: Some("subsystem".to_string()),
                path_prefix: Some("src/api/".to_string()),
                is_external: Some(true),
                external_kind: Some("rest-api".to_string()),
                verifications: Some(vec!["responds in 50ms".to_string()]),
                ..Default::default()
            },
        )
        .unwrap();
        let summary = summarize(&cs.into_stage(), Some(&index)).unwrap();
        let found = summary
            .ops
            .iter()
            .find_map(|op| match op {
                OpSummary::UpdateNode {
                    user_kind,
                    path_prefix,
                    external,
                    external_kind,
                    verifications,
                    ..
                } => Some((
                    user_kind.clone(),
                    path_prefix.clone(),
                    *external,
                    external_kind.clone(),
                    verifications.clone(),
                )),
                _ => None,
            })
            .unwrap();
        // Double-option scalars: Some(Some(v)) = set (vs Some(None) = cleared).
        assert_eq!(found.0, Some(Some("subsystem".to_string())));
        assert_eq!(found.1, Some(Some("src/api/".to_string())));
        assert_eq!(found.2, Some(true));
        assert_eq!(found.3, Some(Some("rest-api".to_string())));
        assert_eq!(found.4, Some(vec!["responds in 50ms".to_string()]));
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

    // ---- boundary flatten ----

    #[test]
    fn flatten_boundary_emits_the_delta_for_a_committed_boundary() {
        let (api, rater, score) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        let index = index_from_graph(&pulled_graph(api, rater, score, 5)).unwrap();
        let mut cs = Changeset::with_index(Stage::empty(), Some(index));
        cs.flatten_boundary("Api").unwrap();
        let d: models::FlattenBoundaryDelta = serde_json::from_value(
            cs.into_stage()
                .deltas
                .into_iter()
                .find(|v| v["type"] == "flatten_boundary")
                .unwrap(),
        )
        .unwrap();
        assert_eq!(d.node_id, api);
    }

    #[test]
    fn flatten_boundary_rejects_a_behavior() {
        let (api, rater, score) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        let index = index_from_graph(&pulled_graph(api, rater, score, 5)).unwrap();
        let mut cs = Changeset::with_index(Stage::empty(), Some(index));
        let err = cs.flatten_boundary("Api.Rater").unwrap_err();
        assert!(matches!(err, CliError::InvalidArgument(_)), "got {err:?}");
        assert!(err.to_string().contains("not a boundary"), "{err}");
    }

    #[test]
    fn flatten_boundary_without_a_pull_fails_loud() {
        let mut cs = empty();
        cs.add_node(&NodeSpec {
            kind: Kind::Boundary,
            ..behavior("Api", None)
        })
        .unwrap();
        let err = cs.flatten_boundary("Api").unwrap_err();
        assert!(err.to_string().contains("hydrate pull"), "{err}");
    }

    #[test]
    fn summarize_renders_a_flatten_by_path() {
        let (api, rater, score) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        let index = index_from_graph(&pulled_graph(api, rater, score, 5)).unwrap();
        let mut cs = Changeset::with_index(Stage::empty(), Some(index.clone()));
        cs.flatten_boundary("Api").unwrap();
        let summary = summarize(&cs.into_stage(), Some(&index)).unwrap();
        let found = summary.ops.iter().find_map(|op| match op {
            OpSummary::Flatten { path } => Some(path.clone()),
            _ => None,
        });
        assert_eq!(found.as_deref(), Some("Api"));
        assert!(!format!("{summary:?}").contains(&api.to_string()));
    }

    #[test]
    fn flatten_boundary_twice_fails_loud() {
        // A second flatten targets a node the first already dissolved — reject it
        // locally, parity with node rm / edge rm double-stage guards.
        let (api, rater, score) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        let index = index_from_graph(&pulled_graph(api, rater, score, 5)).unwrap();
        let mut cs = Changeset::with_index(Stage::empty(), Some(index));
        cs.flatten_boundary("Api").unwrap();
        let err = cs.flatten_boundary("Api").unwrap_err();
        assert!(matches!(err, CliError::InvalidArgument(_)), "got {err:?}");
        assert!(
            err.to_string().contains("already staged for flatten"),
            "{err}"
        );
        let count = cs
            .into_stage()
            .deltas
            .iter()
            .filter(|v| v["type"] == "flatten_boundary")
            .count();
        assert_eq!(count, 1);
    }

    #[test]
    fn flatten_boundary_lowers_among_updates_after_adds_before_deletes() {
        use models::V1DeltasBodyDeltasInner as Inner;
        let (api, rater, score) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        let index = index_from_graph(&pulled_graph(api, rater, score, 5)).unwrap();
        let mut cs = Changeset::with_index(Stage::empty(), Some(index));
        cs.add_node(&behavior("New", None)).unwrap();
        cs.flatten_boundary("Api").unwrap();
        cs.remove_node("Api.Rater").unwrap();
        let lowered = lower(&cs.into_stage()).unwrap();
        assert!(matches!(lowered[0], Inner::AddNode(_)), "add first");
        assert!(
            matches!(lowered[1], Inner::FlattenBoundary(_)),
            "flatten after adds, before deletes"
        );
        assert!(matches!(lowered[2], Inner::DeleteNode(_)), "delete last");
    }

    // ---- node mv (reparent) ----

    #[test]
    fn reparent_node_to_a_committed_boundary() {
        let (api, rater, score) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        let index = index_from_graph(&pulled_graph(api, rater, score, 5)).unwrap();
        let mut cs = Changeset::with_index(Stage::empty(), Some(index));
        // Add a second top-level boundary, then move Api.Rater under it.
        cs.add_node(&NodeSpec {
            kind: Kind::Boundary,
            ..behavior("Core", None)
        })
        .unwrap();
        let core = cs.lookup(&node_key("Core")).unwrap();
        cs.reparent_node("Api.Rater", Some("Core")).unwrap();
        let d: models::ReparentNodeDelta = serde_json::from_value(
            cs.into_stage()
                .deltas
                .into_iter()
                .find(|v| v["type"] == "reparent_node")
                .unwrap(),
        )
        .unwrap();
        assert_eq!(d.node_id, rater);
        // The load-bearing field: the new parent resolves to Core's id, not None.
        assert_eq!(d.parent_id, Some(core));
    }

    #[test]
    fn reparent_node_to_top_level_sets_no_parent() {
        let (api, rater, score) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        let index = index_from_graph(&pulled_graph(api, rater, score, 5)).unwrap();
        let mut cs = Changeset::with_index(Stage::empty(), Some(index));
        cs.reparent_node("Api.Rater", None).unwrap();
        let d: models::ReparentNodeDelta = serde_json::from_value(
            cs.into_stage()
                .deltas
                .into_iter()
                .find(|v| v["type"] == "reparent_node")
                .unwrap(),
        )
        .unwrap();
        assert_eq!(d.parent_id, None);
    }

    #[test]
    fn reparent_node_rejects_moving_under_itself_or_a_descendant() {
        let (api, rater, score) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        let index = index_from_graph(&pulled_graph(api, rater, score, 5)).unwrap();
        let mut cs = Changeset::with_index(Stage::empty(), Some(index));
        // Move Api under Api.Rater (its own descendant) → cycle, rejected.
        let err = cs.reparent_node("Api", Some("Api.Rater")).unwrap_err();
        assert!(matches!(err, CliError::InvalidArgument(_)), "got {err:?}");
        assert!(err.to_string().contains("itself or a descendant"), "{err}");
    }

    #[test]
    fn summarize_renders_a_reparent_by_path() {
        let (api, rater, score) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        let index = index_from_graph(&pulled_graph(api, rater, score, 5)).unwrap();
        let mut cs = Changeset::with_index(Stage::empty(), Some(index.clone()));
        cs.reparent_node("Api.Rater", None).unwrap();
        let summary = summarize(&cs.into_stage(), Some(&index)).unwrap();
        let found = summary.ops.iter().find_map(|op| match op {
            OpSummary::Reparent { path, new_parent } => Some((path.clone(), new_parent.clone())),
            _ => None,
        });
        assert_eq!(found, Some(("Api.Rater".to_string(), None)));
        assert!(!format!("{summary:?}").contains(&rater.to_string()));
    }

    #[test]
    fn reparent_node_rejects_moving_under_itself() {
        let (api, rater, score) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        let index = index_from_graph(&pulled_graph(api, rater, score, 5)).unwrap();
        let mut cs = Changeset::with_index(Stage::empty(), Some(index));
        let err = cs.reparent_node("Api", Some("Api")).unwrap_err();
        assert!(matches!(err, CliError::InvalidArgument(_)), "got {err:?}");
        assert!(err.to_string().contains("itself or a descendant"), "{err}");
    }

    #[test]
    fn reparent_node_lowers_among_updates_after_adds_before_deletes() {
        use models::V1DeltasBodyDeltasInner as Inner;
        let (api, rater, score) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        let index = index_from_graph(&pulled_graph(api, rater, score, 5)).unwrap();
        let mut cs = Changeset::with_index(Stage::empty(), Some(index));
        cs.add_node(&behavior("New", None)).unwrap();
        cs.reparent_node("Api.Rater", None).unwrap();
        cs.remove_node("Api").unwrap();
        let lowered = lower(&cs.into_stage()).unwrap();
        assert!(matches!(lowered[0], Inner::AddNode(_)), "add first");
        assert!(
            matches!(lowered[1], Inner::ReparentNode(_)),
            "reparent after adds, before deletes"
        );
        assert!(matches!(lowered[2], Inner::DeleteNode(_)), "delete last");
    }

    #[test]
    fn summarize_renders_a_reparent_with_a_named_parent() {
        let (api, rater, score) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        let index = index_from_graph(&pulled_graph(api, rater, score, 5)).unwrap();
        let mut cs = Changeset::with_index(Stage::empty(), Some(index.clone()));
        // Add a boundary, then move Api.Rater under it; the Some(parent) path must
        // reverse-resolve to that boundary's dotted path.
        cs.add_node(&NodeSpec {
            kind: Kind::Boundary,
            ..behavior("Core", None)
        })
        .unwrap();
        cs.reparent_node("Api.Rater", Some("Core")).unwrap();
        let summary = summarize(&cs.into_stage(), Some(&index)).unwrap();
        let found = summary.ops.iter().find_map(|op| match op {
            OpSummary::Reparent { path, new_parent } => Some((path.clone(), new_parent.clone())),
            _ => None,
        });
        assert_eq!(
            found,
            Some(("Api.Rater".to_string(), Some("Core".to_string())))
        );
    }

    #[test]
    fn summarize_flags_a_reparent_to_an_unresolvable_node() {
        let mut stage = Stage::empty();
        stage.deltas.push(serde_json::json!({
            "type": "reparent_node", "nodeId": Uuid::from_u128(0xDEAD), "parent_id": null
        }));
        let err = summarize(&stage, None).unwrap_err();
        assert!(matches!(err, CliError::State(_)), "got {err:?}");
        assert!(err.to_string().contains("reparent targets a node"), "{err}");
    }

    #[test]
    fn summarize_flags_a_reparent_to_an_unresolvable_parent() {
        let (api, rater, score) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        let index = index_from_graph(&pulled_graph(api, rater, score, 5)).unwrap();
        let mut stage = Stage::empty();
        // Node resolves (Api == id 1), but the parent id is dangling.
        stage.deltas.push(serde_json::json!({
            "type": "reparent_node", "nodeId": api, "parent_id": Uuid::from_u128(0xDEAD)
        }));
        let _ = (rater, score);
        let err = summarize(&stage, Some(&index)).unwrap_err();
        assert!(matches!(err, CliError::State(_)), "got {err:?}");
        assert!(
            err.to_string().contains("reparent targets a parent"),
            "{err}"
        );
    }

    // ---- edge rm ----

    /// A pulled graph with a committed edge `A.o → B.i` (id `eid`) and a
    /// dangling input `C.x` (no edge). Returns (changeset, index, eid).
    fn committed_edge() -> (Changeset, Index, Uuid) {
        let (ao, bi, cx, eid) = (
            Uuid::from_u128(0x40),
            Uuid::from_u128(0x42),
            Uuid::from_u128(0x43),
            Uuid::from_u128(0xED),
        );
        let graph: models::GraphResponse = serde_json::from_value(serde_json::json!({
            "branch": { "id": Uuid::from_u128(0xB), "version": 2 },
            "project_id": Uuid::from_u128(0xA), "version": "2",
            "nodes": [
                { "id": Uuid::from_u128(1), "kind": "behavior", "parent_id": null,
                  "position": {"x":0.0,"y":0.0},
                  "data": {"name":"A","description":"","status":"idle","isTestNode":false,"is_external":false,
                           "outputs":[{"id":ao,"name":"o","type":"T"}]} },
                { "id": Uuid::from_u128(2), "kind": "behavior", "parent_id": null,
                  "position": {"x":0.0,"y":0.0},
                  "data": {"name":"B","description":"","status":"idle","isTestNode":false,"is_external":false,
                           "inputs":[{"id":bi,"name":"i","type":"T"}]} },
                { "id": Uuid::from_u128(3), "kind": "behavior", "parent_id": null,
                  "position": {"x":0.0,"y":0.0},
                  "data": {"name":"C","description":"","status":"idle","isTestNode":false,"is_external":false,
                           "inputs":[{"id":cx,"name":"x","type":"T"}]} },
            ],
            "edges": [ { "id": eid, "source": Uuid::from_u128(1), "target": Uuid::from_u128(2),
                         "sourceHandle": ao, "targetHandle": bi } ],
        })).unwrap();
        let index = index_from_graph(&graph).unwrap();
        (
            Changeset::with_index(Stage::empty(), Some(index.clone())),
            index,
            eid,
        )
    }

    #[test]
    fn remove_edge_emits_delete_for_a_committed_edge() {
        let (mut cs, _index, eid) = committed_edge();
        cs.remove_edge("A.o", "B.i").unwrap();
        let d: models::DeleteEdgeDelta = serde_json::from_value(
            cs.into_stage()
                .deltas
                .into_iter()
                .find(|v| v["type"] == "delete_edge")
                .unwrap(),
        )
        .unwrap();
        assert_eq!(d.edge_id, eid);
    }

    #[test]
    fn remove_edge_unstages_a_staged_edge_without_a_delete() {
        let mut cs = graph_with_two_ports(); // Maker.dog (out), Rater.raw (in)
        cs.add_edge("Maker.dog", "Rater.raw").unwrap();
        cs.remove_edge("Maker.dog", "Rater.raw").unwrap();
        let stage = cs.into_stage();
        // The staged add_edge is gone; no delete_edge was emitted (it never
        // reached the server), and the edge alias is cleared.
        assert!(stage
            .deltas
            .iter()
            .all(|v| v["type"] != "add_edge" && v["type"] != "delete_edge"));
        assert!(stage.aliases.keys().all(|k| !k.starts_with("edge:")));
    }

    #[test]
    fn remove_edge_fails_loud_when_no_such_edge() {
        let (mut cs, _i, _e) = committed_edge();
        // A.o and C.x both exist, but there is no edge between them.
        let err = cs.remove_edge("A.o", "C.x").unwrap_err();
        assert!(matches!(err, CliError::InvalidArgument(_)), "got {err:?}");
        assert!(err.to_string().contains("no edge from"), "{err}");
    }

    #[test]
    fn lower_orders_delete_edge_after_adds() {
        use models::V1DeltasBodyDeltasInner as Inner;
        let (mut cs, _i, _e) = committed_edge();
        cs.add_node(&behavior("New", None)).unwrap();
        cs.remove_edge("A.o", "B.i").unwrap();
        let lowered = lower(&cs.into_stage()).unwrap();
        assert!(matches!(lowered[0], Inner::AddNode(_)));
        assert!(matches!(lowered[1], Inner::DeleteEdge(_)), "delete last");
    }

    #[test]
    fn summarize_renders_an_edge_deletion_by_ports_not_uuid() {
        let (mut cs, index, eid) = committed_edge();
        cs.remove_edge("A.o", "B.i").unwrap();
        let summary = summarize(&cs.into_stage(), Some(&index)).unwrap();
        let found = summary.ops.iter().find_map(|op| match op {
            OpSummary::DeleteEdge { from, to } => Some((from.clone(), to.clone())),
            _ => None,
        });
        assert_eq!(found, Some(("A.o".to_string(), "B.i".to_string())));
        assert!(!format!("{summary:?}").contains(&eid.to_string()));
    }

    #[test]
    fn remove_edge_twice_fails_loud() {
        // The index isn't mutated, so a second remove of a committed edge must be
        // rejected, not silently stage a duplicate delete (parity with node rm).
        let (mut cs, _i, _e) = committed_edge();
        cs.remove_edge("A.o", "B.i").unwrap();
        let err = cs.remove_edge("A.o", "B.i").unwrap_err();
        assert!(matches!(err, CliError::InvalidArgument(_)), "got {err:?}");
        assert!(
            err.to_string().contains("already staged for deletion"),
            "{err}"
        );
        // Exactly one delete_edge was staged.
        let count = cs
            .into_stage()
            .deltas
            .iter()
            .filter(|v| v["type"] == "delete_edge")
            .count();
        assert_eq!(count, 1);
    }

    #[test]
    fn remove_edge_reversed_direction_fails_loud() {
        // Giving the input as --from (B.i) must fail with the directional hint,
        // never silently match the A.o -> B.i edge.
        let (mut cs, _i, _e) = committed_edge();
        let err = cs.remove_edge("B.i", "A.o").unwrap_err();
        assert!(matches!(err, CliError::InvalidArgument(_)), "got {err:?}");
        assert!(
            err.to_string()
                .contains("an edge runs from an output (--from) to an input (--to)"),
            "{err}"
        );
    }

    #[test]
    fn remove_edge_bad_port_path_fails_loud() {
        let (mut cs, _i, _e) = committed_edge();
        let err = cs.remove_edge("Aooo", "B.i").unwrap_err();
        assert!(matches!(err, CliError::InvalidArgument(_)), "got {err:?}");
        assert!(err.to_string().contains("not a port path"), "{err}");
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
        // A delta kind this version doesn't itemize (forward-compat). All current
        // /v1 deltas are now rendered, so use a hypothetical future kind.
        let mut stage = Stage::empty();
        stage
            .deltas
            .push(serde_json::json!({"type": "future_delta_kind", "nodeId": Uuid::new_v4()}));
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
            .push(serde_json::json!({"type": "future_delta_kind", "id": Uuid::new_v4()}));
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
    fn index_from_graph_fails_loud_on_a_typeless_port() {
        let graph: models::GraphResponse = serde_json::from_value(serde_json::json!({
            "branch": { "id": Uuid::from_u128(0xB), "version": 1 },
            "project_id": Uuid::from_u128(0xA), "version": "1",
            "edges": [],
            "nodes": [ { "id": Uuid::from_u128(1), "kind": "behavior", "parent_id": null,
                "position": {"x":0.0,"y":0.0},
                "data": {"name":"A","description":"","status":"idle","isTestNode":false,"is_external":false,
                         "outputs":[{"id":Uuid::from_u128(7),"name":"o"}]} } ],
        }))
        .unwrap();
        let err = index_from_graph(&graph).unwrap_err();
        assert!(matches!(err, CliError::State(_)), "got {err:?}");
        assert!(err.to_string().contains("with no type"), "{err}");
    }

    #[test]
    fn index_from_graph_keeps_an_unnamed_port_in_node_info() {
        // Unnamed ports aren't path-addressable (excluded from `entries`), but
        // node_info MUST keep them (id + type) so `node set` can resend the full
        // list without dropping them.
        let node = Uuid::from_u128(1);
        let unnamed = Uuid::from_u128(0x99);
        let graph: models::GraphResponse = serde_json::from_value(serde_json::json!({
            "branch": { "id": Uuid::from_u128(0xB), "version": 1 },
            "project_id": Uuid::from_u128(0xA), "version": "1",
            "edges": [],
            "nodes": [ { "id": node, "kind": "behavior", "parent_id": null,
                "position": {"x":0.0,"y":0.0},
                "data": {"name":"A","description":"","status":"idle","isTestNode":false,"is_external":false,
                         "outputs":[{"id":unnamed,"type":"T"}]} } ],
        }))
        .unwrap();
        let index = index_from_graph(&graph).unwrap();
        // Not path-addressable...
        assert!(index.entries.keys().all(|k| !k.starts_with("port:")));
        // ...but preserved in node_info (id kept, empty name).
        let outs = &index.node_info(&node).unwrap().outputs;
        assert_eq!(outs.len(), 1);
        assert_eq!(outs[0].id, unnamed);
        assert_eq!(outs[0].name, "");
    }

    #[test]
    fn index_from_graph_fails_loud_on_two_edges_between_the_same_ports() {
        let (src, tgt) = (Uuid::from_u128(0x50), Uuid::from_u128(0x70));
        let graph: models::GraphResponse = serde_json::from_value(serde_json::json!({
            "branch": { "id": Uuid::from_u128(0xB), "version": 1 },
            "project_id": Uuid::from_u128(0xA), "version": "1",
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
            "edges": [
                { "id": Uuid::from_u128(0xE1), "source": Uuid::from_u128(1), "target": Uuid::from_u128(2), "sourceHandle": src, "targetHandle": tgt },
                { "id": Uuid::from_u128(0xE2), "source": Uuid::from_u128(1), "target": Uuid::from_u128(2), "sourceHandle": src, "targetHandle": tgt },
            ],
        }))
        .unwrap();
        let err = index_from_graph(&graph).unwrap_err();
        assert!(
            err.to_string().contains("two edges between the same ports"),
            "{err}"
        );
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
        cs.update_node("Api.Rater", &desc_edit("new prompt"))
            .unwrap();
        let d: models::UpdateNodeDataDelta =
            serde_json::from_value(cs.into_stage().deltas[0].clone()).unwrap();
        assert_eq!(d.node_id, rater);
        // Only the description is present; other fields are left untouched (None).
        assert_eq!(d.after.description.as_deref(), Some("new prompt"));
        assert_eq!(d.after.constraints, None);
        assert_eq!(d.after.name, None);
    }

    /// Pull a node with one input (`raw:Patty`, id 0x4ABC) and one output
    /// (`score:Rating`, id == `score`), for the port-edit tests.
    fn rater_changeset() -> (Changeset, Uuid, Uuid) {
        let (api, rater, score) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        let index = index_from_graph(&pulled_graph(api, rater, score, 5)).unwrap();
        (
            Changeset::with_index(Stage::empty(), Some(index)),
            Uuid::from_u128(0x4ABC),
            score,
        )
    }

    fn update_delta(cs: Changeset) -> models::UpdateNodeDataDelta {
        serde_json::from_value(
            cs.into_stage()
                .deltas
                .into_iter()
                .find(|v| v["type"] == "update_node_data")
                .unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn update_node_add_port_resends_full_list_preserving_existing_ids() {
        let (mut cs, _raw, _score) = rater_changeset();
        cs.update_node(
            "Api.Rater",
            &NodeEdit {
                add_out: vec![port("extra", "Blob")],
                ..Default::default()
            },
        )
        .unwrap();
        let alias_present = cs
            .into_stage()
            .aliases
            .contains_key("port:Api.Rater:out:extra");
        assert!(alias_present, "added port must be wireable this session");

        let (mut cs, _raw, score) = rater_changeset();
        cs.update_node(
            "Api.Rater",
            &NodeEdit {
                add_out: vec![port("extra", "Blob")],
                ..Default::default()
            },
        )
        .unwrap();
        let d = update_delta(cs);
        let outs = d.after.outputs.unwrap();
        // The existing output is resent with its ORIGINAL id (edges stay intact).
        assert!(outs
            .iter()
            .any(|p| p.id == score && p.name.as_deref() == Some("score")));
        // The new port has a fresh id and the given type.
        let extra = outs
            .iter()
            .find(|p| p.name.as_deref() == Some("extra"))
            .unwrap();
        assert_ne!(extra.id, score);
        assert_eq!(extra.r#type.as_deref(), Some("Blob"));
        // The untouched side stays None (key-presence: leave it).
        assert_eq!(d.after.inputs, None);
    }

    #[test]
    fn update_node_retype_keeps_the_port_id_changes_only_the_type() {
        let (mut cs, _raw, score) = rater_changeset();
        cs.update_node(
            "Api.Rater",
            &NodeEdit {
                retype_out: vec![port("score", "NewType")],
                ..Default::default()
            },
        )
        .unwrap();
        let d = update_delta(cs);
        let s = d
            .after
            .outputs
            .unwrap()
            .into_iter()
            .find(|p| p.name.as_deref() == Some("score"))
            .unwrap();
        assert_eq!(s.id, score, "retype must preserve the port UUID");
        assert_eq!(s.r#type.as_deref(), Some("NewType"));
    }

    #[test]
    fn update_node_rm_port_drops_it() {
        let (mut cs, _raw, _score) = rater_changeset();
        cs.update_node(
            "Api.Rater",
            &NodeEdit {
                rm_in: vec!["raw".to_string()],
                ..Default::default()
            },
        )
        .unwrap();
        let d = update_delta(cs);
        // The only input was removed → inputs is present-but-empty (that side replaced).
        assert_eq!(d.after.inputs.unwrap().len(), 0);
        assert_eq!(d.after.outputs, None);
    }

    #[test]
    fn update_node_port_edits_fail_loud() {
        // rm a port that isn't there
        let (mut cs, _r, _s) = rater_changeset();
        let err = cs
            .update_node(
                "Api.Rater",
                &NodeEdit {
                    rm_in: vec!["nope".to_string()],
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert!(err.to_string().contains("no in port 'nope'"), "{err}");

        // add a port that already exists
        let (mut cs, _r, _s) = rater_changeset();
        let err = cs
            .update_node(
                "Api.Rater",
                &NodeEdit {
                    add_in: vec![port("raw", "X")],
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("already has a in port 'raw'"),
            "{err}"
        );
    }

    #[test]
    fn update_node_port_edit_without_a_pull_fails_loud() {
        // A staged-only node has no pulled ports, so port edits can't proceed.
        let mut cs = empty();
        cs.add_node(&behavior("Rater", None)).unwrap();
        let err = cs
            .update_node(
                "Rater",
                &NodeEdit {
                    add_out: vec![port("x", "T")],
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert!(err.to_string().contains("hydrate pull"), "{err}");
    }

    #[test]
    fn update_node_rename_sets_the_name() {
        let (mut cs, _r, _s) = rater_changeset();
        cs.update_node(
            "Api.Rater",
            &NodeEdit {
                name: Some("Scorer".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(update_delta(cs).after.name.as_deref(), Some("Scorer"));
    }

    #[test]
    fn update_node_sets_boundary_and_external_scalars() {
        let (mut cs, _r, _s) = rater_changeset();
        cs.update_node(
            "Api.Rater",
            &NodeEdit {
                user_kind: Some("subsystem".to_string()),
                path_prefix: Some("src/api/".to_string()),
                is_external: Some(true),
                external_kind: Some("rest-api".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        let d = update_delta(cs);
        // Double-option wire fields carry Some(Some(value)); is_external is a bool.
        assert_eq!(d.after.user_kind, Some(Some("subsystem".to_string())));
        assert_eq!(d.after.path_prefix, Some(Some("src/api/".to_string())));
        assert_eq!(d.after.is_external, Some(true));
        assert_eq!(d.after.external_kind, Some(Some("rest-api".to_string())));
        // Untouched fields stay None (key-presence).
        assert_eq!(d.after.description, None);
        assert_eq!(d.after.verifications, None);
    }

    #[test]
    fn update_node_accepts_a_lone_boundary_scalar() {
        // Each new scalar alone must satisfy is_empty (not be rejected as
        // "nothing to set") and land in the delta.
        let cases: [(&str, NodeEdit, Option<Option<String>>); 3] = [
            (
                "user_kind",
                NodeEdit {
                    user_kind: Some("subsystem".to_string()),
                    ..Default::default()
                },
                Some(Some("subsystem".to_string())),
            ),
            (
                "path_prefix",
                NodeEdit {
                    path_prefix: Some("src/".to_string()),
                    ..Default::default()
                },
                Some(Some("src/".to_string())),
            ),
            (
                "external_kind",
                NodeEdit {
                    external_kind: Some("queue".to_string()),
                    ..Default::default()
                },
                Some(Some("queue".to_string())),
            ),
        ];
        for (label, edit, want) in cases {
            let (mut cs, _r, _s) = rater_changeset();
            cs.update_node("Api.Rater", &edit)
                .unwrap_or_else(|e| panic!("{label} alone rejected: {e}"));
            let d = update_delta(cs);
            let got = match label {
                "user_kind" => d.after.user_kind,
                "path_prefix" => d.after.path_prefix,
                _ => d.after.external_kind,
            };
            assert_eq!(got, want, "{label}");
        }
    }

    #[test]
    fn update_node_accepts_each_lone_clear_or_class_b_flag() {
        // Each clear flag and each new scalar, alone, must satisfy is_empty (not
        // be rejected as "nothing to set"). Pins every new is_empty clause.
        let lone: Vec<(&str, NodeEdit)> = vec![
            (
                "clear_description",
                NodeEdit {
                    clear_description: true,
                    ..Default::default()
                },
            ),
            (
                "clear_user_kind",
                NodeEdit {
                    clear_user_kind: true,
                    ..Default::default()
                },
            ),
            (
                "clear_path_prefix",
                NodeEdit {
                    clear_path_prefix: true,
                    ..Default::default()
                },
            ),
            (
                "clear_external_kind",
                NodeEdit {
                    clear_external_kind: true,
                    ..Default::default()
                },
            ),
            (
                "clear_protocol",
                NodeEdit {
                    clear_protocol: true,
                    ..Default::default()
                },
            ),
            (
                "clear_doc_url",
                NodeEdit {
                    clear_doc_url: true,
                    ..Default::default()
                },
            ),
            (
                "protocol",
                NodeEdit {
                    protocol: Some("gRPC".to_string()),
                    ..Default::default()
                },
            ),
            (
                "doc_url",
                NodeEdit {
                    doc_url: Some("https://x".to_string()),
                    ..Default::default()
                },
            ),
            (
                "is_test_node",
                NodeEdit {
                    is_test_node: Some(true),
                    ..Default::default()
                },
            ),
        ];
        for (label, edit) in lone {
            let (mut cs, _r, _s) = rater_changeset();
            cs.update_node("Api.Rater", &edit)
                .unwrap_or_else(|e| panic!("lone {label} rejected: {e}"));
        }
    }

    #[test]
    fn update_node_clears_a_scalar_to_null() {
        // `--clear-user-kind` → wire Some(None) (explicit null), distinct from
        // untouched (None) and set (Some(Some(v))).
        let (mut cs, _r, _s) = rater_changeset();
        cs.update_node(
            "Api.Rater",
            &NodeEdit {
                clear_user_kind: true,
                ..Default::default()
            },
        )
        .unwrap();
        let d = update_delta(cs);
        assert_eq!(d.after.user_kind, Some(None), "cleared to null");
        assert_eq!(d.after.path_prefix, None, "others untouched");
    }

    #[test]
    fn update_node_clear_description_sets_empty() {
        // description has no null on the wire — clear means empty string, which is
        // distinct from untouched (None).
        let (mut cs, _r, _s) = rater_changeset();
        cs.update_node(
            "Api.Rater",
            &NodeEdit {
                clear_description: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(update_delta(cs).after.description, Some(String::new()));
    }

    #[test]
    fn update_node_clear_overrides_a_value() {
        // A clear flag wins over a value for the same field.
        let (mut cs, _r, _s) = rater_changeset();
        cs.update_node(
            "Api.Rater",
            &NodeEdit {
                external_kind: Some("rest-api".to_string()),
                clear_external_kind: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(update_delta(cs).after.external_kind, Some(None));
    }

    #[test]
    fn update_node_sets_protocol_doc_url_and_test_node() {
        let (mut cs, _r, _s) = rater_changeset();
        cs.update_node(
            "Api.Rater",
            &NodeEdit {
                protocol: Some("gRPC".to_string()),
                doc_url: Some("https://x/docs".to_string()),
                is_test_node: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
        let d = update_delta(cs);
        assert_eq!(d.after.protocol, Some(Some("gRPC".to_string())));
        assert_eq!(
            d.after.documentation_url,
            Some(Some("https://x/docs".to_string()))
        );
        assert_eq!(d.after.is_test_node, Some(true));
    }

    #[test]
    fn add_node_sets_protocol_doc_url_and_test_node() {
        let mut cs = empty();
        cs.add_node(&NodeSpec {
            is_external: true,
            external_kind: Some("rest-api"),
            protocol: Some("HTTPS REST"),
            doc_url: Some("https://x/api"),
            is_test_node: true,
            ..behavior("Ext", None)
        })
        .unwrap();
        let d: models::AddNodeDelta =
            serde_json::from_value(cs.into_stage().deltas.remove(0)).unwrap();
        let data = d.node.data.unwrap();
        assert_eq!(data.protocol, Some(Some("HTTPS REST".to_string())));
        assert_eq!(
            data.documentation_url,
            Some(Some("https://x/api".to_string()))
        );
        assert_eq!(data.is_test_node, Some(true));
    }

    #[test]
    fn add_node_mints_config_ports() {
        let mut cs = empty();
        cs.add_node(&NodeSpec {
            config: vec![port("region", "String"), port("retries", "Int")],
            ..behavior("Worker", None)
        })
        .unwrap();
        let d: models::AddNodeDelta =
            serde_json::from_value(cs.into_stage().deltas.remove(0)).unwrap();
        let config = d.node.data.unwrap().config.unwrap();
        let names: Vec<&str> = config.iter().filter_map(|p| p.name.as_deref()).collect();
        assert_eq!(names, vec!["region", "retries"]);
        // Each config port gets a minted id + the given type.
        assert_eq!(config[0].r#type.as_deref(), Some("String"));
    }

    #[test]
    fn update_node_adds_a_config_port_resending_full_list() {
        // A node with a pulled config port; --add-config resends the full config
        // list (existing kept with its id) and leaves inputs/outputs untouched.
        let (api, rater, score) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        let cfg_id = Uuid::from_u128(0xC0);
        let mut graph = pulled_graph(api, rater, score, 5);
        // Give Api.Rater a committed config port.
        for n in graph.nodes.iter_mut() {
            if n.id == rater {
                n.data.config = Some(vec![models::WirePort {
                    description: None,
                    id: cfg_id,
                    name: Some("region".to_string()),
                    r#type: Some("String".to_string()),
                }]);
            }
        }
        let index = index_from_graph(&graph).unwrap();
        // index_from_graph populated the config channel.
        assert_eq!(index.node_info(&rater).unwrap().config.len(), 1);

        let mut cs = Changeset::with_index(Stage::empty(), Some(index));
        cs.update_node(
            "Api.Rater",
            &NodeEdit {
                add_config: vec![port("retries", "Int")],
                ..Default::default()
            },
        )
        .unwrap();
        let d = update_delta(cs);
        let config = d.after.config.unwrap();
        // Existing config port kept (same id), new one appended.
        assert!(config
            .iter()
            .any(|p| p.id == cfg_id && p.name.as_deref() == Some("region")));
        assert!(config.iter().any(|p| p.name.as_deref() == Some("retries")));
        // Inputs/outputs untouched (key-presence).
        assert_eq!(d.after.inputs, None);
        assert_eq!(d.after.outputs, None);
    }

    #[test]
    fn add_node_drops_a_blank_protocol_and_doc_url() {
        let mut cs = empty();
        cs.add_node(&NodeSpec {
            protocol: Some("  "),
            doc_url: Some(""),
            ..behavior("N", None)
        })
        .unwrap();
        let d: models::AddNodeDelta =
            serde_json::from_value(cs.into_stage().deltas.remove(0)).unwrap();
        let data = d.node.data.unwrap();
        assert_eq!(data.protocol, None, "blank protocol omitted");
        assert_eq!(data.documentation_url, None, "blank doc-url omitted");
    }

    #[test]
    fn update_node_no_external_sets_is_external_false() {
        let (mut cs, _r, _s) = rater_changeset();
        cs.update_node(
            "Api.Rater",
            &NodeEdit {
                is_external: Some(false),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(update_delta(cs).after.is_external, Some(false));
    }

    #[test]
    fn update_node_replaces_verifications_with_minted_rows() {
        let (mut cs, _r, _s) = rater_changeset();
        cs.update_node(
            "Api.Rater",
            &NodeEdit {
                verifications: Some(vec![
                    "responds within 50ms".to_string(),
                    "is idempotent".to_string(),
                ]),
                ..Default::default()
            },
        )
        .unwrap();
        let vs = update_delta(cs).after.verifications.unwrap();
        let texts: Vec<&str> = vs.iter().map(|v| v.text.as_str()).collect();
        assert_eq!(texts, vec!["responds within 50ms", "is idempotent"]);
        // Each minted verification has a fresh id (distinct).
        assert_ne!(vs[0].id, vs[1].id);
    }

    #[test]
    fn update_node_clear_verifications_sets_an_empty_list() {
        let (mut cs, _r, _s) = rater_changeset();
        cs.update_node(
            "Api.Rater",
            &NodeEdit {
                verifications: Some(vec![]),
                ..Default::default()
            },
        )
        .unwrap();
        // Present-but-empty = cleared (distinct from None = untouched).
        assert_eq!(update_delta(cs).after.verifications.unwrap().len(), 0);
    }

    #[test]
    fn update_node_second_same_side_edit_builds_on_the_first_staged_list() {
        // Two port edits on the same side in one working copy: the second must
        // build on the first's staged list, not re-derive from the pulled
        // snapshot — else it silently drops the port the first edit added.
        let (mut cs, _raw, score) = rater_changeset();
        cs.update_node(
            "Api.Rater",
            &NodeEdit {
                add_out: vec![port("extra", "Blob")],
                ..Default::default()
            },
        )
        .unwrap();
        cs.update_node(
            "Api.Rater",
            &NodeEdit {
                add_out: vec![port("extra2", "Blob")],
                ..Default::default()
            },
        )
        .unwrap();
        // Inspect the LAST update_node_data delta — the one that commits last.
        let last = cs
            .into_stage()
            .deltas
            .into_iter()
            .rfind(|v| v["type"] == "update_node_data")
            .unwrap();
        let d: models::UpdateNodeDataDelta = serde_json::from_value(last).unwrap();
        let outs = d.after.outputs.unwrap();
        let names: Vec<&str> = outs.iter().filter_map(|p| p.name.as_deref()).collect();
        assert!(names.contains(&"score"), "original survives: {names:?}");
        assert!(
            names.contains(&"extra"),
            "first staged edit's port must NOT be dropped: {names:?}"
        );
        assert!(names.contains(&"extra2"), "second edit's port: {names:?}");
        // The surviving original keeps its pulled id (edges stay intact).
        assert!(outs.iter().any(|p| p.id == score));
    }

    #[test]
    fn update_node_retype_missing_port_fails_loud() {
        let (mut cs, _r, _s) = rater_changeset();
        let err = cs
            .update_node(
                "Api.Rater",
                &NodeEdit {
                    retype_out: vec![port("nope", "X")],
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("no out port 'nope' to retype"),
            "{err}"
        );
    }

    #[test]
    fn update_node_rename_to_an_invalid_slug_fails_loud() {
        let (mut cs, _r, _s) = rater_changeset();
        let err = cs
            .update_node(
                "Api.Rater",
                &NodeEdit {
                    name: Some("has space".to_string()),
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert!(matches!(err, CliError::InvalidArgument(_)), "{err:?}");
    }

    #[test]
    fn update_node_empty_name_fails_loud() {
        // A blank rename is garbage (no "clear name" semantics) — surface it.
        let (mut cs, _r, _s) = rater_changeset();
        let err = cs
            .update_node(
                "Api.Rater",
                &NodeEdit {
                    name: Some("   ".to_string()),
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert!(matches!(err, CliError::InvalidArgument(_)), "{err:?}");
    }

    #[test]
    fn update_node_in_side_add_records_a_wireable_alias() {
        // The Side::In alias-record path is symmetric to Out but exercised here.
        let (mut cs, _raw, _score) = rater_changeset();
        cs.update_node(
            "Api.Rater",
            &NodeEdit {
                add_in: vec![port("extra", "Blob")],
                ..Default::default()
            },
        )
        .unwrap();
        assert!(cs
            .into_stage()
            .aliases
            .contains_key("port:Api.Rater:in:extra"));
    }

    #[test]
    fn update_node_clear_constraints_sets_an_empty_list() {
        let mut cs = empty();
        cs.add_node(&behavior("Rater", None)).unwrap();
        cs.update_node(
            "Rater",
            &NodeEdit {
                constraints: Some(vec![]),
                ..Default::default()
            },
        )
        .unwrap();
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
        let err = cs.update_node("Rater", &NodeEdit::default()).unwrap_err();
        assert!(matches!(err, CliError::InvalidArgument(_)), "got {err:?}");
        assert!(err.to_string().contains("nothing to set"), "{err}");
    }

    #[test]
    fn update_node_unknown_path_fails_loud() {
        let mut cs = empty();
        let err = cs.update_node("Ghost", &desc_edit("x")).unwrap_err();
        assert!(err.to_string().contains("Ghost"), "{err}");
    }

    #[test]
    fn update_node_delta_is_commit_ready_and_lowers_after_adds_before_deletes() {
        use models::V1DeltasBodyDeltasInner as Inner;
        let mut cs = empty();
        cs.add_node(&behavior("Rater", None)).unwrap();
        cs.add_node(&behavior("Gone", None)).unwrap();
        cs.update_node("Rater", &desc_edit("edited")).unwrap();
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
        cs.update_node(
            "Api.Rater",
            &NodeEdit {
                description: Some("edited".to_string()),
                constraints: Some(vec!["c".to_string()]),
                ..Default::default()
            },
        )
        .unwrap();
        let summary = summarize(&cs.into_stage(), Some(&index)).unwrap();
        assert_eq!(summary.updates, 1);
        let found = summary.ops.iter().find_map(|op| match op {
            OpSummary::UpdateNode {
                path,
                description,
                constraints,
                ..
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
        let err = cs.update_node("Rater", &desc_edit("x")).unwrap_err();
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
