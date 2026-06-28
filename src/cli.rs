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
// `node set` carries the full NodeData write surface, so its Args struct is much
// larger than the niladic verbs (Guide/Pull/…). The enum is parsed once at
// startup, so the size spread is irrelevant — boxing would only obscure the clap
// derive.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Print an orientation to authoring graphs: the loop, the concepts, a
    /// worked example, and a pointer to the full docs. Start here.
    Guide,

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

    /// Operate on a boundary node.
    Boundary {
        #[command(subcommand)]
        action: BoundaryAction,
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

    /// Stage a reparent of a node under a new boundary (or to the top level).
    Mv(NodeMvArgs),
}

#[derive(Debug, Args)]
pub struct NodeMvArgs {
    /// Node to move, by dotted path (e.g. `Api.Rater`).
    #[arg(value_name = "PATH")]
    pub path: String,

    /// New parent boundary, by dotted path. Omit with --top for the top level.
    #[arg(long, conflicts_with = "top")]
    pub parent: Option<String>,

    /// Move the node to the top level (no parent).
    #[arg(long)]
    pub top: bool,
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

    /// Rename the node (its leaf name within its parent scope).
    #[arg(long)]
    pub name: Option<String>,

    /// New description (free text). Only the fields you pass change.
    #[arg(long)]
    pub description: Option<String>,

    /// Replace the node's constraints with these (repeatable).
    #[arg(long = "constraint", value_name = "TEXT")]
    pub constraints: Vec<String>,

    /// Remove all constraints (mutually exclusive with --constraint).
    #[arg(long, conflicts_with = "constraints")]
    pub clear_constraints: bool,

    /// Add an input port `name:type` (repeatable).
    #[arg(long = "add-in", value_name = "NAME:TYPE")]
    pub add_in: Vec<String>,

    /// Add an output port `name:type` (repeatable).
    #[arg(long = "add-out", value_name = "NAME:TYPE")]
    pub add_out: Vec<String>,

    /// Remove an input port by name (repeatable).
    #[arg(long = "rm-in", value_name = "NAME")]
    pub rm_in: Vec<String>,

    /// Remove an output port by name (repeatable).
    #[arg(long = "rm-out", value_name = "NAME")]
    pub rm_out: Vec<String>,

    /// Change an input port's type, keeping its identity: `name:newtype` (repeatable).
    #[arg(long = "retype-in", value_name = "NAME:TYPE")]
    pub retype_in: Vec<String>,

    /// Change an output port's type, keeping its identity: `name:newtype` (repeatable).
    #[arg(long = "retype-out", value_name = "NAME:TYPE")]
    pub retype_out: Vec<String>,

    /// Add a config port `name:type` (repeatable).
    #[arg(long = "add-config", value_name = "NAME:TYPE")]
    pub add_config: Vec<String>,

    /// Remove a config port by name (repeatable).
    #[arg(long = "rm-config", value_name = "NAME")]
    pub rm_config: Vec<String>,

    /// Change a config port's type, keeping its identity: `name:newtype` (repeatable).
    #[arg(long = "retype-config", value_name = "NAME:TYPE")]
    pub retype_config: Vec<String>,

    /// The user-facing kind label. On a boundary it classifies the boundary;
    /// on a state node it carries the state kind (e.g. `postgres-db`). Behavior
    /// nodes don't carry it.
    #[arg(long)]
    pub user_kind: Option<String>,

    /// Boundary path prefix (e.g. `src/media/`). Boundary nodes only.
    #[arg(long)]
    pub path_prefix: Option<String>,

    /// Mark the node external (an outside system the graph depends on).
    #[arg(long, conflicts_with = "no_external")]
    pub external: bool,

    /// Unmark the node external.
    #[arg(long = "no-external")]
    pub no_external: bool,

    /// The external system's kind label (e.g. `rest-api`). For external nodes;
    /// the server validates it against the node's external state.
    #[arg(long)]
    pub external_kind: Option<String>,

    /// Replace the node's verifications (the checks it must satisfy) with these
    /// (repeatable).
    #[arg(long = "verification", value_name = "TEXT")]
    pub verifications: Vec<String>,

    /// Remove all verifications (mutually exclusive with --verification).
    #[arg(long, conflicts_with = "verifications")]
    pub clear_verifications: bool,

    /// External-only: the system's protocol (e.g. `gRPC`).
    #[arg(long)]
    pub protocol: Option<String>,

    /// A documentation URL for the node (http/https).
    #[arg(long = "doc-url")]
    pub doc_url: Option<String>,

