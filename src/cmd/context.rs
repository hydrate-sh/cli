//! Shared branch-context helpers used by the project/branch verbs.

use std::path::PathBuf;

use hydrate_wire::models::ProjectOut;

use crate::error::CliError;

/// The single-project rule.
///
/// M1 has no project-selection flag and no project-init verb, so the common
/// path acts on the account's one project. Pick the sole **active** project, or
/// fail loud: zero projects is [`CliError::NoProject`], more than one is
/// [`CliError::AmbiguousProject`]. We never silently pick the first of several —
/// that would act on the wrong project without a word.
pub fn select_project(projects: Vec<ProjectOut>) -> Result<ProjectOut, CliError> {
    let active = projects.into_iter().filter(|p| !p.archived);
    let mut iter = active;
    match (iter.next(), iter.next()) {
        (None, _) => Err(CliError::NoProject),
        (Some(only), None) => Ok(only),
        // Two already in hand; count the remainder for an accurate message.
        (Some(_), Some(_)) => Err(CliError::AmbiguousProject {
            count: 2 + iter.count(),
        }),
    }
}

/// The current working directory, as a loud error rather than a panic.
pub fn cwd() -> Result<PathBuf, CliError> {
    std::env::current_dir()
        .map_err(|e| CliError::State(format!("could not determine the current directory: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn project(name: &str, archived: bool) -> ProjectOut {
        ProjectOut {
            archived,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            h2o_schema_version: 1,
            id: Uuid::from_u128(0xABCD),
            intent: None,
            language: None,
            last_opened_at: None,
            name: name.to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn single_active_project_is_selected() {
        let chosen = select_project(vec![project("only", false)]).unwrap();
        assert_eq!(chosen.name, "only");
    }

    #[test]
    fn no_project_fails_loud() {
        let err = select_project(vec![]).unwrap_err();
        assert!(matches!(err, CliError::NoProject), "got {err:?}");
        assert_eq!(err.kind(), "no_project");
    }

    #[test]
    fn archived_projects_do_not_count() {
        // One active, one archived -> unambiguous: the active one wins.
        let chosen = select_project(vec![project("live", false), project("old", true)]).unwrap();
        assert_eq!(chosen.name, "live");
        // All archived -> no project to act on.
        let err = select_project(vec![project("old", true)]).unwrap_err();
        assert!(matches!(err, CliError::NoProject), "got {err:?}");
    }

    #[test]
    fn multiple_active_projects_are_ambiguous_with_count() {
        let err = select_project(vec![
            project("a", false),
            project("b", false),
            project("c", false),
        ])
        .unwrap_err();
        match err {
            CliError::AmbiguousProject { count } => assert_eq!(count, 3),
            other => panic!("expected AmbiguousProject, got {other:?}"),
        }
    }
}
