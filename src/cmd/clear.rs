//! `clear` — stage the removal of every top-level node on the bound branch, to
//! wipe it and rebuild in place. Each top-level `delete_node` cascades its
//! subtree + incident edges, so the whole graph is cleared. Requires a prior
//! `pull` (the top-level set comes from the local index); nothing hits the
//! server until `commit`.

use super::context::require_workdir;
use crate::error::CliError;
use crate::output::OutputMode;
use crate::staging::Changeset;
use crate::state::{Index, Stage};

pub fn run(mode: OutputMode) -> Result<(), CliError> {
    let base = require_workdir()?;
    let index = Index::load(&base)?;
    // Without a pull there's no local view of what to clear — fail loud rather
    // than silently "succeed" having staged nothing.
    if index.is_none() {
        return Err(CliError::Other(
            "nothing pulled — run `hydrate pull` first so `clear` knows what's on the branch"
                .to_string(),
        ));
    }
    let mut changeset = Changeset::with_index(Stage::load(&base)?, index);
    let top = changeset.top_level_node_paths();
    for path in &top {
        changeset.remove_node(path)?;
    }
    changeset.into_stage().save(&base)?;

    println!("{}", render(&top, mode));
    Ok(())
}

fn render(paths: &[String], mode: OutputMode) -> String {
    match mode {
        OutputMode::Json => serde_json::json!({
            "cleared": { "top_level": paths, "count": paths.len() }
        })
        .to_string(),
        OutputMode::Human => {
            if paths.is_empty() {
                "Nothing to clear — the branch has no nodes.".to_string()
            } else {
                format!(
                    "Staged removal of {} top-level node{} (cascades the rest): {}.",
                    paths.len(),
                    if paths.len() == 1 { "" } else { "s" },
                    paths.join(", "),
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_lists_the_cleared_top_level_nodes() {
        let out = render(&["Api".to_string(), "Store".to_string()], OutputMode::Human);
        assert!(out.contains("2 top-level nodes"), "{out}");
        assert!(out.contains("Api, Store"), "{out}");
    }

    #[test]
    fn human_reports_an_empty_branch() {
        assert_eq!(
            render(&[], OutputMode::Human),
            "Nothing to clear — the branch has no nodes."
        );
    }

    #[test]
    fn json_carries_the_top_level_set_and_count() {
        let out = render(&["Api".to_string()], OutputMode::Json);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["cleared"]["count"], 1);
        assert_eq!(v["cleared"]["top_level"][0], "Api");
    }
}
