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

/// Where a project selection came from — used to word the not-found error so it
/// blames the right input (a stale binding is not something the user typed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionSource {
    /// The `--project` flag.
    Flag,
    /// The `HYD_PROJECT` environment variable.
    Env,
    /// This directory's `.hydrate/` binding.
    Binding,
}

/// A resolved project selection: the value (id or name) plus where it came from.
pub type Selection = (String, SelectionSource);

/// Pick the effective project selection from the precedence chain
/// **flag > env > binding**, or `None` (fall through to the single-active rule),
/// tagging each with its [`SelectionSource`].
///
/// Pure over its inputs so the ordering is unit-testable without touching the
/// real environment or filesystem (mirrors [`crate::config::Config::from_lookup`]).
/// An env value that is blank/whitespace is treated as unset, so an exported-but-
/// empty `HYD_PROJECT` doesn't shadow the binding.
pub fn choose_selection(
    flag: Option<&str>,
    env: Option<String>,
    binding: Option<&str>,
) -> Option<Selection> {
    if let Some(f) = flag {
        return Some((f.to_string(), SelectionSource::Flag));
    }
    if let Some(e) = env.filter(|s| !s.trim().is_empty()) {
        return Some((e, SelectionSource::Env));
    }
    binding.map(|b| (b.to_string(), SelectionSource::Binding))
}

/// Read `HYD_PROJECT` from the real environment. A blank value is treated as
/// unset by [`choose_selection`]; a value that is not valid Unicode is loud
/// corruption, not a silent `None` — a malformed `HYD_PROJECT` must surface.
pub fn env_project() -> Result<Option<String>, CliError> {
    match std::env::var(PROJECT_ENV) {
        Ok(v) => Ok(Some(v)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(CliError::InvalidArgument(format!(
            "{PROJECT_ENV} is set to a value that is not valid text; unset it or set a project name/id"
        ))),
    }
}

/// Resolve a project from an explicit `selection` (an id or a name, plus its
/// source), or fall back to the single-active rule ([`select_project`]) when
/// `selection` is `None`.
///
/// A selection that parses as a UUID matches a project by id; otherwise it
/// matches by exact name. An explicit selection may target an archived project
/// (you named it on purpose), but it must be unambiguous: a name shared by more
/// than one project, or an id/name that matches nothing, fails loud rather than
/// guessing. When the miss came from this directory's binding, the error says so
/// (it blames the binding, not an id the user never typed). The `None`
/// fall-through keeps `select_project`'s exactly-one-active behavior unchanged.
pub fn resolve_project(
    selection: Option<Selection>,
    projects: Vec<ProjectOut>,
) -> Result<ProjectOut, CliError> {
    let Some((sel, source)) = selection else {
        return select_project(projects);
    };

    // A UUID selection addresses a project by id, unambiguously.
    if let Ok(id) = Uuid::parse_str(&sel) {
        return projects
            .into_iter()
            .find(|p| p.id == id)
            .ok_or_else(|| not_found(&sel, source, "id"));
    }

    // Otherwise, match by exact name — which must be unique among all projects.
    let mut matches: Vec<ProjectOut> = projects.into_iter().filter(|p| p.name == sel).collect();
    match matches.len() {
        0 => Err(not_found(&sel, source, "name")),
        1 => Ok(matches.remove(0)),
        n => Err(CliError::InvalidArgument(format!(
            "'{sel}' is not unique — {n} projects share that name; select it by id instead \
             (run `hydrate projects` to see the ids)"
        ))),
    }
}

/// Word the "no such project" error for the selection's source. A stale binding
/// gets a message that names the binding (not the raw id), so the user isn't
/// blamed for an id they never typed.
fn not_found(sel: &str, source: SelectionSource, kind: &str) -> CliError {
    match source {
        SelectionSource::Binding => CliError::Other(format!(
            "this directory is bound to project '{sel}', which is no longer available; \
             run `hydrate projects` to see your projects (and `hydrate fork` to re-bind)"
        )),
        SelectionSource::Flag | SelectionSource::Env => CliError::InvalidArgument(format!(
            "no project with {kind} '{sel}'; run `hydrate projects` to see the names and ids you can use"
        )),
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
        use SelectionSource::*;
        // Flag wins over everything (and is tagged as coming from the flag).
        assert_eq!(
            choose_selection(Some("flag"), Some("env".into()), Some("bind")),
            Some(("flag".to_string(), Flag))
        );
        // No flag: env wins over binding.
        assert_eq!(
            choose_selection(None, Some("env".into()), Some("bind")),
            Some(("env".to_string(), Env))
        );
        // No flag, no env: the binding is used (and tagged as such).
        assert_eq!(
            choose_selection(None, None, Some("bind")),
            Some(("bind".to_string(), Binding))
        );
        // A blank env does not shadow the binding (exported-but-empty is "unset").
        assert_eq!(
            choose_selection(None, Some("   ".into()), Some("bind")),
            Some(("bind".to_string(), Binding))
        );
        // Nothing set: fall through (auto / single-active rule).
        assert_eq!(choose_selection(None, None, None), None);
    }

    fn sel(value: &str) -> Option<Selection> {
        Some((value.to_string(), SelectionSource::Flag))
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
        let by_name = resolve_project(sel("beta"), projects()).unwrap();
        assert_eq!(by_name.name, "beta");
        // By id string.
        let by_id = resolve_project(sel(&id.to_string()), projects()).unwrap();
        assert_eq!(by_id.name, "alpha");
        assert_eq!(by_id.id, id);
    }

    #[test]
    fn selection_can_target_an_archived_project() {
        // Explicit selection is deliberate, so archived is fine — unlike the
        // single-active fall-through, which skips archived projects.
        let chosen = resolve_project(
            sel("old"),
            vec![project("old", true), project("live", false)],
        )
        .unwrap();
        assert_eq!(chosen.name, "old");
    }

    #[test]
    fn unknown_selection_fails_loud() {
        // Unknown name.
        let err = resolve_project(sel("ghost"), vec![project("real", false)]).unwrap_err();
        assert!(matches!(err, CliError::InvalidArgument(_)), "got {err:?}");
        // Unknown id.
        let missing = Uuid::from_u128(0xDEAD).to_string();
        let err = resolve_project(sel(&missing), vec![project("real", false)]).unwrap_err();
        assert!(matches!(err, CliError::InvalidArgument(_)), "got {err:?}");
    }

    #[test]
    fn vanished_binding_project_blames_the_binding_not_the_id() {
        // A stale binding to a now-gone project must not blame an id the user
        // never typed — it names the binding and points at recovery.
        let gone = Uuid::from_u128(0xC0FFEE).to_string();
        let err = resolve_project(
            Some((gone.clone(), SelectionSource::Binding)),
            vec![project("still-here", false)],
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("bound to project"), "{msg}");
        assert!(msg.contains("no longer available"), "{msg}");
        assert!(msg.contains("hydrate projects"), "{msg}");
        // A flag/env miss keeps the plain "no project with id" wording.
        let flag_err = resolve_project(sel(&gone), vec![project("still-here", false)]).unwrap_err();
        assert!(
            flag_err.to_string().contains("no project with id"),
            "{flag_err}"
        );
    }

    #[test]
    fn duplicate_name_selection_is_ambiguous() {
        let err = resolve_project(
            sel("dup"),
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
