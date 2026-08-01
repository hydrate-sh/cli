//! Proves `walk` actually issues a SCOPED request.
//!
//! Every other test in this repo exercises the renderers with hand-built
//! responses, which means the dispatch itself — the entire point of the scoped
//! reads — is unverified: reverting `walk` to fetch the whole branch graph
//! leaves the unit suite green. This drives the real binary against a local
//! listener and asserts the request line, so "we stopped putting the whole
//! branch on the wire" is a checked claim rather than a described one.
//!
//! Uses a stdlib TcpListener rather than a mock-server crate: the assertion is
//! one request line, which does not justify a new dependency.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::process::Command;
use std::sync::mpsc;

const BOUND_BRANCH: &str = "11111111-1111-1111-1111-111111111111";
const NODE_ID: &str = "22222222-2222-2222-2222-222222222222";

/// Serve one request, reply with `body`, and report the request line.
fn serve_once(body: &'static str) -> (String, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = format!("http://{}", listener.local_addr().unwrap());
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            let mut reader = BufReader::new(&stream);
            let mut line = String::new();
            let _ = reader.read_line(&mut line);
            let _ = tx.send(line.trim().to_string());
            let mut stream = &stream;
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.flush();
        }
    });
    (addr, rx)
}

/// A working copy whose index also records the node's KIND, as a real pull does.
fn workdir_with_kind(kind: &str) -> tempfile::TempDir {
    let dir = workdir();
    std::fs::write(
        dir.path().join(".hydrate").join("index.json"),
        serde_json::json!({
            "version": 2,
            "entries": { "node:Api": NODE_ID },
            "node_info": {
                NODE_ID: { "kind": kind, "inputs": [], "outputs": [], "config": [] }
            },
            "edges": {},
        })
        .to_string(),
    )
    .unwrap();
    dir
}

/// A working copy bound to BOUND_BRANCH whose index knows `Api` -> NODE_ID.
fn workdir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let hy = dir.path().join(".hydrate");
    std::fs::create_dir_all(&hy).unwrap();
    std::fs::write(
        hy.join("config.toml"),
        format!(
            "project_id = \"33333333-3333-3333-3333-333333333333\"\n\
             project_name = \"p\"\n\
             branch_id = \"{BOUND_BRANCH}\"\n\
             branch_name = \"work\"\n"
        ),
    )
    .unwrap();
    std::fs::write(
        hy.join("index.json"),
        serde_json::json!({
            "version": 2,
            "entries": { "node:Api": NODE_ID },
            "node_info": {},
            "edges": {},
        })
        .to_string(),
    )
    .unwrap();
    dir
}

fn run_walk(dir: &tempfile::TempDir, base_url: &str, args: &[&str]) {
    Command::new(env!("CARGO_BIN_EXE_hydrate"))
        .args(args)
        .current_dir(dir.path())
        .env("HYD_BASE_URL", base_url)
        .env("HYD_API_KEY", "test-key-not-a-real-credential")
        .output()
        .expect("binary should run");
}

#[test]
fn walk_requests_the_scoped_node_endpoint_on_the_bound_branch() {
    let body = r#"{"version":"v1","project_id":"33333333-3333-3333-3333-333333333333",
        "branch":{"id":"11111111-1111-1111-1111-111111111111","version":1},
        "node":{"id":"22222222-2222-2222-2222-222222222222","kind":"behavior",
                "parent_id":null,"position":{"x":0,"y":0},
                "data":{"name":"Api","description":"","status":"idle",
                        "is_test_node":false,"is_external":false}},
        "neighbors":[],"edges":[],
        "paths":{"22222222-2222-2222-2222-222222222222":"Api"},
        "unaddressable":{}}"#;
    let (addr, rx) = serve_once(body);
    let dir = workdir();
    run_walk(&dir, &addr, &["walk", "Api"]);

    let request = rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
    assert!(
        request.contains(&format!("/v1/branches/{BOUND_BRANCH}/node/{NODE_ID}")),
        "walk must issue the SCOPED read on the bound branch, got: {request}"
    );
    assert!(
        !request.contains("/graph"),
        "walk must NOT fetch the whole branch graph, got: {request}"
    );
}

