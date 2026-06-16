//! Integration smoke test against a live `/v1`: the client authenticates and
//! performs a real read (liveness + an authenticated list).
//!
//! Ignored by default, so it shows as `ignored` in the test summary rather than
//! masquerading as a passing run. Run it against a live backend:
//!
//!   HYD_BASE_URL=... HYD_API_KEY=... cargo test --test runtime -- --ignored

use hydrate::client::Client;
use hydrate::config::Config;

#[test]
#[ignore = "requires a live backend: set HYD_BASE_URL + HYD_API_KEY and run with --ignored"]
fn live_health_and_authenticated_read() {
    let base_url =
        std::env::var("HYD_BASE_URL").expect("HYD_BASE_URL must be set to run this test");
    let api_key = std::env::var("HYD_API_KEY").expect("HYD_API_KEY must be set to run this test");

    let client = Client::new(&Config { base_url, api_key }).expect("build client");

    // Unauthenticated liveness read — proves base URL + transport.
    let health = client.health().expect("health read failed");
    assert!(health.ok, "service reported not-ok");

    // Authenticated read — proves the Bearer credential is sent and accepted.
    client
        .list_projects()
        .expect("authenticated projects read failed");
}
