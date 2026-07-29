//! `guide` — print a self-contained orientation to the tool: the authoring
//! loop, the core concepts, a worked example, and a pointer to the full docs.
//! Aimed at a first-time reader (human or agent) so `--help` can stay a terse
//! reference. Prints the same text in both modes (JSON wraps it in `{guide}`)
//! and touches nothing — no network, no state.

use crate::error::CliError;
use crate::output::OutputMode;

/// The guide text. Deliberately scoped to the public graph-authoring surface:
/// it documents how to author a typed graph, never any server-side behavior, and
/// it never prints a credential.
const GUIDE: &str = "\
hydrate — author a typed graph from the terminal.

A project is a graph of nodes. There are two kinds: boundaries, which contain
other nodes, and behaviors, which do not. Nodes connect through typed ports, and
each node has a free-text description. An edge runs from an output port to an
input port of the same type.

The authoring loop
  1. hydrate fork <name>     create a working branch and bind this directory to it
  2. hydrate pull            sync a local view of the branch's graph
  3. hydrate node add ...    stage nodes (with --description)
     hydrate edge add ...    connect an output port to a matching-typed input port
  4. hydrate diff            review what is staged; nothing has hit the server yet
  5. hydrate validate        dry-run the staged change; read the coherence findings
  6. hydrate commit          apply the staged changeset to the branch

Inspecting
  hydrate projects             list your projects (and the ids for --project)
  hydrate branches             list the working branches of the selected project
  hydrate show [path]          read-only view of a branch's graph (optionally a subtree)
  hydrate show <path> --depth N  read only N levels below <path>, fetching just
                               that slice instead of the whole branch
  hydrate walk <path>          read one node's scoped context (node + neighbors);
                               `--boundary` reads a boundary's children + edges
                               (it errors on a non-boundary — use the plain walk)

A scriptable agent surface
  Every command reads human-friendly on a terminal and machine-readable JSON when
  piped (or with --json), so an agent can drive the whole loop. `walk` reads the
  WHOLE node — its description (its prompt), constraints, and verifications — for
  just the node in question, without pulling the entire graph into context.
  `validate` dry-runs the staged change and exits nonzero on error-severity
  findings, so a loop can gate on it: `hydrate validate && hydrate commit`.

If you are a coding agent, run this loop
  Do these in order for every change, so you build from the spec, not a guess:
  1. hydrate walk <area>     Read the scoped spec BEFORE editing — a node and its
                             neighborhood, or a boundary's scope with `--boundary`
                             — so you build from intent, not a guess.
  2. author as you build     Record each decision: `hydrate node add` /
                             `hydrate node set`, `hydrate edge add`.
  3. hydrate validate        Run it BEFORE committing and fix every error-severity
                             finding. It exits nonzero on errors, so
                             `hydrate validate && hydrate commit` gates the commit.
  4. hydrate commit          Commit once validate is clean.

Editing in place
  hydrate node set <path> ...  edit a node's description, constraints, or ports
  hydrate node rm <path>...    remove nodes (cascades the subtree)
  hydrate clear                stage removal of every top-level node, then commit

Choosing a project
  Commands resolve the project from --project <name|id>, else the HYD_PROJECT
  environment variable, else this directory's binding, else your one active
  project. With more than one and no selection, the command asks you to pick.

Conventions
  - Paths are dotted: `Api.Rater` is node Rater inside boundary Api;
    `Api.Rater.score` is its port `score`.
  - Ports are `name:type`, type required: `--in raw:HotDog --out score:Rating`.
    An edge runs from an output to an input of the SAME type.
  - --description is a free-text field on the node. --constraint adds a free-text
    constraint (repeatable).
  - Output is human on a terminal, JSON when piped (force with --json / --human).

Worked example
  hydrate fork demo
  hydrate node add --kind boundary --name Api
  hydrate node add --kind behavior --name Shorten --parent Api --out url:LongUrl \\
      --description 'POST /shorten: validate the body, normalize the URL, emit it.'
  hydrate node add --kind behavior --name Encoder --parent Api \\
      --in url:LongUrl --out code:ShortCode \\
      --description 'Mint a collision-free base62 short code for a URL.'
  hydrate edge add --from Api.Shorten.url --to Api.Encoder.url
  hydrate diff
  hydrate commit

Auth
  Set HYD_API_KEY in your environment (or a .env file). It is never written to
  disk or printed.

Full reference: https://docs.hydrate.sh\
";

pub fn run(mode: OutputMode) -> Result<(), CliError> {
    println!("{}", render(mode));
    Ok(())
}

/// The rendered guide for `mode`, returned (not printed) so the human/JSON
/// branch selection is directly testable. Human = the text; JSON = the same
/// text under one stable `guide` key (dual-output parity).
fn render(mode: OutputMode) -> String {
    match mode {
        OutputMode::Human => GUIDE.to_string(),
        OutputMode::Json => serde_json::json!({ "guide": GUIDE }).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guide_covers_the_loop_concepts_and_docs_pointer() {
        // The orientation must actually orient: the loop verbs, the typed-port
        // and node-description concepts, and the docs reference.
        for needle in [
            "hydrate fork",
            "hydrate pull",
            "hydrate node add",
            "hydrate edge add",
            "hydrate validate",
            "hydrate commit",
            "hydrate projects",
            "hydrate show",
            "hydrate walk",
            "--project",
            "HYD_PROJECT",
            "node set",
            "typed ports",
            "--description",
            "name:type",
            "https://docs.hydrate.sh",
        ] {
            assert!(GUIDE.contains(needle), "guide is missing: {needle}");
        }
    }

    #[test]
    fn guide_states_the_agent_loop_in_order() {
        // The agent-facing section must spell out the imperative loop — read
        // before editing, then author, validate, commit — so a coding agent can
        // follow it straight from `guide`.
        assert!(
            GUIDE.contains("If you are a coding agent"),
            "guide is missing the agent loop section"
        );
        for needle in ["BEFORE editing", "BEFORE committing", "author as you build"] {
            assert!(GUIDE.contains(needle), "agent loop is missing: {needle}");
        }
        // The steps appear in loop order: walk → validate → commit.
        let walk = GUIDE.find("If you are a coding agent").unwrap();
        let validate = GUIDE[walk..].find("hydrate validate").unwrap();
        let commit = GUIDE[walk..].find("hydrate commit").unwrap();
        assert!(
            validate < commit,
            "the loop must validate before it commits"
        );
    }

    #[test]
    fn guide_references_the_api_key_by_name_without_a_value() {
        // It tells the reader to set HYD_API_KEY but must never embed a secret.
        assert!(GUIDE.contains("HYD_API_KEY"));
        assert!(
            GUIDE.contains("never written to") || GUIDE.contains("never printed"),
            "guide should reassure the key is not persisted/printed"
        );
        // No `KEY=value`-shaped assignment that could read as a real credential.
        assert!(
            !GUIDE.contains("HYD_API_KEY="),
            "guide must not show an assigned key value"
        );
    }

    #[test]
    fn render_human_is_the_raw_text() {
        assert_eq!(render(OutputMode::Human), GUIDE);
    }

    #[test]
    fn render_json_wraps_the_same_text_under_one_key() {
        // Exercises the actual JSON branch of `render` (not a re-typed literal):
        // a single `guide` key carrying the identical text — dual-output parity.
        let v: serde_json::Value = serde_json::from_str(&render(OutputMode::Json)).unwrap();
        assert_eq!(v["guide"], GUIDE);
    }
}
