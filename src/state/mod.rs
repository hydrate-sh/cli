//! On-disk working-directory state, kept under a `.hydrate/` directory at the
//! root of the user's working copy.
//!
//! Two files, two formats, deliberately:
//!   - `config.toml` — the [`Binding`]: which project and branch this workdir is
//!     attached to. Human-editable, so TOML (Rust-native, no silent type
//!     coercion). Absent until a branch is bound; absence is "not bound", a
//!     distinct state from "corrupt".
//!   - `stage.json`  — the [`Stage`]: the staged changeset plus the
//!     name→UUID alias table. Machine-written, so JSON. Absent means "nothing
//!     staged" (an empty stage), never an error.
//!
//! Every read distinguishes *absent* (a normal, expected state) from *corrupt*
//! (a loud [`CliError::State`]); we never silently paper over a malformed file
//! with a default, because that would discard a user's staged work without a
//! word.
//!
//! Consumed by the branch-context verbs (`fork`, `branches`, …) as they are
//! implemented; until then this module is exercised by its own tests only.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::CliError;

/// The workdir-state directory, relative to the working-copy root.
pub const DIR: &str = ".hydrate";
const CONFIG_FILE: &str = "config.toml";
const STAGE_FILE: &str = "stage.json";

/// Absolute path to the `.hydrate/` directory under `base`.
fn state_dir(base: &Path) -> PathBuf {
    base.join(DIR)
}

/// Create the `.hydrate/` directory if it does not exist.
fn ensure_dir(base: &Path) -> Result<PathBuf, CliError> {
    let dir = state_dir(base);
    std::fs::create_dir_all(&dir)
        .map_err(|e| CliError::State(format!("could not create {}: {e}", dir.display())))?;
    Ok(dir)
}

/// Which project and branch this working copy is attached to. Written by `fork`,
/// read by every verb that needs a branch context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binding {
    /// The project this workdir belongs to.
    pub project_id: Uuid,
    /// The project's human name (cached for display; the id is authoritative).
    pub project_name: String,
    /// The branch this workdir is currently working on.
    pub branch_id: Uuid,
    /// The branch's name (cached for display; the id is authoritative).
    pub branch_name: String,
}

impl Binding {
    /// Read the binding from `base/.hydrate/config.toml`.
    ///
    /// Returns `Ok(None)` when the file does not exist (the workdir is simply
    /// not bound yet). Returns [`CliError::State`] when the file exists but
    /// cannot be read or parsed — a corrupt binding is loud, never silent.
    pub fn load(base: &Path) -> Result<Option<Binding>, CliError> {
        let path = state_dir(base).join(CONFIG_FILE);
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(CliError::State(format!(
                    "could not read {}: {e}",
                    path.display()
                )))
            }
        };
        toml::from_str(&raw)
            .map(Some)
            .map_err(|e| CliError::State(format!("{} is corrupt: {e}", path.display())))
    }

    /// Write the binding to `base/.hydrate/config.toml`, creating `.hydrate/`.
    pub fn save(&self, base: &Path) -> Result<(), CliError> {
        let dir = ensure_dir(base)?;
        let path = dir.join(CONFIG_FILE);
        let body = toml::to_string_pretty(self)
            .map_err(|e| CliError::State(format!("could not serialize binding: {e}")))?;
        std::fs::write(&path, body)
            .map_err(|e| CliError::State(format!("could not write {}: {e}", path.display())))
    }
}

/// The staged changeset for the bound branch: a batch of typed deltas plus the
/// name→UUID alias table that lets the user address freshly-minted nodes by name
/// before the server has assigned ids.
///
/// In this phase the stage is only round-tripped; the authoring verbs that fill
/// `deltas` and `aliases` land in a later phase. The delta payloads are held as
/// opaque JSON so the stage format does not have to track every wire delta shape.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Stage {
    /// The staged deltas, in author order. Opaque here; shaped by the verbs.
    #[serde(default)]
    pub deltas: Vec<serde_json::Value>,
    /// Local name → server-assigned UUID, for names already committed upstream.
    #[serde(default)]
    pub aliases: BTreeMap<String, Uuid>,
}

impl Stage {
    /// An empty stage — nothing staged.
    pub fn empty() -> Stage {
        Stage::default()
    }

