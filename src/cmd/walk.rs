//! `walk <path>` — the agent's scoped read. Reads the SAME bound branch as
//! `show`/`pull` (the whole-graph branch read), then computes the requested slice
//! client-side: a node plus its 1-hop neighborhood by default, or (with
//! `--boundary`) a boundary's scope — its children and the edges interior to it.
//!
//! Where `show` is the human's whole-graph / subtree view, `walk` is the scoped
//! slice an agent reads so it never pulls a large graph into context. Every node
//! is rendered whole (its full [`view::NodeView`]), and `--json` is the agent
//! path. This verb is read-only: it fetches the branch graph and never stages or
//! commits anything.

use std::collections::{HashMap, HashSet};

use hydrate_wire::models::{self, GraphResponse, WireNode};
use uuid::Uuid;

use super::context::require_workdir;
use super::scoped;
use super::view::{self, EdgeView, NodeView};
use crate::client::Client;
use crate::config::Config;
use crate::error::CliError;
use crate::output::OutputMode;
use crate::state::Binding;

pub fn run(args: crate::cli::WalkArgs, mode: OutputMode) -> Result<(), CliError> {
    let base = require_workdir()?;
    let binding = Binding::load(&base)?.ok_or_else(|| {
        CliError::Other(
            "this working copy is not bound to a branch; run `hydrate fork`".to_string(),
        )
    })?;

    let config = Config::load()?;
    let client = Client::new(&config)?;

    // SCOPED read first: fetch only the slice. Before this, `walk` fetched the
    // WHOLE branch graph and sliced it locally — "scoped" was true of the
    // output and false of the request. It needs the node's id, which the
    // pulled index supplies; `walk` always reads the branch it is bound to, so
    // the index (which records no branch identity of its own) applies.
    match scoped::plan(Some(&base), &args.path, true)? {
        scoped::Plan::Scoped { id: node_id, kind } => {
            // Reject a recognised non-boundary BEFORE the request. The server
            // 404s such an id, so the check inside the renderer never runs —
            // it is the /boundary route's contract that a non-boundary is not
            // found, not a generic property. An UNRECOGNISED kind defers: the
            // index may have been written by a newer CLI.
            if args.boundary {
                if let Some(kind) = kind.as_deref() {
                    if scoped::is_known_non_boundary(kind) {
                        return Err(not_a_boundary(&args.path, kind));
                    }
                } else {
                    // The index resolved the path but carries no kind — an
                    // index pulled before kinds were recorded. Say so, rather
                    // than letting the request 404 with no explanation of why
                    // the local check didn't help.
                    eprintln!(
                        "note: this working copy's index has no kind for \
                         '{}', so --boundary could not be checked locally; \
                         run `hydrate pull` to refresh it.",
                        args.path,
                    );
                }
            }
            let out = if args.boundary {
                let cell = client
                    .fetch_branch_boundary(binding.branch_id, node_id)
                    .map_err(|e| boundary_404_error(e, &args.path))?;
                render_boundary_scoped(&cell, &args.path, mode)?
            } else {
                let hood = client
                    .fetch_branch_node(binding.branch_id, node_id)
                    .map_err(|e| stale_index_error(e, &args.path))?;
                render_neighborhood_scoped(&hood, &args.path, mode)?
            };
            println!("{out}");
            return Ok(());
        }
        scoped::Plan::WholeGraph(why) => {
            eprintln!("{}", scoped::fallback_note(&args.path, why));
        }
    }

    // Fallback: the whole-graph read, sliced locally. Reading the project's
    // main-branch graph instead would return stale or wrong data on any
    // diverged branch, so this stays branch-addressed too.
    let graph = client.fetch_branch_graph(binding.branch_id)?;

    let out = if args.boundary {
        render_boundary(&graph, &args.path, mode)?
    } else {
        render_neighborhood(&graph, &args.path, mode)?
    };
    println!("{out}");
    Ok(())
}

/// Translate a scoped read's `404` into the guidance the whole-graph path gives.
///
/// The id came from this working copy's index. A `404` therefore means the index
/// resolved a path to a node the branch no longer has — deleted since the last
/// `pull` — and the raw `service error (404)` says nothing a caller can act on,
/// while the same lookup through the whole-graph path says `no node 'X' on this
/// branch`. Two paths answering one question must not differ in what the user
/// can do next.
///
/// Only `404` is remapped: every other status keeps its own meaning.
fn stale_index_error(err: CliError, path: &str) -> CliError {
    match err {
        CliError::Service { status: 404, .. } => CliError::StaleView(format!(
            "no node '{path}' on this branch; this working copy's index still \
             has it, so run `hydrate pull` to refresh it"
        )),
        other => other,
    }
}

