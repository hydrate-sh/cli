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

use hydrate_wire::models::{BranchMeta, GraphResponse};
use uuid::Uuid;

use super::context::{choose_selection, current_binding, env_project, resolve_project};
use super::scoped;
use super::view::{self, EdgeView, NodeView};
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

    // --depth asks for a SCOPED read: fetch only the slice, not the branch.
    // Gated on `bound == Some(branch_id)` because the local index records no
    // branch identity — it is implicitly the BOUND branch's, so resolving a
    // path through it and then reading a different branch could return a
    // different node under the name the user typed.
    if let (Some(depth), Some(path)) = (args.depth, args.path.as_deref()) {
        let on_bound_branch = bound == Some(branch_id);
        match scoped::plan(scoped::base_dir().as_deref(), path, on_bound_branch)? {
            scoped::Plan::Scoped { id: node_id, .. } => {
                let subtree = client.fetch_branch_subtree(branch_id, node_id, depth)?;
                println!(
                    "{}",
                    render_subtree(&subtree, &project.name, &branch_name, path, mode)?
                );
                return Ok(());
            }
            scoped::Plan::WholeGraph(why) => {
                eprintln!("{}", scoped::fallback_note(path, why));
            }
        }
    }

    // The one and only network read of graph content — and it is a GET.
    let graph = client.fetch_branch_graph(branch_id)?;
    println!(
        "{}",
        render(
            &graph,
            &project.name,
            &branch_name,
            args.path.as_deref(),
            args.depth,
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

/// The whole rendered view (before mode selection). Nodes and edges are the
/// shared, complete [`view`] projections — every node is rendered whole.
struct View {
    nodes: Vec<NodeView>,
    edges: Vec<EdgeView>,
    /// Edges with exactly one endpoint inside the filtered subtree — hidden from
    /// `edges` (the other end is out of view), but surfaced as a loud count so a
    /// filtered inspection never drops a wire without a word. Always 0 unfiltered.
    cross_boundary: usize,
    /// Whether `--depth` cut nodes off the bottom of this view. Reported for the
    /// same reason the scoped read reports it: a bounded slice is a PARTIAL
    /// answer, and a reader who cannot tell partial from complete will treat a
    /// truncated graph as the whole graph.
    truncated: bool,
}

/// Render the branch graph in `mode`, optionally filtered to one node's subtree.
/// A pure function of the fetched graph — the read/mutation boundary is that this
/// takes a [`GraphResponse`] and returns a `String`, so `show` cannot mutate.
fn render(
    graph: &GraphResponse,
    project_name: &str,
    branch_name: &str,
    filter: Option<&str>,
    // The `--depth` the user asked for, when they asked for one. The fallback
    // path must honour it: dropping it here would silently return the whole
    // subtree to someone who explicitly bounded their read — the exact
    // context blow-up `--depth` exists to prevent, with no signal.
    depth: Option<u32>,
    mode: OutputMode,
) -> Result<String, CliError> {
    let view = build_view(graph, filter, depth, None, None)?;
    Ok(render_view(&view, project_name, branch_name, mode))
}

/// Turn a built [`View`] into output. Split from [`render`] so the scoped read
/// can reuse it with server-supplied paths instead of local reconstruction.
fn render_view(view: &View, project_name: &str, branch_name: &str, mode: OutputMode) -> String {
    match mode {
        OutputMode::Json => serde_json::json!({
            "project": project_name,
            "branch": branch_name,
            "nodes": view.nodes,
            "edges": view.edges,
            "cross_boundary_edges": view.cross_boundary,
            "truncated": view.truncated,
        })
        .to_string(),
        OutputMode::Human => human(view, project_name, branch_name),
    }
}

/// Build the display view from the fetched graph: reconstruct each node's dotted
/// path, project its ports, translate edge handles back to dotted port paths,
/// and (when `filter` is set) narrow to that node's subtree.
fn build_view(
    graph: &GraphResponse,
    filter: Option<&str>,
    // Levels below `filter` to keep, mirroring the server's `depth`. `None`
    // keeps the whole subtree. Only meaningful with a filter, which is why
    // `--depth` requires a path.
    depth: Option<u32>,
    // Server-rendered paths, when the caller has them. A SCOPED read cannot
    // reconstruct paths locally: the slice does not contain the ancestors a
    // dotted path is built from, so `node_paths` fails with "references a
    // missing parent". The server holds the whole branch and is the only party
    // that can answer.
    server_paths: Option<&HashMap<Uuid, String>>,
    // Reasons for nodes the server could not address. A slice legitimately
    // contains them (an unnamed node is legal while designing), so they must
    // RENDER — skipping drops their ports from the port table and the next
    // edge lookup then fails blaming the server.
    unaddressable: Option<&HashMap<Uuid, String>>,
) -> Result<View, CliError> {
    // node id -> dotted path (local reconstruction for the whole-graph read).
    let mut paths = match server_paths {
        Some(p) => p.clone(),
        // Same leniency as the scoped path: report an unaddressable node
        // rather than failing the whole read on it.
        None => {
            let (mut p, un) = view::node_paths_reporting(&graph.nodes)?;
            for node in &graph.nodes {
                p.entry(node.id).or_insert_with(|| {
                    scoped::unaddressable_label(
                        un.get(&node.id.to_string())
                            .map(String::as_str)
                            .unwrap_or("unknown"),
                    )
                });
            }
            p
        }
    };
    // Fill in a label for every node the server could not path, so the map is
    // total from here down and nothing has to guess whether indexing is safe.
    if let Some(un) = unaddressable {
        for node in &graph.nodes {
            paths.entry(node.id).or_insert_with(|| {
                scoped::unaddressable_label(
                    un.get(&node.id).map(String::as_str).unwrap_or("unknown"),
                )
            });
        }
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

    // Which node paths are in scope: the filter subtree, bounded by `depth`
    // levels below it when one was asked for. Counting segments is the same
    // rule the server applies, so a fallback answers with the same node set a
    // scoped read would have — the note tells the user the FETCH was unscoped,
    // and that must remain the only difference.
    let in_scope = |path: &str| match filter {
        Some(f) => {
            if path == f {
                return true;
            }
            let Some(rest) = path.strip_prefix(&format!("{f}.")) else {
                return false;
            };
            match depth {
                Some(d) => rest.matches('.').count() < d as usize,
                None => true,
            }
        }
        None => true,
    };

    let mut nodes: Vec<NodeView> = graph
        .nodes
        .iter()
        .filter(|n| in_scope(&paths[&n.id]))
        .map(|n| view::node_view(n, paths[&n.id].clone()))
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
            (true, true) => edges.push(EdgeView { from, to }),
            // Exactly one endpoint in the subtree: it crosses the boundary.
            (true, false) | (false, true) => cross_boundary += 1,
            (false, false) => {}
        }
    }
    edges.sort_by(|a, b| (a.from.as_str(), a.to.as_str()).cmp(&(b.from.as_str(), b.to.as_str())));

    // A node under the filter that `depth` excluded means there is more below.
    let truncated = match (filter, depth) {
        (Some(f), Some(_)) => {
            let prefix = format!("{f}.");
            graph
                .nodes
                .iter()
                .any(|n| paths[&n.id].starts_with(&prefix) && !in_scope(&paths[&n.id]))
        }
        _ => false,
    };

    Ok(View {
        nodes,
        edges,
        cross_boundary,
        truncated,
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

/// Render the human, indented-tree form. Each node renders whole (its full
/// [`NodeView`] block), indented by its depth in the tree.
fn human(view: &View, project_name: &str, branch_name: &str) -> String {
    let mut out = format!("Project '{project_name}' branch '{branch_name}':");
    if view.nodes.is_empty() {
        out.push_str("\n  (no nodes)");
    }
    for node in &view.nodes {
        let depth = node.path.matches('.').count();
        out.push('\n');
        out.push_str(&node.human(depth));
    }
    if !view.edges.is_empty() {
        out.push_str("\nEdges:");
        for e in &view.edges {
            out.push_str(&format!("\n  {} -> {}", e.from, e.to));
        }
    }
    if view.cross_boundary > 0 {
        let (plural, verb) = if view.cross_boundary == 1 {
            ("", "crosses")
        } else {
            ("s", "cross")
        };
        out.push_str(&format!(
            "\n{} edge{plural} {verb} out of this subtree — run `hydrate show` for the full graph",
            view.cross_boundary
        ));
    }
    if view.truncated {
        out.push_str(
            "\n(cut at the requested depth — there are more nodes below; raise --depth to see them)",
        );
    }
    out
}

/// Re-key a server map by `Uuid`, failing loud on a key that isn't one.
///
/// Dropping an unparseable key would silently turn "the server addressed this
/// node" into "this node has no path" — the same silent-drop the port resolver
/// two functions away explicitly refuses.
fn uuid_keyed(
    map: &std::collections::HashMap<String, String>,
    field: &str,
) -> Result<HashMap<Uuid, String>, CliError> {
    map.iter()
        .map(|(k, v)| {
            Uuid::parse_str(k).map(|id| (id, v.clone())).map_err(|_| {
                CliError::State(format!(
                    "the server sent `{field}` keyed by a non-uuid '{k}'"
                ))
            })
        })
        .collect()
}

/// Render a scoped subtree through the SAME renderer the whole-graph view uses,
/// so the two are indistinguishable in shape to a reader or a parser.
///
/// The subtree is repackaged as a `GraphResponse` carrying exactly the node set
/// the server returned. It is NOT re-filtered locally: the server already
/// scoped it, and filtering by path prefix would drop precisely the nodes it
/// could not give a path — the ones the user needs to see in order to fix them.
/// Only what the scoped read uniquely knows is added on top: which slice this
/// is, how deep, whether it was CUT, and what could not be addressed.
fn render_subtree(
    subtree: &hydrate_wire::models::SubtreeResponse,
    project_name: &str,
    branch_name: &str,
    path: &str,
    mode: OutputMode,
) -> Result<String, CliError> {
    let mut nodes = vec![(*subtree.root).clone()];
    nodes.extend(subtree.nodes.iter().cloned());
    // NOT merged with cross_boundary_edges: by definition those have one
    // endpoint outside the slice, so the port resolver cannot place them and
    // errors blaming the server. The server already classified them; take its
    // count rather than re-deriving one we cannot compute here.
    let edges = subtree.edges.clone();

    let graph = GraphResponse {
        branch: subtree.branch.clone(),
        edges,
        nodes,
        project_id: subtree.project_id,
        version: subtree.version.clone(),
    };
    // Server-rendered paths — the whole reason the scoped read returns them.
    // Reconstructing here fails: the slice has no ancestors to walk.
    let server_paths = uuid_keyed(&subtree.paths, "paths")?;
    let unaddressable = uuid_keyed(&subtree.unaddressable, "unaddressable")?;
    // No local filter: the SERVER already scoped this response to the slice,
    // and filtering by path prefix would drop exactly the nodes it could not
    // give a path — the ones the user most needs to see in order to fix them.
    let mut view = build_view(
        &graph,
        None,
        None,
        Some(&server_paths),
        Some(&unaddressable),
    )?;
    // The server counted the edges leaving this slice; we cannot.
    view.cross_boundary = subtree.cross_boundary_edges.len();
    let rendered = render_view(&view, project_name, branch_name, mode);

    match mode {
        OutputMode::Json => {
            // Augment rather than re-derive, so the scoped payload stays a
            // superset of the familiar one.
            let mut v: serde_json::Value = serde_json::from_str(&rendered)
                .map_err(|e| CliError::Other(format!("rendering the subtree: {e}")))?;
            // Says a slice was read, and which one. Without this a scoped
            // read and a whole-graph fetch are indistinguishable on stdout —
            // the one thing the flag exists to control.
            v["scoped"] = serde_json::json!(true);
            v["root"] = serde_json::json!(path);
            v["depth"] = serde_json::json!(subtree.depth);
            v["truncated"] = serde_json::json!(subtree.truncated);
            // Same information `walk` reports: which nodes here cannot be
            // addressed, and why. Labels, not ids — this CLI does not surface
            // node ids, and an id joins to nothing else in the payload.
            v["unaddressable"] = serde_json::json!(unaddressable
                .values()
                .map(|reason| serde_json::json!({
                    "label": scoped::unaddressable_label(reason),
                    "reason": scoped::sanitize(reason),
                }))
                .collect::<Vec<_>>());
            Ok(v.to_string())
        }
        OutputMode::Human => {
            let mut out = format!(
                "slice of '{path}', {} level(s) deep\n{rendered}",
                subtree.depth,
            );
            if !unaddressable.is_empty() {
                out.push_str(&format!(
                    "\n{} node(s) here have no addressable path — name them to \
                     reference them in other commands.\n",
                    unaddressable.len()
                ));
            }
            if subtree.truncated {
                // Reported, never implied: a reader who cannot tell a complete
                // cell from a slice will treat the slice as complete.
                out.push_str(&format!(
                    "\n(cut at depth {} — there are more nodes below; raise --depth to see them)\n",
                    subtree.depth
                ));
            }
            Ok(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use hydrate_wire::models::SubtreeResponse;

    use super::*;
    use hydrate_wire::models::{
        self, BranchRef, Position, WireEdge, WireNode, WireNodeData, WirePort,
    };

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
            external: None,
            contract_name: None,
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
    pub(super) fn sample_graph() -> GraphResponse {
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

    /// Three levels: `Api` > `Api.Inner` > `Api.Inner.Leaf`, plus `Api.Direct`.
    /// `sample_graph` is only two deep, so it cannot tell "depth 1" from "the
    /// whole subtree" — the exact confusion the fallback bug hid behind.
    fn deep_graph() -> GraphResponse {
        use models::wire_node::Kind;
        GraphResponse {
            branch: Box::new(BranchRef::new(Uuid::from_u128(2), 1)),
            project_id: Uuid::from_u128(0xFEED),
            version: "1".to_string(),
            nodes: vec![
                node(0x20, "Api", Kind::Boundary, None, vec![], vec![]),
                node(0x21, "Inner", Kind::Boundary, Some(0x20), vec![], vec![]),
                node(0x22, "Leaf", Kind::Behavior, Some(0x21), vec![], vec![]),
                node(0x23, "Direct", Kind::Behavior, Some(0x20), vec![], vec![]),
            ],
            edges: vec![],
        }
    }

    #[test]
    fn fallback_render_honours_depth() {
        // The whole-graph path is what `show --depth` falls back to when there is
        // no usable index. Dropping the bound there returns the WHOLE subtree to
        // someone who explicitly asked for one level — a silent context blow-up
        // in the one case `--depth` exists to prevent.
        let g = deep_graph();

        let d1 = render(&g, "proj", "main", Some("Api"), Some(1), OutputMode::Json).unwrap();
        let v: serde_json::Value = serde_json::from_str(&d1).unwrap();
        let paths: Vec<&str> = v["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["path"].as_str().unwrap())
            .collect();
        assert_eq!(paths, vec!["Api", "Api.Direct", "Api.Inner"], "{d1}");
        assert_eq!(v["truncated"], true, "{d1}");

        // Depth 2 reaches the leaf and is then complete.
        let d2 = render(&g, "proj", "main", Some("Api"), Some(2), OutputMode::Json).unwrap();
        let v2: serde_json::Value = serde_json::from_str(&d2).unwrap();
        assert_eq!(v2["nodes"].as_array().unwrap().len(), 4, "{d2}");
        assert_eq!(v2["truncated"], false, "{d2}");

        // No depth: the whole subtree, and nothing claims truncation.
        let all = render(&g, "proj", "main", Some("Api"), None, OutputMode::Json).unwrap();
        let va: serde_json::Value = serde_json::from_str(&all).unwrap();
        assert_eq!(va["nodes"].as_array().unwrap().len(), 4, "{all}");
        assert_eq!(va["truncated"], false, "{all}");
    }

    #[test]
    fn truncated_fallback_says_so_in_human_output() {
        // JSON consumers get `truncated`; a human reading the terminal needs the
        // same signal, or a cut graph reads as the whole graph.
        let g = deep_graph();
        let human = render(&g, "proj", "main", Some("Api"), Some(1), OutputMode::Human).unwrap();
        assert!(human.contains("cut at the requested depth"), "{human}");
        assert!(human.contains("--depth"), "{human}");
        assert!(!human.contains("Api.Inner.Leaf"), "{human}");

        let complete = render(&g, "proj", "main", Some("Api"), Some(2), OutputMode::Human).unwrap();
        assert!(!complete.contains("cut at"), "{complete}");
    }

    #[test]
    fn depth_without_a_filter_is_inert() {
        // `--depth` requires a path at the CLI layer; if that ever relaxes, an
        // unfiltered render must not silently start cutting the graph.
        let g = deep_graph();
        let json = render(&g, "proj", "main", None, Some(1), OutputMode::Json).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["nodes"].as_array().unwrap().len(), 4, "{json}");
        assert_eq!(v["truncated"], false, "{json}");
    }

    #[test]
    fn crossing_edge_count_agrees_with_its_verb() {
        // "1 edge cross out" reads as broken English and undermines a line whose
        // whole job is to be believed.
        let g = sample_graph();
        let one = render(
            &g,
            "proj",
            "main",
            Some("Api.Rater"),
            None,
            OutputMode::Human,
        )
        .unwrap();
        assert!(one.contains("1 edge crosses out"), "{one}");
        assert!(!one.contains("1 edge cross out"), "{one}");
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
    fn kind_str_maps_interface() {
        assert_eq!(
            view::kind_str(models::wire_node::Kind::Interface),
            "interface"
        );
    }

    #[test]
    fn show_renders_an_interface_node_in_both_modes() {
        // A graph containing a kind=interface node must render without panicking
        // and surface the kind token in both output modes (additive kind).
        use models::wire_node::Kind;
        let g = GraphResponse {
            branch: Box::new(BranchRef::new(Uuid::from_u128(2), 1)),
            project_id: Uuid::from_u128(0xFEED),
            version: "1".to_string(),
            nodes: vec![node(0x20, "Ports", Kind::Interface, None, vec![], vec![])],
            edges: vec![],
        };

        let human = render(&g, "proj", "main", None, None, OutputMode::Human).unwrap();
        assert!(human.contains("Ports  [interface]"), "{human}");

        let json = render(&g, "proj", "main", None, None, OutputMode::Json).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let n = v["nodes"].as_array().unwrap();
        assert_eq!(n.len(), 1);
        assert_eq!(n[0]["kind"], "interface");
    }

    #[test]
    fn show_tolerates_per_port_external_and_contract_name() {
        // A port may carry the additive `external` + `contract_name` fields —
        // data (not matched by kind), so `show` renders a graph carrying them
        // without error in both modes, surfacing the port by name — accept-and-ignore.
        use models::wire_node::Kind;
        let external_port = WirePort {
            description: None,
            id: Uuid::from_u128(0xE0),
            name: Some("hook".to_string()),
            r#type: Some("Payload".to_string()),
            external: Some(true),
            contract_name: Some(Some("PaymentWebhook".to_string())),
        };
        let g = GraphResponse {
            branch: Box::new(BranchRef::new(Uuid::from_u128(2), 1)),
            project_id: Uuid::from_u128(0xFEED),
            version: "1".to_string(),
            nodes: vec![node(
                0x30,
                "Api",
                Kind::Boundary,
                None,
                vec![external_port],
                vec![],
            )],
            edges: vec![],
        };

        // Both modes render without panicking, and the port is still surfaced by
        // its name/type (the extra fields are accepted, not required to appear).
        let human = render(&g, "proj", "main", None, None, OutputMode::Human).unwrap();
        assert!(human.contains("hook:Payload"), "{human}");

        let json = render(&g, "proj", "main", None, None, OutputMode::Json).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["nodes"].as_array().unwrap().len(), 1);
        // the port survives into JSON too (by name), not just Human mode
        assert!(json.contains("hook"), "{json}");
    }

    #[test]
    fn render_tree_human_and_json_parity() {
        let g = sample_graph();
        let human = render(&g, "proj", "main", None, None, OutputMode::Human).unwrap();
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
        let json = render(&g, "proj", "main", None, None, OutputMode::Json).unwrap();
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
    fn show_renders_the_whole_node_not_a_skeleton() {
        // The read is complete: a node's description, constraints, and
        // verifications must reach BOTH outputs — the lossy projection that
        // dropped them is the bug being fixed (a node's description is its prompt).
        let mut g = sample_graph();
        // Enrich Rater (index 2 in sample_graph) with the fields the old
        // projection dropped.
        let data = &mut g.nodes[2].data;
        data.description = "Rate a hot dog on a 0-10 scale.".to_string();
        data.constraints = Some(vec!["deterministic".to_string()]);
        data.verifications = Some(vec![models::WireVerification {
            author: models::wire_verification::Author::User,
            id: Uuid::from_u128(0xA1),
            text: "score is within 0..=10".to_string(),
            r#type: None,
        }]);

        let json = render(&g, "proj", "main", None, None, OutputMode::Json).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let rater = v["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["path"] == "Api.Rater")
            .unwrap();
        assert_eq!(rater["description"], "Rate a hot dog on a 0-10 scale.");
        assert_eq!(rater["constraints"][0], "deterministic");
        assert_eq!(rater["verifications"][0]["text"], "score is within 0..=10");
        assert_eq!(rater["verifications"][0]["author"], "user");

        let human = render(&g, "proj", "main", None, None, OutputMode::Human).unwrap();
        assert!(human.contains("Rate a hot dog on a 0-10 scale."), "{human}");
        assert!(human.contains("deterministic"), "{human}");
        assert!(human.contains("score is within 0..=10"), "{human}");
    }

    #[test]
    fn boundary_language_is_shown_in_both_modes() {
        // A boundary with a codegen language surfaces it in show — otherwise the
        // web UI is the only place to confirm what `--language` set. It rides on
        // the boundary node line (human) and as a `language` field (JSON).
        let mut g = sample_graph();
        // Api is the boundary node (index 0 in sample_graph).
        g.nodes[0].data.language = Some(Some("python".to_string()));

        let human = render(&g, "proj", "main", None, None, OutputMode::Human).unwrap();
        assert!(
            human.contains("Api  [boundary]  (python)"),
            "human view must annotate the boundary's language: {human}"
        );

        let json = render(&g, "proj", "main", None, None, OutputMode::Json).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let api = v["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["path"] == "Api")
            .unwrap();
        assert_eq!(api["language"], "python", "{json}");
    }

    #[test]
    fn node_without_language_emits_no_language_value() {
        // A node with no language must not emit a bogus or "null"-string value in
        // either mode. The sample graph carries no language on any node.
        let g = sample_graph();

        let json = render(&g, "proj", "main", None, None, OutputMode::Json).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        for n in v["nodes"].as_array().unwrap() {
            assert!(
                n.get("language").is_none(),
                "a languageless node must omit the field: {n}"
            );
        }
        assert!(!json.contains("language"), "{json}");

        let human = render(&g, "proj", "main", None, None, OutputMode::Human).unwrap();
        // The language annotation is the only `]  (` sequence show emits (it rides
        // right after a node's `[kind]`); assert that exact signature is absent
        // rather than any stray `(`, so unrelated future output can't false-trip.
        assert!(
            !human.contains("]  ("),
            "no language annotation expected: {human}"
        );
    }

    #[test]
    fn position_field_is_omitted() {
        // The graph endpoint's placeholder position must never surface in show.
        let g = sample_graph();
        let json = render(&g, "proj", "main", None, None, OutputMode::Json).unwrap();
        assert!(!json.contains("position"), "{json}");
        let human = render(&g, "proj", "main", None, None, OutputMode::Human).unwrap();
        assert!(!human.to_lowercase().contains("position"), "{human}");
    }

    #[test]
    fn path_filter_narrows_to_subtree() {
        let g = sample_graph();
        let json = render(
            &g,
            "proj",
            "main",
            Some("Api.Rater"),
            None,
            OutputMode::Json,
        )
        .unwrap();
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
        let json = render(
            &g,
            "proj",
            "main",
            Some("Api.Rater"),
            None,
            OutputMode::Json,
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["cross_boundary_edges"], 1, "{json}");
        // Human: a loud footnote naming the escape hatch.
        let human = render(
            &g,
            "proj",
            "main",
            Some("Api.Rater"),
            None,
            OutputMode::Human,
        )
        .unwrap();
        assert!(human.contains("1 edge cross"), "{human}");
        assert!(human.contains("hydrate show"), "{human}");
        // The whole-graph view has nothing crossing out.
        let full = render(&g, "proj", "main", None, None, OutputMode::Json).unwrap();
        let fv: serde_json::Value = serde_json::from_str(&full).unwrap();
        assert_eq!(fv["cross_boundary_edges"], 0, "{full}");
        let full_human = render(&g, "proj", "main", None, None, OutputMode::Human).unwrap();
        assert!(!full_human.contains("cross out"), "{full_human}");
    }

    #[test]
    fn unknown_path_filter_fails_loud() {
        let g = sample_graph();
        let err = render(&g, "proj", "main", Some("Nope"), None, OutputMode::Json).unwrap_err();
        assert!(matches!(err, CliError::InvalidArgument(_)), "got {err:?}");
    }

    #[test]
    fn edge_to_unknown_handle_fails_loud() {
        // A dangling edge handle is corruption, not a silently-dropped edge.
        let mut g = sample_graph();
        g.edges[0].source_handle = Some(Uuid::from_u128(0xBEEF));
        let err = render(&g, "proj", "main", None, None, OutputMode::Json).unwrap_err();
        assert!(matches!(err, CliError::State(_)), "got {err:?}");
    }

    #[test]
    fn edge_missing_a_port_handle_fails_loud() {
        // A null handle (no port at all) is corruption too — surface it rather
        // than skip the edge and under-report the graph's connections.
        let mut g = sample_graph();
        g.edges[0].source_handle = None;
        let err = render(&g, "proj", "main", None, None, OutputMode::Json).unwrap_err();
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
        let json = render(&g, "proj", "main", None, None, OutputMode::Json).unwrap();
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

    fn subtree_of(sample: &GraphResponse, root_idx: usize, truncated: bool) -> SubtreeResponse {
        SubtreeResponse {
            branch: sample.branch.clone(),
            cross_boundary_edges: vec![],
            depth: 1,
            edges: vec![],
            nodes: vec![],
            project_id: sample.project_id,
            root: Box::new(sample.nodes[root_idx].clone()),
            truncated,
            version: sample.version.clone(),
            // The server always sends a path for every returned node; an
            // empty map would model a server that doesn't, which the contract
            // no longer permits.
            paths: sample
                .nodes
                .iter()
                .map(|n| (n.id.to_string(), n.data.name.clone()))
                .collect(),
            unaddressable: Default::default(),
        }
    }

    #[test]
    fn a_cut_subtree_says_so_in_both_modes() {
        // The signal that separates "this is the cell" from "this is a slice".
        // A reader who cannot tell them apart will act on a partial spec.
        let g = sample_graph();
        let cut = subtree_of(&g, 0, true);
        let human = render_subtree(&cut, "proj", "br", "Api", OutputMode::Human).unwrap();
        assert!(human.contains("cut at depth 1"), "{human}");

        let json: serde_json::Value = serde_json::from_str(
            &render_subtree(&cut, "proj", "br", "Api", OutputMode::Json).unwrap(),
        )
        .unwrap();
        assert_eq!(json["truncated"], true);
        assert_eq!(json["depth"], 1);
    }

    #[test]
    fn a_complete_subtree_makes_no_truncation_claim() {
        let g = sample_graph();
        let whole = subtree_of(&g, 0, false);
        let human = render_subtree(&whole, "proj", "br", "Api", OutputMode::Human).unwrap();
        assert!(!human.contains("cut at depth"), "{human}");

        let json: serde_json::Value = serde_json::from_str(
            &render_subtree(&whole, "proj", "br", "Api", OutputMode::Json).unwrap(),
        )
        .unwrap();
        assert_eq!(json["truncated"], false);
    }

    #[test]
    fn the_scoped_json_is_a_superset_of_the_whole_graph_json() {
        // The two views must be interchangeable to a parser — the scoped read
        // adds fields, it does not reshape the payload.
        let g = sample_graph();
        let scoped: serde_json::Value = serde_json::from_str(
            &render_subtree(
                &subtree_of(&g, 0, false),
                "proj",
                "br",
                "Api",
                OutputMode::Json,
            )
            .unwrap(),
        )
        .unwrap();
        let full: serde_json::Value = serde_json::from_str(
            &render(&g, "proj", "br", Some("Api"), None, OutputMode::Json).unwrap(),
        )
        .unwrap();
        for key in full.as_object().unwrap().keys() {
            assert!(scoped.get(key).is_some(), "scoped view dropped '{key}'");
        }
    }

    #[test]
    fn no_working_copy_means_no_scoped_read() {
        // No index means no id, which must degrade to the whole-graph read
        // rather than erroring — asking for a slice never denies you the graph.
        assert_eq!(
            scoped::plan(None, "Api", true).unwrap(),
            scoped::Plan::WholeGraph(scoped::Fallback::NoWorkingCopy),
        );
    }
}

#[cfg(test)]
mod scoped_subtree_tests {
    use super::tests::sample_graph;
    use super::*;
    use hydrate_wire::models::SubtreeResponse;

    /// A REAL slice: a root whose own parent is outside the slice, a child, an
    /// interior edge, and an edge leaving the subtree. The previous fixture was
    /// a single parentless node with no edges, which is why it could not reach
    /// either crash — local reconstruction succeeds on it, so the server-path
    /// branch was never load-bearing.
    fn real_subtree(unaddressable: Vec<(uuid::Uuid, &str)>, crossing: usize) -> SubtreeResponse {
        let g = sample_graph();
        let root = g.nodes[0].clone();
        let child = g.nodes[1].clone();
        let mut paths = std::collections::HashMap::new();
        // Server paths are FULL and absolute — note the ancestor `Outer` is
        // not in the slice at all, which is exactly what local reconstruction
        // cannot do.
        paths.insert(root.id.to_string(), "Outer.Api".to_string());
        paths.insert(child.id.to_string(), "Outer.Api.Maker".to_string());
        let mut un = std::collections::HashMap::new();
        for (id, reason) in &unaddressable {
            paths.remove(&id.to_string());
            un.insert(id.to_string(), (*reason).to_string());
        }
        SubtreeResponse {
            branch: g.branch.clone(),
            cross_boundary_edges: g.edges.iter().take(crossing).cloned().collect(),
            depth: 1,
            edges: vec![],
            nodes: vec![child],
            project_id: g.project_id,
            root: Box::new(root),
            truncated: false,
            version: g.version.clone(),
            paths,
            unaddressable: un,
        }
    }

    #[test]
    fn a_slice_whose_ancestors_are_absent_renders_the_server_paths() {
        // The bug this whole change exists to fix: local reconstruction here
        // fails with "references a missing parent", because `Outer` is not in
        // the payload.
        let st = real_subtree(vec![], 0);
        let out = render_subtree(&st, "proj", "br", "Outer.Api", OutputMode::Json).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let rendered = v.to_string();
        assert!(rendered.contains("Outer.Api"), "{rendered}");
    }

    #[test]
    fn an_unaddressable_node_renders_instead_of_panicking() {
        // `paths` is deliberately not total; indexing it aborts the process.
        let g = sample_graph();
        let child_id = g.nodes[1].id;
        let st = real_subtree(vec![(child_id, "empty_name")], 0);
        let out = render_subtree(&st, "proj", "br", "Outer.Api", OutputMode::Human)
            .expect("must render, not panic");
        assert!(out.contains("<unnamed"), "{out}");
        assert!(out.contains("no addressable path"), "{out}");
    }

    #[test]
    fn the_unaddressable_reason_reaches_json_without_ids() {
        let g = sample_graph();
        let child_id = g.nodes[1].id;
        let st = real_subtree(vec![(child_id, "ambiguous")], 0);
        let v: serde_json::Value = serde_json::from_str(
            &render_subtree(&st, "proj", "br", "Outer.Api", OutputMode::Json).unwrap(),
        )
        .unwrap();
        assert_eq!(v["unaddressable"][0]["reason"], "ambiguous");
        assert!(
            !v.to_string().contains(&child_id.to_string()),
            "node id leaked into show --depth --json"
        );
    }

    #[test]
    fn a_slice_with_an_edge_leaving_it_still_renders() {
        // A cross-boundary edge has one endpoint OUTSIDE the slice by
        // definition, so pushing it through the port resolver fails and blames
        // the server. Every real subtree with a dependency hits this.
        let st = real_subtree(vec![], 1);
        let v: serde_json::Value = serde_json::from_str(
            &render_subtree(&st, "proj", "br", "Outer.Api", OutputMode::Json)
                .expect("a slice with an outward edge must render"),
        )
        .unwrap();
        // The server counted them; we report its count rather than re-deriving.
        assert_eq!(v["cross_boundary_edges"], 1);
    }

    #[test]
    fn a_non_uuid_path_key_fails_loud() {
        let mut st = real_subtree(vec![], 0);
        st.paths.insert("not-a-uuid".to_string(), "X".to_string());
        let err = render_subtree(&st, "proj", "br", "Outer.Api", OutputMode::Json)
            .expect_err("a malformed key is a contract violation, not a shrug");
        assert!(format!("{err}").contains("non-uuid"), "{err}");
    }
}
