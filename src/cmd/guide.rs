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
hydrate — author a typed system graph from the terminal.

You build a graph of decisions: BOUNDARIES (groupings / subsystems) and
BEHAVIORS (units of work), wired together through TYPED PORTS. The graph is the
source of truth, and a node's DESCRIPTION is its full specification — the prompt
that drives what the component does. Strong typing on the connections is how the
system checks that it makes sense.

The authoring loop
  1. hydrate fork <name>     create a working branch and bind this directory to it
  2. hydrate pull            sync a local view of the branch's live graph
  3. hydrate node add ...    stage behaviors and boundaries (with --description)
     hydrate edge add ...    wire one output port to a matching-typed input port
  4. hydrate diff            review what is staged — nothing has hit the server yet
  5. hydrate commit          apply the staged changeset to the branch

Editing in place
  hydrate node set <path> ...  edit a node's spec (description / constraints)
  hydrate node rm <path>...    remove nodes (cascades the subtree)
  hydrate clear                wipe the branch to rebuild in place, then commit

Conventions
  - Paths are dotted: `Api.Rater` is node Rater inside boundary Api;
    `Api.Rater.score` is its port `score`.
  - Ports are `name:type`, type required: `--in raw:HotDog --out score:Rating`.
    An edge runs from an output to an input of the SAME type.
  - --description is the component's full spec — behavior, inputs/outputs, errors,
    edge cases — not a one-liner. --constraint adds an invariant (repeatable).
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

Full reference and concepts: https://docs.hydrate.sh\
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
        // and description-is-the-spec concepts, and the docs reference.
        for needle in [
            "hydrate fork",
            "hydrate pull",
            "hydrate node add",
            "hydrate edge add",
            "hydrate commit",
            "node set",
            "TYPED PORTS",
            "full specification",
            "name:type",
            "https://docs.hydrate.sh",
        ] {
            assert!(GUIDE.contains(needle), "guide is missing: {needle}");
        }
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
