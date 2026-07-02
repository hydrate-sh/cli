//! Verb handlers. [`dispatch`] routes a parsed [`Cli`] to exactly one handler.
//!
//! Implemented verbs return `Result<(), CliError>` and render their own success
//! output in the selected mode; [`finish`] prints any error in that same mode
//! and maps it to a stable exit code. The remaining verbs are still stubs that
//! fail loud (they never pretend to succeed) until their phase lands.

use std::process::ExitCode;

use crate::cli::{BoundaryAction, Cli, Command, EdgeAction, NodeAction};
use crate::error::CliError;
use crate::output::{self, OutputMode};

mod boundary;
mod branches;
mod clear;
mod commit;
mod context;
mod diff;
mod edge;
mod fork;
mod guide;
mod node;
mod projects;
mod pull;
mod show;
mod status;

/// Route a parsed command to its handler.
pub fn dispatch(cli: Cli) -> ExitCode {
    let mode = OutputMode::from_flags(cli.json, cli.human);
    // The global `--project` selector applies to the project/branch verbs; it is
    // resolved (against env + binding) inside each handler that needs it.
    let project = cli.project;
    match cli.command {
        Command::Guide => finish(guide::run(mode), mode),
        Command::Projects => finish(projects::run(mode), mode),
        Command::Fork(args) => finish(fork::run(args, project, mode), mode),
        Command::Branches => finish(branches::run(project, mode), mode),
        Command::Show(args) => finish(show::run(args, project, mode), mode),
        Command::Pull => finish(pull::run(mode), mode),
        Command::Node { action } => match action {
            NodeAction::Add(args) => finish(node::add(args, mode), mode),
            NodeAction::Rm(args) => finish(node::rm(args, mode), mode),
            NodeAction::Set(args) => finish(node::set(args, mode), mode),
            NodeAction::Mv(args) => finish(node::mv(args, mode), mode),
        },
        Command::Clear => finish(clear::run(mode), mode),
        Command::Edge { action } => match action {
            EdgeAction::Add(args) => finish(edge::add(args, mode), mode),
            EdgeAction::Rm(args) => finish(edge::rm(args, mode), mode),
        },
        Command::Boundary { action } => match action {
            BoundaryAction::Flatten(args) => finish(boundary::flatten(args, mode), mode),
        },
        Command::Status => finish(status::run(mode), mode),
        Command::Diff => finish(diff::run(mode), mode),
        Command::Commit => finish(commit::run(mode), mode),
    }
}

/// Render a verb's outcome: success was already printed by the verb, so map it
/// to `0`; an error is printed in `mode` and mapped to its stable exit code.
fn finish(result: Result<(), CliError>, mode: OutputMode) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            output::print_error(&e, mode);
            ExitCode::from(e.exit_code())
        }
    }
}
