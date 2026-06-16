//! `fork <name>` — create a working branch off main and bind this directory to
//! it, so subsequent verbs author against that branch.

use hydrate_wire::models::{BranchMeta, ProjectOut};

use super::context::{cwd, select_project};
use crate::cli::ForkArgs;
use crate::client::Client;
use crate::config::Config;
use crate::error::CliError;
use crate::output::OutputMode;
use crate::state::{self, Binding};

pub fn run(args: ForkArgs, mode: OutputMode) -> Result<(), CliError> {
    // A cheap client-side shape check for fast, clear feedback. The server stays
    // the authority on naming and collisions — this only rejects input that
    // could never be a valid slug, never decides what the server would accept.
    validate_branch_name(&args.name)?;

    let config = Config::load()?;
    let client = Client::new(&config)?;

    let project = select_project(client.list_projects()?.projects)?;
    let created = client.create_branch(project.id, &args.name)?;
    let branch = *created.branch;

    // Bind the working copy. If a `.hydrate/` already exists above us, reuse it
    // rather than nest a second one that would alias one project to two paths.
    let base = state::find_root(&cwd()?).map_or_else(cwd, Ok)?;
    Binding {
        project_id: project.id,
        project_name: project.name.clone(),
        branch_id: branch.id,
        branch_name: branch.name.clone(),
    }
    .save(&base)?;

    render(&project, &branch, mode);
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

fn render(project: &ProjectOut, branch: &BranchMeta, mode: OutputMode) {
    match mode {
        OutputMode::Json => {
            let v = serde_json::json!({
                "forked": {
                    "project": { "id": project.id, "name": project.name },
                    "branch": { "id": branch.id, "name": branch.name, "is_main": branch.is_main },
                    "bound": true,
                }
            });
            println!("{v}");
        }
        OutputMode::Human => {
            println!(
                "Forked branch '{}' in project '{}'.",
                branch.name, project.name
            );
            println!("This directory is now bound to it.");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
