//! Configuration loaded from the environment (and an optional `.env`).
//!
//! `HYD_API_KEY` is required; `HYD_BASE_URL` overrides the baked default service
//! URL (for local development). The key is read from the environment only and is
//! never written to disk.

use crate::error::CliError;

/// The default service URL. Overridable via `HYD_BASE_URL`.
pub const DEFAULT_BASE_URL: &str = "https://api.hydrate.sh";

#[derive(Debug, Clone)]
pub struct Config {
    pub base_url: String,
    pub api_key: String,
}

impl Config {
    /// Load from the process environment, reading a `.env` file first if present
    /// (its absence is not an error).
    pub fn load() -> Result<Config, CliError> {
        let _ = dotenvy::dotenv(); // best-effort; missing .env is fine
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
        Ok(Config { base_url, api_key })
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
        let r = Config::from_lookup(env(&[]));
        assert!(matches!(r, Err(CliError::MissingApiKey)));
    }

    #[test]
    fn blank_api_key_is_treated_as_missing() {
        let r = Config::from_lookup(env(&[("HYD_API_KEY", "   ")]));
        assert!(matches!(r, Err(CliError::MissingApiKey)));
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
}