#[test]
fn walk_boundary_requests_the_scoped_boundary_endpoint() {
    let body = r#"{"version":"v1","project_id":"33333333-3333-3333-3333-333333333333",
        "branch":{"id":"11111111-1111-1111-1111-111111111111","version":1},
        "boundary":{"id":"22222222-2222-2222-2222-222222222222","kind":"boundary",
                "parent_id":null,"position":{"x":0,"y":0},
                "data":{"name":"Api","description":"","status":"idle",
                        "is_test_node":false,"is_external":false}},
        "children":[],"edges":[],
        "paths":{"22222222-2222-2222-2222-222222222222":"Api"},
        "unaddressable":{}}"#;
    let (addr, rx) = serve_once(body);
    let dir = workdir();
    run_walk(&dir, &addr, &["walk", "Api", "--boundary"]);

    let request = rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
    assert!(
        request.contains(&format!("/v1/branches/{BOUND_BRANCH}/boundary/{NODE_ID}")),
        "got: {request}"
    );
}

#[test]
fn without_an_index_walk_falls_back_to_the_whole_graph_read() {
    // The fallback is a real behaviour, not an error path — and it is the one
    // case where fetching everything is correct. Pin it so the dispatch can't
    // silently lose the distinction.
    let dir = tempfile::tempdir().unwrap();
    let hy = dir.path().join(".hydrate");
    std::fs::create_dir_all(&hy).unwrap();
    std::fs::write(
        hy.join("config.toml"),
        format!(
            "project_id = \"33333333-3333-3333-3333-333333333333\"\n\
             project_name = \"p\"\n\
             branch_id = \"{BOUND_BRANCH}\"\n\
             branch_name = \"work\"\n"
        ),
    )
    .unwrap();
    let (addr, rx) = serve_once(
        r#"{"version":"v1","project_id":"33333333-3333-3333-3333-333333333333","branch":{"id":"11111111-1111-1111-1111-111111111111","version":1},"nodes":[],"edges":[]}"#,
    );
    run_walk(&dir, &addr, &["walk", "Api"]);

    let request = rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
    assert!(
        request.contains(&format!("/v1/branches/{BOUND_BRANCH}/graph")),
        "with no index the whole-graph read is correct, got: {request}"
    );
}

#[test]
fn walk_boundary_on_a_behavior_fails_before_making_a_request() {
    // The server 404s a non-boundary id, so a check that runs on the RESPONSE
    // can never fire — the user just gets `service error (404)`. The guard has
    // to preempt the request, and the way to prove it did is that no request
    // ever arrives.
    let (addr, rx) = serve_once("{}");
    let dir = workdir_with_kind("behavior");
    let out = Command::new(env!("CARGO_BIN_EXE_hydrate"))
        .args(["walk", "Api", "--boundary"])
        .current_dir(dir.path())
        .env("HYD_BASE_URL", &addr)
        .env("HYD_API_KEY", "test-key-not-a-real-credential")
        .output()
        .expect("binary should run");

    // Pin the CONTRACT, not the phrasing: it must name the problem, the
    // remedy, and — because this verdict comes from a snapshot — that the
    // index may be behind.
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("is not a boundary"), "got: {stderr}");
    assert!(
        stderr.contains("hydrate walk Api"),
        "must name the remedy: {stderr}"
    );
    assert!(
        stderr.contains("hydrate pull"),
        "must admit the index may be behind: {stderr}"
    );
    assert_eq!(
        out.status.code(),
        Some(1),
        "stable exit code for a bad argument"
    );
    assert!(
        out.stdout.is_empty(),
        "the error belongs on stderr so piped stdout stays parseable"
    );
    assert!(
        rx.recv_timeout(std::time::Duration::from_millis(750))
            .is_err(),
        "the guard must fire BEFORE the request, but one was sent"
    );
}

