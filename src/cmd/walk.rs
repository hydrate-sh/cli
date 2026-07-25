//! `walk <path>` — the agent's scoped read. Resolves a dotted path to a node via
//! the local index, then fetches just that node's context from the server: the
//! node plus its 1-hop neighborhood by default, or (with `--boundary`) a
//! boundary's scope — its children and the edges interior to it.
//!
//! Where `show` is the human's whole-graph / subtree view, `walk` is the scoped
//! slice an agent reads so it never pulls a large graph into context. Every node
//! is rendered whole (its full [`view::NodeView`]), and `--json` is the agent
//! path. This verb is read-only: it reaches only the scoped read endpoints and
//! never stages or commits anything.

use hydrate_wire::models::{BoundaryResponse, NodeNeighborhoodResponse, WireNode};

use super::context::require_workdir;
use super::view::{self, EdgeView, NodeView};
use crate::client::Client;
use crate::config::Config;
use crate::error::CliError;
use crate::output::OutputMode;
use crate::state::{Binding, Index};

pub fn run(args: crate::cli::WalkArgs, mode: OutputMode) -> Result<(), CliError> {
    let base = require_workdir()?;
    let binding = Binding::load(&base)?.ok_or_else(|| {
        CliError::Other(
            "this working copy is not bound to a branch; run `hydrate fork`".to_string(),
        )
    })?;
    // The path→UUID resolver is the pulled index. Without a pull there is nothing
    // to resolve against — say so, rather than silently guess.
    let index = Index::load(&base)?.ok_or_else(|| {
        CliError::Other("no local graph to resolve against; run `hydrate pull` first".to_string())
    })?;
    let node_id = index.get(&format!("node:{}", args.path)).ok_or_else(|| {
        CliError::InvalidArgument(format!(
            "unknown node '{}'; run `hydrate pull` to refresh, or `hydrate show` to list nodes",
            args.path
        ))
    })?;

    let config = Config::load()?;
    let client = Client::new(&config)?;

    let out = if args.boundary {
        let resp = client.fetch_boundary(binding.project_id, node_id)?;
        render_boundary(&args.path, &resp, mode)?
    } else {
        let resp = client.fetch_node_with_neighbors(binding.project_id, node_id)?;
        render_neighborhood(&args.path, &resp, mode)?
    };
    println!("{out}");
    Ok(())
}

/// Render a node + its 1-hop neighborhood. The focal node keeps the dotted
/// `path` the caller queried; neighbors — which may sit anywhere in the graph —
/// are keyed by their own name (the reliable identity in a scoped slice, where
/// their ancestors are not returned). Edges are translated to dotted
/// `node.port` paths over exactly the returned node set.
fn render_neighborhood(
    path: &str,
    resp: &NodeNeighborhoodResponse,
    mode: OutputMode,
) -> Result<String, CliError> {
    let focus = view::node_view(&resp.node, path.to_string());
    let neighbors: Vec<NodeView> = resp
        .neighbors
        .iter()
        .map(|n| view::node_view(n, n.data.name.clone()))
        .collect();

    let mut labeled: Vec<(String, &WireNode)> = vec![(path.to_string(), &resp.node)];
    for n in &resp.neighbors {
        labeled.push((n.data.name.clone(), n));
    }
    let edges = view::resolve_edges(&view::port_labels(&labeled), &resp.edges)?;

    Ok(match mode {
        OutputMode::Json => serde_json::json!({
            "node": focus,
            "neighbors": neighbors,
            "edges": edges,
            "version": resp.version,
        })
        .to_string(),
        OutputMode::Human => {
            let mut out = format!("Node '{path}' (version {}):", resp.version);
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
            out
        }
    })
}

