//! `hydrate` — command-line client for hydrate.sh.
//!
//! This client stages edits locally and commits them as one typed delta batch
//! under optimistic-concurrency control. The server is the sole authority for
//! validation; the client never mirrors the server's validation rules, so a bad
//! batch is rejected by the server.
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
mod cmd;
mod wire;

// The runtime building blocks the command handlers compose. `client` + `config`
// are `pub` because the integration test (tests/runtime.rs) drives them, and
// `error` (`CliError`) appears in their public signatures, so it is pub too. The
// rest are consumed only in-crate.
pub mod client;
pub mod config;
pub mod error;
pub(crate) mod exit;
pub(crate) mod output;
pub(crate) mod staging;
pub(crate) mod state;

/// Parse arguments and dispatch to the matching verb handler.
///
/// Returns the process [`ExitCode`]; both the `hydrate` and `hyd` binaries are
/// thin wrappers over this.
pub fn run() -> ExitCode {
    let cli = cli::Cli::parse();
    cmd::dispatch(cli)
}
