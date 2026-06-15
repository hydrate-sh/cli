//! Command-line surface: the verb tree, parsed by `clap` (derive).
//!
//! Grammar is flag-driven and explicit — never positional beyond the single
//! branch/node name — so a command reads the same in a script as on the
//! terminal, e.g.:
//!
//!   hydrate node add --kind behavior --name Rater --in raw:HotDog --out score:Score
//!   hydrate edge add --from Maker.dog --to Rater.raw
//!
//! This module only describes the surface; each verb's behavior lives in `cmd`.

use clap::{Args, Parser, Subcommand, ValueEnum};

/// Top-level parser for the `hydrate` / `hyd` binaries.
#[derive(Debug, Parser)]
#[command(
    name = "hydrate",
    version,
    about = "Author your hydrate.sh system graph from the terminal.",
    long_about = None,
    propagate_version = true,
)]
pub struct Cli {
    /// Force machine-readable JSON output (default when stdout is not a TTY).
    #[arg(long, global = true, conflicts_with = "human")]
    pub json: bool,

    /// Force human-readable output (default when stdout is a TTY).
    #[arg(long, global = true, conflicts_with = "json")]
    pub human: bool,

    #[command(subcommand)]
    pub command: Command,
}

/// The verb set: branch context, authoring, inspection, commit.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Fork a working branch from main and bind this directory to it.
    Fork(ForkArgs),

    /// List your working branches.
    Branches,

    /// Stage a node (behavior or boundary) into the changeset.
    Node {
        #[command(subcommand)]
        action: NodeAction,
    },

    /// Stage an edge between two typed ports into the changeset.
    Edge {
        #[command(subcommand)]
        action: EdgeAction,
    },

    /// Show the bound branch and a summary of staged operations.
    Status,

    /// Show the staged operations in detail.
    Diff,

    /// Commit the staged changeset to the bound branch.
    Commit,
}

#[derive(Debug, Args)]
pub struct ForkArgs {
    /// Name for the new working branch (a slug: letters, digits, '-', '_').
    pub name: String,
}

#[derive(Debug, Subcommand)]
pub enum NodeAction {
    /// Add a node to the staged changeset.
    Add(NodeAddArgs),
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum NodeKind {
    Behavior,
    Boundary,
}

#[derive(Debug, Args)]
pub struct NodeAddArgs {
    /// Node kind.
    #[arg(long, value_enum)]
    pub kind: NodeKind,

    /// Node name — unique within its parent scope.
    #[arg(long)]
    pub name: String,

    /// Parent node, addressed by dotted path (e.g. `Api.Rater`).
    #[arg(long)]
    pub parent: Option<String>,

    /// Input port as `name:type` (repeatable). Type is required.
    #[arg(long = "in", value_name = "NAME:TYPE")]
    pub inputs: Vec<String>,

    /// Output port as `name:type` (repeatable). Type is required.
    #[arg(long = "out", value_name = "NAME:TYPE")]
    pub outputs: Vec<String>,

    /// Config port as `name:type` (repeatable). Type is required.
    #[arg(long = "config", value_name = "NAME:TYPE")]
    pub config: Vec<String>,

    /// Boundary-only: the user-facing kind label.
    #[arg(long)]
    pub user_kind: Option<String>,

    /// Boundary-only: the path prefix the boundary owns.
    #[arg(long)]
    pub path_prefix: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum EdgeAction {
    /// Add an edge to the staged changeset.
    Add(EdgeAddArgs),
}

#[derive(Debug, Args)]
pub struct EdgeAddArgs {
    /// Source port, addressed by dotted path (e.g. `Maker.dog`).
    #[arg(long)]
    pub from: String,

    /// Target port, addressed by dotted path (e.g. `Rater.raw`).
    #[arg(long)]
    pub to: String,
}
