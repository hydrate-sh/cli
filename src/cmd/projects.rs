//! `projects` — list every project on the account, so a user can discover the
//! names and ids to feed to `--project` / `HYD_PROJECT`. Read-only: it lists
//! and never mutates, needs no binding, and works from any directory.

use hydrate_wire::models::ProjectOut;
use serde::Serialize;
use uuid::Uuid;

use crate::client::Client;
use crate::config::Config;
use crate::error::CliError;
use crate::output::OutputMode;

pub fn run(mode: OutputMode) -> Result<(), CliError> {
    let config = Config::load()?;
    let client = Client::new(&config)?;
    let listed = client.list_projects()?;
    println!("{}", render(&rows(&listed.projects), mode));
    Ok(())
}

#[derive(Debug, PartialEq, Serialize)]
struct ProjectRow {
    id: Uuid,
    name: String,
    /// The project's implementation language, if the server has one on file.
    language: Option<String>,
    /// The project's stated intent, if any.
    intent: Option<String>,
    archived: bool,
    /// When the project was last opened (ISO-8601), if ever.
    last_opened_at: Option<String>,
}

/// Project the wire projects into display rows. Every project is listed —
/// archived ones are flagged, not hidden — so the ids are always discoverable.
fn rows(projects: &[ProjectOut]) -> Vec<ProjectRow> {
    projects
        .iter()
        .map(|p| ProjectRow {
            id: p.id,
            name: p.name.clone(),
            language: p.language.clone(),
            intent: p.intent.clone(),
            archived: p.archived,
            last_opened_at: p.last_opened_at.clone(),
        })
        .collect()
}

/// Build the listing output. Returns the string to print so it can be tested.
fn render(rows: &[ProjectRow], mode: OutputMode) -> String {
    match mode {
        OutputMode::Json => serde_json::json!({ "projects": rows }).to_string(),
        OutputMode::Human => {
            if rows.is_empty() {
                return "No projects on this account.".to_string();
            }
            let mut out = String::from("Projects:");
            for r in rows {
                let archived = if r.archived { " [archived]" } else { "" };
                let language = r
                    .language
                    .as_deref()
                    .map(|l| format!(" ({l})"))
                    .unwrap_or_default();
                out.push_str(&format!("\n  {}{}{}", r.name, language, archived));
                out.push_str(&format!("\n      id: {}", r.id));
                if let Some(intent) = r.intent.as_deref().filter(|s| !s.is_empty()) {
                    out.push_str(&format!("\n      intent: {intent}"));
                }
                if let Some(last) = r.last_opened_at.as_deref() {
                    out.push_str(&format!("\n      last opened: {last}"));
                }
            }
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(name: &str, id: u128, archived: bool) -> ProjectOut {
        ProjectOut {
            archived,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            h2o_schema_version: 1,
            id: Uuid::from_u128(id),
            intent: Some("do a thing".to_string()),
            language: Some("rust".to_string()),
            last_opened_at: Some("2026-06-01T00:00:00Z".to_string()),
            name: name.to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn human_render_lists_name_id_language_intent_archived_lastopened() {
        let out = render(&rows(&[project("alpha", 0xA1, false)]), OutputMode::Human);
        assert!(out.contains("alpha"), "{out}");
        assert!(out.contains(&Uuid::from_u128(0xA1).to_string()), "{out}");
        assert!(out.contains("(rust)"), "{out}");
        assert!(out.contains("do a thing"), "{out}");
        assert!(out.contains("last opened"), "{out}");
        // An active project carries no archived flag.
        assert!(!out.contains("[archived]"), "{out}");
    }

    #[test]
    fn json_render_carries_same_fields_as_human() {
        let out = render(&rows(&[project("alpha", 0xA1, false)]), OutputMode::Json);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let p = &v["projects"][0];
        assert_eq!(p["name"], "alpha");
        assert_eq!(p["id"], Uuid::from_u128(0xA1).to_string());
        assert_eq!(p["language"], "rust");
        assert_eq!(p["intent"], "do a thing");
        assert_eq!(p["archived"], false);
        assert_eq!(p["last_opened_at"], "2026-06-01T00:00:00Z");
    }

    #[test]
    fn archived_projects_are_listed_and_flagged() {
        let out = render(
            &rows(&[project("live", 0x1, false), project("old", 0x2, true)]),
            OutputMode::Human,
        );
        // Both appear; only the archived one is flagged.
        assert!(out.contains("live"), "{out}");
        assert!(out.contains("old"), "{out}");
        assert!(out.contains("[archived]"), "{out}");
        // The archived flag rides on 'old', not 'live'.
        let old_line = out.lines().find(|l| l.contains("old")).unwrap();
        assert!(old_line.contains("[archived]"), "{out}");
        let live_line = out
            .lines()
            .find(|l| l.trim_start().starts_with("live"))
            .unwrap();
        assert!(!live_line.contains("[archived]"), "{out}");
        // JSON carries the flag too.
        let jout = render(
            &rows(&[project("live", 0x1, false), project("old", 0x2, true)]),
            OutputMode::Json,
        );
        let v: serde_json::Value = serde_json::from_str(&jout).unwrap();
        let arr = v["projects"].as_array().unwrap();
        let old = arr.iter().find(|p| p["name"] == "old").unwrap();
        assert_eq!(old["archived"], true);
    }

    #[test]
    fn human_render_handles_empty_list() {
        let out = render(&[], OutputMode::Human);
        assert!(out.contains("No projects"), "{out}");
    }

    #[test]
    fn optional_fields_are_omitted_when_absent() {
        let mut bare = project("bare", 0x3, false);
        bare.language = None;
        bare.intent = None;
        bare.last_opened_at = None;
        let out = render(&rows(&[bare]), OutputMode::Human);
        assert!(out.contains("bare"), "{out}");
        assert!(!out.contains("intent:"), "{out}");
        assert!(!out.contains("last opened"), "{out}");
        assert!(!out.contains("()"), "{out}");
    }
}