/// The same translation for the `--boundary` read, which has a second cause.
///
/// A `404` from the boundary route does NOT mean the node is gone: it is that
/// route's contract that a **non-boundary** is not found (see the pre-flight
/// check above, which exists precisely because of it). The pre-flight catches
/// that when the index records a kind — but when it records none, or records an
/// unrecognised one, the request goes out and this is where it lands. Claiming
/// a stale index there would be a confident wrong diagnosis on a node that is
/// present and perfectly healthy.
///
/// A 404 is also the authorization answer — the service returns an identical
/// body whether a resource is missing or not accessible — so the message names
/// what it cannot distinguish rather than picking one.
fn boundary_404_error(err: CliError, path: &str) -> CliError {
    match err {
        CliError::Service { status: 404, .. } => CliError::StaleView(format!(
            "could not read '{path}' as a boundary. It may not be a boundary, it \
             may no longer be on this branch, or this key may not have access to \
             it. Run `hydrate walk {path}` for its neighborhood, or \
             `hydrate pull` if this working copy is behind"
        )),
        other => other,
    }
}

/// Locate the node addressed by `path` in the branch graph, alongside the map of
/// every node's dotted path. An unknown path fails loud with the same guidance
/// `show` gives.
/// The focal node, every node's dotted path, and `id → reason` for the nodes
/// that have none — the same split the server's scoped reads return.
type Located<'g> = (&'g WireNode, HashMap<Uuid, String>, HashMap<String, String>);

fn locate<'g>(graph: &'g GraphResponse, path: &str) -> Result<Located<'g>, CliError> {
    // Report rather than abort: an unnamed node is legal while designing, and
    // the scoped path renders it. See `node_paths_reporting`.
    let (mut paths, unaddressable) = view::node_paths_reporting(&graph.nodes)?;
    for node in &graph.nodes {
        paths.entry(node.id).or_insert_with(|| {
            scoped::unaddressable_label(
                unaddressable
                    .get(&node.id.to_string())
                    .map(String::as_str)
                    .unwrap_or("unknown"),
            )
        });
    }
    // A placeholder is a LABEL, not an address. It is written into the same map
    // this search reads, and every unaddressable node gets the same text — so
    // without this, a label whose own words say "give it a name to address it"
    // would be accepted as a path, and with several such nodes the first in
    // graph order would win silently. A dotted path is a slug; refuse anything
    // that could not be one.
    if path.starts_with('<') {
        return Err(CliError::InvalidArgument(format!(
            "'{path}' is a placeholder for a node that has no addressable path, \
             not a path you can read. Give the node a name to address it"
        )));
    }
    let node = graph
        .nodes
        .iter()
        .find(|n| paths.get(&n.id).map(String::as_str) == Some(path))
        .ok_or_else(|| {
            CliError::InvalidArgument(format!(
                "no node '{path}' on this branch; run `hydrate show` to see the whole graph"
            ))
        })?;
    Ok((node, paths, unaddressable))
}

/// Render a node + its 1-hop neighborhood computed from the branch graph: the
/// focal node, every node reachable across one incident edge, and those edges.
/// The focal node keeps the dotted `path` the caller queried; neighbors keep
/// their own reconstructed dotted paths. Edges are translated to dotted
/// `node.port` paths over exactly the returned node set.
/// Label a node from the server's `paths`, falling back to the reported reason.
///
/// NEVER indexes `paths`: the map is deliberately not total — the server
/// returns unnamed nodes (legal while designing) and reports why in
/// `unaddressable`. Indexing it would panic on an ordinary graph.
fn label_of(
    id: &str,
    paths: &std::collections::HashMap<String, String>,
    unaddressable: &std::collections::HashMap<String, String>,
) -> String {
    paths
        .get(id)
        .map(|p| scoped::sanitize(p))
        .unwrap_or_else(|| {
            scoped::unaddressable_label(
                unaddressable
                    .get(id)
                    .map(String::as_str)
                    .unwrap_or("unknown"),
            )
        })
}

/// The "you asked for a boundary and this isn't one" error.
///
/// One builder for all three sites (the local preflight, and both renderers'
/// server-data checks) so the guidance cannot drift between the scoped and
/// whole-graph paths — which is the divergence this whole line of work exists
/// to remove.
///
/// `from_index` hedges the claim. The preflight reads a SNAPSHOT: a node's
/// kind is mutable over the wire, so a node that was a behavior at pull time
/// may be a boundary now. Stating it as present-tense fact would refuse a
/// legitimate read while asserting something false, with no remedy named.
fn not_a_boundary_msg(path: &str, kind: &str, from_index: bool) -> String {
    let kind = scoped::sanitize(kind);
    if from_index {
        format!(
            "'{path}' is not a boundary — this working copy's index has it as \
             a {kind}. Run `hydrate walk {path}` for its neighborhood, or \
             `hydrate pull` if the index is behind."
        )
    } else {
        format!(
            "'{path}' is not a boundary (it is a {kind}); run \
             `hydrate walk {path}` for its neighborhood"
        )
    }
}

/// The preflight variant: the claim comes from the local index.
fn not_a_boundary(path: &str, kind: &str) -> CliError {
    CliError::InvalidArgument(not_a_boundary_msg(path, kind, true))
}