#[test]
fn walk_boundary_on_a_real_boundary_still_requests() {
    let body = r#"{"version":"v1","project_id":"33333333-3333-3333-3333-333333333333",
        "branch":{"id":"11111111-1111-1111-1111-111111111111","version":1},
        "boundary":{"id":"22222222-2222-2222-2222-222222222222","kind":"boundary",
                "parent_id":null,"position":{"x":0,"y":0},
                "data":{"name":"Api","description":"","status":"idle",
                        "is_test_node":false,"is_external":false}},
        "children":[],"edges":[],
        "paths":{"22222222-2222-2222-2222-222222222222":"Api"},
        "unaddressable":{}}"#;
    let (addr, rx) = serve_once(body);
    let dir = workdir_with_kind("boundary");
    run_walk(&dir, &addr, &["walk", "Api", "--boundary"]);
    let request = rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
    assert!(request.contains("/boundary/"), "got: {request}");
}

#[test]
fn an_index_without_kinds_defers_to_the_server() {
    // `node_info` is #[serde(default)] so an index pulled by an older CLI
    // still loads. It resolves the path but knows no kind — the request must
    // still go out, and the user must be told why the local check was skipped.
    let body = r#"{"version":"v1","project_id":"33333333-3333-3333-3333-333333333333",
        "branch":{"id":"11111111-1111-1111-1111-111111111111","version":1},
        "boundary":{"id":"22222222-2222-2222-2222-222222222222","kind":"boundary",
                "parent_id":null,"position":{"x":0,"y":0},
                "data":{"name":"Api","description":"","status":"idle",
                        "is_test_node":false,"is_external":false}},
        "children":[],"edges":[],
        "paths":{"22222222-2222-2222-2222-222222222222":"Api"},
        "unaddressable":{}}"#;
    let (addr, rx) = serve_once(body);
    let dir = workdir(); // index has `entries` but an empty `node_info`
    let out = Command::new(env!("CARGO_BIN_EXE_hydrate"))
        .args(["walk", "Api", "--boundary"])
        .current_dir(dir.path())
        .env("HYD_BASE_URL", &addr)
        .env("HYD_API_KEY", "test-key-not-a-real-credential")
        .output()
        .expect("binary should run");

    let request = rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
    assert!(request.contains("/boundary/"), "must defer, got: {request}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no kind for") && stderr.contains("hydrate pull"),
        "the skipped local check must be reported: {stderr}"
    );
}

#[test]
fn an_unrecognised_kind_defers_rather_than_rejecting() {
    // An index written by a NEWER CLI can carry a kind this build predates.
    // Rejecting it would block a legal request with no way past; the server
    // is the authority on what its own route accepts.
    let (addr, rx) = serve_once("{}");
    let dir = workdir_with_kind("hyperboundary");
    Command::new(env!("CARGO_BIN_EXE_hydrate"))
        .args(["walk", "Api", "--boundary"])
        .current_dir(dir.path())
        .env("HYD_BASE_URL", &addr)
        .env("HYD_API_KEY", "test-key-not-a-real-credential")
        .output()
        .expect("binary should run");

    let request = rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
    assert!(
        request.contains("/boundary/"),
        "an unknown kind must defer to the server, got: {request}"
    );
}

