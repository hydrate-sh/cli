//! `commit` — lower the staged changeset into a typed delta batch and apply it
//! to the bound branch under optimistic concurrency, then clear the stage.

use sha2::{Digest, Sha256};

use super::context::require_workdir;
use crate::client::Client;
use crate::config::Config;
use crate::error::CliError;
use crate::output::OutputMode;
use crate::staging::lower;
use crate::state::{Binding, Index, Stage};

pub fn run(mode: OutputMode) -> Result<(), CliError> {
    let base = require_workdir()?;
    let binding = Binding::load(&base)?.ok_or_else(|| {
        CliError::Other(
            "this working copy is not bound to a branch; run `hydrate fork`".to_string(),
        )
    })?;
    let stage = Stage::load(&base)?;
    if stage.deltas.is_empty() {
        println!("{}", render_nothing(&binding, mode));
        return Ok(());
    }

    let deltas = lower(&stage)?;
    let key = idempotency_key(&deltas)?;

    let config = Config::load()?;
    let client = Client::with_idempotency_key(&config, &key)?;

    // The OCC token. If this working copy has pulled the live graph, the staged
    // deltas were resolved against THAT snapshot's version, so we commit against
    // it: if the branch has moved since the pull, the server rejects with a 409
    // (exit 4) rather than applying handles resolved against a stale graph —
    // recover with `hydrate pull` and re-commit. With no pull (the within-session
    // flow), fall back to fetching the current version.
    let index = Index::load(&base)?;
    // Delete/update deltas resolved their target ids from a pull; committing them
    // without an index means falling back to fetch-current-version, applying
    // against a branch state we never pulled. Require the index so the OCC token
    // is the pulled version — fail loud rather than blindly mutate.
    require_index_for_mutation(index.is_some(), &stage)?;
    let expected_version = match &index {
        Some(index) => u32::try_from(index.version),
        None => u32::try_from(client.branch_version(binding.project_id, binding.branch_id)?),
    }
    .map_err(|_| CliError::Other("the server reported a negative branch version".to_string()))?;

    let body = hydrate_wire::models::V1DeltasBody {
        deltas: Some(deltas),
        expected_version,
        positions: None,
    };
    let applied = client.apply_deltas(binding.branch_id, body)?;

    println!("{}", finalize(&base, &binding, &applied, mode)?);
    Ok(())
}

/// Decide the outcome of an apply and, only on a confirmed success, clear the
/// stage. The HTTP request succeeding is not enough: the server reports
/// `applied` to say whether the batch actually committed. If it did not, the
/// staged work is KEPT and the failure is surfaced — we never wipe a user's
/// stage on the strength of a 2xx alone.
fn finalize(
    base: &std::path::Path,
    binding: &Binding,
    applied: &hydrate_wire::models::DeltaApplyResponse,
    mode: OutputMode,
) -> Result<String, CliError> {
    if !applied.applied {
        return Err(CliError::Other(
            "the server did not apply the batch; your staged changes are kept".to_string(),
        ));
    }
    // The batch is on the branch, so the local stage is spent.
    Stage::empty().save(base)?;
    // Advance the pulled index's version to the just-committed one (when this
    // workdir has an index), so a subsequent commit in the same session targets
    // the new version instead of false-conflicting against the pre-commit one.
    // The index's path→UUID entries stay as pulled — referencing nodes created
    // by THIS commit still needs a fresh `pull` — only the OCC token moves.
    if let Some(mut index) = Index::load(base)? {
        index.version = applied.branch.version;
        index.save(base)?;
    }
    Ok(render(applied, binding, mode))
}

/// The `Idempotency-Key`: the SHA-256 of the lowered batch, hex-encoded. Stable
/// across retries of the same staged batch, so a re-applied commit is a no-op
/// server-side rather than a duplicate.
fn idempotency_key(
    deltas: &[hydrate_wire::models::V1DeltasBodyDeltasInner],
) -> Result<String, CliError> {
    let bytes = serde_json::to_vec(deltas)
        .map_err(|e| CliError::Other(format!("could not encode the batch: {e}")))?;
    let digest = Sha256::digest(&bytes);
    Ok(digest.iter().fold(String::with_capacity(64), |mut s, b| {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
        s
    }))
}

/// Fail loud if the batch deletes/edits but no index has been pulled — those
/// deltas were resolved against a pulled snapshot, so committing them without
/// one would fall back to fetch-current-version and mutate a branch state we
/// never pulled. Pure-add batches are unaffected.
fn require_index_for_mutation(index_present: bool, stage: &Stage) -> Result<(), CliError> {
    if !index_present && stage_has_mutation(stage) {
        return Err(CliError::Other(
            "this changeset deletes or edits nodes but nothing is pulled; run `hydrate pull` before committing"
                .to_string(),
        ));
    }
    Ok(())
}