/// The `unaddressable` map as something a consumer can act on.
///
/// The server keys it by node id, but this CLI does not surface ids — and a
/// raw uuid is not joinable to anything else in the payload, since every other
/// node here is addressed by path. Emit the label the human sees plus the
/// reason, so both modes carry the same, usable information.
fn unaddressable_report(un: &std::collections::HashMap<String, String>) -> Vec<serde_json::Value> {
    let mut out: Vec<serde_json::Value> = un
        .values()
        .map(|reason| {
            serde_json::json!({
                "label": scoped::unaddressable_label(reason),
                "reason": scoped::sanitize(reason),
            })
        })
        .collect();
    out.sort_by(|a, b| a["label"].as_str().cmp(&b["label"].as_str()));
    out
}

/// The human tail naming how many nodes could not be addressed. Shared by both
/// scoped renderers so they cannot disagree about whether to mention it.
fn unaddressable_summary(un: &std::collections::HashMap<String, String>) -> String {
    if un.is_empty() {
        return String::new();
    }
    format!(
        "\n{} node(s) here have no addressable path — name them to reference \
         them in other commands.\n",
        un.len()
    )
}

/// Render a node + 1-hop neighborhood from the SCOPED read. Paths come from
/// the server, which is the only party holding the ancestors a dotted path is
/// built from — the slice does not contain them.
fn render_neighborhood_scoped(
    hood: &models::NodeNeighborhoodResponse,
    path: &str,
    mode: OutputMode,
) -> Result<String, CliError> {
    let paths = &hood.paths;
    let un = &hood.unaddressable;
    let node_id = hood.node.id.to_string();

    let hint = if hood.node.kind == models::wire_node::Kind::Boundary {
        format!(" (it is a boundary — run `hydrate walk {path} --boundary` for its scope)")
    } else {
        String::new()
    };

    let mut neighbors: Vec<(String, &models::WireNode)> = hood
        .neighbors
        .iter()
        .map(|n| (label_of(&n.id.to_string(), paths, un), n))
        .collect();
    neighbors.sort_by(|a, b| a.0.cmp(&b.0));

    let focus = view::node_view(&hood.node, label_of(&node_id, paths, un));
    let neighbor_views: Vec<NodeView> = neighbors
        .iter()
        .map(|(p, n)| view::node_view(n, p.clone()))
        .collect();

    let mut labeled: Vec<(String, &models::WireNode)> =
        vec![(label_of(&node_id, paths, un), &hood.node)];
    labeled.extend(neighbors.iter().map(|(p, n)| (p.clone(), *n)));
    let edges = view::resolve_edges(&view::port_labels(&labeled), &hood.edges)?;

    Ok(match mode {
        OutputMode::Json => serde_json::json!({
            "node": focus,
            "neighbors": neighbor_views,
            "edges": edges,
            "version": hood.version,
            // Surfaced, not swallowed: a node without a path is a thing the
            // user can fix, and a consumer needs to know which ones.
            "unaddressable": unaddressable_report(un),
        })
        .to_string(),
        OutputMode::Human => {
            let mut out = format!("Node '{path}' (version {}):{hint}\n", hood.version);
            out.push_str(&focus.human(0));
            if !neighbor_views.is_empty() {
                out.push_str("\nNeighbors:\n");
                for n in &neighbor_views {
                    out.push_str(&n.human(1));
                }
            }
            if !edges.is_empty() {
                out.push_str("\nEdges:\n");
                for e in &edges {
                    out.push_str(&format!("  {} -> {}\n", e.from, e.to));
                }
            }
            out.push_str(&unaddressable_summary(un));
            out
        }
    })
}

/// Render a boundary's children from the SCOPED read.
fn render_boundary_scoped(
    cell: &models::BoundaryResponse,
    path: &str,
    mode: OutputMode,
) -> Result<String, CliError> {
    if cell.boundary.kind != models::wire_node::Kind::Boundary {
        // Defence in depth, and normally unreachable: the /boundary route 404s
        // a non-boundary id, so a 200 body should always be one. Kept because
        // it is the only check that does not depend on a local index — if the
        // route's contract ever widens, this is what still catches a mismatch.
        return Err(CliError::InvalidArgument(not_a_boundary_msg(
            path,
            view::kind_str(cell.boundary.kind),
            false,
        )));
    }
    let paths = &cell.paths;
    let un = &cell.unaddressable;

    let mut children: Vec<(String, &models::WireNode)> = cell
        .children
        .iter()
        .map(|n| (label_of(&n.id.to_string(), paths, un), n))
        .collect();
    children.sort_by(|a, b| a.0.cmp(&b.0));

    let boundary_label = label_of(&cell.boundary.id.to_string(), paths, un);
    let focus = view::node_view(&cell.boundary, boundary_label.clone());
    let child_views: Vec<NodeView> = children
        .iter()
        .map(|(p, n)| view::node_view(n, p.clone()))
        .collect();

    let mut labeled: Vec<(String, &models::WireNode)> = vec![(boundary_label, &cell.boundary)];
    labeled.extend(children.iter().map(|(p, n)| (p.clone(), *n)));
    let edges = view::resolve_edges(&view::port_labels(&labeled), &cell.edges)?;

    Ok(match mode {
        OutputMode::Json => serde_json::json!({
            "boundary": focus,
            "children": child_views,
            "edges": edges,
            "version": cell.version,
            "unaddressable": unaddressable_report(un),
        })
        .to_string(),
        OutputMode::Human => {
            let mut out = format!("Boundary '{path}' (version {}):\n", cell.version);
            out.push_str(&focus.human(0));
            if !child_views.is_empty() {
                out.push_str("\nChildren:\n");
                for c in &child_views {
                    out.push_str(&c.human(1));
                }
            }
            if !edges.is_empty() {
                out.push_str("\nInterior edges:\n");
                for e in &edges {
                    out.push_str(&format!("  {} -> {}\n", e.from, e.to));
                }
            }
            out.push_str(&unaddressable_summary(un));
            out
        }
    })
}