/// Render a boundary's scope: the boundary node, its children, and the edges
/// interior to it. Children are addressed by their dotted path under the queried
/// boundary (their parent is that boundary, so the path is exact).
fn render_boundary(
    path: &str,
    resp: &BoundaryResponse,
    mode: OutputMode,
) -> Result<String, CliError> {
    let boundary = view::node_view(&resp.boundary, path.to_string());
    let children: Vec<NodeView> = resp
        .children
        .iter()
        .map(|c| view::node_view(c, format!("{path}.{}", c.data.name)))
        .collect();

    let mut labeled: Vec<(String, &WireNode)> = vec![(path.to_string(), &resp.boundary)];
    for c in &resp.children {
        labeled.push((format!("{path}.{}", c.data.name), c));
    }
    let edges = view::resolve_edges(&view::port_labels(&labeled), &resp.edges)?;

    Ok(match mode {
        OutputMode::Json => serde_json::json!({
            "boundary": boundary,
            "children": children,
            "edges": edges,
            "version": resp.version,
        })
        .to_string(),
        OutputMode::Human => {
            let mut out = format!("Boundary '{path}' (version {}):", resp.version);
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

    /// Focal Rater (in raw:HotDog) with a neighbor Maker (out dog:HotDog) wired
    /// Maker.dog -> Rater.raw.
    fn neighborhood() -> NodeNeighborhoodResponse {
        use models::wire_node::Kind;
        let rater_in = port(0xF0, "raw", "HotDog");
        let maker_out = port(0xD0, "dog", "HotDog");
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
        let maker = node(
            0x11,
            "Maker",
            Kind::Behavior,
            Some(0x10),
            vec![],
            vec![maker_out.clone()],
        );
        NodeNeighborhoodResponse {
            branch: Box::new(BranchRef::new(Uuid::from_u128(2), 3)),
            edges: vec![WireEdge {
                id: Uuid::from_u128(0xED),
                source: Uuid::from_u128(0x11),
                source_handle: Some(maker_out.id),
                target: Uuid::from_u128(0x12),
                target_handle: Some(rater_in.id),
            }],
            neighbors: vec![maker],
            node: Box::new(rater),
            project_id: Uuid::from_u128(0xFEED),
            version: "7".to_string(),
        }
    }

    /// Boundary Api { Maker (out dog:HotDog), Rater (in raw:HotDog) }, interior
    /// edge Maker.dog -> Rater.raw.
    fn boundary() -> BoundaryResponse {
        use models::wire_node::Kind;
        let rater_in = port(0xF0, "raw", "HotDog");
        let maker_out = port(0xD0, "dog", "HotDog");
        let api = node(0x10, "Api", Kind::Boundary, None, vec![], vec![]);
        let maker = node(
            0x11,
            "Maker",
            Kind::Behavior,
            Some(0x10),
            vec![],
            vec![maker_out.clone()],
        );
        let rater = node(
            0x12,
            "Rater",
            Kind::Behavior,
            Some(0x10),
            vec![rater_in.clone()],
            vec![],
        );
        BoundaryResponse {
            boundary: Box::new(api),
            branch: Box::new(BranchRef::new(Uuid::from_u128(2), 3)),
            children: vec![maker, rater],
            edges: vec![WireEdge {
                id: Uuid::from_u128(0xED),
                source: Uuid::from_u128(0x11),
                source_handle: Some(maker_out.id),
                target: Uuid::from_u128(0x12),
                target_handle: Some(rater_in.id),
            }],
            project_id: Uuid::from_u128(0xFEED),
            version: "7".to_string(),
        }
    }

    #[test]
    fn neighborhood_json_carries_the_whole_focal_node_neighbors_and_edges() {
        let json = render_neighborhood("Api.Rater", &neighborhood(), OutputMode::Json).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        // The focal node keeps its queried path and is rendered WHOLE (its
        // description + verifications, not a skeleton).
        assert_eq!(v["node"]["path"], "Api.Rater");
        assert_eq!(v["node"]["description"], "Rater does a thing.");
        assert_eq!(v["node"]["verifications"][0]["text"], "score within 0..=10");
        // The 1-hop neighbor is present, keyed by its own name.
        let neighbors = v["neighbors"].as_array().unwrap();
        assert_eq!(neighbors.len(), 1);
        assert_eq!(neighbors[0]["path"], "Maker");
        // The connecting edge is translated to dotted port paths.
        let edges = v["edges"].as_array().unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0]["from"], "Maker.dog");
        assert_eq!(edges[0]["to"], "Api.Rater.raw");
        assert_eq!(v["version"], "7");
    }

    #[test]
    fn neighborhood_human_and_json_carry_the_same_information() {
        let human = render_neighborhood("Api.Rater", &neighborhood(), OutputMode::Human).unwrap();
        assert!(human.contains("Node 'Api.Rater'"), "{human}");
        assert!(human.contains("Rater  [behavior]"), "{human}");
        assert!(human.contains("Rater does a thing."), "{human}");
        assert!(human.contains("score within 0..=10"), "{human}");
        assert!(human.contains("Neighbors:"), "{human}");
        assert!(human.contains("Maker  [behavior]"), "{human}");
        assert!(human.contains("Maker.dog -> Api.Rater.raw"), "{human}");
    }

    #[test]
    fn boundary_json_carries_the_boundary_children_and_interior_edges() {
        let json = render_boundary("Api", &boundary(), OutputMode::Json).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["boundary"]["path"], "Api");
        assert_eq!(v["boundary"]["kind"], "boundary");
        // Children are addressed by their dotted path under the boundary.
        let children = v["children"].as_array().unwrap();
        assert_eq!(children.len(), 2);
        let paths: Vec<&str> = children
            .iter()
            .map(|c| c["path"].as_str().unwrap())
            .collect();
        assert!(paths.contains(&"Api.Maker"), "{paths:?}");
        assert!(paths.contains(&"Api.Rater"), "{paths:?}");
        // The interior edge is translated over the boundary's own scope.
        let edges = v["edges"].as_array().unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0]["from"], "Api.Maker.dog");
        assert_eq!(edges[0]["to"], "Api.Rater.raw");
    }

    #[test]
    fn boundary_human_lists_children_under_the_boundary() {
        let human = render_boundary("Api", &boundary(), OutputMode::Human).unwrap();
        assert!(human.contains("Boundary 'Api'"), "{human}");
        assert!(human.contains("Api  [boundary]"), "{human}");
        assert!(human.contains("Children:"), "{human}");
        assert!(human.contains("Maker  [behavior]"), "{human}");
        assert!(human.contains("Rater  [behavior]"), "{human}");
        assert!(human.contains("Api.Maker.dog -> Api.Rater.raw"), "{human}");
    }

    #[test]
    fn a_corrupt_edge_handle_fails_loud() {
        // An edge naming a port outside the returned slice is corruption, never a
        // silently-dropped connection.
        let mut n = neighborhood();
        n.edges[0].source_handle = Some(Uuid::from_u128(0xBEEF));
        let err = render_neighborhood("Api.Rater", &n, OutputMode::Json).unwrap_err();
        assert!(matches!(err, CliError::State(_)), "got {err:?}");
    }
}
