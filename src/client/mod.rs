//! Hand-written ergonomics over the generated [`hydrate_wire`] client.
//!
//! Builds the wire [`Configuration`] from our [`Config`] (base URL + bearer
//! auth), drives the async generated calls on a small current-thread runtime so
//! callers stay synchronous, and maps wire errors to [`CliError`]. The command
//! handlers use these methods as they are implemented.

use hydrate_wire::apis::configuration::Configuration;
use hydrate_wire::apis::{branches_api, health_api, projects_api};
use hydrate_wire::models;
use uuid::Uuid;

use crate::config::Config;
use crate::error::CliError;

pub struct Client {
    cfg: Configuration,
    rt: tokio::runtime::Runtime,
}

impl Client {
    /// Build a client bound to the configured base URL, sending the API key as a
    /// Bearer token. The key lives only in memory here — never logged or written.
    pub fn new(config: &Config) -> Result<Client, CliError> {
        let cfg = Configuration {
            base_path: config.base_url.trim_end_matches('/').to_string(),
            bearer_access_token: Some(config.api_key.clone()),
            ..Configuration::default()
        };
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| CliError::Other(format!("could not start the async runtime: {e}")))?;
        Ok(Client { cfg, rt })
    }

    /// Like [`Client::new`], but every request also carries an `Idempotency-Key`
    /// header — used by `commit` so a retried apply is at-most-once. The header
    /// rides on the HTTP client itself (the generated apply call takes no
    /// per-request headers); harmless on the read it also makes.
    pub fn with_idempotency_key(config: &Config, key: &str) -> Result<Client, CliError> {
        let mut client = Client::new(config)?;
        let value = reqwest::header::HeaderValue::from_str(key)
            .map_err(|e| CliError::Other(format!("invalid idempotency key: {e}")))?;
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::HeaderName::from_static("idempotency-key"),
            value,
        );
        client.cfg.client = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .map_err(|e| CliError::Network(e.to_string()))?;
        Ok(client)
    }

    /// Liveness probe (unauthenticated). Proves base URL + transport end to end.
    pub fn health(&self) -> Result<models::HealthzResponse, CliError> {
        self.rt
            .block_on(health_api::healthz_v1_healthz_get(&self.cfg))
            .map_err(CliError::from)
    }

    /// List the projects the authenticated principal can see (an authenticated
    /// read — exercises the Bearer credential).
    pub fn list_projects(&self) -> Result<models::ProjectsListResponse, CliError> {
        let params = projects_api::ListProjectsV1ProjectsGetParams { limit: None };
        self.rt
            .block_on(projects_api::list_projects_v1_projects_get(
                &self.cfg, params,
            ))
            .map_err(CliError::from)
    }

    /// Create a new project owned by the caller (server-defaulted language and
    /// intent). The server enforces name uniqueness among your active projects
    /// (case-insensitive, 409 `name_taken`) — this makes no local check.
    pub fn create_project(&self, name: &str) -> Result<models::ProjectCreateResponse, CliError> {
        let params = projects_api::CreateProjectV1ProjectsPostParams {
            v1_create_project_body: models::V1CreateProjectBody::new(name.to_string()),
        };
        self.rt
            .block_on(projects_api::create_project_v1_projects_post(
                &self.cfg, params,
            ))
            .map_err(CliError::from)
    }

    /// Permanently delete a project the caller owns, along with its branches,
    /// graph, and stored artifacts. Irreversible.
    ///
    /// Requires the `project:delete` scope — separate from `graph:write` and
    /// not implied by it, so a 403 here does not mean "not authenticated", it
    /// means "this key was never minted with permission to delete". Callers
    /// should map that 403 to [`CliError::MissingScope`] rather than the
    /// generic `Service` variant (see its doc comment for why that mapping is
    /// route-specific and fragile).
    pub fn delete_project(&self, project_id: Uuid) -> Result<(), CliError> {
        let params = projects_api::DeleteProjectV1ProjectsProjectIdDeleteParams {
            project_id: project_id.to_string(),
        };
        self.rt
            .block_on(projects_api::delete_project_v1_projects_project_id_delete(
                &self.cfg, params,
            ))
            .map_err(CliError::from)
    }

    /// Rename a project and/or set its archived state. `name`/`archived` are
    /// each only sent when `Some`, so an omitted field is left unchanged
    /// server-side — passing both `None` is a caller bug (the server rejects an
    /// empty patch with 422 `no_fields`, but callers should not rely on that).
    pub fn patch_project(
        &self,
        project_id: Uuid,
        name: Option<&str>,
        archived: Option<bool>,
    ) -> Result<models::ProjectPatchResponse, CliError> {
        let params = projects_api::PatchProjectV1ProjectsProjectIdPatchParams {
            project_id: project_id.to_string(),
            v1_patch_project_body: models::V1PatchProjectBody {
                name: name.map(|n| Some(n.to_string())),
                archived: archived.map(Some),
            },
        };
        self.rt
            .block_on(projects_api::patch_project_v1_projects_project_id_patch(
                &self.cfg, params,
            ))
            .map_err(CliError::from)
    }

    /// Create a new working branch off main in `project_id`, named `name`.
    ///
    /// The server does not reject a duplicate branch name, so callers that want
    /// a name to be unique must check first (see `fork`); this method just
    /// issues the create. Invalid input and server-side failures surface loudly.
    pub fn create_branch(
        &self,
        project_id: Uuid,
        name: &str,
    ) -> Result<models::BranchCreateResponse, CliError> {
        let params = branches_api::CreateBranchV1ProjectsProjectIdBranchesPostParams {
            project_id: project_id.to_string(),
            v1_create_branch_body: models::V1CreateBranchBody {
                name: Some(Some(name.to_string())),
            },
        };
        self.rt
            .block_on(
                branches_api::create_branch_v1_projects_project_id_branches_post(&self.cfg, params),
            )
            .map_err(CliError::from)
    }

    /// List the branches of `project_id`.
    pub fn list_branches(&self, project_id: Uuid) -> Result<models::BranchListResponse, CliError> {
        let params = branches_api::ListBranchesV1ProjectsProjectIdBranchesGetParams {
            project_id: project_id.to_string(),
        };
        self.rt
            .block_on(
                branches_api::list_branches_v1_projects_project_id_branches_get(&self.cfg, params),
            )
            .map_err(CliError::from)
    }

    /// The current version of `branch_id` (the optimistic-concurrency token to
    /// pass as `expected_version`). Fails loud if the branch is gone.
    pub fn branch_version(&self, project_id: Uuid, branch_id: Uuid) -> Result<i32, CliError> {
        self.list_branches(project_id)?
            .branches
            .iter()
            .find(|b| b.id == branch_id)
            .map(|b| b.version)
            .ok_or_else(|| {
                CliError::Other(
                    "the bound branch was not found on the server; it may have been deleted"
                        .to_string(),
                )
            })
    }

    /// Fetch the full graph (nodes + edges + branch version) of `branch_id`.
    /// This is the read that backs `pull`: it materializes the live branch so
    /// the author can reference already-committed nodes by their dotted path.
    pub fn fetch_branch_graph(&self, branch_id: Uuid) -> Result<models::GraphResponse, CliError> {
        let params = branches_api::FetchBranchGraphV1BranchesBranchIdGraphGetParams {
            branch_id: branch_id.to_string(),
        };
        self.rt
            .block_on(
                branches_api::fetch_branch_graph_v1_branches_branch_id_graph_get(&self.cfg, params),
            )
            .map_err(CliError::from)
    }

    /// Read one node's subtree on `branch_id`, to a bounded `depth` — a
    /// SCOPED read.
    ///
    /// Unlike [`Client::fetch_branch_graph`], the response is bounded by the
    /// depth requested rather than by the size of the branch, so reading a
    /// slice of a large project does not put the whole graph on the wire.
    /// `depth` 1 is direct children.
    ///
    /// Branch-addressed on purpose: the `/v1/graph/{project_id}/...` twins
    /// resolve the project's MAIN branch, and edits happen on working
    /// branches, so those cannot serve an authoring client at all.
    pub fn fetch_branch_subtree(
        &self,
        branch_id: Uuid,
        node_id: Uuid,
        depth: u32,
    ) -> Result<models::SubtreeResponse, CliError> {
        let params = branches_api::FetchBranchSubtreeV1BranchesBranchIdSubtreeNodeIdGetParams {
            branch_id: branch_id.to_string(),
            node_id: node_id.to_string(),
            depth: Some(depth),
        };
        self.rt
            .block_on(
                branches_api::fetch_branch_subtree_v1_branches_branch_id_subtree_node_id_get(
                    &self.cfg, params,
                ),
            )
            .map_err(CliError::from)
    }

    /// Read one node and its 1-hop neighborhood on `branch_id` — the scoped
    /// read behind `walk`. Only the node, its immediate neighbors, and the
    /// edges between them cross the wire.
    pub fn fetch_branch_node(
        &self,
        branch_id: Uuid,
        node_id: Uuid,
    ) -> Result<models::NodeNeighborhoodResponse, CliError> {
        let params = branches_api::FetchBranchNodeV1BranchesBranchIdNodeNodeIdGetParams {
            branch_id: branch_id.to_string(),
            node_id: node_id.to_string(),
        };
        self.rt
            .block_on(
                branches_api::fetch_branch_node_v1_branches_branch_id_node_node_id_get(
                    &self.cfg, params,
                ),
            )
            .map_err(CliError::from)
    }

    /// Read a boundary's direct children and their interior edges on
    /// `branch_id` — the scoped read behind `walk --boundary`.
    pub fn fetch_branch_boundary(
        &self,
        branch_id: Uuid,
        node_id: Uuid,
    ) -> Result<models::BoundaryResponse, CliError> {
        let params = branches_api::FetchBranchBoundaryV1BranchesBranchIdBoundaryNodeIdGetParams {
            branch_id: branch_id.to_string(),
            node_id: node_id.to_string(),
        };
        self.rt
            .block_on(
                branches_api::fetch_branch_boundary_v1_branches_branch_id_boundary_node_id_get(
                    &self.cfg, params,
                ),
            )
            .map_err(CliError::from)
    }

    /// Dry-run a typed delta batch against `branch_id` and get back the coherence
    /// findings — never mutating the branch. Mirrors [`Client::apply_deltas`], but
    /// the server rolls back before commit, so it carries no optimistic-concurrency
    /// token: it reports what the resulting graph's coherence would be.
    pub fn validate_deltas(
        &self,
        branch_id: Uuid,
        body: models::V1ValidateBody,
    ) -> Result<models::ValidateResponse, CliError> {
        let params = branches_api::ValidateBranchDeltasV1BranchesBranchIdValidatePostParams {
            branch_id: branch_id.to_string(),
            v1_validate_body: body,
        };
        self.rt
            .block_on(
                branches_api::validate_branch_deltas_v1_branches_branch_id_validate_post(
                    &self.cfg, params,
                ),
            )
            .map_err(CliError::from)
    }

    /// Apply a typed delta batch to `branch_id` under optimistic concurrency.
    pub fn apply_deltas(
        &self,
        branch_id: Uuid,
        body: models::V1DeltasBody,
    ) -> Result<models::DeltaApplyResponse, CliError> {
        let params = branches_api::ApplyBranchDeltasV1BranchesBranchIdDeltasPostParams {
            branch_id: branch_id.to_string(),
            v1_deltas_body: body,
        };
        self.rt
            .block_on(
                branches_api::apply_branch_deltas_v1_branches_branch_id_deltas_post(
                    &self.cfg, params,
                ),
            )
            .map_err(CliError::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_client_builds_without_network() {
        // Constructing a client must not touch the network (it only sets up the
        // Configuration + runtime); a bogus URL is fine until a call is made.
        let cfg = Config {
            base_url: "http://127.0.0.1:1".to_string(),
            api_key: "k".to_string(),
        };
        assert!(Client::new(&cfg).is_ok());
    }
}