fn render_neighborhood(
    graph: &GraphResponse,
    path: &str,
    mode: OutputMode,
) -> Result<String, CliError> {
    let (target, paths, unaddressable) = locate(graph, path)?;

    // A boundary's scope is read with `--boundary`; a plain walk still returns its
    // neighborhood, but point the caller at the richer read.
    let hint = if target.kind == models::wire_node::Kind::Boundary {
        format!(" (it is a boundary — run `hydrate walk {path} --boundary` for its scope)")
    } else {
        String::new()
    };

    // Edges incident to the focal node (by node identity), and the neighbor node
    // ids they reach (the endpoint that is not the focal node).
    let mut incident = Vec::new();
    let mut neighbor_ids = HashSet::new();
    for edge in &graph.edges {
        if edge.source == target.id || edge.target == target.id {
            incident.push(edge.clone());
            for end in [edge.source, edge.target] {
                if end != target.id {
                    neighbor_ids.insert(end);
                }
            }
        }
    }

    let by_id: HashMap<Uuid, &WireNode> = graph.nodes.iter().map(|n| (n.id, n)).collect();
    let mut neighbor_nodes: Vec<&WireNode> = neighbor_ids
        .iter()
        .map(|id| {
            by_id.get(id).copied().ok_or_else(|| {
                CliError::State(format!(
                    "the branch graph has an edge to a missing node {id}"
                ))
            })
        })
        .collect::<Result<_, _>>()?;
    neighbor_nodes.sort_by(|a, b| paths[&a.id].cmp(&paths[&b.id]));

    let focus = view::node_view(target, path.to_string());
    let neighbors: Vec<NodeView> = neighbor_nodes
        .iter()
        .map(|n| view::node_view(n, paths[&n.id].clone()))
        .collect();

    let mut labeled: Vec<(String, &WireNode)> = vec![(path.to_string(), target)];
    for n in &neighbor_nodes {
        labeled.push((paths[&n.id].clone(), n));
    }
    let edges = view::resolve_edges(&view::port_labels(&labeled), &incident)?;

    Ok(match mode {
        OutputMode::Json => serde_json::json!({
            "node": focus,
            "neighbors": neighbors,
            "edges": edges,
            "version": graph.version,
            // The fallback renders unaddressable nodes as placeholder labels, so
            // it owes the same degradation channel the scoped read has. Without
            // it a consumer sees a placeholder sitting where a dotted path
            // belongs, with nothing in the payload saying anything degraded.
            "unaddressable": unaddressable_report(&unaddressable),
        })
        .to_string(),
        OutputMode::Human => {
            let mut out = format!("Node '{path}' (version {}):{hint}", graph.version);
            out.push('\n');
            out.push_str(&focus.human(0));
            out.push_str("\nNeighbors:");
            if neighbors.is_empty() {
                out.push_str("\n  (none)");
            }
            for n in &neighbors {
                out.push('\n');
                out.push_str(&n.human(0));
            }
            out.push_str(&edge_lines(&edges));
            out.push_str(&unaddressable_summary(&unaddressable));
            out
        }
    })
}

/// Render a boundary's scope from the branch graph: the boundary node, its direct
/// children, and the edges interior to it (both endpoints within the boundary or
/// its children). Children are addressed by their reconstructed dotted path.
fn render_boundary(
    graph: &GraphResponse,
    path: &str,
    mode: OutputMode,
) -> Result<String, CliError> {
    let (target, paths, unaddressable) = locate(graph, path)?;

    // `--boundary` only makes sense on a boundary; a friendly local check beats an
    // empty or confusing scope on any other kind.
    if target.kind != models::wire_node::Kind::Boundary {
        return Err(CliError::InvalidArgument(format!(
            "'{path}' is not a boundary (it is a {}); run `hydrate walk {path}` for its neighborhood",
            view::kind_str(target.kind)
        )));
    }

    let mut child_nodes: Vec<&WireNode> = graph
        .nodes
        .iter()
        .filter(|n| n.parent_id == Some(target.id))
        .collect();
    child_nodes.sort_by(|a, b| paths[&a.id].cmp(&paths[&b.id]));

    // The boundary's scope: itself plus its direct children. An edge is interior
    // when BOTH endpoints sit in that scope.
    let mut scope = HashSet::new();
    scope.insert(target.id);
    for c in &child_nodes {
        scope.insert(c.id);
    }
    let interior: Vec<_> = graph
        .edges
        .iter()
        .filter(|e| scope.contains(&e.source) && scope.contains(&e.target))
        .cloned()
        .collect();

    let boundary = view::node_view(target, path.to_string());
    let children: Vec<NodeView> = child_nodes
        .iter()
        .map(|c| view::node_view(c, paths[&c.id].clone()))
        .collect();

    let mut labeled: Vec<(String, &WireNode)> = vec![(path.to_string(), target)];
    for c in &child_nodes {
        labeled.push((paths[&c.id].clone(), c));
    }
    let edges = view::resolve_edges(&view::port_labels(&labeled), &interior)?;

    Ok(match mode {
        OutputMode::Json => serde_json::json!({
            "boundary": boundary,
            "children": children,
            "edges": edges,
            "version": graph.version,
            "unaddressable": unaddressable_report(&unaddressable),
        })
        .to_string(),
        OutputMode::Human => {
            let mut out = format!("Boundary '{path}' (version {}):", graph.version);
            out.push('\n');
            out.push_str(&boundary.human(0));
            out.push_str("\nChildren:");
            if children.is_empty() {
                out.push_str("\n  (none)");
            }
            for c in &children {
                out.push('\n');
                out.push_str(&c.human(1));
            }
            out.push_str(&edge_lines(&edges));
            out.push_str(&unaddressable_summary(&unaddressable));
            out
        }
    })
}

