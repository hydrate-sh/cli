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
//! (`expect` can't replace `allow` here: under `--all-targets` the test build
//! uses every item, so `dead_code` never fires there and an `expect` would be
//! reported unfulfilled.)
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
const INDEX_FILE: &str = "index.json";

/// Absolute path to the `.hydrate/` directory under `base`.
fn state_dir(base: &Path) -> PathBuf {
    base.join(DIR)
}

/// Walk up from `start` looking for the working-copy root: the nearest ancestor
/// (including `start` itself) that contains a `.hydrate/` directory.
///
/// This is the git-style discovery that lets a verb run from any subdirectory
/// bind to the one working copy above it, instead of silently creating a second,
/// nested `.hydrate/` that would alias the same project to two locations.
/// Returns `None` when no ancestor is bound.
pub fn find_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|dir| dir.join(DIR).is_dir())
        .map(Path::to_path_buf)
}

/// Create the `.hydrate/` directory if it does not exist.
fn ensure_dir(base: &Path) -> Result<PathBuf, CliError> {
    let dir = state_dir(base);
    std::fs::create_dir_all(&dir)
        .map_err(|e| CliError::State(format!("could not create {}: {e}", dir.display())))?;
    Ok(dir)
}

/// Write `body` to `path` atomically: write a sibling temp file, then `rename`
/// it into place. A crash or full disk mid-write leaves the previous file
/// intact rather than a truncated one — staged work is never half-written away.
/// The temp file is a sibling so the rename stays within one filesystem (where
/// it is atomic), and the rename replaces any existing target in a single step.
fn atomic_write(path: &Path, body: &[u8]) -> Result<(), CliError> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, body)
        .map_err(|e| CliError::State(format!("could not write {}: {e}", tmp.display())))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        // Best-effort cleanup; the rename error is the one that matters.
        let _ = std::fs::remove_file(&tmp);
        CliError::State(format!("could not replace {}: {e}", path.display()))
    })
}

/// Which project and branch this working copy is attached to. Written by `fork`,
/// read by every verb that needs a branch context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
        atomic_write(&path, body.as_bytes())
    }
}

/// The staged changeset for the bound branch: a batch of typed deltas plus the
/// name→UUID alias table that lets the user address freshly-minted nodes by name
/// before the server has assigned ids.
///
/// In this phase the stage is only round-tripped; the authoring verbs that fill
/// `deltas` and `aliases` land in a later phase. The delta payloads are held as
/// opaque JSON so the stage format does not have to track every wire delta shape.
///
/// Both fields are required on disk and unknown keys are rejected: a real stage
/// always writes both, so a `stage.json` missing one or carrying a typo'd/renamed
/// key is corruption, and corruption is surfaced loudly rather than papered over
/// with an empty default that would silently drop the user's staged batch.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stage {
    /// The staged deltas, in author order. Opaque here; shaped by the verbs.
    pub deltas: Vec<serde_json::Value>,
    /// Local name → the UUID minted for it at stage time, so a later delta in the
    /// same batch can address a freshly-created node by its path/name before the
    /// server has confirmed it.
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
        atomic_write(&path, body.as_bytes())
    }
}

/// A pulled, read-only snapshot of the bound branch's graph, reduced to what the
/// authoring verbs need to resolve a dotted `node.port` path against the **live**
/// graph: a map from the same `node:`/`port:` keys the [`Stage`] alias table uses
/// to the server's UUIDs, plus the branch version the snapshot was taken at.
///
/// This is the local "working copy" — but an *index*, not a round-trippable
/// document. It is never a source of deltas (the stage is); it only lets
/// `edge add` / `node add --parent` reference nodes that were committed in an
/// earlier session. Absent means "no pull yet" (resolution falls back to the
/// stage alone), a normal state; a malformed file is loud corruption, never
/// papered over — a stale-but-parseable index that silently mis-resolves a path
/// to the wrong UUID is exactly the failure we refuse.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Index {
    /// The branch version this snapshot was pulled at — the optimistic-
    /// concurrency token `commit` sends so a branch that moved since the pull
    /// is rejected loudly rather than committed against blindly.
    pub version: i32,
    /// `node:<path>` / `port:<path>:<side>:<name>` → the server's UUID. The key
    /// scheme is identical to the stage's alias table, so resolution can consult
    /// one then the other with the same key.
    pub entries: BTreeMap<String, Uuid>,
    /// Per-node kind + current ports, keyed by node UUID. Needed by `node set`
    /// (to resend the full port list while preserving surviving port UUIDs) and
    /// `boundary flatten` (the boundary-only check). `default` so an index pulled
    /// by an older CLI still loads — the verbs that need it fail loud (with a
    /// `pull` hint) when it's absent rather than mis-resolve.
    #[serde(default)]
    pub node_info: BTreeMap<Uuid, NodeInfo>,
    /// `<source_handle_uuid>:<target_handle_uuid>` → edge UUID. Needed by
    /// `edge rm --from --to`. `default` for the same back-compat reason.
    #[serde(default)]
    pub edges: BTreeMap<String, Uuid>,
}

