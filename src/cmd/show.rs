//! `show [path]` — a read-only view of a branch's committed graph: its nodes
//! (as an indented tree by dotted path), each node's ports, and its edges.
//!
//! This verb is strictly read-only. It reaches only the read endpoints
//! (`list_projects`, `list_branches`, `fetch_branch_graph`) and NEVER creates a
//! branch or applies a delta — the render is a pure function of the fetched
//! [`models::GraphResponse`], so no mutation call is even reachable from it.
//!
//! The graph endpoint returns a placeholder `position` per node; it is omitted
//! from this view entirely (it is not authoritative layout).

use std::collections::HashMap;

use hydrate_wire::models::{self, BranchMeta, GraphResponse, WireNode, WirePort};
use serde::Serialize;
use uuid::Uuid;

use super::context::{choose_selection, current_binding, env_project, resolve_project};
use crate::client::Client;
use crate::config::Config;
use crate::error::CliError;
use crate::output::OutputMode;

pub fn run(
    args: crate::cli::ShowArgs,
    project_flag: Option<String>,
    mode: OutputMode,
) -> Result<(), CliError> {
    let config = Config::load()?;
    let client = Client::new(&config)?;

    // Resolve the project (flag > env > binding > single-active rule).
    let binding = current_binding()?;
    let binding_project = binding.as_ref().map(|b| b.project_id.to_string());
    let selection = choose_selection(
        project_flag.as_deref(),
        env_project()?,
        binding_project.as_deref(),
    );
    let project = resolve_project(selection, client.list_projects()?.projects)?;

    // Pick the branch: --branch name, else the binding's branch (only when it
    // belongs to this project), else the project's main branch.
    let bound = binding
        .as_ref()
        .filter(|b| b.project_id == project.id)
        .map(|b| b.branch_id);
    let branches = client.list_branches(project.id)?.branches;
    let (branch_id, branch_name) = pick_branch(&branches, args.branch.as_deref(), bound)?;

    // The one and only network read of graph content — and it is a GET.
    let graph = client.fetch_branch_graph(branch_id)?;
    println!(
        "{}",
        render(
            &graph,
            &project.name,
            &branch_name,
            args.path.as_deref(),
            mode
        )?
    );
    Ok(())
}

/// Choose which branch to show. `requested` (a `--branch` name) wins; else the
/// `bound` branch when it is still present on the server; else the project's
/// main branch. Fails loud when a requested name is unknown or the project has
/// no main branch to fall back to.
fn pick_branch(
    branches: &[BranchMeta],
    requested: Option<&str>,
    bound: Option<Uuid>,
) -> Result<(Uuid, String), CliError> {
    if let Some(name) = requested {
        return branches
            .iter()
            .find(|b| b.name == name)
            .map(|b| (b.id, b.name.clone()))
            .ok_or_else(|| {
                CliError::InvalidArgument(format!(
                    "no branch named '{name}' in this project; run `hydrate branches` to list them"
                ))
            });
    }
    if let Some(id) = bound {
        if let Some(b) = branches.iter().find(|b| b.id == id) {
            return Ok((b.id, b.name.clone()));
        }
    }
    branches
        .iter()
        .find(|b| b.is_main)
        .map(|b| (b.id, b.name.clone()))
        .ok_or_else(|| {
            CliError::Other(
                "this project has no main branch to show; pass --branch <name>".to_string(),
            )
        })
}

/// A port in the view: its name (unnamed ports are rendered as `<unnamed>`) and
/// type. No id, no position — this is a human/machine inspection surface.
#[derive(Debug, Clone, PartialEq, Serialize)]
struct ShowPort {
    name: Option<String>,
    r#type: Option<String>,
}

impl ShowPort {
    fn label(&self) -> String {
        let name = self.name.as_deref().unwrap_or("<unnamed>");
        match &self.r#type {
            Some(t) => format!("{name}:{t}"),
            None => name.to_string(),
        }
    }
}

