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
    #[arg(long, global = true)]
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

    /// Refresh the local view of the bound branch's live graph, so you can
    /// reference already-committed nodes by their dotted path.
    Pull,

    /// Stage the removal of every top-level node — wipe the branch to rebuild
    /// in place (cascade removes their subtrees). Requires a prior `pull`.
    Clear,

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

    /// Stage the removal of one or more nodes (cascades the subtree).
    Rm(NodeRmArgs),

    /// Stage an edit to an existing node's spec (description / constraints).
    Set(NodeSetArgs),
}

#[derive(Debug, Args)]
pub struct NodeRmArgs {
    /// Node(s) to remove, by dotted path (e.g. `Api.Rater`). Repeatable.
    #[arg(required = true, value_name = "PATH")]
    pub paths: Vec<String>,
}

#[derive(Debug, Args)]
pub struct NodeSetArgs {
    /// Node to edit, by dotted path (e.g. `Api.Rater`).
    #[arg(value_name = "PATH")]
    pub path: String,

    /// New description (the spec/prompt). Only the fields you pass change.
    #[arg(long)]
    pub description: Option<String>,

    /// Replace the node's constraints with these (repeatable).
    #[arg(long = "constraint", value_name = "TEXT")]
    pub constraints: Vec<String>,

    /// Remove all constraints (mutually exclusive with --constraint).
    #[arg(long, conflicts_with = "constraints")]
    pub clear_constraints: bool,
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

    /// The node's description — the spec/prompt that drives what it does.
    #[arg(long)]
    pub description: Option<String>,

    /// A constraint on the node (repeatable). Plain text; part of its spec.
    #[arg(long = "constraint", value_name = "TEXT")]
    pub constraints: Vec<String>,

    /// Parent node, addressed by dotted path (e.g. `Api.Rater`).
    #[arg(long)]
    pub parent: Option<String>,

    /// Input port as `name:type` (repeatable). Type is required.
    #[arg(long = "in", value_name = "NAME:TYPE")]
    pub inputs: Vec<String>,

    /// Output port as `name:type` (repeatable). Type is required.
    #[arg(long = "out", value_name = "NAME:TYPE")]
    pub outputs: Vec<String>,

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