/// The human `Edges:` block (empty string when there are none).
fn edge_lines(edges: &[EdgeView]) -> String {
    if edges.is_empty() {
        return String::new();
    }
    let mut out = String::from("\nEdges:");
    for e in edges {
        out.push_str(&format!("\n  {} -> {}", e.from, e.to));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use hydrate_wire::models::{
        self, BranchRef, Position, WireEdge, WireNodeData, WirePort, WireVerification,
    };
    use uuid::Uuid;

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
            format!("{name} does a thing."),
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

    /// Api (boundary) { Maker (out dog:HotDog), Rater (in raw:HotDog) }, interior
    /// edge Maker.dog -> Rater.raw. Rater also carries a verification. A second
    /// boundary Other { Lone } sits outside so slices must exclude it.
    fn sample_graph() -> GraphResponse {
        use models::wire_node::Kind;
        let maker_out = port(0xD0, "dog", "HotDog");
        let rater_in = port(0xF0, "raw", "HotDog");
        let mut rater = node(
            0x12,
            "Rater",
            Kind::Behavior,
            Some(0x10),
            vec![rater_in.clone()],
            vec![],
        );
        rater.data.verifications = Some(vec![WireVerification {
            author: models::wire_verification::Author::User,
            id: Uuid::from_u128(0xA1),
            text: "score within 0..=10".to_string(),
            r#type: None,
        }]);
        GraphResponse {
            branch: Box::new(BranchRef::new(Uuid::from_u128(2), 3)),
            project_id: Uuid::from_u128(0xFEED),
            version: "7".to_string(),
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
                rater,
                node(0x20, "Other", Kind::Boundary, None, vec![], vec![]),
                node(0x21, "Lone", Kind::Behavior, Some(0x20), vec![], vec![]),
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
    fn a_scoped_404_reads_as_a_stale_index_not_a_service_error() {
        // The id came from the local index, so a 404 means the index resolved a
        // path the branch no longer has. The raw `service error (404)` gives a
        // caller nothing to do; the whole-graph path for the same lookup says
        // `no node 'X' on this branch`. Two paths, one question, one remedy.
        let err = stale_index_error(
            CliError::Service {
                status: 404,
                kind: "not_found".to_string(),
                reason: None,
            },
            "Api.Gone",
        );
        let msg = err.to_string();
        assert!(matches!(err, CliError::StaleView(_)), "got {err:?}");
        assert!(msg.contains("no node 'Api.Gone' on this branch"), "{msg}");
        // A distinct kind, so a consumer can tell "the branch no longer has
        // this" from "you typed a bad path" — they want different recovery.
        assert_eq!(err.kind(), "stale_view", "{msg}");
        assert!(msg.contains("hydrate pull"), "{msg}");
        assert!(!msg.contains("service error"), "{msg}");
    }

    #[test]
    fn the_boundary_404_names_every_cause_it_cannot_tell_apart() {
        // The /boundary route 404s a NON-BOUNDARY as its contract — the
        // pre-flight above exists because of it. When the index records no kind
        // the request goes out and lands here, on a node that is present and
        // healthy. Telling that caller their index is stale is a confident
        // wrong diagnosis. A 404 is also the authz answer, which is
        // indistinguishable by design.
        let err = boundary_404_error(
            CliError::Service {
                status: 404,
                kind: "not_found".to_string(),
                reason: None,
            },
            "Api.Rater",
        );
        let msg = err.to_string();
        assert!(msg.contains("may not be a boundary"), "{msg}");
        assert!(msg.contains("no longer be on this branch"), "{msg}");
        assert!(msg.contains("access"), "{msg}");
        // It must NOT assert the stale-index cause as fact, which is what the
        // neighborhood path says and what this one may not.
        assert!(
            !msg.contains("index still has it"),
            "boundary 404 states a stale index as fact: {msg}"
        );
    }

    #[test]
    fn other_statuses_keep_their_own_meaning() {
        // Remapping everything would turn a 500 or a 403 into "run pull",
        // which is worse than the bare status it replaced.
        for status in [403u16, 422, 500] {
            let err = stale_index_error(
                CliError::Service {
                    status,
                    kind: "boom".to_string(),
                    reason: Some("upstream".to_string()),
                },
                "Api.Rater",
            );
            assert!(
                matches!(err, CliError::Service { status: s, .. } if s == status),
                "status {status} was remapped: {err:?}"
            );
        }
    }

    #[test]
    fn neighborhood_reads_the_branch_slice_around_the_node() {
        let json = render_neighborhood(&sample_graph(), "Api.Rater", OutputMode::Json).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        // The focal node keeps its queried path and is rendered WHOLE (description
        // + verifications, not a skeleton).
        assert_eq!(v["node"]["path"], "Api.Rater");
        assert_eq!(v["node"]["description"], "Rater does a thing.");
        assert_eq!(v["node"]["verifications"][0]["text"], "score within 0..=10");
        // Exactly the 1-hop neighbor is present, keyed by its full dotted path —
        // Lone (in a different boundary, not connected) is excluded.
        let neighbors = v["neighbors"].as_array().unwrap();
        assert_eq!(neighbors.len(), 1);
        assert_eq!(neighbors[0]["path"], "Api.Maker");
        // The connecting edge is translated to dotted port paths.
        let edges = v["edges"].as_array().unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0]["from"], "Api.Maker.dog");
        assert_eq!(edges[0]["to"], "Api.Rater.raw");
        // The branch version rides through (proving the branch graph is the source).
        assert_eq!(v["version"], "7");
    }

    #[test]
    fn neighborhood_human_and_json_carry_the_same_information() {
        let human = render_neighborhood(&sample_graph(), "Api.Rater", OutputMode::Human).unwrap();
        assert!(human.contains("Node 'Api.Rater'"), "{human}");
        assert!(human.contains("Rater  [behavior]"), "{human}");
        assert!(human.contains("Rater does a thing."), "{human}");
        assert!(human.contains("score within 0..=10"), "{human}");
        assert!(human.contains("Neighbors:"), "{human}");
        assert!(human.contains("Maker  [behavior]"), "{human}");
        assert!(human.contains("Api.Maker.dog -> Api.Rater.raw"), "{human}");
    }

    #[test]
    fn boundary_reads_children_and_interior_edges_from_the_branch() {
        let json = render_boundary(&sample_graph(), "Api", OutputMode::Json).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["boundary"]["path"], "Api");
        assert_eq!(v["boundary"]["kind"], "boundary");
        // Children are the boundary's direct descendants, by dotted path — the
        // other boundary's child (Other.Lone) is not in scope.
        let children = v["children"].as_array().unwrap();
        let paths: Vec<&str> = children
            .iter()
            .map(|c| c["path"].as_str().unwrap())
            .collect();
        assert_eq!(children.len(), 2);
        assert!(paths.contains(&"Api.Maker"), "{paths:?}");
        assert!(paths.contains(&"Api.Rater"), "{paths:?}");
        assert!(!paths.contains(&"Other.Lone"), "{paths:?}");
        // The interior edge is translated over the boundary's own scope.
        let edges = v["edges"].as_array().unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0]["from"], "Api.Maker.dog");
        assert_eq!(edges[0]["to"], "Api.Rater.raw");
        assert_eq!(v["version"], "7");
    }

    #[test]
    fn boundary_human_lists_children_under_the_boundary() {
        let human = render_boundary(&sample_graph(), "Api", OutputMode::Human).unwrap();
        assert!(human.contains("Boundary 'Api'"), "{human}");
        assert!(human.contains("Api  [boundary]"), "{human}");
        assert!(human.contains("Children:"), "{human}");
        assert!(human.contains("Maker  [behavior]"), "{human}");
        assert!(human.contains("Rater  [behavior]"), "{human}");
        assert!(human.contains("Api.Maker.dog -> Api.Rater.raw"), "{human}");
    }

    #[test]
    fn a_placeholder_label_is_not_an_address() {
        // Every unaddressable node gets the SAME label, written into the very
        // map `locate` searches. Without a guard, a label whose own words say
        // "give it a name to address it" is accepted as a path, and with
        // several such nodes the first in graph order wins — silently, with a
        // real answer about an arbitrary node.
        let mut g = sample_graph();
        g.nodes[1].data.name = String::new();
        let label = scoped::unaddressable_label("empty_name");
        let err = render_neighborhood(&g, &label, OutputMode::Json).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("placeholder"), "{msg}");
        assert!(msg.contains("Give the node a name"), "{msg}");
    }

    #[test]
    fn the_fallback_reports_unaddressable_nodes_too() {
        // The fallback renders placeholders, so it owes the same degradation
        // channel the scoped read has. Otherwise a consumer sees a placeholder
        // where a dotted path belongs and nothing says anything degraded.
        let mut g = sample_graph();
        g.nodes[1].data.name = String::new();

        let json = render_neighborhood(&g, "Api.Rater", OutputMode::Json).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let un = v["unaddressable"].as_array().expect("unaddressable array");
        assert_eq!(un.len(), 1, "{json}");
        assert!(un[0]["label"].is_string(), "{json}");
        assert!(un[0]["reason"] == "empty_name", "{json}");

        let human = render_neighborhood(&g, "Api.Rater", OutputMode::Human).unwrap();
        assert!(human.contains("no addressable path"), "{human}");
    }

    #[test]
    fn unknown_path_fails_loud() {
        // The whole-graph path genuinely has no such node, which is a bad
        // argument — not a stale view of a node that exists elsewhere.
        let err = render_neighborhood(&sample_graph(), "Nope", OutputMode::Json).unwrap_err();
        assert!(matches!(err, CliError::InvalidArgument(_)), "got {err:?}");
    }

    #[test]
    fn boundary_on_a_non_boundary_node_fails_with_a_friendly_message() {
        // `--boundary` on a behavior node points the caller at the plain read
        // rather than emitting an empty or confusing scope.
        let err = render_boundary(&sample_graph(), "Api.Rater", OutputMode::Json).unwrap_err();
        match err {
            CliError::InvalidArgument(m) => {
                assert!(m.contains("is not a boundary"), "{m}");
                assert!(m.contains("hydrate walk Api.Rater"), "{m}");
            }
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
    }

    #[test]
    fn plain_walk_on_a_boundary_returns_its_neighborhood_and_hints_at_boundary_scope() {
        // A plain walk on a boundary is not an error (it returns the boundary's
        // 1-hop neighborhood); the human view nudges toward `--boundary`.
        let human = render_neighborhood(&sample_graph(), "Api", OutputMode::Human).unwrap();
        assert!(human.contains("Api  [boundary]"), "{human}");
        assert!(human.contains("--boundary"), "{human}");
    }

    #[test]
    fn a_corrupt_edge_handle_fails_loud() {
        // An incident edge naming a port outside the returned slice is corruption,
        // never a silently-dropped connection.
        let mut g = sample_graph();
        g.edges[0].source_handle = Some(Uuid::from_u128(0xBEEF));
        let err = render_neighborhood(&g, "Api.Rater", OutputMode::Json).unwrap_err();
        assert!(matches!(err, CliError::State(_)), "got {err:?}");
    }
}