/// A node in the view: its dotted path, kind, and ports (no position).
#[derive(Debug, Clone, PartialEq, Serialize)]
struct ShowNode {
    path: String,
    kind: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    inputs: Vec<ShowPort>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    outputs: Vec<ShowPort>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    config: Vec<ShowPort>,
}

/// An edge in the view: source and target as dotted `node.port` paths.
#[derive(Debug, Clone, PartialEq, Serialize)]
struct ShowEdge {
    from: String,
    to: String,
}

/// The whole rendered view (before mode selection).
struct View {
    nodes: Vec<ShowNode>,
    edges: Vec<ShowEdge>,
    /// Edges with exactly one endpoint inside the filtered subtree — hidden from
    /// `edges` (the other end is out of view), but surfaced as a loud count so a
    /// filtered inspection never drops a wire without a word. Always 0 unfiltered.
    cross_boundary: usize,
}

/// Render the branch graph in `mode`, optionally filtered to one node's subtree.
/// A pure function of the fetched graph — the read/mutation boundary is that this
/// takes a [`GraphResponse`] and returns a `String`, so `show` cannot mutate.
fn render(
    graph: &GraphResponse,
    project_name: &str,
    branch_name: &str,
    filter: Option<&str>,
    mode: OutputMode,
) -> Result<String, CliError> {
    let view = build_view(graph, filter)?;
    Ok(match mode {
        OutputMode::Json => serde_json::json!({
            "project": project_name,
            "branch": branch_name,
            "nodes": view.nodes,
            "edges": view.edges,
            "cross_boundary_edges": view.cross_boundary,
        })
        .to_string(),
        OutputMode::Human => human(&view, project_name, branch_name),
    })
}

/// Build the display view from the fetched graph: reconstruct each node's dotted
/// path, project its ports, translate edge handles back to dotted port paths,
/// and (when `filter` is set) narrow to that node's subtree.
fn build_view(graph: &GraphResponse, filter: Option<&str>) -> Result<View, CliError> {
    let by_id: HashMap<Uuid, &WireNode> = graph.nodes.iter().map(|n| (n.id, n)).collect();

    // node id -> dotted path.
    let mut paths: HashMap<Uuid, String> = HashMap::new();
    for node in &graph.nodes {
        paths.insert(node.id, node_path(node, &by_id)?);
    }

    // port id -> (owning node's dotted path, port name).
    let mut port_owner: HashMap<Uuid, (String, Option<String>)> = HashMap::new();
    for node in &graph.nodes {
        let path = &paths[&node.id];
        for side in [
            node.data.inputs.as_deref(),
            node.data.outputs.as_deref(),
            node.data.config.as_deref(),
        ] {
            for p in side.unwrap_or_default() {
                port_owner.insert(p.id, (path.clone(), p.name.clone()));
            }
        }
    }

    // Which node paths are in scope (the filter subtree, or all).
    let in_scope = |path: &str| match filter {
        Some(f) => path == f || path.starts_with(&format!("{f}.")),
        None => true,
    };

    let mut nodes: Vec<ShowNode> = graph
        .nodes
        .iter()
        .filter(|n| in_scope(&paths[&n.id]))
        .map(|n| ShowNode {
            path: paths[&n.id].clone(),
            kind: kind_str(n.kind).to_string(),
            inputs: show_ports(n.data.inputs.as_deref()),
            outputs: show_ports(n.data.outputs.as_deref()),
            config: show_ports(n.data.config.as_deref()),
        })
        .collect();
    nodes.sort_by(|a, b| a.path.cmp(&b.path));

    if let Some(f) = filter {
        if nodes.is_empty() {
            return Err(CliError::InvalidArgument(format!(
                "no node '{f}' on this branch; run `hydrate show` to see the whole graph"
            )));
        }
    }

    // Edges: translate each handle to a dotted port path. A handle that names no
    // known port is corruption in the server's response — surface it loudly
    // rather than drop the edge (which would hide a real connection). Keep only
    // edges whose BOTH endpoints are in scope, so a filtered view is
    // self-contained; but COUNT the ones that cross out so the caller can report
    // them (an inspection tool must not hide a wire silently).
    let mut edges = Vec::new();
    let mut cross_boundary = 0usize;
    for edge in &graph.edges {
        let (Some(src), Some(tgt)) = (edge.source_handle, edge.target_handle) else {
            return Err(CliError::State(
                "the branch graph has an edge missing a port handle".to_string(),
            ));
        };
        let (from, from_node) = port_path(&port_owner, src)?;
        let (to, to_node) = port_path(&port_owner, tgt)?;
        match (in_scope(&from_node), in_scope(&to_node)) {
            (true, true) => edges.push(ShowEdge { from, to }),
            // Exactly one endpoint in the subtree: it crosses the boundary.
            (true, false) | (false, true) => cross_boundary += 1,
            (false, false) => {}
        }
    }
    edges.sort_by(|a, b| (a.from.as_str(), a.to.as_str()).cmp(&(b.from.as_str(), b.to.as_str())));

    Ok(View {
        nodes,
        edges,
        cross_boundary,
    })
}

