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

/// End-to-end against a live backend: create a project, rename it, archive it
/// (which drops it from the default listing but must NOT make it
/// unreachable), restore it, then delete it — the full `hydrate project`
/// lifecycle this PR adds, exercised at the `Client` layer (the `hydrate
/// project` CLI verbs additionally translate a name to an id via
/// `cmd::project::find_by_name`, and a bare-403 delete into
/// [`hydrate::error::CliError::MissingScope`]; both live in the command
/// layer, which this crate does not expose to integration tests — see their
/// own unit coverage in `cmd::project`).
///
/// The archive/restore half is the round-trip acceptance criterion: archiving
/// must be reversible, not just server-side but through this same client, or
/// `hydrate project archive` is a trap regardless of what its own success
/// message claims.
///
/// Needs a key with BOTH `graph:write` (create/rename/archive/restore) and
/// `project:delete` (delete); a key that lacks the latter is an accepted
/// outcome here too — the delete call comes back a plain 403 `Service` error,
/// which this test tolerates rather than treats as a hard failure, so the test
/// still exercises the rest of the lifecycle against a live backend even with
/// an older key.
#[test]
#[ignore = "requires a live backend AND mutates it (creates + deletes a project): run with --ignored"]
fn live_project_create_rename_archive_restore_delete() {
    use hydrate::error::CliError;

    let base_url =
        std::env::var("HYD_BASE_URL").expect("HYD_BASE_URL must be set to run this test");
    let api_key = std::env::var("HYD_API_KEY").expect("HYD_API_KEY must be set to run this test");

    let client = Client::new(&Config { base_url, api_key }).expect("build client");

    let name = format!("cli-it-{}", std::process::id());
    let created = client.create_project(&name).expect("create failed");
    assert_eq!(created.project.name, name);

    let renamed_to = format!("{name}-renamed");
    let patched = client
        .patch_project(created.project.id, Some(&renamed_to), None)
        .expect("rename failed");
    assert_eq!(patched.project.name, renamed_to);

    let archived = client
        .patch_project(created.project.id, None, Some(true))
        .expect("archive failed");
    assert!(archived.project.archived);
    // The project must now be invisible to the DEFAULT listing (server
    // behavior `hydrate projects` relies on)...
    assert!(
        !client
            .list_projects()
            .expect("list failed")
            .projects
            .iter()
            .any(|p| p.id == created.project.id),
        "an archived project must not appear in the default GET /v1/projects"
    );
    // ...but must still be visible, and by its current name, through the
    // archived-inclusive listing the name-addressed `project` verbs use to
    // resolve `archive`/`restore`/`rename`/`delete` targets. If this regresses,
    // an archived project's name becomes unreachable by every one of them.
    let seen_archived = client
        .list_projects_including_archived()
        .expect("archived-inclusive list failed")
        .projects
        .into_iter()
        .find(|p| p.id == created.project.id)
        .expect("archived project missing from the archived-inclusive listing");
    assert_eq!(seen_archived.name, renamed_to);
    assert!(seen_archived.archived);

    // Restore: the other half of the round trip. Must reappear in the
    // default listing.
    let restored = client
        .patch_project(created.project.id, None, Some(false))
        .expect("restore failed");
    assert!(!restored.project.archived);
    assert!(
        client
            .list_projects()
            .expect("list failed")
            .projects
            .iter()
            .any(|p| p.id == created.project.id),
        "a restored project must reappear in the default GET /v1/projects"
    );

    match client.delete_project(created.project.id) {
        Ok(()) => {}
        // The key this test runs with may not have been minted with
        // project:delete — an accepted, documented outcome (the CLI verb
        // turns this specific shape into CliError::MissingScope; see its own
        // unit coverage in cmd::project), not a failure of this test. A key
        // that does hold the scope reaches the Ok(()) arm above and leaves
        // nothing behind.
        Err(CliError::Service { status: 403, .. }) => {}
        Err(e) => panic!("delete failed with an unexpected error: {e}"),
    }
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
