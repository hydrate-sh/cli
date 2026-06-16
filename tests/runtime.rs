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

/// End-to-end: drive the real binary through the whole authoring flow
/// (`fork → node add ×N → edge add → status → diff → commit`) in a fresh
/// working copy, against a live backend. This is the demo graph from
/// `scripts/demo-hotdog-rater.sh`, asserted programmatically.
#[test]
#[ignore = "requires a live backend AND mutates it (forks + commits): run with --ignored"]
fn live_e2e_author_and_commit() {
    use std::path::Path;
    use std::process::Command;

    // Env (HYD_BASE_URL/HYD_API_KEY) is inherited by the spawned binary.
    std::env::var("HYD_BASE_URL").expect("HYD_BASE_URL must be set");
    std::env::var("HYD_API_KEY").expect("HYD_API_KEY must be set");

    let bin = env!("CARGO_BIN_EXE_hydrate");
    let dir = tempfile::TempDir::new().expect("temp workdir");

    // Run `hydrate <args...>` in the temp workdir; assert it exits 0 and return stdout.
    let run = |args: &[&str]| -> String {
        let out = Command::new(bin)
            .args(args)
            .current_dir(dir.path())
            .output()
            .unwrap_or_else(|e| panic!("spawn {args:?}: {e}"));
        assert!(
            out.status.success(),
            "`hydrate {}` failed ({}): {}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).expect("utf8 stdout")
    };

    let branch = format!("cli-e2e-{}", std::process::id());
    run(&["fork", &branch]);
    assert!(Path::new(dir.path()).join(".hydrate").is_dir());

    run(&[
        "node",
        "add",
        "--kind",
        "boundary",
        "--name",
        "Api",
        "--user-kind",
        "service",
    ]);
    run(&[
        "node",
        "add",
        "--kind",
        "behavior",
        "--name",
        "Maker",
        "--parent",
        "Api",
        "--out",
        "dog:HotDog",
    ]);
    run(&[
        "node",
        "add",
        "--kind",
        "behavior",
        "--name",
        "Rater",
        "--parent",
        "Api",
        "--in",
        "raw:HotDog",
        "--out",
        "score:Score",
    ]);
    run(&[
        "edge",
        "add",
        "--from",
        "Api.Maker.dog",
        "--to",
        "Api.Rater.raw",
    ]);

    // status/diff reflect exactly what was staged (4 ops: 3 nodes + 1 edge).
    let status = run(&["--json", "status"]);
    assert!(status.contains("\"total\":4"), "status: {status}");
    let diff = run(&["--json", "diff"]);
    assert!(diff.contains("add_edge"), "diff: {diff}");

    // Commit lowers + applies the batch; the response reports 4 deltas.
    let committed = run(&["--json", "commit"]);
    assert!(
        committed.contains("\"delta_count\":4"),
        "commit: {committed}"
    );

    // The stage is spent — a second commit has nothing to do.
    let again = run(&["--json", "commit"]);
    assert!(
        again.contains("\"delta_count\":0"),
        "second commit: {again}"
    );
}
