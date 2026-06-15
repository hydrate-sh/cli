//! `hydrate` — command-line client for hydrate.sh.
//!
//! The graph is the source of truth and the server is the sole authority for
//! validation; this client stages edits locally and commits them as one typed
//! delta batch under optimistic-concurrency control. It never mirrors the
//! server's validation rules — a bad batch is rejected by the server, loudly.
//!
//! Module layout:
//!   - [`cli`]    — the clap verb tree (the command surface).
//!   - [`cmd`]    — one handler per verb (the behavior).
//!   - [`client`] — the hand-written ergonomics layer over the wire client.
//!   - [`wire`]   — the generated typed client (from the vendored OpenAPI spec).
//!   - [`state`]  — the on-disk working-directory state (branch binding + stage).
//!   - [`exit`]   — process exit codes (stable, retry-relevant cases distinct).

use std::process::ExitCode;

use clap::Parser;

mod cli;
pub mod client;
mod cmd;
pub mod exit;
pub mod state;
pub mod wire;

/// Parse arguments and dispatch to the matching verb handler.
///
/// Returns the process [`ExitCode`]; both the `hydrate` and `hyd` binaries are
/// thin wrappers over this.
pub fn run() -> ExitCode {
    let cli = cli::Cli::parse();
    cmd::dispatch(cli)
}