#[cfg(test)]
mod scoped_tests {
    use super::*;
    use hydrate_wire::models::{
        BoundaryResponse, NodeNeighborhoodResponse, WireNode, WireNodeData,
    };
    use std::collections::HashMap;
    use uuid::Uuid;

    fn node(id: Uuid, name: &str) -> WireNode {
        node_of_kind(id, name, models::wire_node::Kind::Behavior)
    }

    fn node_of_kind(id: Uuid, name: &str, kind: models::wire_node::Kind) -> WireNode {
        WireNode {
            id,
            kind,
            parent_id: None,
            position: Box::new(models::Position { x: 0.0, y: 0.0 }),
            data: Box::new(WireNodeData {
                name: name.to_string(),
                description: String::new(),
                status: "idle".to_string(),
                inputs: None,
                outputs: None,
                config: None,
                constraints: None,
                verifications: None,
                is_test_node: false,
                is_external: false,
                source_decisions: None,
                user_kind: None,
                path_prefix: None,
                language: None,
                external_kind: None,
                protocol: None,
                documentation_url: None,
            }),
        }
    }

    fn hood(
        paths: HashMap<String, String>,
        un: HashMap<String, String>,
    ) -> NodeNeighborhoodResponse {
        let focus = Uuid::from_u128(1);
        let neighbor = Uuid::from_u128(2);
        NodeNeighborhoodResponse {
            version: "v1".to_string(),
            project_id: Uuid::from_u128(9),
            branch: Box::new(models::BranchRef {
                id: Uuid::from_u128(8),
                version: 1,
            }),
            node: Box::new(node(focus, "Focus")),
            neighbors: vec![node(neighbor, "")],
            edges: vec![],
            paths,
            unaddressable: un,
        }
    }