/// Translate a port handle to its dotted `node.port` path plus the owning node's
/// path (for scope checks). Fails loud when the handle is unknown.
fn port_path(
    owners: &HashMap<Uuid, (String, Option<String>)>,
    handle: Uuid,
) -> Result<(String, String), CliError> {
    let (node_path, name) = owners.get(&handle).ok_or_else(|| {
        CliError::State(format!(
            "the branch graph has an edge to an unknown port handle {handle}"
        ))
    })?;
    let port = name.as_deref().unwrap_or("<unnamed>");
    Ok((format!("{node_path}.{port}"), node_path.clone()))
}

fn show_ports(ports: Option<&[WirePort]>) -> Vec<ShowPort> {
    ports
        .unwrap_or_default()
        .iter()
        .map(|p| ShowPort {
            name: p.name.clone(),
            r#type: p.r#type.clone(),
        })
        .collect()
}

/// Render the human, indented-tree form.
fn human(view: &View, project_name: &str, branch_name: &str) -> String {
    let mut out = format!("Project '{project_name}' branch '{branch_name}':");
    if view.nodes.is_empty() {
        out.push_str("\n  (no nodes)");
    }
    for node in &view.nodes {
        let depth = node.path.matches('.').count();
        let indent = "  ".repeat(depth + 1);
        // A dotted path always has at least one segment (names are non-empty), so
        // `rsplit` yields the leaf; there is no fallback case to handle.
        let leaf = node
            .path
            .rsplit('.')
            .next()
            .expect("a node path always has at least one segment");
        out.push_str(&format!("\n{indent}{leaf}  [{}]", node.kind));
        let ports = "  ".repeat(depth + 2);
        if !node.inputs.is_empty() {
            out.push_str(&format!("\n{ports}in:  {}", join_ports(&node.inputs)));
        }
        if !node.outputs.is_empty() {
            out.push_str(&format!("\n{ports}out: {}", join_ports(&node.outputs)));
        }
        if !node.config.is_empty() {
            out.push_str(&format!("\n{ports}config: {}", join_ports(&node.config)));
        }
    }
    if !view.edges.is_empty() {
        out.push_str("\nEdges:");
        for e in &view.edges {
            out.push_str(&format!("\n  {} -> {}", e.from, e.to));
        }
    }
    if view.cross_boundary > 0 {
        let plural = if view.cross_boundary == 1 { "" } else { "s" };
        out.push_str(&format!(
            "\n{} edge{plural} cross out of this subtree — run `hydrate show` for the full graph",
            view.cross_boundary
        ));
    }
    out
}

fn join_ports(ports: &[ShowPort]) -> String {
    ports
        .iter()
        .map(ShowPort::label)
        .collect::<Vec<_>>()
        .join(", ")
}

