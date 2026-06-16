//! `fork <name>` — create a working branch off main and bind this directory to
//! it, so subsequent verbs author against that branch.

use std::path::Path;

use hydrate_wire::models::{BranchMeta, ProjectOut};
use uuid::Uuid;

use super::context::{cwd, select_project};
use crate::cli::ForkArgs;
use crate::client::Client;
use crate::config::Config;
use crate::error::CliError;
use crate::output::OutputMode;
use crate::state::{self, Binding, Index, Stage};

pub fn run(args: ForkArgs, mode: OutputMode) -> Result<(), CliError> {
    // A cheap client-side shape check for fast, clear feedback. The server stays
    // the authority on naming — this only rejects input that could never be a
    // valid slug, never decides what the server would accept.
    validate_branch_name(&args.name)?;

    // Forking re-binds this working copy. Reuse a `.hydrate/` above the cwd if
    // one exists, rather than nest a second one that would alias one project to
    // two paths — and refuse if doing so would orphan staged work.
    let base = state::find_root(&cwd()?).map(Ok).unwrap_or_else(cwd)?;
    guard_no_staged_work(&base)?;

    let config = Config::load()?;
    let client = Client::new(&config)?;

    let project = select_project(client.list_projects()?.projects)?;
    // The server does not enforce branch-name uniqueness, so surface a collision
    // here instead of silently creating a second branch with the same name.
    ensure_name_available(&client, project.id, &args.name)?;

    let created = client.create_branch(project.id, &args.name)?;
    let branch = *created.branch;

    Binding {
        project_id: project.id,
        project_name: project.name.clone(),
        branch_id: branch.id,
        branch_name: branch.name.clone(),
    }
    .save(&base)
    // The branch now exists on the server; make the partial success explicit so
    // the user does not blindly retry and create a duplicate.
    .map_err(|e| {
        CliError::State(format!(
            "branch '{}' was created on the server, but this directory could not be bound to it: {e}",
            branch.name
        ))
    })?;

    // Re-binding to a new branch: drop any index from the previously-bound
    // branch first, so that if the pull below fails we fall back to stage-only
    // resolution rather than resolving paths against the OLD branch's stale
    // path→UUID map.
    Index::remove(&base)?;

    // Pull the fresh branch's seed into the local index so the working copy is
    // immediately usable — `edge add` / `node add --parent` can reference the
    // seeded nodes without a separate `pull`. The branch is already created and
    // bound, so a pull failure is a partial success: report it loudly and point
    // at the recovery rather than leaving the author wondering.
    super::pull::refresh_index(&client, branch.id, &base).map_err(|e| {
        CliError::State(format!(
            "branch '{}' was forked and bound, but its graph could not be pulled: {e} — run `hydrate pull`",
            branch.name
        ))
    })?;

    println!("{}", render(&project, &branch, &base, mode));
    Ok(())
}

/// Reject input that could never be a valid branch slug: empty, or containing
/// anything outside letters, digits, `-`, `_`.
fn validate_branch_name(name: &str) -> Result<(), CliError> {
    if name.is_empty() {
        return Err(CliError::InvalidArgument(
            "branch name must not be empty".to_string(),
        ));
    }
    if let Some(bad) = name
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || *c == '-' || *c == '_'))
    {
        return Err(CliError::InvalidArgument(format!(
            "invalid branch name `{name}`: '{bad}' is not allowed — use letters, digits, '-', or '_'"
        )));
    }
    Ok(())
}

/// Refuse to fork if this directory is already bound and has staged work: the
/// stage belongs to the current branch, and forking would silently leave it
/// behind on a branch the user is no longer on.
fn guard_no_staged_work(base: &Path) -> Result<(), CliError> {
    let Some(binding) = Binding::load(base)? else {
        return Ok(());
    };
    let staged = Stage::load(base)?.deltas.len();
    if staged > 0 {
        return Err(CliError::Other(format!(
            "refusing to fork: {staged} staged change(s) on branch '{}' would be left behind — \
             commit or discard them first",
            binding.branch_name
        )));
    }
    Ok(())
}

/// Fail loud if a branch with this name already exists in the project (the
/// server would otherwise accept the duplicate).
fn ensure_name_available(client: &Client, project_id: Uuid, name: &str) -> Result<(), CliError> {
    if client
        .list_branches(project_id)?
        .branches
        .iter()
        .any(|b| b.name == name)
    {
        return Err(CliError::Other(format!(
            "a branch named '{name}' already exists in this project; choose another name"
        )));
    }
    Ok(())
}