    #[test]
    fn an_unaddressable_neighbor_does_not_panic() {
        // The server deliberately returns nodes with no path — an unnamed node
        // is legal while designing. Indexing `paths` would panic on an
        // ordinary graph, so indexing the map aborts the command.
        let mut paths = HashMap::new();
        paths.insert(Uuid::from_u128(1).to_string(), "Focus".to_string());
        let mut un = HashMap::new();
        un.insert(Uuid::from_u128(2).to_string(), "empty_name".to_string());

        let out = render_neighborhood_scoped(&hood(paths, un), "Focus", OutputMode::Human)
            .expect("must render, not panic");
        assert!(out.contains("<unnamed"), "{out}");
        assert!(out.contains("no addressable path"), "{out}");
    }

    #[test]
    fn the_unaddressable_reason_reaches_json_consumers() {
        let mut paths = HashMap::new();
        paths.insert(Uuid::from_u128(1).to_string(), "Focus".to_string());
        let mut un = HashMap::new();
        un.insert(
            Uuid::from_u128(2).to_string(),
            "reserved_separator".to_string(),
        );

        let v: serde_json::Value = serde_json::from_str(
            &render_neighborhood_scoped(&hood(paths, un), "Focus", OutputMode::Json).unwrap(),
        )
        .unwrap();
        // Keyed by nothing — a raw node id is not surfaced by this CLI and
        // would join to nothing else in the payload. The consumer gets the
        // same label the human sees, plus the reason.
        assert_eq!(v["unaddressable"][0]["reason"], "reserved_separator");
        assert_eq!(
            v["unaddressable"][0]["label"],
            "<name contains '.', which separates path segments>"
        );
        // And the rendered neighbor path is the label, not an id.
        let rendered = v.to_string();
        assert!(
            !rendered.contains(&Uuid::from_u128(2).to_string()),
            "node id leaked into walk --json: {rendered}"
        );
    }

