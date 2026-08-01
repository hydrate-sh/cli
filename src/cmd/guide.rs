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

A project is a graph of nodes. Boundaries contain other nodes; behaviors, state,
io, and interface nodes do not. Nodes connect through typed ports, and each node
has a free-text description. An edge runs from an output port to an input port;
the two types should match, but a mismatch is reported rather than refused.

The authoring loop
  1. hydrate fork <name>     create a working branch and bind this directory to it
  2. hydrate pull            sync a local view of the branch's graph
  3. hydrate node add ...    stage nodes (with --description)
     hydrate edge add ...    connect an output port to an input port
  4. hydrate diff            review what is staged; nothing has hit the server yet
  5. hydrate validate        read the findings your change adds
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
  `validate` reports the coherence findings YOUR staged change adds, and exits
  nonzero when it adds an error-severity one, so a loop can gate on it:
  `hydrate validate && hydrate commit`. Findings already on the branch are
  listed but do not gate, so the gate works on a branch that is not yet clean.
  `--whole-branch` grades the whole graph instead.

  A commit is NOT refused for coherence findings. Unwired inputs, dangling
  edges, and type mismatches are all reported and all committable — a node
  legitimately exists before the edge that feeds it, so a half-wired graph must
  stay committable while you design. `validate` is the check you opt into, not
  a barrier the server imposes. What a commit DOES refuse is a delta it cannot
  apply: an unresolved path, a name collision, an edge that breaks the state/io
  connection rules.

If you are a coding agent, run this loop
  Do these in order for every change, so you build from the spec, not a guess:
  1. hydrate walk <area>     Read the scoped spec BEFORE editing — a node and its
                             neighborhood, or a boundary's scope with `--boundary`
                             — so you build from intent, not a guess.
  2. author as you build     Record each decision: `hydrate node add` /
                             `hydrate node set`, `hydrate edge add`.
  3. hydrate validate        Run it BEFORE committing and fix the findings it
                             attributes to your change. Findings you inherited
                             are listed separately and do not gate, so there is
                             nothing to compare by hand.
  4. hydrate commit          Commit once your own findings are clear.

Editing in place
  hydrate node set <path> ...  edit a node's description, constraints, or ports
  hydrate node rm <path>...    remove nodes (cascades the subtree)
  hydrate clear                stage removal of every top-level node, then commit
  hydrate stage discard        throw away everything staged (local; keeps a
                               recoverable copy). NOT the same as `clear`, which
                               stages deletions rather than undoing your edits

Choosing a project
  Commands resolve the project from --project <name|id>, else the HYD_PROJECT
  environment variable, else this directory's binding, else your one active
  project. With more than one and no selection, the command asks you to pick.

Conventions
  - Paths are dotted: `Api.Rater` is node Rater inside boundary Api;
    `Api.Rater.score` is its port `score`.
  - Ports are `name:type`, type required: `--in raw:HotDog --out score:Rating`.
    An edge runs from an output to an input. Matching types are the intent;
    a mismatch is reported as a finding, not refused.
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
  hydrate validate
  hydrate commit

