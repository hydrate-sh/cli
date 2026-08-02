//! `project` — create, archive, delete, and rename projects: the mutating
//! counterpart to the read-only `projects` listing.
//!
//! Every verb here addresses its target by NAME, resolved against
//! `GET /v1/projects` — never a UUID; ids stay an internal wire detail these
//! commands don't surface (`hydrate projects` is the one place that shows
//! them, for `--project`). Two consequences of that choice, both intentional:
//!
//! * the match must be EXACT — no fuzzy/substring matching, so a typo fails
//!   loud instead of silently landing on the wrong project;
//! * `GET /v1/projects` excludes archived projects (server behavior, not a CLI
//!   choice — see its own doc comment), so an ALREADY-archived project cannot
//!   be resolved by name through these verbs today. `archive`/`rename`/`delete`
//!   on such a name fail with a clear, specific message that says why, rather
//!   than a bare 404 or a silent no-op. See [`find_by_name`].
//!
//! `delete` is the one irreversible verb here. Per `stage discard`'s posture,
//! there is no confirmation prompt — this CLI is driven non-interactively and
//! a prompt would break piping — so the mitigation is saying plainly, before
//! the call, exactly what is about to be destroyed and that it cannot be
//! undone.

use hydrate_wire::models::ProjectOut;

use crate::cli::{ProjectCreateArgs, ProjectNameArgs, ProjectRenameArgs};
use crate::client::Client;
use crate::config::Config;
use crate::error::CliError;
use crate::output::OutputMode;

pub fn create(args: ProjectCreateArgs, mode: OutputMode) -> Result<(), CliError> {
    let config = Config::load()?;
    let client = Client::new(&config)?;
    let created = client.create_project(&args.name)?;
    println!("{}", render_create(&created.project, mode));
    Ok(())
}

pub fn archive(args: ProjectNameArgs, mode: OutputMode) -> Result<(), CliError> {
    let config = Config::load()?;
    let client = Client::new(&config)?;
    let project = find_by_name(client.list_projects()?.projects, &args.name)?;
    let patched = client.patch_project(project.id, None, Some(true))?;
    println!("{}", render_archive(&patched.project, mode));
    Ok(())
}

pub fn delete(args: ProjectNameArgs, mode: OutputMode) -> Result<(), CliError> {
    let config = Config::load()?;
    let client = Client::new(&config)?;
    let project = find_by_name(client.list_projects()?.projects, &args.name)?;

    // The record of what's about to go, printed BEFORE the irreversible call —
    // mirrors `stage discard`: no confirmation prompt, so this announcement is
    // the entire mitigation. Human-only: JSON has one document, emitted after.
    if let OutputMode::Human = mode {
        eprintln!(
            "Deleting project '{}' — this permanently removes its branches, graph, \
             and stored artifacts. This cannot be undone.",
            project.name
        );
    }

    client
        .delete_project(project.id)
        .map_err(translate_delete_error)?;

    println!("{}", render_delete(&project.name, mode));
    Ok(())
}

pub fn rename(args: ProjectRenameArgs, mode: OutputMode) -> Result<(), CliError> {
    let config = Config::load()?;
    let client = Client::new(&config)?;
    let project = find_by_name(client.list_projects()?.projects, &args.old_name)?;
    let patched = client.patch_project(project.id, Some(&args.new_name), None)?;
    println!("{}", render_rename(&args.old_name, &patched.project, mode));
    Ok(())
}

/// A bare 403 from `delete` is an unacceptable UX: this is the ONE `/v1` route
/// gated by `project:delete` (a scope separate from, and not implied by,
/// `graph:write`), so a 403 here means specifically "this key was never minted
/// with permission to delete", not "not authenticated" or some other gate.
///
/// This is inferred from the ROUTE alone, not from the response body — every
/// `/v1` scope gate returns the fixed `{"detail": "forbidden"}` string with no
/// `code` or `missing_scope` field, so there is nothing in the wire to key off.
/// That makes the mapping fragile: it is only correct because this call site
/// hits exactly one route with exactly one extra scope requirement. If
/// `DELETE /v1/projects/{id}` ever grows a second scope gate, this needs
/// revisiting — a 403 could then mean either one.
fn translate_delete_error(err: CliError) -> CliError {
    match err {
        CliError::Service { status: 403, .. } => CliError::MissingScope {
            scope: "project:delete".to_string(),
        },
        other => other,
    }
}