    #[test]
    fn a_fully_addressable_slice_reports_nothing_unaddressable() {
        let mut paths = HashMap::new();
        paths.insert(Uuid::from_u128(1).to_string(), "Focus".to_string());
        paths.insert(Uuid::from_u128(2).to_string(), "Other".to_string());
        let out =
            render_neighborhood_scoped(&hood(paths, HashMap::new()), "Focus", OutputMode::Human)
                .unwrap();
        assert!(!out.contains("no addressable path"), "{out}");
        assert!(out.contains("Other"), "{out}");
    }

    #[test]
    fn a_boundary_child_without_a_path_does_not_panic() {
        let b = Uuid::from_u128(1);
        let child = Uuid::from_u128(2);
        let mut paths = HashMap::new();
        paths.insert(b.to_string(), "Cell".to_string());
        let mut un = HashMap::new();
        un.insert(child.to_string(), "ambiguous".to_string());
        let cell = BoundaryResponse {
            version: "v1".to_string(),
            project_id: Uuid::from_u128(9),
            branch: Box::new(models::BranchRef {
                id: Uuid::from_u128(8),
                version: 1,
            }),
            boundary: Box::new(node_of_kind(b, "Cell", models::wire_node::Kind::Boundary)),
            children: vec![node(child, "dup")],
            edges: vec![],
            paths,
            unaddressable: un,
        };
        let out = render_boundary_scoped(&cell, "Cell", OutputMode::Human)
            .expect("must render, not panic");
        assert!(out.contains("share a name"), "{out}");

        // And the JSON payload must not name the node by id. The neighborhood
        // path has had a reporter for this since it shipped; the boundary path
        // emitted the raw map, keyed by UUID, straight into --boundary --json.
        // Ids in output an author consumes are the thing this product exists to
        // hide, and there was no test on this side to catch it.
        let json = render_boundary_scoped(&cell, "Cell", OutputMode::Json).unwrap();
        assert!(
            !json.contains(&child.to_string()),
            "raw node id leaked into --boundary --json:\n{json}"
        );
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let un = v["unaddressable"].as_array().expect("unaddressable array");
        assert_eq!(un.len(), 1, "{json}");
        assert!(un[0]["label"].is_string(), "{json}");
        assert!(un[0]["reason"].is_string(), "{json}");
    }

    #[test]
    fn both_scoped_payloads_shape_unaddressable_the_same_way() {
        // A consumer switching between `walk X` and `walk X --boundary` must not
        // meet two different shapes for the same degradation channel.
        let b = Uuid::from_u128(1);
        let child = Uuid::from_u128(2);
        let mut paths = HashMap::new();
        paths.insert(b.to_string(), "Cell".to_string());
        let mut un = HashMap::new();
        un.insert(child.to_string(), "empty_name".to_string());
        let cell = BoundaryResponse {
            version: "v1".to_string(),
            project_id: Uuid::from_u128(9),
            branch: Box::new(models::BranchRef {
                id: Uuid::from_u128(8),
                version: 1,
            }),
            boundary: Box::new(node_of_kind(b, "Cell", models::wire_node::Kind::Boundary)),
            children: vec![node(child, "")],
            edges: vec![],
            paths: paths.clone(),
            unaddressable: un.clone(),
        };
        let bj: serde_json::Value =
            serde_json::from_str(&render_boundary_scoped(&cell, "Cell", OutputMode::Json).unwrap())
                .unwrap();

        let mut np = HashMap::new();
        np.insert(Uuid::from_u128(1).to_string(), "Focus".to_string());
        let mut nu = HashMap::new();
        nu.insert(Uuid::from_u128(2).to_string(), "empty_name".to_string());
        let nj: serde_json::Value = serde_json::from_str(
            &render_neighborhood_scoped(&hood(np, nu), "Focus", OutputMode::Json).unwrap(),
        )
        .unwrap();

        let keys = |v: &serde_json::Value| -> Vec<String> {
            let mut k: Vec<String> = v["unaddressable"][0]
                .as_object()
                .expect("object entries")
                .keys()
                .cloned()
                .collect();
            k.sort();
            k
        };
        assert_eq!(keys(&bj), keys(&nj), "boundary={bj}\nneighborhood={nj}");
    }
}
