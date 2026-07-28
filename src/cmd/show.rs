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
    // That needs the node's id, and the CLI addresses nodes by dotted path, so
    // the local index is what makes it possible. No index (never pulled, or not
    // bound here) means no id, and we fall back to the whole-graph read — but
    // say so, because the whole point of the flag is what crosses the wire.
    if let (Some(depth), Some(path)) = (args.depth, args.path.as_deref()) {
        match scoped_target(&base_dir(), path)? {
            Some(node_id) => {
                // Branch-addressed: the /v1/graph twins answer about the
                // project's MAIN branch, and this reads the branch you are
                // bound to, which is where the edits are.
                let subtree = client.fetch_branch_subtree(branch_id, node_id, depth)?;
                println!(
                    "{}",
                    render_subtree(&subtree, &project.name, &branch_name, path, mode)?
                );
                return Ok(());
            }
            None => eprintln!(
                "note: no local index for '{path}', so the whole branch was \
                 fetched and filtered here. Run `hydrate pull` in a bound \
                 working copy to read just the slice."
            ),
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
    let view = build_view(graph, filter, None)?;
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
    // Server-rendered paths, when the caller has them. A SCOPED read cannot
    // reconstruct paths locally: the slice does not contain the ancestors a
    // dotted path is built from, so `node_paths` fails with "references a
    // missing parent". The server holds the whole branch and is the only party
    // that can answer.
    server_paths: Option<&HashMap<Uuid, String>>,
) -> Result<View, CliError> {
    // node id -> dotted path (local reconstruction for the whole-graph read).
    let paths = match server_paths {
        Some(p) => p.clone(),
        None => view::node_paths(&graph.nodes)?,
    };

    // port id -> (owning node's dotted path, port name).
    let mut port_owner: HashMap<Uuid, (String, Option<String>)> = HashMap::new();
    for node in &graph.nodes {
        let Some(path) = paths.get(&node.id) else {
            // No path: the server reported this node as unaddressable (an
            // unnamed node is legal while designing). Its ports simply cannot
            // be referenced by path; skip rather than index and panic.
            continue;
        };
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
        let plural = if view.cross_boundary == 1 { "" } else { "s" };
        out.push_str(&format!(
            "\n{} edge{plural} cross out of this subtree — run `hydrate show` for the full graph",
            view.cross_boundary
        ));
    }
    out
}

/// The working-copy root, or `None` when this directory is not one. `show`
/// deliberately works outside a working copy (it takes `--project`), so a
/// missing root is an ordinary state, not an error.
fn base_dir() -> Option<std::path::PathBuf> {
    crate::cmd::context::cwd()
        .ok()
        .and_then(|c| crate::state::find_root(&c))
}

/// Resolve `path` to the node id the scoped read needs, using the pulled index.
///
/// `None` means "cannot do a scoped read here" — no working copy, no index, or
/// the path is not in it (a stale index, or a typo). The caller falls back to
/// the whole-graph read and reports that, rather than failing: asking for a
/// slice should degrade to the old behaviour, never deny you the graph.
fn scoped_target(base: &Option<std::path::PathBuf>, path: &str) -> Result<Option<Uuid>, CliError> {
    let Some(base) = base.as_ref() else {
        return Ok(None);
    };
    let Some(index) = crate::state::Index::load(base)? else {
        return Ok(None);
    };
    Ok(index.entries.get(&format!("node:{path}")).copied())
}

/// Render a scoped subtree through the SAME renderer the whole-graph view uses,
/// so the two are indistinguishable in shape to a reader or a parser.
///
/// The subtree is repackaged as a `GraphResponse` carrying exactly the returned
/// node set and both edge lists; passing `path` as the filter then makes the
/// existing `build_view` split interior from crossing edges the way it always
/// has. Only what the scoped read uniquely knows is added on top: `depth`, and
/// whether the walk was CUT.
fn render_subtree(
    subtree: &hydrate_wire::models::SubtreeResponse,
    project_name: &str,
    branch_name: &str,
    path: &str,
    mode: OutputMode,
) -> Result<String, CliError> {
    let mut nodes = vec![(*subtree.root).clone()];
    nodes.extend(subtree.nodes.iter().cloned());
    let mut edges = subtree.edges.clone();
    edges.extend(subtree.cross_boundary_edges.iter().cloned());

    let graph = GraphResponse {
        branch: subtree.branch.clone(),
        edges,
        nodes,
        project_id: subtree.project_id,
        version: subtree.version.clone(),
    };
    // Server-rendered paths — the whole reason the scoped read returns them.
    // Reconstructing here fails: the slice has no ancestors to walk.
    let server_paths: HashMap<Uuid, String> = subtree
        .paths
        .iter()
        .filter_map(|(k, v)| Uuid::parse_str(k).ok().map(|id| (id, v.clone())))
        .collect();
    let view = build_view(&graph, Some(path), Some(&server_paths))?;
    let rendered = render_view(&view, project_name, branch_name, mode);

    match mode {
        OutputMode::Json => {
            // Augment rather than re-derive, so the scoped payload stays a
            // superset of the familiar one.
            let mut v: serde_json::Value = serde_json::from_str(&rendered)
                .map_err(|e| CliError::Other(format!("rendering the subtree: {e}")))?;
            v["depth"] = serde_json::json!(subtree.depth);
            v["truncated"] = serde_json::json!(subtree.truncated);
            Ok(v.to_string())
        }
        OutputMode::Human => {
            let mut out = rendered;
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

        let human = render(&g, "proj", "main", None, OutputMode::Human).unwrap();
        assert!(human.contains("Ports  [interface]"), "{human}");

        let json = render(&g, "proj", "main", None, OutputMode::Json).unwrap();
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
        let human = render(&g, "proj", "main", None, OutputMode::Human).unwrap();
        assert!(human.contains("hook:Payload"), "{human}");

        let json = render(&g, "proj", "main", None, OutputMode::Json).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["nodes"].as_array().unwrap().len(), 1);
        // the port survives into JSON too (by name), not just Human mode
        assert!(json.contains("hook"), "{json}");
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

        let json = render(&g, "proj", "main", None, OutputMode::Json).unwrap();
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

        let human = render(&g, "proj", "main", None, OutputMode::Human).unwrap();
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

        let human = render(&g, "proj", "main", None, OutputMode::Human).unwrap();
        assert!(
            human.contains("Api  [boundary]  (python)"),
            "human view must annotate the boundary's language: {human}"
        );

        let json = render(&g, "proj", "main", None, OutputMode::Json).unwrap();
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

        let json = render(&g, "proj", "main", None, OutputMode::Json).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        for n in v["nodes"].as_array().unwrap() {
            assert!(
                n.get("language").is_none(),
                "a languageless node must omit the field: {n}"
            );
        }
        assert!(!json.contains("language"), "{json}");

        let human = render(&g, "proj", "main", None, OutputMode::Human).unwrap();
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
        let full: serde_json::Value =
            serde_json::from_str(&render(&g, "proj", "br", Some("Api"), OutputMode::Json).unwrap())
                .unwrap();
        for key in full.as_object().unwrap().keys() {
            assert!(scoped.get(key).is_some(), "scoped view dropped '{key}'");
        }
    }

    #[test]
    fn scoped_target_is_none_outside_a_working_copy() {
        // No index means no id, which must degrade to the whole-graph read
        // rather than erroring — asking for a slice never denies you the graph.
        assert_eq!(scoped_target(&None, "Api").unwrap(), None);
    }
}
