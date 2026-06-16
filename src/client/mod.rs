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

    /// Create a new working branch off main in `project_id`, named `name`.
    /// The server is the authority on naming and collisions — a duplicate or
    /// otherwise-invalid name comes back as a loud error, never a silent reuse.
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