/// Resolve `name` to a project by EXACT match against the given listing (the
/// caller passes `GET /v1/projects`'s result), which excludes archived
/// projects — a server behavior, not a choice made here.
///
/// Two loud failure modes, deliberately distinct from a panic or a raw 404
/// passthrough:
///
/// * zero matches — genuinely unknown, OR archived and therefore invisible to
///   this listing. The two are indistinguishable from here (there is no
///   archived-inclusive listing route to check against), so the message says
///   that plainly instead of asserting "no such project" as if it never
///   existed — an already-archived project must not read as a typo;
/// * more than one match — the server keeps active project names unique per
///   caller, so this should not be reachable; refusing instead of silently
///   picking one avoids acting on the wrong project if that invariant ever
///   breaks.
fn find_by_name(projects: Vec<ProjectOut>, name: &str) -> Result<ProjectOut, CliError> {
    let mut matches: Vec<ProjectOut> = projects.into_iter().filter(|p| p.name == name).collect();
    match matches.len() {
        0 => Err(CliError::InvalidArgument(format!(
            "no active project named '{name}'; run `hydrate projects` to check the exact \
             spelling. If '{name}' is an ARCHIVED project, this verb can't reach it — \
             archived projects don't appear in that listing, so their names aren't \
             resolvable here today; manage it at https://hydrate.sh instead."
        ))),
        1 => Ok(matches.remove(0)),
        n => Err(CliError::InvalidArgument(format!(
            "'{name}' is not unique — {n} active projects share that name, which should \
             not be possible (the server keeps active project names unique per account); \
             refusing to guess which one you mean"
        ))),
    }
}

/// Build the `create` success output. Names only — never the id (see the
/// module doc comment).
fn render_create(project: &ProjectOut, mode: OutputMode) -> String {
    match mode {
        OutputMode::Json => serde_json::json!({
            "created": {
                "name": project.name,
                "language": project.language,
                "intent": project.intent,
            }
        })
        .to_string(),
        OutputMode::Human => format!("Created project '{}'.", project.name),
    }
}

/// Build the `archive` success output.
fn render_archive(project: &ProjectOut, mode: OutputMode) -> String {
    match mode {
        OutputMode::Json => serde_json::json!({
            "archived": { "name": project.name, "value": project.archived }
        })
        .to_string(),
        OutputMode::Human => format!(
            "Archived project '{}'. It will no longer appear in `hydrate projects`.",
            project.name
        ),
    }
}

/// Build the `delete` success output. Only the name the caller already typed —
/// the project is gone, so there is nothing left to look up an id from even if
/// this verb wanted to show one.
fn render_delete(name: &str, mode: OutputMode) -> String {
    match mode {
        OutputMode::Json => serde_json::json!({ "deleted": { "name": name } }).to_string(),
        OutputMode::Human => format!("Deleted project '{name}'."),
    }
}

