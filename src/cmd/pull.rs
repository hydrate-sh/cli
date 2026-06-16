//! `pull` — fetch the bound branch's live graph and refresh the local
//! resolution index, so authoring verbs can reference already-committed nodes
//! by their dotted path. Read-only on the server; it never mutates the branch
//! and never touches the staged changeset.

use std::path::Path;

use uuid::Uuid;

use super::context::require_workdir;
use crate::client::Client;
use crate::config::Config;
use crate::error::CliError;
use crate::output::OutputMode;
use crate::staging::index_from_graph;
use crate::state::Binding;

/// What a refresh recorded, for rendering.
pub struct PullStats {
    pub nodes: usize,
    pub edges: usize,
    pub version: i32,
}

pub fn run(mode: OutputMode) -> Result<(), CliError> {
    let base = require_workdir()?;
    let binding = Binding::load(&base)?.ok_or_else(|| {
        CliError::Other(
            "this working copy is not bound to a branch; run `hydrate fork`".to_string(),
        )
    })?;

    let config = Config::load()?;
    let client = Client::new(&config)?;
    let stats = refresh_index(&client, binding.branch_id, &base)?;

    println!("{}", render(&binding, &stats, mode));
    Ok(())
}

/// Fetch `branch_id`'s graph, build the resolution index, and write it under
/// `base/.hydrate/`. Shared by `pull` and `fork` (which pulls its fresh seed so
/// the working copy is immediately usable). Returns the snapshot's stats.
pub fn refresh_index(client: &Client, branch_id: Uuid, base: &Path) -> Result<PullStats, CliError> {
    let graph = client.fetch_branch_graph(branch_id)?;
    let index = index_from_graph(&graph)?;
    let stats = PullStats {
        nodes: graph.nodes.len(),
        edges: graph.edges.len(),
        version: index.version,
    };
    index.save(base)?;
    Ok(stats)
}

fn render(binding: &Binding, stats: &PullStats, mode: OutputMode) -> String {
    match mode {
        OutputMode::Json => serde_json::json!({
            "pulled": {
                "branch": binding.branch_name,
                "nodes": stats.nodes,
                "edges": stats.edges,
                "version": stats.version,
            }
        })
        .to_string(),
        OutputMode::Human => format!(
            "Pulled branch '{}' at version {}: {} node{}, {} edge{} now addressable.",
            binding.branch_name,
            stats.version,
            stats.nodes,
            if stats.nodes == 1 { "" } else { "s" },
            stats.edges,
            if stats.edges == 1 { "" } else { "s" },
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding() -> Binding {
        Binding {
            project_id: Uuid::from_u128(1),
            project_name: "p".to_string(),
            branch_id: Uuid::from_u128(2),
            branch_name: "spicy".to_string(),
        }
    }

    #[test]
    fn human_render_reports_branch_version_and_counts() {
        let out = render(
            &binding(),
            &PullStats {
                nodes: 9,
                edges: 4,
                version: 5,
            },
            OutputMode::Human,
        );
        assert!(out.contains("spicy"), "{out}");
        assert!(out.contains("version 5"), "{out}");
        assert!(out.contains("9 nodes"), "{out}");
        assert!(out.contains("4 edges"), "{out}");
    }

    #[test]
    fn human_render_singularizes_one_node_one_edge() {
        let out = render(
            &binding(),
            &PullStats {
                nodes: 1,
                edges: 1,
                version: 2,
            },
            OutputMode::Human,
        );
        assert!(out.contains("1 node,"), "{out}");
        assert!(out.contains("1 edge "), "{out}");
        assert!(!out.contains("1 nodes"), "{out}");
        assert!(!out.contains("1 edges"), "{out}");
    }

    #[test]
    fn json_render_carries_branch_counts_and_version() {
        let out = render(
            &binding(),
            &PullStats {
                nodes: 9,
                edges: 4,
                version: 5,
            },
            OutputMode::Json,
        );
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["pulled"]["branch"], "spicy");
        assert_eq!(v["pulled"]["nodes"], 9);
        assert_eq!(v["pulled"]["edges"], 4);
        assert_eq!(v["pulled"]["version"], 5);
    }
}