Exit codes
  0 success; 1 failure; 2 usage error (the command never ran, so retrying it
  unchanged cannot succeed); 4 conflict (the branch moved — re-run to retry);
  5 `validate` returned a not-coherent verdict; 6 network failure.

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
    fn guide_does_not_restate_retired_rules() {
        // The guide is the agent onboarding surface, so a stale rule here
        // propagates into authored graphs. Two claims were wrong for releases:
        // that there are only two node kinds, and that an edge requires equal
        // types (it does not — a mismatch is a finding, not a refusal).
        for stale in [
            "two kinds",
            "SAME type",
            "matching-typed",
            "input port of the same type",
        ] {
            assert!(
                !GUIDE.contains(stale),
                "guide restates a retired rule: {stale}"
            );
        }
        // All five kinds are nameable from the guide.
        for kind in ["boundar", "behavior", "state", "io", "interface"] {
            assert!(GUIDE.contains(kind), "guide never mentions kind: {kind}");
        }
    }

    #[test]
    fn guide_worked_example_runs_the_loop_it_teaches() {
        // The example is what gets copy-pasted. It skipped `validate` while the
        // loop above it listed validate as step 5 and the agent section called
        // it mandatory.
        let example = GUIDE
            .split("Worked example")
            .nth(1)
            .expect("worked example");
        let diff = example.find("hydrate diff").expect("diff in example");
        let validate = example
            .find("hydrate validate")
            .expect("validate in example");
        let commit = example.find("hydrate commit").expect("commit in example");
        assert!(
            diff < validate && validate < commit,
            "example out of order:\n{example}"
        );
    }

    #[test]
    fn guide_scopes_the_validate_verdict_to_the_users_change() {
        // The verdict used to cover the whole branch, which sent an agent on an
        // imported graph into an endless loop fixing findings it did not cause.
        // The guide must now say whose findings gate, and name the escape hatch
        // for the branch-health question the old default answered.
        assert!(
            GUIDE.contains("YOUR staged change") || GUIDE.contains("your staged change"),
            "guide does not scope the verdict to the user's change"
        );
        assert!(
            GUIDE.contains("do not gate"),
            "guide does not say inherited findings are non-gating"
        );
        assert!(
            GUIDE.contains("--whole-branch"),
            "guide does not name the whole-branch probe"
        );
        // And it must not still teach the manual baseline the flag replaces.
        assert!(
            !GUIDE.contains("baseline"),
            "guide still teaches a manual baseline comparison"
        );
    }

    #[test]
    fn guide_never_claims_a_commit_enforces_coherence() {
        // A denylist of spellings is not enough: a review defeated the first
        // version of this test by APPENDING ", but the server will reject the
        // changeset until they are clear" to the very sentence being pinned,
        // and every assertion still passed because the true prefix survived.
        // So assert over the PARAGRAPH and reject any rejection-verb applied to
        // a commit inside its permissive half.
        let start = GUIDE
            .find("A commit is NOT refused")
            .expect("guide no longer states that a commit survives coherence findings");
        let para = GUIDE[start..]
            .split("\n\n")
            .next()
            .expect("paragraph")
            .to_lowercase();

        // The paragraph deliberately ends by naming what a commit DOES refuse.
        // Scan only the permissive half, and strip the one negated use.
        let claim = para
            .split("what a commit does refuse")
            .next()
            .expect("permissive half")
            .replacen("not refused", "", 1);

        for verb in [
            "reject", "refus", "block", "prevent", "stop ", "enforc", "disallow",
        ] {
            assert!(
                !claim.contains(verb),
                "the commit-permissive paragraph contains {verb:?}, which reads as \
                 enforcement:\n{para}"
            );
        }

        assert!(
            GUIDE.contains("DOES refuse"),
            "guide does not name what a commit refuses"
        );
    }

    #[test]
    fn guide_lists_every_exit_code_the_binary_emits() {
        // Derived from the constants, NOT from a second hardcoded copy of the
        // same list: a test that compares the guide against a literal cannot
        // fail when the code it documents changes, which is the definition of
        // decoration. Changing exit::CONFLICT must break this.
        use crate::exit;
        for (code, noun) in [
            (exit::SUCCESS, "success"),
            (exit::GENERIC, "failure"),
            (exit::CONFLICT, "conflict"),
            (exit::NETWORK, "network"),
        ] {
            let expected = format!("{code} {noun}");
            assert!(
                GUIDE.contains(&expected),
                "guide does not document exit {code} as {noun:?}; looked for {expected:?}"
            );
        }
        assert!(
            GUIDE.contains(&format!("{} `validate`", exit::VALIDATION)),
            "guide does not document exit {} for validate",
            exit::VALIDATION
        );
        // 2 comes from the argument parser and has no constant to derive from;
        // `usage_errors_exit_two` pins the behaviour itself.
        assert!(GUIDE.contains("2 usage error"), "guide omits exit 2");
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