/// Build the `rename` success output.
fn render_rename(old_name: &str, project: &ProjectOut, mode: OutputMode) -> String {
    match mode {
        OutputMode::Json => serde_json::json!({
            "renamed": { "from": old_name, "to": project.name }
        })
        .to_string(),
        OutputMode::Human => format!("Renamed project '{old_name}' to '{}'.", project.name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn project(name: &str, id: u128, archived: bool) -> ProjectOut {
        ProjectOut {
            archived,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            h2o_schema_version: 1,
            id: Uuid::from_u128(id),
            intent: Some("cli".to_string()),
            language: Some("python".to_string()),
            last_opened_at: None,
            name: name.to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn find_by_name_matches_exactly() {
        let projects = vec![project("alpha", 1, false), project("beta", 2, false)];
        let found = find_by_name(projects, "alpha").unwrap();
        assert_eq!(found.id, Uuid::from_u128(1));
    }

    #[test]
    fn find_by_name_rejects_substring_and_case_variants() {
        // Exact match only — no fuzzy/partial matching, per the plan.
        let projects = vec![project("hotdog-rater", 1, false)];
        assert!(find_by_name(projects.clone(), "hotdog").is_err());
        assert!(find_by_name(projects.clone(), "Hotdog-Rater").is_err());
        assert!(find_by_name(projects, "hotdog-rater-2").is_err());
    }

    #[test]
    fn find_by_name_no_match_names_the_archived_possibility() {
        // A name that resolves to nothing must not read as a flat "no such
        // project" — it might be archived and simply invisible to this
        // listing, which is a real, current limitation worth stating rather
        // than leaving the caller to guess why a name they know is real
        // isn't found.
        let err = find_by_name(vec![], "probe").unwrap_err();
        assert!(matches!(err, CliError::InvalidArgument(_)));
        let msg = err.to_string();
        assert!(msg.contains("probe"), "{msg}");
        assert!(
            msg.contains("ARCHIVED") || msg.contains("archived"),
            "{msg}"
        );
    }

    #[test]
    fn find_by_name_no_match_is_not_a_confusing_404_passthrough() {
        // The error must be actionable text, never a bare wire status code.
        let err = find_by_name(vec![], "nope").unwrap_err();
        let msg = err.to_string();
        assert!(!msg.contains("404"), "{msg}");
        assert!(msg.contains("hydrate projects"), "{msg}");
    }

    #[test]
    fn find_by_name_refuses_to_guess_among_duplicates() {
        // Should not be reachable given server-enforced uniqueness, but a
        // defensive refusal beats silently acting on the wrong project if
        // that invariant is ever violated.
        let projects = vec![project("dup", 1, false), project("dup", 2, false)];
        let err = find_by_name(projects, "dup").unwrap_err();
        assert!(err.to_string().contains("not unique"));
    }

    #[test]
    fn archived_flag_on_a_candidate_does_not_block_an_exact_active_match() {
        // find_by_name itself does no archived filtering — that already
        // happened server-side in what GET /v1/projects returned. Passing a
        // list that (hypothetically) contained an archived entry alongside an
        // active one with a different name must still resolve the active one
        // cleanly.
        let projects = vec![project("kept-active", 1, false), project("gone", 2, true)];
        let found = find_by_name(projects, "kept-active").unwrap();
        assert!(!found.archived);
    }

    #[test]
    fn render_create_names_only_never_an_id() {
        let out = render_create(&project("hotdog-rater", 0xA1, false), OutputMode::Human);
        assert!(out.contains("hotdog-rater"), "{out}");
        assert!(
            !out.contains(&Uuid::from_u128(0xA1).to_string()),
            "id leaked into create output: {out}"
        );

        let json = render_create(&project("hotdog-rater", 0xA1, false), OutputMode::Json);
        assert!(
            !json.contains(&Uuid::from_u128(0xA1).to_string()),
            "id leaked into create JSON: {json}"
        );
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["created"]["name"], "hotdog-rater");
    }

    #[test]
    fn render_archive_says_it_leaves_the_default_listing() {
        let out = render_archive(&project("old", 1, true), OutputMode::Human);
        assert!(out.contains("old"), "{out}");
        assert!(out.contains("hydrate projects"), "{out}");
    }

    #[test]
    fn render_delete_names_the_deleted_project() {
        let out = render_delete("probe", OutputMode::Human);
        assert!(out.contains("Deleted"));
        assert!(out.contains("probe"));
        let json = render_delete("probe", OutputMode::Json);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["deleted"]["name"], "probe");
    }

    #[test]
    fn render_rename_names_both_the_old_and_new_name() {
        let out = render_rename(
            "old-name",
            &project("new-name", 1, false),
            OutputMode::Human,
        );
        assert!(out.contains("old-name"), "{out}");
        assert!(out.contains("new-name"), "{out}");
        let json = render_rename("old-name", &project("new-name", 1, false), OutputMode::Json);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["renamed"]["from"], "old-name");
        assert_eq!(v["renamed"]["to"], "new-name");
    }

    #[test]
    fn delete_403_is_translated_to_a_named_missing_scope_not_a_bare_forbidden() {
        // The plan calls a bare 403 from `project delete` unacceptable UX.
        // This 403->MissingScope mapping is only safe because THIS route has
        // exactly one extra scope gate; see `translate_delete_error`'s doc.
        let translated = translate_delete_error(CliError::Service {
            status: 403,
            kind: "service_error".to_string(),
            reason: None,
        });
        assert!(
            matches!(&translated, CliError::MissingScope { scope } if scope == "project:delete")
        );
        assert!(translated.to_string().contains("project:delete"));
    }

    #[test]
    fn non_403_errors_pass_through_the_delete_translation_unchanged() {
        // Only the 403 shape gets reinterpreted; a 404 (no such project — a
        // race with something else deleting it first) or a network failure
        // must reach the user as themselves, not get relabeled as a scope
        // problem they don't have.
        let not_found = translate_delete_error(CliError::Service {
            status: 404,
            kind: "not_found".to_string(),
            reason: None,
        });
        assert!(matches!(not_found, CliError::Service { status: 404, .. }));

        let network = translate_delete_error(CliError::Network("boom".to_string()));
        assert!(matches!(network, CliError::Network(_)));
    }
}
