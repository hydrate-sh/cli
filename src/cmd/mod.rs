//! Verb handlers. [`dispatch`] routes a parsed [`Cli`] to exactly one handler.
//!
//! The handlers are stubs in this scaffold: they establish the surface and exit
//! codes; behavior lands in later phases. A stub fails loud (it does not pretend
//! to succeed) so an accidental call in a script is obvious, never silent.

use std::process::ExitCode;

use crate::cli::{Cli, Command, EdgeAction, NodeAction};
use crate::exit;

mod branches;
mod commit;
mod diff;
mod edge;
mod fork;
mod node;
mod status;

/// Route a parsed command to its handler.
pub fn dispatch(cli: Cli) -> ExitCode {
    match cli.command {
        Command::Fork(args) => fork::run(args),
        Command::Branches => branches::run(),
        Command::Node { action } => match action {
            NodeAction::Add(args) => node::add(args),
        },
        Command::Edge { action } => match action {
            EdgeAction::Add(args) => edge::add(args),
        },
        Command::Status => status::run(),
        Command::Diff => diff::run(),
        Command::Commit => commit::run(),
    }
}

/// Shared stub: report that a verb is not implemented yet and fail loud.
fn not_implemented(verb: &str) -> ExitCode {
    eprintln!("hydrate: `{verb}` is not implemented yet");
    ExitCode::from(exit::GENERIC)
}
