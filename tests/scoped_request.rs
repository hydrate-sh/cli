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
