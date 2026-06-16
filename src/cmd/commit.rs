//! `commit` — lower the staged changeset into a typed delta batch and apply it
//! to the bound branch under optimistic concurrency, then clear the stage.

use sha2::{Digest, Sha256};

use super::context::require_workdir;
use crate::client::Client;
use crate::config::Config;
use crate::error::CliError;
use crate::output::OutputMode;
use crate::staging::lower;
use crate::state::{Binding, Stage};

pub fn run(mode: OutputMode) -> Result<(), CliError> {
    let base = require_workdir()?;
    let binding = Binding::load(&base)?.ok_or_else(|| {
        CliError::Other(
            "this working copy is not bound to a branch; run `hydrate fork`".to_string(),
        )
    })?;
    let stage = Stage::load(&base)?;
    if stage.deltas.is_empty() {
        println!("{}", render_nothing(mode));
        return Ok(());
    }

    let deltas = lower(&stage)?;
    let key = idempotency_key(&deltas)?;

    let config = Config::load()?;
    let client = Client::with_idempotency_key(&config, &key)?;

    // Fetch-before-commit: the current branch version is the OCC token. A 409
    // here means the branch moved — surfaced as a conflict (exit 4), and because
    // we only clear the stage AFTER a successful apply, the staged work survives.
    let expected_version = u32::try_from(
        client.branch_version(binding.project_id, binding.branch_id)?,
    )
    .map_err(|_| CliError::Other("the server reported a negative branch version".to_string()))?;

    let body = hydrate_wire::models::V1DeltasBody {
        deltas: Some(deltas),
        expected_version,
        positions: None,
    };
    let applied = client.apply_deltas(binding.branch_id, body)?;

    // Success: the batch is on the branch, so the local stage is spent.
    Stage::empty().save(&base)?;

    println!("{}", render(&applied, &binding, mode));
    Ok(())
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

fn render_nothing(mode: OutputMode) -> String {
    match mode {
        OutputMode::Json => serde_json::json!({ "committed": { "delta_count": 0 } }).to_string(),
        OutputMode::Human => "Nothing to commit.".to_string(),
    }
}

fn render(
    applied: &hydrate_wire::models::DeltaApplyResponse,
    binding: &Binding,
    mode: OutputMode,
) -> String {
    match mode {
        OutputMode::Json => serde_json::json!({
            "committed": {
                "branch": binding.branch_name,
                "delta_count": applied.delta_count,
                "version": applied.version,
            }
        })
        .to_string(),
        OutputMode::Human => format!(
            "Committed {} to branch '{}'. Now at version {}.",
            plural(applied.delta_count, "change"),
            binding.branch_name,
            applied.version,
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

    fn node_delta() -> models::V1DeltasBodyDeltasInner {
        models::V1DeltasBodyDeltasInner::AddNode(Box::new(models::AddNodeDelta::new(
            models::Node {
                data: None,
                id: Uuid::from_u128(9),
                kind: models::node::Kind::Behavior,
                parent_id: Some(None),
            },
            models::add_node_delta::Type::AddNode,
        )))
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
        let one = idempotency_key(&[node_delta()]).unwrap();
        let two = idempotency_key(&[node_delta(), node_delta()]).unwrap();
        assert_ne!(one, two);
    }

    #[test]
    fn render_reports_branch_and_count() {
        let applied = models::DeltaApplyResponse {
            applied: true,
            branch: Box::new(models::BranchRef {
                id: Uuid::from_u128(2),
                version: 8,
            }),
            delta_count: 3,
            positions_applied: None,
            project_id: Uuid::from_u128(1),
            version: "8".to_string(),
        };
        let human = render(&applied, &binding(), OutputMode::Human);
        assert!(human.contains("3 changes"), "{human}");
        assert!(human.contains("branch 'spicy'"), "{human}");
        assert!(human.contains("version 8"), "{human}");

        let v: serde_json::Value =
            serde_json::from_str(&render(&applied, &binding(), OutputMode::Json)).unwrap();
        assert_eq!(v["committed"]["delta_count"], 3);
        assert_eq!(v["committed"]["version"], "8");
        assert_eq!(v["committed"]["branch"], "spicy");
    }

    #[test]
    fn nothing_to_commit_is_loud_in_both_modes() {
        assert_eq!(render_nothing(OutputMode::Human), "Nothing to commit.");
        let v: serde_json::Value = serde_json::from_str(&render_nothing(OutputMode::Json)).unwrap();
        assert_eq!(v["committed"]["delta_count"], 0);
    }
}