/// Whether the staged batch contains a delete/update delta (vs. pure adds) —
/// these were resolved against a pulled snapshot, so committing them needs the
/// index present (see `run`).
fn stage_has_mutation(stage: &Stage) -> bool {
    stage.deltas.iter().any(|v| {
        matches!(
            v.get("type").and_then(serde_json::Value::as_str),
            Some("delete_node" | "delete_edge" | "update_node_data" | "reparent_node")
        )
    })
}

fn render_nothing(binding: &Binding, mode: OutputMode) -> String {
    match mode {
        // Same `committed` shape as a real commit (branch present, version null)
        // so a scripting consumer reads the same keys every time.
        OutputMode::Json => serde_json::json!({
            "committed": { "branch": binding.branch_name, "delta_count": 0, "version": null }
        })
        .to_string(),
        OutputMode::Human => "Nothing to commit.".to_string(),
    }
}

fn render(
    applied: &hydrate_wire::models::DeltaApplyResponse,
    binding: &Binding,
    mode: OutputMode,
) -> String {
    // `branch.version` is the numeric branch version; use it (not the response's
    // stringly `version`) so the field is an integer in both modes.
    let version = applied.branch.version;
    match mode {
        OutputMode::Json => serde_json::json!({
            "committed": {
                "branch": binding.branch_name,
                "delta_count": applied.delta_count,
                "version": version,
            }
        })
        .to_string(),
        OutputMode::Human => format!(
            "Committed {} to branch '{}'. Now at version {}.",
            plural(applied.delta_count, "change"),
            binding.branch_name,
            version,
        ),
    }
}

