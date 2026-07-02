//! Shared branch-context helpers used by the project/branch verbs.

use std::path::PathBuf;

use hydrate_wire::models::ProjectOut;
use uuid::Uuid;

use crate::error::CliError;
use crate::state::{self, Binding};

/// The environment variable that names the project to act on when no `--project`
/// flag is given and the working copy is not bound.
pub const PROJECT_ENV: &str = "HYD_PROJECT";

/// The single-project rule.
///
/// There is no project-selection flag and no project-init verb yet, so the
/// common path acts on the account's one project. Pick the sole **active**
/// project, or fail loud: zero projects is [`CliError::NoProject`], more than
/// one is [`CliError::AmbiguousProject`]. We never silently pick the first of
/// several — that would act on the wrong project without a word.
pub fn select_project(projects: Vec<ProjectOut>) -> Result<ProjectOut, CliError> {
    let mut iter = projects.into_iter().filter(|p| !p.archived);
    match (iter.next(), iter.next()) {
        (None, _) => Err(CliError::NoProject),
        (Some(only), None) => Ok(only),
        // Two already in hand; count the remainder for an accurate message.
        (Some(_), Some(_)) => Err(CliError::AmbiguousProject {
            count: 2 + iter.count(),
        }),
    }
}

/// Pick the effective project selection from the precedence chain
/// **flag > env > binding**, or `None` (fall through to the single-active rule).
///
/// Pure over its inputs so the ordering is unit-testable without touching the
/// real environment or filesystem (mirrors [`crate::config::Config::from_lookup`]).
/// An env value that is blank/whitespace is treated as unset, so an exported-but-
/// empty `HYD_PROJECT` doesn't shadow the binding.
pub fn choose_selection(
    flag: Option<&str>,
    env: Option<String>,
    binding: Option<&str>,
) -> Option<String> {
    if let Some(f) = flag {
        return Some(f.to_string());
    }
    if let Some(e) = env.filter(|s| !s.trim().is_empty()) {
        return Some(e);
    }
    binding.map(str::to_string)
}

/// Read `HYD_PROJECT` from the real environment (blank is treated as unset by
/// [`choose_selection`]). Kept thin so the resolver logic stays pure/testable.
pub fn env_project() -> Option<String> {
    std::env::var(PROJECT_ENV).ok()
}

/// Resolve a project from an explicit `selection` (an id or a name), or fall
/// back to the single-active rule ([`select_project`]) when `selection` is
/// `None`.
///
/// A selection that parses as a UUID matches a project by id; otherwise it
/// matches by exact name. An explicit selection may target an archived project
/// (you named it on purpose), but it must be unambiguous: a name shared by more
/// than one project, or an id/name that matches nothing, fails loud rather than
/// guessing. This is the flag/env/binding path; the `None` fall-through keeps
/// `select_project`'s exactly-one-active behavior unchanged.
pub fn resolve_project(
    selection: Option<&str>,
    projects: Vec<ProjectOut>,
) -> Result<ProjectOut, CliError> {
    let Some(sel) = selection else {
        return select_project(projects);
    };

    // A UUID selection addresses a project by id, unambiguously.
    if let Ok(id) = Uuid::parse_str(sel) {
        return projects.into_iter().find(|p| p.id == id).ok_or_else(|| {
            CliError::InvalidArgument(format!(
                "no project with id '{sel}'; run `hydrate projects` to see the ids you can use"
            ))
        });
    }

    // Otherwise, match by exact name — which must be unique among all projects.
    let mut matches: Vec<ProjectOut> = projects.into_iter().filter(|p| p.name == sel).collect();
    match matches.len() {
        0 => Err(CliError::InvalidArgument(format!(
            "no project named '{sel}'; run `hydrate projects` to see the names and ids you can use"
        ))),
        1 => Ok(matches.remove(0)),
        n => Err(CliError::InvalidArgument(format!(
            "'{sel}' is not unique — {n} projects share that name; select it by id instead \
             (run `hydrate projects` to see the ids)"
        ))),
    }
}

/// The current working directory, as a loud error rather than a panic.
pub fn cwd() -> Result<PathBuf, CliError> {
    std::env::current_dir()
        .map_err(|e| CliError::State(format!("could not determine the current directory: {e}")))
}

