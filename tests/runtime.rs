//! Integration smoke test for the runtime transport against a live `/v1`.
//!
//! Gated on `HYD_BASE_URL` + `HYD_API_KEY`. When unset it is a no-op that prints
//! a note (it is NOT silently skipped — the line records that it did not run).
//! When set, it proves the client authenticates and performs a real read — the
//! Phase 1 exit criterion. PR CI runs the unit tier only; this runs when a
//! backend is available.

use hydrate::client::Client;
use hydrate::config::Config;

#[test]
fn live_health_and_authenticated_read() {
    let (base_url, api_key) = match (std::env::var("HYD_BASE_URL"), std::env::var("HYD_API_KEY")) {
        (Ok(b), Ok(k)) if !b.trim().is_empty() && !k.trim().is_empty() => (b, k),
        _ => {
            eprintln!(
                "runtime integration test NOT RUN: set HYD_BASE_URL + HYD_API_KEY \
                 to exercise a live /v1 read"
            );
            return;
        }
    };

    let client = Client::new(&Config { base_url, api_key }).expect("build client");

    // Unauthenticated liveness read — proves base URL + transport.
    let health = client.health().expect("health read failed");
    assert!(health.ok, "service reported not-ok");

    // Authenticated read — proves the Bearer credential is sent and accepted.
    client
        .list_projects()
        .expect("authenticated projects read failed");
}
