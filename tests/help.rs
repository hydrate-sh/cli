//! Acceptance: both binaries parse and their `--help` lists every verb. This
//! fails if a verb is dropped from the clap tree or a binary stops sharing the
//! entry point.

use std::process::Command;

const VERBS: [&str; 7] = [
    "fork", "branches", "node", "edge", "status", "diff", "commit",
];

// `CARGO_BIN_EXE_<name>` is provided to integration tests at COMPILE time, so it
// must be read with the `env!` macro, not `std::env::var` (runtime).
const HYDRATE_EXE: &str = env!("CARGO_BIN_EXE_hydrate");
const HYD_EXE: &str = env!("CARGO_BIN_EXE_hyd");

fn help_text(exe: &str) -> String {
    let out = Command::new(exe)
        .arg("--help")
        .output()
        .expect("failed to run binary");
    assert!(out.status.success(), "--help should exit 0");
    String::from_utf8(out.stdout).expect("help output is not UTF-8")
}

#[test]
fn hydrate_help_lists_all_verbs() {
    let text = help_text(HYDRATE_EXE);
    for verb in VERBS {
        assert!(
            text.contains(verb),
            "hydrate --help missing verb {verb:?}\n{text}"
        );
    }
}

#[test]
fn hyd_alias_lists_all_verbs() {
    // The alias shares the entry point, so its surface must be identical.
    let text = help_text(HYD_EXE);
    for verb in VERBS {
        assert!(
            text.contains(verb),
            "hyd --help missing verb {verb:?}\n{text}"
        );
    }
}

#[test]
fn unknown_command_fails_loud() {
    // A bogus verb must be rejected (clap exits non-zero), never silently ignored.
    let out = Command::new(HYDRATE_EXE)
        .arg("definitely-not-a-verb")
        .output()
        .expect("failed to run binary");
    assert!(!out.status.success(), "unknown command should fail");
}

#[test]
fn node_add_help_pins_flag_grammar() {
    // The command surface IS the contract this scaffold locks in — pin the
    // authoring flags so dropping/renaming one (e.g. the `--in` rename) fails.
    let out = Command::new(HYDRATE_EXE)
        .args(["node", "add", "--help"])
        .output()
        .expect("failed to run binary");
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    for flag in ["--kind", "--name", "--parent", "--in", "--out"] {
        assert!(
            text.contains(flag),
            "node add --help missing {flag:?}\n{text}"
        );
    }
}

#[test]
fn edge_add_help_pins_endpoint_flags() {
    let out = Command::new(HYDRATE_EXE)
        .args(["edge", "add", "--help"])
        .output()
        .expect("failed to run binary");
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    for flag in ["--from", "--to"] {
        assert!(
            text.contains(flag),
            "edge add --help missing {flag:?}\n{text}"
        );
    }
}

#[test]
fn json_and_human_conflict() {
    // The two output modes are mutually exclusive — supplying both must fail,
    // not silently pick one.
    let out = Command::new(HYDRATE_EXE)
        .args(["--json", "--human", "status"])
        .output()
        .expect("failed to run binary");
    assert!(
        !out.status.success(),
        "--json --human together should be rejected"
    );
}