/// Usage errors exit `2`, which the argument parser owns and no constant in
/// `exit.rs` describes. Documented in `guide` and in the reference; pinned here
/// against the real binary, because a test comparing one hardcoded list to
/// another cannot fail when the behaviour changes.
#[test]
fn usage_errors_exit_two() {
    for args in [
        vec!["--definitely-not-a-flag"],
        vec!["walk"],                 // required PATH missing
        vec!["show", "--depth", "1"], // --depth requires PATH
    ] {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_hydrate"))
            .args(&args)
            .output()
            .expect("run hydrate");
        assert_eq!(
            out.status.code(),
            Some(2),
            "expected exit 2 for {args:?}, got {:?}\n{}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// The breaking change must leave a runtime trace, and the default must be the
/// partitioned path.
///
/// A review deleted the stderr note, and separately forced `whole_branch` on,
/// and the entire suite still passed both times — so nothing pinned either the
/// signal or the flag dispatch. This runs the real binary against a server whose
/// baseline and staged reports are identical: a caller who used to get exit 5
/// now gets 0, which is exactly the transition the note exists to announce.
#[test]
fn inherited_only_findings_exit_zero_and_say_so_on_stderr() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().take(8) {
            let mut s = match stream {
                Ok(s) => s,
                Err(_) => return,
            };
            use std::io::{Read, Write};
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            let req = String::from_utf8_lossy(&buf).to_string();
            let body = if req.contains("/validate") {
                // Same finding in both reports: inherited, nothing introduced.
                r#"{"valid":false,"project_id":"00000000-0000-0000-0000-000000000001","version":"v1","branch":{"id":"00000000-0000-0000-0000-000000000002","version":4},"findings":[{"code":"unsatisfied_input","locator":"00000000-0000-0000-0000-0000000000cc","message":"input port has no incoming edge","severity":"error"}]}"#.to_string()
            } else if req.contains("/branches") {
                r#"{"branches":[{"id":"00000000-0000-0000-0000-000000000002","name":"demo","is_main":false,"version":4}]}"#.to_string()
            } else {
                r#"{"projects":[{"id":"00000000-0000-0000-0000-000000000001","name":"proj","is_archived":false}]}"#.to_string()
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            let _ = s.write_all(resp.as_bytes());
        }
    });

    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path();
    let hydrate = base.join(".hydrate");
    std::fs::create_dir_all(&hydrate).unwrap();
    std::fs::write(
        hydrate.join("config.toml"),
        "project_id = \"00000000-0000-0000-0000-000000000001\"\n\
         project_name = \"proj\"\n\
         branch_id = \"00000000-0000-0000-0000-000000000002\"\n\
         branch_name = \"demo\"\n",
    )
    .unwrap();
    // A NON-empty stage, so the baseline call actually happens.
    std::fs::write(
        hydrate.join("stage.json"),
        r#"{"deltas":[{"type":"add_node","node":{"id":"00000000-0000-0000-0000-0000000000aa","kind":"behavior","parent_id":null,"data":{"name":"Rater","inputs":[],"outputs":[],"config":[]}}}],"aliases":{}}"#,
    )
    .unwrap();

    let run = |extra: &[&str]| {
        let mut args = vec!["validate", "--human"];
        args.extend_from_slice(extra);
        std::process::Command::new(env!("CARGO_BIN_EXE_hydrate"))
            .args(&args)
            .current_dir(base)
            .env("HYD_API_KEY", "hyd_live_test")
            .env("HYD_BASE_URL", format!("http://127.0.0.1:{port}"))
            .output()
            .expect("run hydrate")
    };

    // Default: the partitioned path. Nothing introduced, so exit 0 — and the
    // note must fire, because this is a caller whose answer just changed.
    let out = run(&[]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");
    assert!(
        stderr.contains("pre-existing coherence finding"),
        "the breaking change left no runtime trace\nstderr: {stderr}"
    );
    assert!(stderr.contains("--whole-branch"), "stderr: {stderr}");

    // The escape hatch keeps the old verdict, which is the whole point of it.
    let whole = run(&["--whole-branch"]);
    assert_eq!(
        whole.status.code(),
        Some(5),
        "--whole-branch did not follow the server verdict\nstdout: {}",
        String::from_utf8_lossy(&whole.stdout)
    );
}
