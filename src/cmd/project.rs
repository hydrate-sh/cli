//! `project` — create, archive, restore, delete, and rename projects: the
//! mutating counterpart to the read-only `projects` listing.
//!
//! Every verb here addresses its target by NAME — never a UUID; ids stay an
//! internal wire detail these commands don't surface (`hydrate projects` is
//! the one place that shows them, for `--project`). The match must be EXACT
//! — no fuzzy/substring matching, so a typo fails loud instead of silently
//! landing on the wrong project.
//!
//! `archive`/`restore`/`rename`/`delete` resolve against
//! `Client::list_projects_including_archived`, not the plain
//! `Client::list_projects` (which is what `hydrate projects` shows and
//! excludes archived rows) — otherwise an archived project's name would be
//! unreachable by every verb that could act on it, making `archive` a
//! one-way door despite the server's `PATCH .../archived:false` existing to
//! reverse it. See [`find_by_name`] for how a name that matches both an
//! active and an archived project (a legal, expected state — archiving does
//! not reserve the name) is resolved.
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
    let project = find_by_name(
        client.list_projects_including_archived()?.projects,
        &args.name,
    )?;
    let patched = client.patch_project(project.id, None, Some(true))?;
    println!("{}", render_archive(&patched.project, mode));
    Ok(())
}

pub fn restore(args: ProjectNameArgs, mode: OutputMode) -> Result<(), CliError> {
    let config = Config::load()?;
    let client = Client::new(&config)?;
    let project = find_by_name(
        client.list_projects_including_archived()?.projects,
        &args.name,
    )?;
    let patched = client.patch_project(project.id, None, Some(false))?;
    println!("{}", render_restore(&patched.project, mode));
    Ok(())
}

pub fn delete(args: ProjectNameArgs, mode: OutputMode) -> Result<(), CliError> {
    let config = Config::load()?;
    let client = Client::new(&config)?;
    let project = find_by_name(
        client.list_projects_including_archived()?.projects,
        &args.name,
    )?;

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
    let project = find_by_name(
        client.list_projects_including_archived()?.projects,
        &args.name,
    )?;
    let patched = client.patch_project(project.id, Some(&args.to), None)?;
    println!("{}", render_rename(&args.name, &patched.project, mode));
    Ok(())
}

/// A bare 403 from `delete` is an unacceptable UX: `DELETE /v1/projects/{id}`
/// requires `project:delete`, a scope separate from — and not implied by —
/// `graph:write`, and every key minted before the scope existed lacks it. So
/// SOME 403s on this route really do mean "this key needs re-minting", and a
/// generic service error would leave the reader to guess that.
///
/// But it is not the ONLY 403 this route can return. Before reaching the
/// scope gate's outcome, the route also runs the per-key project-allowlist
/// check (`require_v1_project_viewer`, Layer 3 in `project_gates.py`): a
/// whitelist-scoped API key whose allowlist excludes this specific project
/// gets refused there too, with a 403 of its own. A key can legitimately
/// HOLD `project:delete` and still hit that gate for an unrelated reason —
/// telling that caller to re-mint with `project:delete` would be a specific,
/// plausible, WRONG diagnosis: they would mint a broader (and unnecessarily
/// more privileged) key and the delete would still fail, for the reason this
/// message never named.
///
/// The two are distinguishable in the response body, so we use that instead
/// of matching on status alone:
///
/// * the scope gate returns the fixed, untyped string `{"detail":
///   "forbidden"}` — no `code` field, so [`crate::error::parse_detail`]'s
///   extraction comes back empty and `CliError::Service.kind` falls back to
///   the generic `"service_error"` token;
/// * the allowlist gate returns a STRUCTURED body, `{"detail": {"code":
///   "project_not_in_key_whitelist", "message": ...}}`, which `parse_detail`
///   turns into that real code as `kind`.
///
/// So only a 403 whose `kind` is still the generic fallback — meaning the
/// body carried no structured code at all — is reinterpreted as
/// `MissingScope`. Any 403 that DID carry a code (this one, or any future
/// one) passes through as itself. This is still a route-specific inference,
/// not something the wire format states outright, and still needs revisiting
/// if this route's 403 shapes change again.
fn translate_delete_error(err: CliError) -> CliError {
    match err {
        CliError::Service {
            status: 403,
            ref kind,
            ..
        } if kind == "service_error" => CliError::MissingScope {
            scope: "project:delete".to_string(),
        },
        other => other,
    }
}