/// Load the [`Binding`] for the working copy this directory belongs to, if any.
/// `None` when the directory is not inside a bound `.hydrate/` working copy —
/// a normal state the project/branch verbs handle by resolving another way.
pub fn current_binding() -> Result<Option<Binding>, CliError> {
    match state::find_root(&cwd()?) {
        Some(root) => Binding::load(&root),
        None => Ok(None),
    }
}

/// The root of the `.hydrate/` working copy this directory belongs to, or a
/// loud [`CliError::NotInWorkdir`] — the staging and inspection verbs have
/// nowhere to read or write without one.
pub fn require_workdir() -> Result<PathBuf, CliError> {
    state::find_root(&cwd()?).ok_or(CliError::NotInWorkdir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn project(name: &str, archived: bool) -> ProjectOut {
        project_with_id(name, archived, Uuid::from_u128(0xABCD))
    }

    fn project_with_id(name: &str, archived: bool, id: Uuid) -> ProjectOut {
        ProjectOut {
            archived,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            h2o_schema_version: 1,
            id,
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

    #[test]
    fn flag_beats_env_beats_binding_beats_auto() {
        // Flag wins over everything.
        assert_eq!(
            choose_selection(Some("flag"), Some("env".into()), Some("bind")),
            Some("flag".to_string())
        );
        // No flag: env wins over binding.
        assert_eq!(
            choose_selection(None, Some("env".into()), Some("bind")),
            Some("env".to_string())
        );
        // No flag, no env: the binding is used.
        assert_eq!(
            choose_selection(None, None, Some("bind")),
            Some("bind".to_string())
        );
        // A blank env does not shadow the binding (exported-but-empty is "unset").
        assert_eq!(
            choose_selection(None, Some("   ".into()), Some("bind")),
            Some("bind".to_string())
        );
        // Nothing set: fall through (auto / single-active rule).
        assert_eq!(choose_selection(None, None, None), None);
    }

    #[test]
    fn selection_by_id_and_by_name() {
        let id = Uuid::from_u128(0x1234);
        let projects = || {
            vec![
                project_with_id("alpha", false, id),
                project_with_id("beta", false, Uuid::from_u128(0x5678)),
            ]
        };
        // By exact name.
        let by_name = resolve_project(Some("beta"), projects()).unwrap();
        assert_eq!(by_name.name, "beta");
        // By id string.
        let by_id = resolve_project(Some(&id.to_string()), projects()).unwrap();
        assert_eq!(by_id.name, "alpha");
        assert_eq!(by_id.id, id);
    }

    #[test]
    fn selection_can_target_an_archived_project() {
        // Explicit selection is deliberate, so archived is fine — unlike the
        // single-active fall-through, which skips archived projects.
        let chosen = resolve_project(
            Some("old"),
            vec![project("old", true), project("live", false)],
        )
        .unwrap();
        assert_eq!(chosen.name, "old");
    }

    #[test]
    fn unknown_selection_fails_loud() {
        // Unknown name.
        let err = resolve_project(Some("ghost"), vec![project("real", false)]).unwrap_err();
        assert!(matches!(err, CliError::InvalidArgument(_)), "got {err:?}");
        // Unknown id.
        let missing = Uuid::from_u128(0xDEAD).to_string();
        let err = resolve_project(Some(&missing), vec![project("real", false)]).unwrap_err();
        assert!(matches!(err, CliError::InvalidArgument(_)), "got {err:?}");
    }

    #[test]
    fn duplicate_name_selection_is_ambiguous() {
        let err = resolve_project(
            Some("dup"),
            vec![
                project_with_id("dup", false, Uuid::from_u128(1)),
                project_with_id("dup", false, Uuid::from_u128(2)),
            ],
        )
        .unwrap_err();
        // Loud, and it points at selecting by id to disambiguate.
        assert!(matches!(err, CliError::InvalidArgument(_)), "got {err:?}");
        assert!(err.to_string().contains("id"), "{err}");
    }

    #[test]
    fn no_selection_falls_through_to_single_active_rule() {
        // One active project: resolved without a selection.
        let chosen = resolve_project(None, vec![project("only", false)]).unwrap();
        assert_eq!(chosen.name, "only");
        // Multiple active with no selection: the ambiguous-project error stands.
        let err =
            resolve_project(None, vec![project("a", false), project("b", false)]).unwrap_err();
        assert!(
            matches!(err, CliError::AmbiguousProject { .. }),
            "got {err:?}"
        );
    }
}
