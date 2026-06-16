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

#[test]
#[ignore = "requires a live backend AND mutates it (creates a branch): run with --ignored"]
fn live_create_branch_then_list_includes_it() {
    let base_url =
        std::env::var("HYD_BASE_URL").expect("HYD_BASE_URL must be set to run this test");
    let api_key = std::env::var("HYD_API_KEY").expect("HYD_API_KEY must be set to run this test");

    let client = Client::new(&Config { base_url, api_key }).expect("build client");

    let project = client
        .list_projects()
        .expect("projects read failed")
        .projects
        .into_iter()
        .find(|p| !p.archived)
        .expect("need at least one active project");

    // Unique-ish per run so reruns don't collide on the branch name.
    let name = format!("cli-it-{}", std::process::id());
    let created = client
        .create_branch(project.id, &name)
        .expect("create branch failed");
    assert_eq!(
        created.branch.name, name,
        "server named the branch differently"
    );

    let listed = client
        .list_branches(project.id)
        .expect("list branches failed");
    assert!(
        listed.branches.iter().any(|b| b.id == created.branch.id),
        "the freshly-created branch is missing from the branch list"
    );
}