    /// Mark the node a test node.
    #[arg(long = "test-node", conflicts_with = "no_test_node")]
    pub test_node: bool,

    /// Unmark the node a test node.
    #[arg(long = "no-test-node")]
    pub no_test_node: bool,

    /// Clear the description (set it empty).
    #[arg(long, conflicts_with = "description")]
    pub clear_description: bool,

    /// Clear the user-kind label / state kind (set it null).
    #[arg(long, conflicts_with = "user_kind")]
    pub clear_user_kind: bool,

    /// Clear the boundary path prefix (set it null).
    #[arg(long, conflicts_with = "path_prefix")]
    pub clear_path_prefix: bool,

    /// Clear the external kind label (set it null).
    #[arg(long, conflicts_with = "external_kind")]
    pub clear_external_kind: bool,

    /// Clear the protocol (set it null).
    #[arg(long, conflicts_with = "protocol")]
    pub clear_protocol: bool,

    /// Clear the documentation URL (set it null).
    #[arg(long = "clear-doc-url", conflicts_with = "doc_url")]
    pub clear_doc_url: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum NodeKind {
    Behavior,
    Boundary,
    State,
}

#[derive(Debug, Args)]
pub struct NodeAddArgs {
    /// Node kind.
    #[arg(long, value_enum)]
    pub kind: NodeKind,

    /// Node name — unique within its parent scope.
    #[arg(long)]
    pub name: String,

    /// The node's description (free text).
    #[arg(long)]
    pub description: Option<String>,

    /// A constraint on the node (repeatable). Plain text; part of its spec.
    #[arg(long = "constraint", value_name = "TEXT")]
    pub constraints: Vec<String>,

    /// A verification on the node (repeatable). Plain text; how it is checked.
    #[arg(long = "verification", value_name = "TEXT")]
    pub verifications: Vec<String>,

    /// Mark the node external (an outside system the graph depends on).
    #[arg(long)]
    pub external: bool,

    /// The external system's kind label (requires --external).
    #[arg(long, requires = "external")]
    pub external_kind: Option<String>,

    /// Parent node, addressed by dotted path (e.g. `Api.Rater`).
    #[arg(long)]
    pub parent: Option<String>,

    /// Input port as `name:type` (repeatable). Type is required. On a state
    /// node, `--in` declares a write port.
    #[arg(long = "in", value_name = "NAME:TYPE")]
    pub inputs: Vec<String>,

    /// Output port as `name:type` (repeatable). Type is required. On a state
    /// node, `--out` declares a read port.
    #[arg(long = "out", value_name = "NAME:TYPE")]
    pub outputs: Vec<String>,

    /// Config port as `name:type` (repeatable). A third channel for
    /// configuration inputs; not wired by edges.
    #[arg(long = "config", value_name = "NAME:TYPE")]
    pub config: Vec<String>,

    /// The user-facing kind label. On a boundary it classifies the boundary;
    /// on a state node it carries the state kind (e.g. `postgres-db`). Behavior
    /// nodes don't carry it.
    #[arg(long)]
    pub user_kind: Option<String>,

    /// Boundary-only: the path prefix the boundary owns.
    #[arg(long)]
    pub path_prefix: Option<String>,

    /// External-only: the system's protocol (e.g. `gRPC`, `HTTPS REST`).
    #[arg(long)]
    pub protocol: Option<String>,

    /// A documentation URL for the node (http/https).
    #[arg(long = "doc-url")]
    pub doc_url: Option<String>,

    /// Mark the node a test node.
    #[arg(long = "test-node")]
    pub test_node: bool,
}

#[derive(Debug, Subcommand)]
pub enum EdgeAction {
    /// Add an edge to the staged changeset.
    Add(EdgeAddArgs),

    /// Stage the removal of the edge between two ports.
    Rm(EdgeRmArgs),
}

#[derive(Debug, Args)]
pub struct EdgeRmArgs {
    /// Source port of the edge to remove (`node.port`).
    #[arg(long)]
    pub from: String,

    /// Target port of the edge to remove (`node.port`).
    #[arg(long)]
    pub to: String,
}

#[derive(Debug, Subcommand)]
pub enum BoundaryAction {
    /// Flatten a boundary: promote its children to its parent and remove it.
    Flatten(BoundaryFlattenArgs),
}

#[derive(Debug, Args)]
pub struct BoundaryFlattenArgs {
    /// Boundary to flatten, by dotted path (e.g. `Api`).
    #[arg(value_name = "PATH")]
    pub path: String,
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
