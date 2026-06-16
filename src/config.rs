//! Configuration loaded from the environment (and an optional `.env`).
//!
//! `HYD_API_KEY` is required; `HYD_BASE_URL` overrides the baked default service
//! URL (for local development). The key is read from the environment only and is
//! never written to disk — nor exposed by a debug-format (see the `Debug` impl).

use std::fmt;

use crate::error::CliError;

/// The default service URL. Overridable via `HYD_BASE_URL`.
pub const DEFAULT_BASE_URL: &str = "https://api.hydrate.sh";

#[derive(Clone)]
pub struct Config {
    pub base_url: String,
    pub api_key: String,
}

// Hand-written (not derived) so the API key can never leak through a `{:?}`.
impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("base_url", &self.base_url)
            .field("api_key", &"<redacted>")
            .finish()
    }
}

impl Config {
    /// Load from the process environment, reading a `.env` file first if present.
    pub fn load() -> Result<Config, CliError> {
        // A missing `.env` is fine; a malformed one is a real config problem, so
        // warn loudly rather than letting the offending vars silently vanish.
        if let Err(e) = dotenvy::dotenv() {
            if !e.not_found() {
                eprintln!("hydrate: warning: ignoring malformed .env file: {e}");
            }
        }
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    /// Pure resolver over a key→value lookup — the unit-testable core (no global
    /// env access, so tests can't race).
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Config, CliError> {
        let api_key = get("HYD_API_KEY")
            .filter(|s| !s.trim().is_empty())
            .ok_or(CliError::MissingApiKey)?;
        let base_url = get("HYD_BASE_URL")
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        validate_base_url(&base_url)?;
        Ok(Config { base_url, api_key })
    }
}

/// Reject a base URL that would send the bearer credential in cleartext to a
/// non-local host: `http` is allowed only for localhost (local development); any
/// other host must use `https`.
fn validate_base_url(base_url: &str) -> Result<(), CliError> {
    let url = url::Url::parse(base_url)
        .map_err(|e| CliError::InvalidBaseUrl(format!("{base_url}: {e}")))?;
    match url.scheme() {
        "https" => Ok(()),
        "http" => {
            let host = url.host_str().unwrap_or("");
            if host == "localhost" || host.starts_with("127.") || host == "::1" || host == "[::1]" {
                Ok(())
            } else {
                Err(CliError::InsecureBaseUrl(base_url.to_string()))
            }
        }
        other => Err(CliError::InvalidBaseUrl(format!(
            "{base_url}: unsupported scheme {other:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| m.get(k).cloned()
    }

    #[test]
    fn missing_api_key_is_an_error() {
        assert!(matches!(
            Config::from_lookup(env(&[])),
            Err(CliError::MissingApiKey)
        ));
    }

    #[test]
    fn blank_api_key_is_treated_as_missing() {
        assert!(matches!(
            Config::from_lookup(env(&[("HYD_API_KEY", "   ")])),
            Err(CliError::MissingApiKey)
        ));
    }

    #[test]
    fn defaults_base_url_when_unset() {
        let c = Config::from_lookup(env(&[("HYD_API_KEY", "k")])).unwrap();
        assert_eq!(c.base_url, DEFAULT_BASE_URL);
        assert_eq!(c.api_key, "k");
    }

    #[test]
    fn base_url_override_is_honored() {
        let c = Config::from_lookup(env(&[
            ("HYD_API_KEY", "k"),
            ("HYD_BASE_URL", "http://localhost:8001"),
        ]))
        .unwrap();
        assert_eq!(c.base_url, "http://localhost:8001");
    }

    #[test]
    fn http_to_localhost_is_allowed() {
        for url in [
            "http://localhost:8001",
            "http://127.0.0.1:8001",
            "http://[::1]:8001",
        ] {
            assert!(
                Config::from_lookup(env(&[("HYD_API_KEY", "k"), ("HYD_BASE_URL", url)])).is_ok(),
                "{url} should be allowed"
            );
        }
    }

    #[test]
    fn http_to_remote_host_is_rejected() {
        let r = Config::from_lookup(env(&[
            ("HYD_API_KEY", "k"),
            ("HYD_BASE_URL", "http://evil.example.com"),
        ]));
        assert!(
            matches!(r, Err(CliError::InsecureBaseUrl(_))),
            "plaintext http to a remote host must be rejected"
        );
    }

    #[test]
    fn https_remote_is_allowed() {
        assert!(Config::from_lookup(env(&[
            ("HYD_API_KEY", "k"),
            ("HYD_BASE_URL", "https://api.hydrate.sh"),
        ]))
        .is_ok());
    }

    #[test]
    fn unparseable_base_url_is_rejected() {
        let r = Config::from_lookup(env(&[("HYD_API_KEY", "k"), ("HYD_BASE_URL", "not a url")]));
        assert!(matches!(r, Err(CliError::InvalidBaseUrl(_))));
    }
}