/// A node's kind and current ports, as pulled — the snapshot `node set` patches.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeInfo {
    /// `behavior` or `boundary`.
    pub kind: String,
    pub inputs: Vec<PortInfo>,
    pub outputs: Vec<PortInfo>,
    /// Config ports (a third channel alongside inputs/outputs; not edge
    /// endpoints). `#[serde(default)]` for back-compat with an index pulled
    /// before this field existed.
    #[serde(default)]
    pub config: Vec<PortInfo>,
}

/// A single port's identity as pulled: its server UUID, name, and type. Editing
/// a node's ports resends these, changing only what the flags touch, so a
/// surviving port keeps its UUID and its edges stay intact.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortInfo {
    pub id: Uuid,
    pub name: String,
    pub r#type: String,
}

/// The edge-map key for an edge between two resolved port UUIDs.
pub fn edge_lookup_key(source: Uuid, target: Uuid) -> String {
    format!("{source}:{target}")
}

impl Index {
    /// Look up a pre-built alias key (`node:…` / `port:…`) in the snapshot.
    pub fn get(&self, key: &str) -> Option<Uuid> {
        self.entries.get(key).copied()
    }

    /// The pulled kind + ports for a committed node id, if present.
    pub fn node_info(&self, id: &Uuid) -> Option<&NodeInfo> {
        self.node_info.get(id)
    }

    /// The committed edge UUID joining two resolved port UUIDs, if present.
    pub fn edge_id(&self, source: Uuid, target: Uuid) -> Option<Uuid> {
        self.edges.get(&edge_lookup_key(source, target)).copied()
    }

    /// Read the index from `base/.hydrate/index.json`.
    ///
    /// A missing file is `Ok(None)` — the workdir simply hasn't pulled yet, a
    /// normal state callers handle by resolving against the stage alone. A file
    /// that exists but cannot be read or parsed is a loud [`CliError::State`].
    pub fn load(base: &Path) -> Result<Option<Index>, CliError> {
        let path = state_dir(base).join(INDEX_FILE);
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
        serde_json::from_str(&raw)
            .map(Some)
            .map_err(|e| CliError::State(format!("{} is corrupt: {e}", path.display())))
    }

    /// Write the index to `base/.hydrate/index.json`, creating `.hydrate/`.
    pub fn save(&self, base: &Path) -> Result<(), CliError> {
        let dir = ensure_dir(base)?;
        let path = dir.join(INDEX_FILE);
        let body = serde_json::to_string_pretty(self)
            .map_err(|e| CliError::State(format!("could not serialize index: {e}")))?;
        atomic_write(&path, body.as_bytes())
    }