/// Resolve `name` to a project by EXACT match against `projects` (the caller
/// passes an archived-inclusive listing for every verb but `create`, which
/// has no need to resolve a name at all).
///
/// An active project's name is unique per caller (server-enforced), but an
/// active and an archived project CAN legitimately share a name — archiving
/// does not reserve it, so a fresh project may reuse an archived one's name
/// at any time. When that happens, the active project wins: "the project
/// named X" almost always means the one you can currently see and work with,
/// and falling back to an archived match only when there is no active one
/// means `archive`/`rename`/`delete` on a plain name keep working exactly as
/// before this resolved archived rows at all.
///
/// Three loud failure modes, deliberately distinct from a panic or a raw 404
/// passthrough:
///
/// * zero matches (active or archived) — genuinely no project by this name;
/// * more than one ACTIVE match — should not be reachable (the server keeps
///   active names unique per caller); refusing instead of silently picking
///   one avoids acting on the wrong project if that invariant ever breaks;
/// * no active match, but more than one ARCHIVED match — a real, reachable
///   case (archived rows have no uniqueness constraint against each other),
///   and there is no id to disambiguate with here, so the caller is told to
///   resolve the collision elsewhere rather than have one guessed for them.
fn find_by_name(projects: Vec<ProjectOut>, name: &str) -> Result<ProjectOut, CliError> {
    let matches: Vec<ProjectOut> = projects.into_iter().filter(|p| p.name == name).collect();

    let mut active: Vec<ProjectOut> = matches.iter().filter(|p| !p.archived).cloned().collect();
    match active.len() {
        0 => {}
        1 => return Ok(active.remove(0)),
        n => {
            return Err(CliError::InvalidArgument(format!(
                "'{name}' is not unique — {n} active projects share that name, which should \
                 not be possible (the server keeps active project names unique per account); \
                 refusing to guess which one you mean"
            )));
        }
    }

    let mut archived: Vec<ProjectOut> = matches.into_iter().filter(|p| p.archived).collect();
    match archived.len() {
        0 => Err(CliError::InvalidArgument(format!(
            "no project named '{name}'; run `hydrate projects` to check the exact spelling \
             of an active project, or `hydrate project restore {name}` if you expected it to \
             be archived under this name"
        ))),
        1 => Ok(archived.remove(0)),
        n => Err(CliError::InvalidArgument(format!(
            "{n} archived projects are named '{name}'; this verb can't tell them apart by \
             name alone (only an active project's name is guaranteed unique) — rename one of \
             them at https://hydrate.sh, then retry"
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
            "Archived project '{}'. It will no longer appear in `hydrate projects` until \
             you run `hydrate project restore {}`.",
            project.name, project.name
        ),
    }
}

/// Build the `restore` success output.
fn render_restore(project: &ProjectOut, mode: OutputMode) -> String {
    match mode {
        OutputMode::Json => serde_json::json!({
            "restored": { "name": project.name, "archived": project.archived }
        })
        .to_string(),
        OutputMode::Human => format!(
            "Restored project '{}'. It will appear in `hydrate projects` again.",
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
    fn find_by_name_resolves_an_archived_only_match() {
        // The whole point of switching to the archived-inclusive listing: a
        // name that only an archived project holds must still resolve, or
        // `archive` is a one-way door regardless of what the CLI's copy
        // claims.
        let projects = vec![project("shelved", 1, true)];
        let found = find_by_name(projects, "shelved").unwrap();
        assert!(found.archived);
    }

    #[test]
    fn find_by_name_prefers_the_active_project_when_names_collide() {
        // Archiving does not reserve a name, so an active and an archived
        // project CAN legitimately share one. The active project is what
        // "the project named X" means by default.
        let projects = vec![project("dup", 1, true), project("dup", 2, false)];
        let found = find_by_name(projects, "dup").unwrap();
        assert_eq!(found.id, Uuid::from_u128(2));
        assert!(!found.archived);
    }

    #[test]
    fn find_by_name_no_match_at_all_points_at_projects_and_restore() {
        let err = find_by_name(vec![], "probe").unwrap_err();
        assert!(matches!(err, CliError::InvalidArgument(_)));
        let msg = err.to_string();
        assert!(msg.contains("probe"), "{msg}");
        assert!(msg.contains("hydrate projects"), "{msg}");
        assert!(msg.contains("hydrate project restore"), "{msg}");
        // No caps-for-emphasis; plain sentence case, matching the rest of
        // this CLI's error text.
        assert!(!msg.contains("ARCHIVED"), "{msg}");
    }

    #[test]
    fn find_by_name_no_match_is_not_a_confusing_404_passthrough() {
        // The error must be actionable text, never a bare wire status code.
        let err = find_by_name(vec![], "nope").unwrap_err();
        let msg = err.to_string();
        assert!(!msg.contains("404"), "{msg}");
    }

    #[test]
    fn find_by_name_refuses_to_guess_among_duplicate_active_projects() {
        // Should not be reachable given server-enforced uniqueness among
        // active names, but a defensive refusal beats silently acting on the
        // wrong project if that invariant is ever violated.
        let projects = vec![project("dup", 1, false), project("dup", 2, false)];
        let err = find_by_name(projects, "dup").unwrap_err();
        assert!(err.to_string().contains("not unique"));
    }

    #[test]
    fn find_by_name_refuses_to_guess_among_duplicate_archived_projects() {
        // Unlike the active case, this one IS reachable in practice: archived
        // rows carry no uniqueness constraint against each other. No id
        // exists here to disambiguate with, so it must refuse rather than
        // pick one.
        let projects = vec![project("dup", 1, true), project("dup", 2, true)];
        let err = find_by_name(projects, "dup").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains('2'), "{msg}");
        assert!(msg.contains("dup"), "{msg}");
    }

    #[test]
    fn archived_flag_on_a_candidate_does_not_block_an_exact_active_match() {
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
    fn render_archive_names_the_restore_verb() {
        // The round trip only reads as real if the success message itself
        // names the way back, not just the guide/README.
        let out = render_archive(&project("old", 1, true), OutputMode::Human);
        assert!(out.contains("old"), "{out}");
        assert!(out.contains("hydrate projects"), "{out}");
        assert!(out.contains("hydrate project restore old"), "{out}");
    }

    #[test]
    fn render_restore_says_it_returns_to_the_default_listing() {
        let out = render_restore(&project("old", 1, false), OutputMode::Human);
        assert!(out.contains("old"), "{out}");
        assert!(out.contains("hydrate projects"), "{out}");
        let json = render_restore(&project("old", 1, false), OutputMode::Json);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["restored"]["name"], "old");
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
    fn delete_403_with_no_structured_code_is_translated_to_missing_scope() {
        // The scope gate's body is the bare `{"detail": "forbidden"}` string,
        // which carries no `code` — parse_detail's kind extraction comes back
        // empty and falls back to "service_error". That specific shape is
        // what this translation exists to catch.
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
    fn delete_403_with_a_structured_code_passes_through_unchanged() {
        // The per-key project-allowlist gate (Layer 3, `project_gates.py`)
        // raises a SEPARATE 403 on this exact route, with a structured body:
        // `{"code": "project_not_in_key_whitelist", ...}`. A key can hold
        // project:delete and still hit this one for an unrelated reason, so
        // reinterpreting it as MissingScope would be a wrong diagnosis that
        // sends the caller to mint an unnecessarily broader key. It must
        // reach the user as itself.
        let translated = translate_delete_error(CliError::Service {
            status: 403,
            kind: "project_not_in_key_whitelist".to_string(),
            reason: Some(
                "This API key's project whitelist does not include the requested project."
                    .to_string(),
            ),
        });
        assert!(
            matches!(
                &translated,
                CliError::Service { status: 403, kind, .. } if kind == "project_not_in_key_whitelist"
            ),
            "got {translated:?}"
        );
        assert!(translated.to_string().contains("whitelist"));
    }

    #[test]
    fn non_403_errors_pass_through_the_delete_translation_unchanged() {
        // Only a 403 with no structured code gets reinterpreted; a 404 (no
        // such project — a race with something else deleting it first) or a
        // network failure must reach the user as themselves, not get
        // relabeled as a scope problem they don't have.
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
