//! `branches` — list the working branches of the bound (or sole) project,
//! marking the branch this directory is currently bound to.

use hydrate_wire::models::BranchMeta;
use serde::Serialize;
use uuid::Uuid;

use super::context::{cwd, select_project};
use crate::client::Client;
use crate::config::Config;
use crate::error::CliError;
use crate::output::OutputMode;
use crate::state::{self, Binding};

pub fn run(mode: OutputMode) -> Result<(), CliError> {
    let config = Config::load()?;
    let client = Client::new(&config)?;

    // Prefer the workdir binding for the project + the branch to mark; fall back
    // to the single-project rule when this directory is not bound to anything.
    let binding = current_binding()?;
    let (project_id, project_name, bound) = match binding {
        Some(b) => (b.project_id, b.project_name, Some(b.branch_id)),
        None => {
            let p = select_project(client.list_projects()?.projects)?;
            (p.id, p.name, None)
        }
    };

    let listed = client.list_branches(project_id)?;
    println!(
        "{}",
        render(
            project_id,
            &project_name,
            &rows(&listed.branches, bound),
            mode
        )
    );
    Ok(())
}

/// Load the binding for the working copy this directory belongs to, if any.
fn current_binding() -> Result<Option<Binding>, CliError> {
    match state::find_root(&cwd()?) {
        Some(root) => Binding::load(&root),
        None => Ok(None),
    }
}

#[derive(Debug, PartialEq, Serialize)]
struct BranchRow {
    id: Uuid,
    name: String,
    is_main: bool,
    /// True for exactly the branch this directory is bound to.
    bound: bool,
}

/// Project the wire branches into display rows, marking the bound one.
fn rows(branches: &[BranchMeta], bound: Option<Uuid>) -> Vec<BranchRow> {
    branches
        .iter()
        .map(|b| BranchRow {
            id: b.id,
            name: b.name.clone(),
            is_main: b.is_main,
            bound: Some(b.id) == bound,
        })
        .collect()
}

/// Build the listing output. Returns the string to print so it can be tested.
/// The JSON `project` shape (`{id, name}`) matches `fork`'s output.
fn render(project_id: Uuid, project_name: &str, rows: &[BranchRow], mode: OutputMode) -> String {
    match mode {
        OutputMode::Json => serde_json::json!({
            "project": { "id": project_id, "name": project_name },
            "branches": rows,
        })
        .to_string(),
        OutputMode::Human => {
            if rows.is_empty() {
                return format!("No branches in project '{project_name}'.");
            }
            let mut out = format!("Branches in project '{project_name}':");
            for r in rows {
                let marker = if r.bound { '*' } else { ' ' };
                let tag = if r.is_main { " (main)" } else { "" };
                out.push_str(&format!("\n  {marker} {}{}", r.name, tag));
            }
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(name: &str, id: u128, is_main: bool) -> BranchMeta {
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

    #[test]
    fn marks_exactly_the_bound_branch() {
        let branches = [meta("main", 1, true), meta("feature", 2, false)];
        let rows = rows(&branches, Some(Uuid::from_u128(2)));
        assert_eq!(rows.iter().filter(|r| r.bound).count(), 1);
        let bound = rows.iter().find(|r| r.bound).unwrap();
        assert_eq!(bound.name, "feature");
        // The non-bound branch is not marked.
        assert!(!rows.iter().find(|r| r.name == "main").unwrap().bound);
    }

    #[test]
    fn marks_nothing_when_unbound() {
        let branches = [meta("main", 1, true), meta("feature", 2, false)];
        let rows = rows(&branches, None);
        assert!(rows.iter().all(|r| !r.bound));
    }

    #[test]
    fn preserves_branch_order_and_fields() {
        let branches = [meta("main", 1, true), meta("feature", 2, false)];
        let rows = rows(&branches, None);
        assert_eq!(
            rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            ["main", "feature"]
        );
        assert!(rows[0].is_main && !rows[1].is_main);
    }

    #[test]
    fn human_render_stars_the_bound_branch_and_tags_main() {
        let branches = [meta("main", 1, true), meta("feature", 2, false)];
        let pid = Uuid::from_u128(0xFEED);
        let out = render(
            pid,
            "proj",
            &rows(&branches, Some(Uuid::from_u128(2))),
            OutputMode::Human,
        );
        // The bound branch (feature) is starred; main is tagged but not starred.
        assert!(out.contains("* feature"), "{out}");
        assert!(out.contains("main (main)"), "{out}");
        assert!(!out.contains("* main"), "{out}");
    }

    #[test]
    fn json_render_marks_bound_and_carries_project_id() {
        let branches = [meta("main", 1, true), meta("feature", 2, false)];
        let pid = Uuid::from_u128(0xFEED);
        let out = render(
            pid,
            "proj",
            &rows(&branches, Some(Uuid::from_u128(2))),
            OutputMode::Json,
        );
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["project"]["id"], pid.to_string());
        assert_eq!(v["project"]["name"], "proj");
        let arr = v["branches"].as_array().unwrap();
        let feature = arr.iter().find(|b| b["name"] == "feature").unwrap();
        let main = arr.iter().find(|b| b["name"] == "main").unwrap();
        assert_eq!(feature["bound"], true);
        assert_eq!(main["bound"], false);
        assert_eq!(main["is_main"], true);
    }

    #[test]
    fn human_render_handles_empty_list() {
        let out = render(Uuid::from_u128(1), "proj", &[], OutputMode::Human);
        assert!(out.contains("No branches"), "{out}");
    }
}