    /// Delete the index file if present (a missing file is fine). Used when
    /// re-binding the workdir to a different branch (`fork`): the old branch's
    /// path→UUID map must not survive, or a later command could resolve a path
    /// to a UUID from the wrong branch.
    pub fn remove(base: &Path) -> Result<(), CliError> {
        let path = state_dir(base).join(INDEX_FILE);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(CliError::State(format!(
                "could not remove {}: {e}",
                path.display()
            ))),
        }
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
        assert_eq!(err.kind(), "state_error");
    }

    // A structurally-valid JSON object that is missing a required key must NOT
    // degrade to an empty stage — that would silently discard staged work.
    #[test]
    fn stage_missing_key_fails_loud() {
        let tmp = TempDir::new().unwrap();
        ensure_dir(tmp.path()).unwrap();
        std::fs::write(state_dir(tmp.path()).join(STAGE_FILE), r#"{"deltas": []}"#).unwrap();
        let err = Stage::load(tmp.path()).unwrap_err();
        assert!(matches!(err, CliError::State(_)), "got {err:?}");
    }

    // A typo'd / renamed key must be rejected, not silently dropped.
    #[test]
    fn stage_unknown_key_fails_loud() {
        let tmp = TempDir::new().unwrap();
        ensure_dir(tmp.path()).unwrap();
        std::fs::write(
            state_dir(tmp.path()).join(STAGE_FILE),
            r#"{"deltas": [], "aliases": {}, "delta": []}"#,
        )
        .unwrap();
        let err = Stage::load(tmp.path()).unwrap_err();
        assert!(matches!(err, CliError::State(_)), "got {err:?}");
    }

    #[test]
    fn binding_unknown_key_fails_loud() {
        let tmp = TempDir::new().unwrap();
        let mut toml = toml::to_string_pretty(&binding()).unwrap();
        toml.push_str("\nstray_key = \"x\"\n");
        ensure_dir(tmp.path()).unwrap();
        std::fs::write(state_dir(tmp.path()).join(CONFIG_FILE), toml).unwrap();
        let err = Binding::load(tmp.path()).unwrap_err();
        assert!(matches!(err, CliError::State(_)), "got {err:?}");
    }

    // Re-saving overwrites in place (atomic rename) and leaves no `.tmp` litter.
    #[test]
    fn save_overwrites_and_leaves_no_temp_file() {
        let tmp = TempDir::new().unwrap();
        binding().save(tmp.path()).unwrap();
        let mut updated = binding();
        updated.branch_name = "feature".into();
        updated.save(tmp.path()).unwrap();
        assert_eq!(Binding::load(tmp.path()).unwrap(), Some(updated));
        let leftover = state_dir(tmp.path()).join("config.toml.tmp");
        assert!(
            !leftover.exists(),
            "temp file was not cleaned up: {leftover:?}"
        );
    }

    #[test]
    fn find_root_walks_up_to_the_bound_ancestor() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        binding().save(root).unwrap();
        let nested = root.join("services").join("api");
        std::fs::create_dir_all(&nested).unwrap();
        // From a deep subdirectory, discovery finds the ancestor that holds
        // `.hydrate/` — not the subdirectory itself.
        assert_eq!(find_root(&nested).as_deref(), Some(root));
    }

    #[test]
    fn find_root_is_none_when_unbound() {
        let tmp = TempDir::new().unwrap();
        let nested = tmp.path().join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(find_root(&nested), None);
    }

    #[test]
    fn binding_and_stage_coexist_in_one_dir() {
        let tmp = TempDir::new().unwrap();
        binding().save(tmp.path()).unwrap();
        Stage::empty().save(tmp.path()).unwrap();
        assert!(Binding::load(tmp.path()).unwrap().is_some());
        assert_eq!(Stage::load(tmp.path()).unwrap(), Stage::empty());
    }

    fn index() -> Index {
        let mut entries = BTreeMap::new();
        entries.insert(
            "node:Api".to_string(),
            Uuid::from_u128(0x5555_5555_5555_5555_5555_5555_5555_5555),
        );
        entries.insert(
            "port:Api.Rater:out:score".to_string(),
            Uuid::from_u128(0x6666_6666_6666_6666_6666_6666_6666_6666),
        );
        Index {
            version: 7,
            entries,
            ..Default::default()
        }
    }

    #[test]
    fn index_round_trips() {
        let tmp = TempDir::new().unwrap();
        let i = index();
        i.save(tmp.path()).unwrap();
        assert_eq!(Index::load(tmp.path()).unwrap(), Some(i));
    }

    #[test]
    fn missing_index_is_none_not_error() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(Index::load(tmp.path()).unwrap(), None);
    }

    #[test]
    fn corrupt_index_fails_loud() {
        let tmp = TempDir::new().unwrap();
        ensure_dir(tmp.path()).unwrap();
        std::fs::write(state_dir(tmp.path()).join(INDEX_FILE), "{ not valid json").unwrap();
        let err = Index::load(tmp.path()).unwrap_err();
        assert!(matches!(err, CliError::State(_)), "got {err:?}");
        assert_eq!(err.kind(), "state_error");
    }

    // A parseable-but-incomplete index (missing a required key) must fail loud,
    // not silently degrade to an empty/zeroed snapshot that would mis-resolve
    // every committed path.
    #[test]
    fn index_missing_key_fails_loud() {
        let tmp = TempDir::new().unwrap();
        ensure_dir(tmp.path()).unwrap();
        std::fs::write(state_dir(tmp.path()).join(INDEX_FILE), r#"{"entries": {}}"#).unwrap();
        let err = Index::load(tmp.path()).unwrap_err();
        assert!(matches!(err, CliError::State(_)), "got {err:?}");
        // Specifically the parse path (missing `version`), not some other state
        // error — pins that `version` has no silent `#[serde(default)]` to 0.
        assert!(err.to_string().contains("corrupt"), "{err}");
    }

    #[test]
    fn index_remove_deletes_an_existing_file_and_is_ok_when_absent() {
        let tmp = TempDir::new().unwrap();
        // Absent: a no-op, not an error.
        Index::remove(tmp.path()).unwrap();
        index().save(tmp.path()).unwrap();
        assert!(Index::load(tmp.path()).unwrap().is_some());
        Index::remove(tmp.path()).unwrap();
        assert_eq!(Index::load(tmp.path()).unwrap(), None);
    }

    #[test]
    fn index_get_resolves_a_known_key_and_misses_otherwise() {
        let i = index();
        assert_eq!(
            i.get("node:Api"),
            Some(Uuid::from_u128(0x5555_5555_5555_5555_5555_5555_5555_5555))
        );
        assert_eq!(i.get("node:Ghost"), None);
    }

    #[test]
    fn index_loads_without_node_info_or_edges_for_back_compat() {
        // An index.json written by an older CLI (version + entries only) must
        // still load — the new fields default to empty rather than failing the
        // whole load — so an upgrade doesn't strand a bound workdir.
        let tmp = TempDir::new().unwrap();
        ensure_dir(tmp.path()).unwrap();
        std::fs::write(
            state_dir(tmp.path()).join(INDEX_FILE),
            r#"{"version": 3, "entries": {"node:Api": "00000000-0000-0000-0000-000000000001"}}"#,
        )
        .unwrap();
        let i = Index::load(tmp.path()).unwrap().unwrap();
        assert_eq!(i.version, 3);
        assert!(i.node_info.is_empty());
        assert!(i.edges.is_empty());
    }

    #[test]
    fn index_node_info_and_edge_accessors() {
        let node = Uuid::from_u128(0x11);
        let (src, tgt, edge) = (
            Uuid::from_u128(0x50),
            Uuid::from_u128(0x70),
            Uuid::from_u128(0xED),
        );
        let mut node_info = BTreeMap::new();
        node_info.insert(
            node,
            NodeInfo {
                kind: "behavior".to_string(),
                inputs: vec![PortInfo {
                    id: Uuid::from_u128(0x1),
                    name: "i".into(),
                    r#type: "T".into(),
                }],
                outputs: vec![],
                config: vec![],
            },
        );
        let mut edges = BTreeMap::new();
        edges.insert(edge_lookup_key(src, tgt), edge);
        let i = Index {
            version: 1,
            entries: BTreeMap::new(),
            node_info,
            edges,
        };

        assert_eq!(i.node_info(&node).unwrap().kind, "behavior");
        assert!(i.node_info(&Uuid::from_u128(0x99)).is_none());
        assert_eq!(i.edge_id(src, tgt), Some(edge));
        assert_eq!(i.edge_id(src, Uuid::from_u128(0x99)), None);
    }

    // The richer index must survive a real save → load round trip with the new
    // fields populated.
    #[test]
    fn index_round_trips_with_node_info_and_edges() {
        let tmp = TempDir::new().unwrap();
        let node = Uuid::from_u128(0x11);
        let mut node_info = BTreeMap::new();
        node_info.insert(
            node,
            NodeInfo {
                kind: "boundary".into(),
                inputs: vec![],
                outputs: vec![],
                config: vec![],
            },
        );
        let mut edges = BTreeMap::new();
        edges.insert(
            edge_lookup_key(Uuid::from_u128(0x50), Uuid::from_u128(0x70)),
            Uuid::from_u128(0xED),
        );
        let i = Index {
            version: 9,
            entries: BTreeMap::new(),
            node_info,
            edges,
        };
        i.save(tmp.path()).unwrap();
        assert_eq!(Index::load(tmp.path()).unwrap(), Some(i));
    }
}