fn plural(n: i32, noun: &str) -> String {
    if n == 1 {
        format!("{n} {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hydrate_wire::models;
    use uuid::Uuid;

    fn binding() -> Binding {
        Binding {
            project_id: Uuid::from_u128(1),
            project_name: "p".to_string(),
            branch_id: Uuid::from_u128(2),
            branch_name: "spicy".to_string(),
        }
    }

    fn node_delta_with_id(id: u128) -> models::V1DeltasBodyDeltasInner {
        models::V1DeltasBodyDeltasInner::AddNode(Box::new(models::AddNodeDelta::new(
            models::Node {
                data: None,
                id: Uuid::from_u128(id),
                kind: models::node::Kind::Behavior,
                parent_id: Some(None),
            },
            models::add_node_delta::Type::AddNode,
        )))
    }

    fn node_delta() -> models::V1DeltasBodyDeltasInner {
        node_delta_with_id(9)
    }

    fn applied_response(
        applied: bool,
        version: i32,
        delta_count: i32,
    ) -> models::DeltaApplyResponse {
        models::DeltaApplyResponse {
            applied,
            branch: Box::new(models::BranchRef {
                id: Uuid::from_u128(2),
                version,
            }),
            delta_count,
            positions_applied: None,
            project_id: Uuid::from_u128(1),
            version: version.to_string(),
        }
    }

    #[test]
    fn idempotency_key_is_stable_and_hex() {
        let a = idempotency_key(&[node_delta()]).unwrap();
        let b = idempotency_key(&[node_delta()]).unwrap();
        assert_eq!(a, b, "same batch must hash the same");
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()), "{a}");
    }

    #[test]
    fn idempotency_key_differs_for_different_batches() {
        // Different length...
        assert_ne!(
            idempotency_key(&[node_delta()]).unwrap(),
            idempotency_key(&[node_delta(), node_delta()]).unwrap()
        );
        // ...and same length, different content (catches a length-only hash).
        assert_ne!(
            idempotency_key(&[node_delta_with_id(1)]).unwrap(),
            idempotency_key(&[node_delta_with_id(2)]).unwrap()
        );
    }

    #[test]
    fn render_reports_branch_and_count() {
        let applied = applied_response(true, 8, 3);
        let human = render(&applied, &binding(), OutputMode::Human);
        assert!(human.contains("3 changes"), "{human}");
        assert!(human.contains("branch 'spicy'"), "{human}");
        assert!(human.contains("version 8"), "{human}");

        let v: serde_json::Value =
            serde_json::from_str(&render(&applied, &binding(), OutputMode::Json)).unwrap();
        assert_eq!(v["committed"]["delta_count"], 3);
        // Numeric (from branch.version), not the stringly response `version`.
        assert_eq!(v["committed"]["version"], 8);
        assert_eq!(v["committed"]["branch"], "spicy");
    }

    #[test]
    fn nothing_to_commit_carries_the_branch_in_both_modes() {
        assert_eq!(
            render_nothing(&binding(), OutputMode::Human),
            "Nothing to commit."
        );
        let v: serde_json::Value =
            serde_json::from_str(&render_nothing(&binding(), OutputMode::Json)).unwrap();
        assert_eq!(v["committed"]["delta_count"], 0);
        // Same shape as a real commit: branch present (version null).
        assert_eq!(v["committed"]["branch"], "spicy");
        assert!(v["committed"]["version"].is_null());
    }

    // The data-loss-critical invariant: the stage is cleared ONLY when the
    // server confirms the batch applied; otherwise it is preserved and loud.
    fn staged_dir() -> tempfile::TempDir {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut stage = Stage::empty();
        stage.deltas.push(serde_json::json!({"type": "add_node"}));
        stage.save(tmp.path()).unwrap();
        tmp
    }

    #[test]
    fn finalize_clears_the_stage_only_on_a_confirmed_apply() {
        let tmp = staged_dir();
        let out = finalize(
            tmp.path(),
            &binding(),
            &applied_response(true, 8, 1),
            OutputMode::Human,
        )
        .unwrap();
        assert!(out.contains("version 8"), "{out}");
        // Stage is now empty on disk.
        assert!(Stage::load(tmp.path()).unwrap().deltas.is_empty());
    }

    #[test]
    fn finalize_keeps_the_stage_when_the_server_did_not_apply() {
        let tmp = staged_dir();
        let err = finalize(
            tmp.path(),
            &binding(),
            &applied_response(false, 8, 0),
            OutputMode::Human,
        )
        .unwrap_err();
        assert!(err.to_string().contains("kept"), "{err}");
        // The staged work survives — nothing was wiped.
        assert_eq!(Stage::load(tmp.path()).unwrap().deltas.len(), 1);
    }

    #[test]
    fn stage_has_mutation_detects_deletes_and_updates_only() {
        let mut adds = Stage::empty();
        adds.deltas.push(serde_json::json!({"type": "add_node"}));
        adds.deltas.push(serde_json::json!({"type": "add_edge"}));
        assert!(
            !stage_has_mutation(&adds),
            "pure adds aren't a mutation commit"
        );

        for kind in [
            "delete_node",
            "delete_edge",
            "update_node_data",
            "reparent_node",
        ] {
            let mut s = Stage::empty();
            s.deltas.push(serde_json::json!({"type": kind}));
            assert!(
                stage_has_mutation(&s),
                "{kind} should require a pulled index"
            );
        }
    }

    #[test]
    fn require_index_for_mutation_gates_deletes_without_a_pull() {
        let mut del = Stage::empty();
        del.deltas.push(serde_json::json!({"type": "delete_node"}));
        // No index + a deletion → loud error (don't mutate an unpulled branch).
        let err = require_index_for_mutation(false, &del).unwrap_err();
        assert!(err.to_string().contains("hydrate pull"), "{err}");
        // With an index present, the same batch is allowed.
        assert!(require_index_for_mutation(true, &del).is_ok());
        // A pure-add batch needs no index.
        let mut add = Stage::empty();
        add.deltas.push(serde_json::json!({"type": "add_node"}));
        assert!(require_index_for_mutation(false, &add).is_ok());
    }

    #[test]
    fn finalize_advances_the_pulled_index_version_on_success() {
        let tmp = staged_dir();
        // Pulled at v3 with a real entry; the commit lands the branch at v8.
        let mut entries = std::collections::BTreeMap::new();
        entries.insert("node:Api".to_string(), Uuid::from_u128(0xA));
        Index {
            version: 3,
            entries,
        }
        .save(tmp.path())
        .unwrap();

        finalize(
            tmp.path(),
            &binding(),
            &applied_response(true, 8, 1),
            OutputMode::Human,
        )
        .unwrap();

        // Index version moved forward so the NEXT commit targets v8, not v3
        // (which would false-conflict)...
        let index = Index::load(tmp.path()).unwrap().unwrap();
        assert_eq!(index.version, 8);
        // ...and the pulled path→UUID entries are preserved (only the OCC token
        // moves; the committed graph the entries describe didn't change).
        assert_eq!(index.get("node:Api"), Some(Uuid::from_u128(0xA)));
    }

    #[test]
    fn finalize_does_not_fabricate_an_index_when_none_was_pulled() {
        let tmp = staged_dir();
        finalize(
            tmp.path(),
            &binding(),
            &applied_response(true, 8, 1),
            OutputMode::Human,
        )
        .unwrap();
        // No pull happened, so no index should be written by commit.
        assert_eq!(Index::load(tmp.path()).unwrap(), None);
    }

    #[test]
    fn finalize_leaves_the_index_untouched_when_the_server_did_not_apply() {
        let tmp = staged_dir();
        Index {
            version: 3,
            entries: Default::default(),
        }
        .save(tmp.path())
        .unwrap();
        let _ = finalize(
            tmp.path(),
            &binding(),
            &applied_response(false, 8, 0),
            OutputMode::Human,
        )
        .unwrap_err();
        // A failed apply must not advance the OCC token.
        assert_eq!(Index::load(tmp.path()).unwrap().unwrap().version, 3);
    }
}
