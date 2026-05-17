use std::collections::HashMap;

use quire_core::ci::transport::ApiSession;
use quire_core::secret::{Error as SecretError, SecretRegistry, SecretString};

/// Determines how this run resolves secret values into a [`SecretRegistry`].
///
/// Each variant is a separate named type so new sources (env vars, CLI
/// parameters, etc.) can be added without touching the dispatch logic.
pub trait SecretSource {
    fn into_registry(self) -> SecretRegistry;
}

/// Reads revealed secret values that were baked into the bootstrap file by
/// the orchestrator. The current default for orchestrated runs.
///
/// Once the [`ApiSecrets`] path is validated in production, this type and
/// the corresponding values in the bootstrap file will be removed.
pub struct BootstrapSecrets {
    pub secrets: HashMap<String, SecretString>,
}

impl SecretSource for BootstrapSecrets {
    fn into_registry(self) -> SecretRegistry {
        SecretRegistry::from(self.secrets)
    }
}

/// Fetches each secret on demand from quire-server via
/// `GET /api/runs/{run_id}/secrets/{name}`.
///
/// The bootstrap secrets map is ignored in this path; only run metadata
/// travels via the bootstrap file.
pub struct ApiSecrets {
    pub session: ApiSession,
}

impl SecretSource for ApiSecrets {
    fn into_registry(self) -> SecretRegistry {
        let session = self.session;
        SecretRegistry::from(HashMap::new())
            .with_fallback(move |name| fetch_from_api(&session, name))
    }
}

/// Fetch a single secret from quire-server.
///
/// Uses [`tokio::runtime::Handle::block_on`] to drive the async HTTP call
/// from synchronous Lua callback context. Requires the caller to be on a
/// thread that has entered a Tokio runtime (`rt.enter()` in `main`
/// satisfies this).
fn fetch_from_api(session: &ApiSession, name: &str) -> quire_core::secret::Result<String> {
    let url = format!(
        "{}/api/runs/{}/secrets/{}",
        session.server_url, session.run_id, name
    );
    let token = session.auth_token.clone();
    let name_owned = name.to_string();

    tokio::runtime::Handle::current().block_on(async move {
        let resp = reqwest::Client::new()
            .get(&url)
            .bearer_auth(&token)
            .send()
            .await
            .map_err(|e| SecretError::Resolve(e.to_string()))?;

        let status = resp.status();
        if status.is_success() {
            resp.text()
                .await
                .map_err(|e| SecretError::Resolve(e.to_string()))
        } else if status == reqwest::StatusCode::NOT_FOUND {
            Err(SecretError::UnknownSecret(name_owned))
        } else {
            Err(SecretError::Resolve(format!(
                "secret API returned {status} for {name_owned:?}"
            )))
        }
    })
}