    /// Read the stage from `base/.hydrate/stage.json`.
    ///
    /// A missing file is an empty stage (nothing staged yet), not an error. A
    /// file that exists but cannot be read or parsed is a loud
    /// [`CliError::State`]: we refuse to silently discard staged work.
    pub fn load(base: &Path) -> Result<Stage, CliError> {
        let path = state_dir(base).join(STAGE_FILE);
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Stage::empty()),
            Err(e) => {
                return Err(CliError::State(format!(
                    "could not read {}: {e}",
                    path.display()
                )))
            }
        };
        serde_json::from_str(&raw)
            .map_err(|e| CliError::State(format!("{} is corrupt: {e}", path.display())))
    }

    /// Write the stage to `base/.hydrate/stage.json`, creating `.hydrate/`.
    pub fn save(&self, base: &Path) -> Result<(), CliError> {
        let dir = ensure_dir(base)?;
        let path = dir.join(STAGE_FILE);
        let body = serde_json::to_string_pretty(self)
            .map_err(|e| CliError::State(format!("could not serialize stage: {e}")))?;
        std::fs::write(&path, body)
            .map_err(|e| CliError::State(format!("could not write {}: {e}", path.display())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn binding() -> Binding {
        Binding {
            project_id: Uuid::from_u128(0x1111_1111_1111_1111_1111_1111_1111_1111),
            project_name: "hotdog-rater".into(),
            branch_id: Uuid::from_u128(0x2222_2222_2222_2222_2222_2222_2222_2222),
            branch_name: "main".into(),
        }
    }

    #[test]
    fn binding_round_trips() {
        let tmp = TempDir::new().unwrap();
        let b = binding();
        b.save(tmp.path()).unwrap();
        let loaded = Binding::load(tmp.path()).unwrap();
        assert_eq!(loaded, Some(b));
    }

    #[test]
    fn missing_binding_is_none_not_error() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(Binding::load(tmp.path()).unwrap(), None);
    }

    #[test]
    fn corrupt_binding_fails_loud() {
        let tmp = TempDir::new().unwrap();
        ensure_dir(tmp.path()).unwrap();
        std::fs::write(
            state_dir(tmp.path()).join(CONFIG_FILE),
            "this is not = toml [[[",
        )
        .unwrap();
        let err = Binding::load(tmp.path()).unwrap_err();
        assert!(matches!(err, CliError::State(_)), "got {err:?}");
        assert_eq!(err.kind(), "state_error");
    }

    #[test]
    fn save_creates_the_hydrate_dir() {
        let tmp = TempDir::new().unwrap();
        assert!(!state_dir(tmp.path()).exists());
        binding().save(tmp.path()).unwrap();
        assert!(state_dir(tmp.path()).join(CONFIG_FILE).is_file());
    }

    #[test]
    fn stage_round_trips_with_aliases() {
        let tmp = TempDir::new().unwrap();
        let mut stage = Stage::empty();
        stage.deltas.push(serde_json::json!({"op": "add_node"}));
        stage.aliases.insert(
            "svc/api".into(),
            Uuid::from_u128(0x3333_3333_3333_3333_3333_3333_3333_3333),
        );
        stage.save(tmp.path()).unwrap();
        assert_eq!(Stage::load(tmp.path()).unwrap(), stage);
    }

    #[test]
    fn missing_stage_is_empty_not_error() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(Stage::load(tmp.path()).unwrap(), Stage::empty());
    }

    #[test]
    fn corrupt_stage_fails_loud() {
        let tmp = TempDir::new().unwrap();
        ensure_dir(tmp.path()).unwrap();
        std::fs::write(state_dir(tmp.path()).join(STAGE_FILE), "{ not valid json").unwrap();
        let err = Stage::load(tmp.path()).unwrap_err();
        assert!(matches!(err, CliError::State(_)), "got {err:?}");
    }

    #[test]
    fn binding_and_stage_coexist_in_one_dir() {
        let tmp = TempDir::new().unwrap();
        binding().save(tmp.path()).unwrap();
        Stage::empty().save(tmp.path()).unwrap();
        assert!(Binding::load(tmp.path()).unwrap().is_some());
        assert_eq!(Stage::load(tmp.path()).unwrap(), Stage::empty());
    }
}