/// Build the success output. Returns the string to print so it can be tested.
fn render(project: &ProjectOut, branch: &BranchMeta, base: &Path, mode: OutputMode) -> String {
    match mode {
        OutputMode::Json => serde_json::json!({
            "forked": {
                "project": { "id": project.id, "name": project.name },
                "branch": { "id": branch.id, "name": branch.name, "is_main": branch.is_main },
                "workdir": base.display().to_string(),
                "bound": true,
            }
        })
        .to_string(),
        OutputMode::Human => format!(
            "Forked branch '{}' in project '{}'.\nBound {} to it.",
            branch.name,
            project.name,
            base.display()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> ProjectOut {
        ProjectOut {
            archived: false,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            h2o_schema_version: 1,
            id: Uuid::from_u128(0xA11CE),
            intent: None,
            language: None,
            last_opened_at: None,
            name: "hotdog-rater".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    fn branch() -> BranchMeta {
        BranchMeta {
            base_main_version: 0,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            id: Uuid::from_u128(0xB0B),
            is_main: false,
            last_active_at: "2026-01-01T00:00:00Z".to_string(),
            merged_at: None,
            name: "spicy".to_string(),
            owner_id: None,
            project_id: Uuid::from_u128(0xA11CE),
            status: "active".to_string(),
            version: 1,
        }
    }

    #[test]
    fn accepts_a_plain_slug() {
        assert!(validate_branch_name("hotdog-rater").is_ok());
        assert!(validate_branch_name("Feature_2").is_ok());
    }

    #[test]
    fn rejects_empty_name() {
        let err = validate_branch_name("").unwrap_err();
        assert!(matches!(err, CliError::InvalidArgument(_)), "got {err:?}");
        assert_eq!(err.kind(), "invalid_argument");
    }

    #[test]
    fn rejects_disallowed_characters() {
        // Spaces, slashes, and dots are the common mistakes; each must be loud.
        for bad in ["a b", "a/b", "a.b", "feature!", "naïve"] {
            let err = validate_branch_name(bad).unwrap_err();
            assert!(
                matches!(err, CliError::InvalidArgument(_)),
                "expected rejection for {bad:?}, got {err:?}"
            );
        }
    }

    #[test]
    fn human_render_names_branch_project_and_bound_dir() {
        let out = render(
            &project(),
            &branch(),
            Path::new("/tmp/wd"),
            OutputMode::Human,
        );
        assert!(out.contains("spicy"), "{out}");
        assert!(out.contains("hotdog-rater"), "{out}");
        assert!(out.contains("/tmp/wd"), "{out}");
    }

    #[test]
    fn json_render_carries_ids_names_and_workdir() {
        let out = render(
            &project(),
            &branch(),
            Path::new("/tmp/wd"),
            OutputMode::Json,
        );
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["forked"]["branch"]["name"], "spicy");
        assert_eq!(v["forked"]["branch"]["is_main"], false);
        assert_eq!(v["forked"]["project"]["name"], "hotdog-rater");
        assert_eq!(v["forked"]["workdir"], "/tmp/wd");
        assert_eq!(v["forked"]["bound"], true);
        // The id is present (and not the human name) for machine consumers.
        assert_eq!(
            v["forked"]["branch"]["id"],
            Uuid::from_u128(0xB0B).to_string()
        );
    }

    #[test]
    fn guard_allows_unbound_directory() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(guard_no_staged_work(tmp.path()).is_ok());
    }

    #[test]
    fn guard_refuses_when_bound_with_staged_work() {
        let tmp = tempfile::TempDir::new().unwrap();
        Binding {
            project_id: Uuid::from_u128(1),
            project_name: "p".to_string(),
            branch_id: Uuid::from_u128(2),
            branch_name: "current".to_string(),
        }
        .save(tmp.path())
        .unwrap();
        let mut stage = Stage::empty();
        stage.deltas.push(serde_json::json!({"op": "add_node"}));
        stage.save(tmp.path()).unwrap();

        let err = guard_no_staged_work(tmp.path()).unwrap_err();
        // Loud, and it names the branch the work belongs to.
        assert!(err.to_string().contains("current"), "{err}");
    }

    #[test]
    fn guard_allows_bound_directory_with_empty_stage() {
        let tmp = tempfile::TempDir::new().unwrap();
        Binding {
            project_id: Uuid::from_u128(1),
            project_name: "p".to_string(),
            branch_id: Uuid::from_u128(2),
            branch_name: "current".to_string(),
        }
        .save(tmp.path())
        .unwrap();
        Stage::empty().save(tmp.path()).unwrap();
        assert!(guard_no_staged_work(tmp.path()).is_ok());
    }
}