/// A wire node kind as its stable lowercase token.
fn kind_str(kind: models::wire_node::Kind) -> &'static str {
    match kind {
        models::wire_node::Kind::Behavior => "behavior",
        models::wire_node::Kind::Boundary => "boundary",
        models::wire_node::Kind::State => "state",
        models::wire_node::Kind::Io => "io",
    }
}

/// Reconstruct a node's dotted path by walking the `parent_id` chain to the root.
/// A missing parent or a `parent_id` cycle is corruption in the server response —
/// surfaced loudly, never silently dropping a node.
fn node_path(node: &WireNode, by_id: &HashMap<Uuid, &WireNode>) -> Result<String, CliError> {
    let mut parts = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut current = node;
    loop {
        if !seen.insert(current.id) {
            return Err(CliError::State(
                "the branch graph has a parent_id cycle".to_string(),
            ));
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use hydrate_wire::models::{BranchRef, Position, WireEdge, WireNodeData};

    fn branch(name: &str, id: u128, is_main: bool) -> BranchMeta {
        BranchMeta {
            base_main_version: 0,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            id: Uuid::from_u128(id),
            is_main,
            last_active_at: "2026-01-01T00:00:00Z".to_string(),
            merged_at: None,
            name: name.to_string(),
            owner_id: None,
            project_id: Uuid::from_u128(0xFEED),
            status: "active".to_string(),
            version: 1,
        }
    }

    fn port(id: u128, name: &str, ty: &str) -> WirePort {
        WirePort {
            description: None,
            id: Uuid::from_u128(id),
            name: Some(name.to_string()),
            r#type: Some(ty.to_string()),
        }
    }

    fn node(
        id: u128,
        name: &str,
        kind: models::wire_node::Kind,
        parent: Option<u128>,
        inputs: Vec<WirePort>,
        outputs: Vec<WirePort>,
    ) -> WireNode {
        let mut data = WireNodeData::new(
            String::new(),
            false,
            false,
            name.to_string(),
            "draft".to_string(),
        );
        data.inputs = Some(inputs);
        data.outputs = Some(outputs);
        WireNode {
            data: Box::new(data),
            id: Uuid::from_u128(id),
            kind,
            parent_id: parent.map(Uuid::from_u128),
            position: Box::new(Position::new(0.0, 0.0)),
        }
    }

    /// Api (boundary) { Maker (behavior, out dog:HotDog), Rater (behavior, in
    /// raw:HotDog, out score:Score) }, edge Maker.dog -> Rater.raw.
    fn sample_graph() -> GraphResponse {
        use models::wire_node::Kind;
        let maker_out = port(0xD0, "dog", "HotDog");
        let rater_in = port(0xF0, "raw", "HotDog");
        let rater_out = port(0xF1, "score", "Score");
        GraphResponse {
            branch: Box::new(BranchRef::new(Uuid::from_u128(2), 1)),
            project_id: Uuid::from_u128(0xFEED),
            version: "1".to_string(),
            nodes: vec![
                node(0x10, "Api", Kind::Boundary, None, vec![], vec![]),
                node(
                    0x11,
                    "Maker",
                    Kind::Behavior,
                    Some(0x10),
                    vec![],
                    vec![maker_out.clone()],
                ),
                node(
                    0x12,
                    "Rater",
                    Kind::Behavior,
                    Some(0x10),
                    vec![rater_in.clone()],
                    vec![rater_out.clone()],
                ),
            ],
            edges: vec![WireEdge {
                id: Uuid::from_u128(0xED),
                source: Uuid::from_u128(0x11),
                source_handle: Some(maker_out.id),
                target: Uuid::from_u128(0x12),
                target_handle: Some(rater_in.id),
            }],
        }
    }

    #[test]
    fn branch_flag_overrides_binding_overrides_main() {
        let branches = [
            branch("main", 1, true),
            branch("feature", 2, false),
            branch("other", 3, false),
        ];
        // --branch wins.
        let (id, name) = pick_branch(&branches, Some("feature"), Some(Uuid::from_u128(3))).unwrap();
        assert_eq!(name, "feature");
        assert_eq!(id, Uuid::from_u128(2));
        // No flag: the bound branch is used.
        let (_, name) = pick_branch(&branches, None, Some(Uuid::from_u128(3))).unwrap();
        assert_eq!(name, "other");
        // No flag, no (present) binding: main.
        let (_, name) = pick_branch(&branches, None, None).unwrap();
        assert_eq!(name, "main");
        // A bound branch that no longer exists falls through to main.
        let (_, name) = pick_branch(&branches, None, Some(Uuid::from_u128(0xDEAD))).unwrap();
        assert_eq!(name, "main");
    }

    #[test]
    fn unknown_branch_flag_fails_loud() {
        let branches = [branch("main", 1, true)];
        let err = pick_branch(&branches, Some("ghost"), None).unwrap_err();
        assert!(matches!(err, CliError::InvalidArgument(_)), "got {err:?}");
    }

    #[test]
    fn no_main_to_fall_back_to_fails_loud() {
        let branches = [branch("feature", 2, false)];
        let err = pick_branch(&branches, None, None).unwrap_err();
        assert!(matches!(err, CliError::Other(_)), "got {err:?}");
    }

    #[test]
    fn render_tree_human_and_json_parity() {
        let g = sample_graph();
        let human = render(&g, "proj", "main", None, OutputMode::Human).unwrap();
        // The tree carries every node path (as nested leaves), kinds, ports, edge.
        assert!(human.contains("Api  [boundary]"), "{human}");
        assert!(human.contains("Maker  [behavior]"), "{human}");
        assert!(human.contains("Rater  [behavior]"), "{human}");
        assert!(human.contains("dog:HotDog"), "{human}");
        assert!(human.contains("raw:HotDog"), "{human}");
        assert!(human.contains("score:Score"), "{human}");
        assert!(human.contains("Api.Maker.dog -> Api.Rater.raw"), "{human}");
        // Rater is nested deeper than Api (indentation grows with depth).
        let api_indent = human.lines().find(|l| l.contains("Api  [")).unwrap();
        let rater_indent = human.lines().find(|l| l.contains("Rater  [")).unwrap();
        let lead = |s: &str| s.len() - s.trim_start().len();
        assert!(lead(rater_indent) > lead(api_indent), "{human}");

        // JSON carries the same information.
        let json = render(&g, "proj", "main", None, OutputMode::Json).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["project"], "proj");
        assert_eq!(v["branch"], "main");
        let nodes = v["nodes"].as_array().unwrap();
        assert_eq!(nodes.len(), 3);
        let rater = nodes.iter().find(|n| n["path"] == "Api.Rater").unwrap();
        assert_eq!(rater["kind"], "behavior");
        assert_eq!(rater["inputs"][0]["name"], "raw");
        assert_eq!(rater["inputs"][0]["type"], "HotDog");
        assert_eq!(rater["outputs"][0]["name"], "score");
        let edges = v["edges"].as_array().unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0]["from"], "Api.Maker.dog");
        assert_eq!(edges[0]["to"], "Api.Rater.raw");
    }

    #[test]
    fn position_field_is_omitted() {
        // The graph endpoint's placeholder position must never surface in show.
        let g = sample_graph();
        let json = render(&g, "proj", "main", None, OutputMode::Json).unwrap();
        assert!(!json.contains("position"), "{json}");
        let human = render(&g, "proj", "main", None, OutputMode::Human).unwrap();
        assert!(!human.to_lowercase().contains("position"), "{human}");
    }

    #[test]
    fn path_filter_narrows_to_subtree() {
        let g = sample_graph();
        let json = render(&g, "proj", "main", Some("Api.Rater"), OutputMode::Json).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let nodes = v["nodes"].as_array().unwrap();
        // Only Rater is in the subtree; Maker and Api are excluded.
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0]["path"], "Api.Rater");
        // The edge crosses out of the subtree (Maker is outside), so it's not
        // listed among the shown edges.
        assert!(v["edges"].as_array().unwrap().is_empty(), "{json}");
    }

    #[test]
    fn subtree_filter_reports_edges_that_cross_out() {
        // Filtering to Api.Rater hides the Maker.dog -> Rater.raw edge (Maker is
        // out of scope). That must be counted and reported, never silently dropped.
        let g = sample_graph();
        // JSON: an explicit cross-boundary count.
        let json = render(&g, "proj", "main", Some("Api.Rater"), OutputMode::Json).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["cross_boundary_edges"], 1, "{json}");
        // Human: a loud footnote naming the escape hatch.
        let human = render(&g, "proj", "main", Some("Api.Rater"), OutputMode::Human).unwrap();
        assert!(human.contains("1 edge cross"), "{human}");
        assert!(human.contains("hydrate show"), "{human}");
        // The whole-graph view has nothing crossing out.
        let full = render(&g, "proj", "main", None, OutputMode::Json).unwrap();
        let fv: serde_json::Value = serde_json::from_str(&full).unwrap();
        assert_eq!(fv["cross_boundary_edges"], 0, "{full}");
        let full_human = render(&g, "proj", "main", None, OutputMode::Human).unwrap();
        assert!(!full_human.contains("cross out"), "{full_human}");
    }

    #[test]
    fn unknown_path_filter_fails_loud() {
        let g = sample_graph();
        let err = render(&g, "proj", "main", Some("Nope"), OutputMode::Json).unwrap_err();
        assert!(matches!(err, CliError::InvalidArgument(_)), "got {err:?}");
    }

    #[test]
    fn edge_to_unknown_handle_fails_loud() {
        // A dangling edge handle is corruption, not a silently-dropped edge.
        let mut g = sample_graph();
        g.edges[0].source_handle = Some(Uuid::from_u128(0xBEEF));
        let err = render(&g, "proj", "main", None, OutputMode::Json).unwrap_err();
        assert!(matches!(err, CliError::State(_)), "got {err:?}");
    }

    #[test]
    fn edge_missing_a_port_handle_fails_loud() {
        // A null handle (no port at all) is corruption too — surface it rather
        // than skip the edge and under-report the graph's connections.
        let mut g = sample_graph();
        g.edges[0].source_handle = None;
        let err = render(&g, "proj", "main", None, OutputMode::Json).unwrap_err();
        assert!(matches!(err, CliError::State(_)), "got {err:?}");
        assert!(err.to_string().contains("missing a port handle"), "{err}");
    }

    #[test]
    fn render_core_is_a_pure_transform_of_the_fetched_graph() {
        // The read/mutation boundary: the render core takes a fetched
        // GraphResponse and returns a String — no client, no branch id, no delta,
        // so a mutation call is not even reachable from it. Prove it is a faithful,
        // total transform of ONLY that input: every graph node appears, and
        // nothing not derivable from the graph leaks in.
        let g = sample_graph();
        let json = render(&g, "proj", "main", None, OutputMode::Json).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        // Exactly the graph's nodes are rendered (a pure projection of the input).
        assert_eq!(v["nodes"].as_array().unwrap().len(), g.nodes.len());
        for node in &g.nodes {
            let name = &node.data.name;
            assert!(
                v["nodes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|n| n["path"].as_str().unwrap().ends_with(name.as_str())),
                "graph node {name:?} missing from the rendered view: {json}"
            );
        }
        // The projected identifiers stay OUT: no node/port UUIDs, no branch id.
        assert!(!json.contains(&g.branch.id.to_string()), "leaked branch id");
        assert!(
            !json.contains(&g.nodes[0].id.to_string()),
            "leaked node uuid"
        );
    }
}
