//! `validate` — dry-run the staged changeset and report coherence findings.
//!
//! Lowers the current stage to the SAME typed delta batch `commit` builds
//! (`staging::lower`), POSTs it to the branch's validate endpoint, and prints the
//! server's coherence report. Unlike `commit`, it is non-mutating: it never
//! applies the batch and — critically — **never clears the stage**. It exists so
//! an agent can gate a loop, `hydrate validate && hydrate commit`.
//!
//! Exit code: `0` when your staged change adds no error-severity finding,
//! [`exit::VALIDATION`] when it does. With `--whole-branch`, it follows the
//! server's authoritative `valid` verdict verbatim instead.
//!
//! The narrowed rule: **the server owns each verdict; this client owns only
//! which verdict it gates on.** Every finding here is the server's — the client
//! partitions them by comparing two server answers and never decides whether
//! something is a finding, nor re-implements a rule. What it does choose is
//! which of the two verdicts the exit code reports. The
//! findings themselves always print (in both modes); the exit code is only the
//! pass/fail signal. A transport or parse failure keeps its own existing code (it
//! never masquerades as "found errors").

use hydrate_wire::models::{self, ValidateResponse};

use super::context::require_workdir;
use crate::client::Client;
use crate::config::Config;
use crate::error::CliError;
use crate::exit;
use uuid::Uuid;

use super::partition::{self, Partition};
use crate::locator::Locators;
use crate::output::OutputMode;
use crate::staging::lower;
use crate::state::{Binding, Index, Stage};

/// Run `validate`: prepare the request from the stage (read-only), POST it, print
/// the findings, and return the process exit code — `0` when your staged change
/// adds no error-severity finding, [`exit::VALIDATION`] when it does. With
/// `--whole-branch` it follows the server's `valid` verdict verbatim instead.
/// Returns `Err` only for a real
/// failure (no workdir, unbound, transport, parse) — those keep their own exit
/// codes.
pub fn run(args: crate::cli::ValidateArgs, mode: OutputMode) -> Result<u8, CliError> {
    let base = require_workdir()?;
    let binding = Binding::load(&base)?.ok_or_else(|| {
        CliError::Other(
            "this working copy is not bound to a branch; run `hydrate fork`".to_string(),
        )
    })?;

    // Read the stage ONCE and use it for both the request and the labels, so
    // the aliases rendered are the ones that were actually validated.
    let stage = Stage::load(&base)?;
    let body = prepare(&stage)?;

    let config = Config::load()?;
    let client = Client::new(&config)?;
    let response = client.validate_deltas(binding.branch_id, body)?;

    // `--introduced` asks which findings THIS stage caused. Answering needs a
    // second server answer — the branch as it stands — so the two can be
    // diffed. Both calls must succeed and describe the same branch version;
    // anything else falls back to the whole-branch verdict, loudly. A partial
    // verdict is never presented.
    // 94 keeps the inverted default; 93 supplies the extracted helper with the
    // retry-then-conflict guard and the empty-stage short circuit.
    let partition = if args.whole_branch {
        None
    } else {
        partition_against_baseline(&client, &binding, &response, &stage)?
    };

    // Fail loud on a server-verdict / severity disagreement rather than silently
    // trusting one side. It goes to stderr so it is loud in BOTH output modes
    // without polluting the stdout contract (the verbatim JSON, or the human
    // report). The server verdict still governs the exit code below.
    // Cross-checks the STAGED response only. The baseline is a probe used to
    // attribute findings, not a verdict anyone gates on, so a disagreement there
    // would not mislead a caller — and reporting it would be noise about a
    // request the user did not ask for.
    if let Some(warning) = disagreement_warning(&response) {
        eprintln!("{warning}");
    }

    // Findings name server ids; the local index + stage turn those back into the
    // paths you authored with. Resolution is PRESENTATION — it must never cost
    // the caller the verdict they already paid a round trip for, so an index we
    // can't read degrades to raw ids with a loud note instead of propagating.
    let index = match Index::load(&base) {
        Ok(index) => index,
        Err(e) => {
            eprintln!(
                "warning: could not read this working copy's index ({e}); \
                 findings will be shown by id. Run `hydrate pull` to rebuild it."
            );
            None
        }
    };
    let locators = Locators::new(index.as_ref(), &stage);
    for note in resolution_notes(&response, &locators, index.as_ref()) {
        eprintln!("{note}");
    }

    match &partition {
        Some(p) => {
            let computed = if p.introduced_errors().is_empty() {
                exit::SUCCESS
            } else {
                exit::VALIDATION
            };
            // A caller who ran bare `validate` as a branch-health check used to
            // get a non-zero exit here and now gets 0. That is a changed answer
            // to the same command, so it must leave a trace at RUNTIME — a
            // release note is not a runtime signal, and a CI log is where
            // someone will look. stderr, so both output modes carry it without
            // touching the stdout contract.
            // Fire whenever the answer CHANGED, not merely when the introduced
            // set is empty. Two 5→0 transitions escape the narrower condition:
            // an introduced set holding only warning-severity findings, and a
            // `valid:false` report with no findings to bucket. Neither is
            // reachable against today's server, but the warning arm is kept in
            // the contract deliberately, so the guard must not assume it away.
            let changed = exit_code(&response) != computed && !p.inherited.is_empty();
            if changed {
                eprintln!(
                    "note: {} on this branch, none caused by your staged change, \
                     so they do not affect the exit code. Run \
                     `hydrate validate --whole-branch` to grade the whole graph.",
                    plural(p.inherited.len(), "pre-existing coherence finding")
                );
            }
            println!(
                "{}",
                render_partitioned(p, &response, &binding, &locators, mode)
            );
            // The gate is the introduced set. Everything else is reported but
            // does not decide the exit code — that is the whole point of the
            // flag.
            Ok(computed)
        }
        None => {
            println!("{}", render(&response, &binding, &locators, mode));
            Ok(exit_code(&response))
        }
    }
}

/// Fetch the branch's current coherence and split `staged` against it.
///
/// The baseline probe is `prepare` over an EMPTY stage: pure, writes nothing,
/// and already a supported request shape (the server answers an empty batch with
/// the branch's current coherence). The real stage is never touched, swapped, or
/// restored.
///
/// `Ok(None)` means the split was refused and the caller must fall back to the
/// whole-branch verdict — and say so, in both output modes.
fn partition_against_baseline(
    client: &Client,
    binding: &Binding,
    response: &ValidateResponse,
    stage: &Stage,
) -> Result<Option<Partition>, CliError> {
    // An empty stage cannot introduce anything, so the baseline is the staged
    // report by construction. Skipping the call here matters: `validate` is
    // write-scope gated and rate-limited per principal, so a second request on
    // every invocation halves an agent loop's quota — and this is the one case
    // where it buys nothing at all.
    if stage.deltas.is_empty() {
        return Ok(Some(Partition::all_inherited(response)));
    }

    let baseline = client.validate_deltas(binding.branch_id, prepare(&Stage::empty())?)?;
    match partition::partition(&baseline, response) {
        Ok(p) => Ok(Some(p)),
        Err(partition::Untrusted::BranchMoved { .. }) => {
            // The branch moved between the two reads. Retry once — a single
            // concurrent commit is the common case and a second attempt usually
            // lands on a settled branch.
            let retry = client.validate_deltas(binding.branch_id, prepare(&Stage::empty())?)?;
            match partition::partition(&retry, response) {
                Ok(p) => Ok(Some(p)),
                Err(why) => {
                    // Still moving. Refusing to attribute is the honest answer;
                    // a conflict is what "the branch moved under you" means
                    // everywhere else in this CLI, and it is retryable.
                    eprintln!("warning: {why}");
                    Err(CliError::VersionConflict {
                        current_version: None,
                    })
                }
            }
        }
        Err(why) => {
            eprintln!("warning: {why}");
            Ok(None)
        }
    }
}

/// Lower the current stage into the validate request body — a read of the stage,
/// never a write. An empty stage lowers to an empty batch, which the server
/// answers with the branch's *current* coherence (a useful "is it coherent now?"
/// probe), so — unlike `commit` — there is no "nothing staged" short-circuit.
fn prepare(stage: &Stage) -> Result<models::V1ValidateBody, CliError> {
    let deltas = lower(stage)?;
    Ok(models::V1ValidateBody {
        deltas: Some(deltas),
    })
}

/// One rendered finding line, shared by the whole-branch and changeset-relative
/// reports so the two cannot describe the same finding differently.
///
/// Prefers the authored path; falls back to the raw id when the local view
/// cannot place it, rather than hiding the finding.
fn finding_line(f: &models::Finding, locators: &Locators) -> String {
    let locator = locators
        .resolve(&f.locator)
        .map(|r| r.label)
        .unwrap_or_else(|| f.locator.clone());
    format!(
        "  [{}] {}  {}: {}",
        severity_str(f.severity),
        f.code,
        locator,
        locators.rewrite(&f.message),
    )
}

/// One bucket, each finding carrying its own resolved path inline.
///
/// Inline rather than a parallel array because a bucketed payload has no single
/// `findings` list to index into, and the locator cannot serve as a join key —
/// the same port can carry two findings.
fn bucket_json(findings: &[models::Finding], locators: &Locators) -> Vec<serde_json::Value> {
    findings
        .iter()
        .map(|f| {
            let resolved = locators.resolve(&f.locator);
            serde_json::json!({
                "code": f.code,
                "severity": severity_str(f.severity),
                "locator": f.locator,
                "path": resolved.as_ref().map(|r| r.label.clone()),
                // False when part of `path` is still a raw id, so a consumer can
                // tell a complete answer from a partial one without sniffing.
                "path_complete": resolved.as_ref().map(|r| r.complete),
                "message": locators.rewrite(&f.message),
            })
        })
        .collect()
}

/// The `located` array: every finding with its id resolved to an authored path.
fn located_json(response: &ValidateResponse, locators: &Locators) -> Vec<serde_json::Value> {
    response
        .findings
        .as_deref()
        .unwrap_or_default()
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let resolved = locators.resolve(&f.locator);
            serde_json::json!({
                "finding_index": i,
                "code": f.code,
                "severity": severity_str(f.severity),
                "locator": f.locator,
                "path": resolved.as_ref().map(|r| r.label.clone()),
                "path_complete": resolved.as_ref().map(|r| r.complete),
                "message": locators.rewrite(&f.message),
            })
        })
        .collect()
}

/// Render a changeset-relative report: what your stage introduced, what it
/// inherited, and what it resolved.
///
/// The three buckets are separate arrays in JSON rather than one list with a
/// flag, so a consumer cannot gate on the wrong set by reading the wrong field.
/// `valid` is redefined to match the exit code — introduced-relative — and the
/// server's whole-branch answer keeps its own key rather than silently meaning
/// something different from the exit status beside it.
fn render_partitioned(
    p: &Partition,
    response: &ValidateResponse,
    binding: &Binding,
    locators: &Locators,
    mode: OutputMode,
) -> String {
    let introduced_errors = p.introduced_errors().len();
    match mode {
        OutputMode::Json => serde_json::json!({
            "valid": introduced_errors == 0,
            "whole_branch_valid": response.valid,
            // Each bucket carries its own resolved paths. A shared `located`
            // array keyed by an index into `findings` would be unjoinable here:
            // this payload has no `findings` array to index into, and the
            // locator is explicitly not a join key (two findings can share one).
            "introduced": bucket_json(&p.introduced, locators),
            "inherited": bucket_json(&p.inherited, locators),
            "resolved": bucket_json(&p.resolved, locators),
            "branch": binding.branch_name,
        })
        .to_string(),
        OutputMode::Human => {
            let mut out = String::new();
            if p.introduced.is_empty() {
                out.push_str("No new coherence findings from your staged change.");
            } else {
                out.push_str(&format!(
                    "{} from your staged change:",
                    plural(p.introduced.len(), "coherence finding")
                ));
                for f in &p.introduced {
                    out.push_str(&format!("\n{}", finding_line(f, locators)));
                }
            }
            if !p.resolved.is_empty() {
                out.push_str(&format!(
                    "\n\nYour change resolves {}.",
                    plural(p.resolved.len(), "existing finding")
                ));
            }
            if !p.inherited.is_empty() {
                out.push_str(&format!(
                    "\n\n{} already on branch '{}', not caused by your change. \
                     Run `hydrate validate --whole-branch` to list them.",
                    plural(p.inherited.len(), "coherence finding"),
                    binding.branch_name
                ));
            }
            out.push_str(&format!(
                "\n\n{}",
                if introduced_errors == 0 {
                    format!(
                        "Valid: your change adds no coherence errors on branch '{}'.",
                        binding.branch_name
                    )
                } else {
                    format!(
                        "Invalid: your change adds {} on branch '{}'.",
                        plural(introduced_errors, "coherence error"),
                        binding.branch_name
                    )
                }
            ));
            out
        }
    }
}

/// The error-severity findings in the report. Used for the displayed count, to
/// cross-check the server verdict, and — under `--whole-branch` — never to
/// derive it: that path reports `response.valid` verbatim. `warning`-severity
/// findings are advisory.
fn error_findings(response: &ValidateResponse) -> Vec<&models::Finding> {
    response
        .findings
        .iter()
        .flatten()
        .filter(|f| f.severity == models::finding::Severity::Error)
        .collect()
}

/// `0` when the server's authoritative `valid` verdict is true,
/// [`exit::VALIDATION`] when it is false — the gate an agent scripts against
/// (`validate && commit`). The server is the sole authority; the client never
/// re-derives this from the findings.
fn exit_code(response: &ValidateResponse) -> u8 {
    if response.valid {
        exit::SUCCESS
    } else {
        exit::VALIDATION
    }
}

/// A loud warning when the server's `valid` verdict disagrees with the presence of
/// error-severity findings — server says `valid` yet ships error findings, or says
/// `invalid` with none. This is a contract signal, not a thing the client resolves
/// (the server verdict still governs); we surface it rather than swallow it. `None`
/// when the two agree.
fn disagreement_warning(response: &ValidateResponse) -> Option<String> {
    let error_count = error_findings(response).len();
    let has_errors = error_count > 0;
    // Disagreement iff `valid` and `has_errors` are the same boolean.
    if response.valid == has_errors {
        Some(format!(
            "warning: server verdict (valid={}) disagrees with {} shown; \
             trusting the server verdict.",
            response.valid,
            plural(error_count, "error-severity finding"),
        ))
    } else {
        None
    }
}

/// Everything worth saying about how well the report could be traced back to
/// authored paths. Empty when resolution was clean and the view is current.
///
/// Three distinct conditions, because they have three different remedies:
///   * the local view is BEHIND the branch — a pull fixes it, and this fires
///     even when every id resolved, because a stale-but-resolvable id yields a
///     confidently WRONG path, which is worse than a raw one;
///   * some ids could not be placed at all;
///   * some are only half-placed (a dangling edge whose far end is gone) — a
///     pull will not conjure a port that no longer exists.
fn resolution_notes(
    response: &ValidateResponse,
    locators: &Locators,
    index: Option<&Index>,
) -> Vec<String> {
    let mut notes = Vec::new();
    let findings: &[models::Finding] = response.findings.as_deref().unwrap_or_default();

    // Staleness is checkable independently of whether anything resolved, and it
    // is the only condition under which a shown path can be silently wrong.
    if let Some(index) = index {
        if index.version != response.branch.version {
            notes.push(format!(
                "warning: this working copy was pulled at branch version {}, \
                 but the branch is now at {} — paths shown may be out of date; \
                 run `hydrate pull`.",
                index.version, response.branch.version,
            ));
        }
    }

    if findings.is_empty() {
        return notes;
    }

    // Only count ids we FAILED to place. A locator that isn't an id at all is a
    // different condition entirely, and telling someone to pull wouldn't help.
    let unplaced = findings
        .iter()
        .filter(|f| Uuid::parse_str(&f.locator).is_ok())
        .filter(|f| locators.resolve(&f.locator).is_none())
        .count();
    if unplaced > 0 {
        notes.push(format!(
            "note: {} shown by id — this working copy's view may be behind the \
             branch; run `hydrate pull` to see paths.",
            plural(unplaced, "finding"),
        ));
    }

    let partial = findings
        .iter()
        .filter_map(|f| locators.resolve(&f.locator))
        .filter(|r| !r.complete)
        .count();
    if partial > 0 {
        notes.push(format!(
            "note: {} name a port this working copy cannot place; the raw id is \
             shown for that side.",
            plural(partial, "finding"),
        ));
    }

    notes
}

/// The machine token for a finding's severity.
fn severity_str(severity: models::finding::Severity) -> &'static str {
    match severity {
        models::finding::Severity::Error => "error",
        models::finding::Severity::Warning => "warning",
    }
}

/// Render the coherence report for `mode`. JSON carries the verbatim
/// `{valid, findings[]}` contract; human lays out the same findings as a readable
/// list plus a clear valid/invalid summary. Both carry the same information.
fn render(
    response: &ValidateResponse,
    binding: &Binding,
    locators: &Locators,
    mode: OutputMode,
) -> String {
    // Borrow, don't clone: normalize `null` to `[]` without copying the vec.
    let findings: &[models::Finding] = response.findings.as_deref().unwrap_or_default();
    match mode {
        OutputMode::Json => serde_json::json!({
            "valid": response.valid,
            // `findings` stays verbatim — the server's ids are the contract, and
            // a consumer may need to correlate them. `located` is the same list
            // with the ids resolved to authored paths, so an agent can act on a
            // finding without a lookup per id. `path` is null when the local
            // view can't place the id (stale index — see the hint in `run`).
            "findings": findings,
            "located": located_json(response, locators),
        })
        .to_string(),
        OutputMode::Human => {
            let mut out = String::new();
            if findings.is_empty() {
                out.push_str("No coherence findings.");
            } else {
                out.push_str(&format!("{}:", plural(findings.len(), "coherence finding")));
                for f in findings {
                    out.push_str(&format!("\n{}", finding_line(f, locators)));
                }
                out.push('\n');
            }
            // The verdict line is what an agent (and a human) reads first, and it
            // is driven by the server's authoritative `valid` — never re-derived
            // from the local severity scan. It says which branch.
            if response.valid {
                out.push_str(&format!(
                    "\nValid: no coherence errors on branch '{}'.",
                    binding.branch_name
                ));
            } else {
                let errors = error_findings(response).len();
                if errors > 0 {
                    out.push_str(&format!(
                        "\nInvalid: {} on branch '{}'. A commit would still be \
                         accepted; this check is the gate.",
                        plural(errors, "coherence error"),
                        binding.branch_name
                    ));
                } else {
                    // Server said invalid with no error-severity finding to show.
                    out.push_str(&format!(
                        "\nInvalid: branch '{}' is not coherent. A commit would still \
                         be accepted; this check is the gate.",
                        binding.branch_name
                    ));
                }
            }
            out
        }
    }
}

/// `n noun`, pluralizing the noun for anything but one.
fn plural(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("{n} {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hydrate_wire::models::finding::Severity;

    // The server publishes `code` as an open string, so these are plain
    // literals rather than generated enum variants. That is the point of
    // the change: a code this build has never heard of is renderable.
    const UNSAT: &str = "unsatisfied_input";
    const DANGLING: &str = "dangling_wire";
    const MISMATCH: &str = "type_mismatch";

    /// A resolver that knows nothing — the pre-existing tests assert the
    /// raw-id rendering, which is exactly what an empty local view produces.
    fn no_locators() -> Locators {
        Locators::new(None, &crate::state::Stage::empty())
    }

    fn binding() -> Binding {
        Binding {
            project_id: Uuid::from_u128(1),
            project_name: "p".to_string(),
            branch_id: Uuid::from_u128(2),
            branch_name: "spicy".to_string(),
        }
    }

    fn finding(code: &str, severity: Severity, locator: &str, message: &str) -> models::Finding {
        models::Finding {
            code: code.to_string(),
            severity,
            locator: locator.to_string(),
            message: message.to_string(),
        }
    }

    /// A resolver that can place `port_id` at an authored path — the state
    /// after a `pull`.
    fn locators_knowing(port_id: Uuid, path: &str) -> Locators {
        let mut stage = crate::state::Stage::empty();
        stage.aliases.insert(format!("port:{path}"), port_id);
        Locators::new(None, &stage)
    }

    #[test]
    fn human_output_names_the_authored_path_not_the_raw_id() {
        // The finding an agent actually gets: the server reports a port id, and
        // an id is not something you can act on. Both the locator column AND the
        // message must come back in authored terms.
        let port = Uuid::from_u128(77);
        let r = response(
            false,
            vec![finding(
                UNSAT,
                Severity::Error,
                &port.to_string(),
                &format!("input port {port} has no incoming edge"),
            )],
        );
        let locators = locators_knowing(port, "Api.Rater:in:key");
        let human = render(&r, &binding(), &locators, OutputMode::Human);

        // The dotted spelling `status`/`diff`/`show` use — and the one the
        // authoring verbs accept, so it can be pasted straight back.
        assert!(human.contains("Api.Rater.key"), "{human}");
        assert!(
            !human.contains(&port.to_string()),
            "raw id leaked into the human report: {human}"
        );
    }

    #[test]
    fn json_adds_located_while_keeping_findings_verbatim() {
        let port = Uuid::from_u128(78);
        let raw_message = format!("input port {port} has no incoming edge");
        let r = response(
            false,
            vec![finding(
                UNSAT,
                Severity::Error,
                &port.to_string(),
                &raw_message,
            )],
        );
        let locators = locators_knowing(port, "Api.Rater:in:key");
        let v: serde_json::Value =
            serde_json::from_str(&render(&r, &binding(), &locators, OutputMode::Json)).unwrap();

        // `findings` is the server's contract — unchanged, ids intact, so a
        // consumer can still correlate against the server.
        assert_eq!(v["findings"][0]["locator"], port.to_string());
        assert_eq!(v["findings"][0]["message"], raw_message);
        // `located` is the actionable view.
        assert_eq!(v["located"][0]["path"], "Api.Rater.key");
        assert_eq!(
            v["located"][0]["message"],
            "input port Api.Rater.key has no incoming edge"
        );
        assert_eq!(v["located"][0]["finding_index"], 0);
        assert_eq!(v["located"][0]["path_complete"], true);
        assert_eq!(v["located"][0]["code"], "unsatisfied_input");
        assert_eq!(v["located"][0]["severity"], "error");
    }

    #[test]
    fn an_unresolvable_id_keeps_its_raw_form_and_a_null_path() {
        // Degrading to the raw id is correct — dropping the finding, or faking a
        // path, would be worse. `path: null` is how a consumer detects it.
        let port = Uuid::from_u128(79);
        let r = response(
            false,
            vec![finding(
                UNSAT,
                Severity::Error,
                &port.to_string(),
                "input port has no incoming edge",
            )],
        );
        let human = render(&r, &binding(), &no_locators(), OutputMode::Human);
        assert!(human.contains(&port.to_string()), "{human}");

        let v: serde_json::Value =
            serde_json::from_str(&render(&r, &binding(), &no_locators(), OutputMode::Json))
                .unwrap();
        assert!(v["located"][0]["path"].is_null());
        assert_eq!(v["located"][0]["locator"], port.to_string());
    }

    #[test]
    fn unplaced_findings_produce_a_pull_hint() {
        let r = response(
            false,
            vec![finding(
                UNSAT,
                Severity::Error,
                &Uuid::from_u128(80).to_string(),
                "unfed",
            )],
        );
        let notes = resolution_notes(&r, &no_locators(), None);
        assert!(
            notes.iter().any(|n| n.contains("hydrate pull")),
            "{notes:?}"
        );
    }

    #[test]
    fn no_hint_when_every_finding_resolves() {
        let port = Uuid::from_u128(81);
        let r = response(
            false,
            vec![finding(UNSAT, Severity::Error, &port.to_string(), "unfed")],
        );
        assert!(resolution_notes(&r, &locators_knowing(port, "Api:in:k"), None).is_empty());
    }

    #[test]
    fn no_hint_on_a_clean_report() {
        let r = response(true, vec![]);
        assert!(resolution_notes(&r, &no_locators(), None).is_empty());
    }

    fn response(valid: bool, findings: Vec<models::Finding>) -> ValidateResponse {
        ValidateResponse {
            branch: Box::new(models::BranchRef::new(Uuid::from_u128(2), 5)),
            findings: Some(findings),
            project_id: Uuid::from_u128(1),
            valid,
            version: "5".to_string(),
        }
    }

    #[test]
    fn clean_report_exits_zero_and_reads_valid_in_both_modes() {
        let r = response(true, vec![]);
        assert_eq!(exit_code(&r), exit::SUCCESS);

        let human = render(&r, &binding(), &no_locators(), OutputMode::Human);
        assert!(human.contains("Valid"), "{human}");
        assert!(human.contains("branch 'spicy'"), "{human}");

        let v: serde_json::Value =
            serde_json::from_str(&render(&r, &binding(), &no_locators(), OutputMode::Json))
                .unwrap();
        assert_eq!(v["valid"], true);
        assert!(v["findings"].as_array().unwrap().is_empty());
    }

    #[test]
    fn error_findings_exit_nonzero_with_the_distinct_validation_code() {
        let r = response(
            false,
            vec![finding(
                UNSAT,
                Severity::Error,
                "node-1",
                "input port 'raw' has no incoming edge",
            )],
        );
        // The dedicated code — not conflict(4), not network(6) — so an agent can
        // tell "found errors" apart from a transport failure.
        assert_eq!(exit_code(&r), exit::VALIDATION);
        assert_ne!(exit::VALIDATION, exit::CONFLICT);
        assert_ne!(exit::VALIDATION, exit::NETWORK);
        assert_ne!(exit::VALIDATION, exit::GENERIC);
    }

    #[test]
    fn error_findings_print_in_full_in_both_modes() {
        let r = response(
            false,
            vec![finding(
                MISMATCH,
                Severity::Error,
                "edge-9",
                "endpoint types differ: HotDog vs Rating",
            )],
        );
        // Human: the finding is spelled out (code, severity, locator, message) and
        // the verdict says invalid.
        let human = render(&r, &binding(), &no_locators(), OutputMode::Human);
        assert!(human.contains("type_mismatch"), "{human}");
        assert!(human.contains("error"), "{human}");
        assert!(human.contains("edge-9"), "{human}");
        assert!(human.contains("endpoint types differ"), "{human}");
        assert!(human.contains("Invalid"), "{human}");

        // JSON: the verbatim {valid, findings[]} contract, every field intact.
        let v: serde_json::Value =
            serde_json::from_str(&render(&r, &binding(), &no_locators(), OutputMode::Json))
                .unwrap();
        assert_eq!(v["valid"], false);
        let arr = v["findings"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["code"], "type_mismatch");
        assert_eq!(arr[0]["severity"], "error");
        assert_eq!(arr[0]["locator"], "edge-9");
        assert_eq!(arr[0]["message"], "endpoint types differ: HotDog vs Rating");
    }

    #[test]
    fn warning_only_findings_are_advisory_and_still_pass() {
        // A warning-severity finding is reported but does NOT fail the check —
        // only error-severity findings gate the exit code.
        let r = response(
            true,
            vec![finding(UNSAT, Severity::Warning, "node-7", "advisory only")],
        );
        assert_eq!(exit_code(&r), exit::SUCCESS);
        let human = render(&r, &binding(), &no_locators(), OutputMode::Human);
        assert!(human.contains("advisory only"), "{human}");
        assert!(human.contains("Valid"), "{human}");
    }

    #[test]
    fn mixed_findings_gate_on_the_error_and_still_show_the_warning() {
        let r = response(
            false,
            vec![
                finding(UNSAT, Severity::Warning, "node-7", "advisory"),
                finding(
                    DANGLING,
                    Severity::Error,
                    "edge-2",
                    "wire to a missing port",
                ),
            ],
        );
        assert_eq!(exit_code(&r), exit::VALIDATION);
        let v: serde_json::Value =
            serde_json::from_str(&render(&r, &binding(), &no_locators(), OutputMode::Json))
                .unwrap();
        // Both findings ride in the payload; the exit code is gated on the error.
        assert_eq!(v["findings"].as_array().unwrap().len(), 2);
    }

    // The non-mutating invariant: preparing the request reads the stage and hands
    // back the delta batch, but leaves the staged work on disk untouched. If
    // someone ever made validate consume/clear the stage, this fails.
    fn staged_dir() -> tempfile::TempDir {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut stage = Stage::empty();
        stage.deltas.push(serde_json::json!({
            "type": "add_node",
            "node": { "id": Uuid::from_u128(9), "kind": "behavior", "parent_id": null }
        }));
        stage.save(tmp.path()).unwrap();
        tmp
    }

    #[test]
    fn verdict_line_never_claims_a_commit_is_blocked() {
        // The verdict read "not safe to commit" for sixteen releases. A commit
        // is accepted with error findings, so that line told an agent the
        // server would stop it when nothing would. This is the string an agent
        // reads, so it matters more than the prose in `guide`.
        let port = Uuid::from_u128(91);
        let r = response(
            false,
            vec![finding(
                UNSAT,
                Severity::Error,
                &port.to_string(),
                &format!("input port {port} has no incoming edge"),
            )],
        );
        let human = render(
            &r,
            &binding(),
            &locators_knowing(port, "Api.Rater:in:key"),
            OutputMode::Human,
        );
        assert!(!human.contains("not safe to commit"), "{human}");
        assert!(human.contains("Invalid:"), "{human}");
        assert!(
            human.contains("would still be") || human.contains("still be accepted"),
            "verdict does not say a commit is accepted anyway:\n{human}"
        );

        // The OTHER invalid arm: server says invalid with no error-severity
        // finding to show. Unreachable today (the server derives `valid` from
        // exactly those findings), but it carries the same string, and a review
        // proved the old claim could be restored there with every test passing.
        let bare = render(
            &response(false, vec![]),
            &binding(),
            &Locators::new(None, &crate::state::Stage::empty()),
            OutputMode::Human,
        );
        assert!(!bare.contains("not safe to commit"), "{bare}");
        assert!(bare.contains("Invalid:"), "{bare}");
    }

    #[test]
    fn prepare_lowers_the_stage_without_clearing_it() {
        let tmp = staged_dir();
        let before = Stage::load(tmp.path()).unwrap();

        let body = prepare(&Stage::load(tmp.path()).unwrap()).unwrap();
        // The staged delta is carried into the request body...
        assert_eq!(body.deltas.as_ref().unwrap().len(), 1);

        // ...and the stage on disk is byte-for-byte what it was: validate never
        // wipes a user's staged work.
        let after = Stage::load(tmp.path()).unwrap();
        assert_eq!(before.deltas, after.deltas);
        assert_eq!(after.deltas.len(), 1);
    }

    #[test]
    fn server_invalid_verdict_exits_five_even_with_no_findings() {
        // The server is the sole authority for validity. If it says `valid:false`
        // with an EMPTY findings list (a future warning-arm the contract already
        // anticipates), the CLI must still exit VALIDATION and read "Invalid" —
        // never re-derive the verdict from the (empty) severity scan and pass.
        let r = response(false, vec![]);
        assert_eq!(exit_code(&r), exit::VALIDATION);

        let human = render(&r, &binding(), &no_locators(), OutputMode::Human);
        assert!(human.contains("Invalid"), "{human}");
        assert!(human.contains("branch 'spicy'"), "{human}");

        let v: serde_json::Value =
            serde_json::from_str(&render(&r, &binding(), &no_locators(), OutputMode::Json))
                .unwrap();
        assert_eq!(v["valid"], false);
    }

    #[test]
    fn server_invalid_verdict_exits_five_with_warning_only_findings() {
        // valid:false but only warning-severity findings: the server verdict, not
        // the local error-severity scan, governs. Exit VALIDATION, read "Invalid".
        let r = response(
            false,
            vec![finding(UNSAT, Severity::Warning, "node-7", "advisory only")],
        );
        assert_eq!(exit_code(&r), exit::VALIDATION);
        let human = render(&r, &binding(), &no_locators(), OutputMode::Human);
        assert!(human.contains("Invalid"), "{human}");
        assert!(human.contains("advisory only"), "{human}");
    }

    #[test]
    fn server_valid_verdict_exits_zero() {
        // Even were error-severity findings present, a `valid:true` server verdict
        // governs the exit code — SUCCESS. (This pairing is itself a disagreement,
        // surfaced separately by `disagreement_warning`; the verdict still governs.)
        let r = response(true, vec![]);
        assert_eq!(exit_code(&r), exit::SUCCESS);
    }

    #[test]
    fn disagreement_warning_fires_when_verdict_and_severities_conflict() {
        // valid:false but no error-severity finding → disagreement.
        let r = response(false, vec![]);
        assert!(disagreement_warning(&r).is_some());

        // valid:true but an error-severity finding present → disagreement.
        let r = response(true, vec![finding(DANGLING, Severity::Error, "e", "m")]);
        assert!(disagreement_warning(&r).is_some());
    }

    #[test]
    fn disagreement_warning_silent_when_verdict_and_severities_agree() {
        // valid:true + no error findings → agree.
        assert!(disagreement_warning(&response(true, vec![])).is_none());
        // valid:false + an error finding → agree.
        let r = response(false, vec![finding(DANGLING, Severity::Error, "e", "m")]);
        assert!(disagreement_warning(&r).is_none());
    }

    #[test]
    fn partitioned_json_redefines_valid_and_keeps_the_branch_answer_separate() {
        // The exit code follows the INTRODUCED set. If `valid` kept carrying
        // the whole-branch answer, the most gateable field in the payload would
        // disagree with the exit status beside it — on a dirty branch, literally
        // `valid:false` with exit 0.
        let port = Uuid::from_u128(55);
        let inherited = finding(UNSAT, Severity::Error, &port.to_string(), "inherited");
        let whole = response(false, vec![inherited.clone()]);
        let p = Partition {
            introduced: vec![],
            inherited: vec![inherited],
            resolved: vec![],
        };
        let json = render_partitioned(
            &p,
            &whole,
            &binding(),
            &Locators::new(None, &crate::state::Stage::empty()),
            OutputMode::Json,
        );
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["valid"], true, "introduced-relative valid: {json}");
        assert_eq!(v["whole_branch_valid"], false, "{json}");
        // Separate arrays, so a consumer cannot gate on the wrong set.
        assert_eq!(v["introduced"].as_array().unwrap().len(), 0, "{json}");
        assert_eq!(v["inherited"].as_array().unwrap().len(), 1, "{json}");
        assert!(v["resolved"].is_array(), "{json}");
        // Each finding carries its own path. A shared `located` array indexed
        // into a `findings` list would be unjoinable — this payload has no such
        // list, and the locator is not a join key.
        assert!(
            v["located"].is_null(),
            "unjoinable located array present: {json}"
        );
        let inh = &v["inherited"][0];
        assert!(inh["path"].is_string() || inh["path"].is_null(), "{json}");
        assert!(
            inh["path_complete"].is_boolean() || inh["path_complete"].is_null(),
            "{json}"
        );
        assert!(inh["code"].is_string(), "{json}");
        assert!(inh["severity"].is_string(), "{json}");
    }

    #[test]
    fn partitioned_human_report_separates_the_buckets() {
        let port = Uuid::from_u128(56);
        let mine = finding(UNSAT, Severity::Error, &port.to_string(), "mine");
        let theirs = finding(
            MISMATCH,
            Severity::Error,
            &Uuid::from_u128(57).to_string(),
            "theirs",
        );
        let p = Partition {
            introduced: vec![mine.clone()],
            inherited: vec![theirs.clone()],
            resolved: vec![],
        };
        let out = render_partitioned(
            &p,
            &response(false, vec![mine, theirs]),
            &binding(),
            &Locators::new(None, &crate::state::Stage::empty()),
            OutputMode::Human,
        );
        assert!(out.contains("from your staged change"), "{out}");
        assert!(out.contains("not caused by your change"), "{out}");
        assert!(out.contains("Invalid: your change adds"), "{out}");
        // The old whole-branch phrasing must not survive here: it would claim
        // the branch is the subject when the verdict is about the change.
        assert!(!out.contains("no coherence errors on branch"), "{out}");
    }

    #[test]
    fn a_clean_change_on_a_dirty_branch_reads_as_valid() {
        // The acceptance case for the whole feature.
        let inherited: Vec<models::Finding> = (0..99)
            .map(|i| {
                finding(
                    UNSAT,
                    Severity::Error,
                    &Uuid::from_u128(1000 + i).to_string(),
                    "inherited",
                )
            })
            .collect();
        let p = Partition {
            introduced: vec![],
            inherited: inherited.clone(),
            resolved: vec![],
        };
        let out = render_partitioned(
            &p,
            &response(false, inherited),
            &binding(),
            &Locators::new(None, &crate::state::Stage::empty()),
            OutputMode::Human,
        );
        assert!(out.contains("No new coherence findings"), "{out}");
        assert!(
            out.contains("Valid: your change adds no coherence errors"),
            "{out}"
        );
        assert!(
            out.contains("99 coherence findings already on branch"),
            "{out}"
        );
    }

    #[test]
    fn whole_branch_still_follows_the_server_verdict_verbatim() {
        // The escape hatch must keep the OLD contract exactly: the server owns
        // that verdict and the client does not re-derive it. If this drifted,
        // the flag would not actually answer the branch-health question it
        // exists for.
        let r = response(false, vec![]);
        assert_eq!(exit_code(&r), exit::VALIDATION);
        let ok = response(true, vec![]);
        assert_eq!(exit_code(&ok), exit::SUCCESS);
    }

    #[test]
    fn inherited_only_findings_do_not_gate_but_are_announced() {
        // A caller running bare `validate` as a branch-health check used to get
        // a non-zero exit and now gets 0. Same command, different answer, so it
        // must leave a runtime trace — a release note is not one.
        let port = Uuid::from_u128(71);
        let f = finding(UNSAT, Severity::Error, &port.to_string(), "inherited");
        let p = Partition {
            introduced: vec![],
            inherited: vec![f.clone()],
            resolved: vec![],
        };
        assert!(
            p.introduced_errors().is_empty(),
            "an inherited-only report must not gate"
        );

        let out = render_partitioned(
            &p,
            &response(false, vec![f]),
            &binding(),
            &Locators::new(None, &crate::state::Stage::empty()),
            OutputMode::Human,
        );
        assert!(out.contains("not caused by your change"), "{out}");
    }

    #[test]
    fn prepare_on_an_empty_stage_sends_an_empty_batch() {
        // Unlike commit, validate does not short-circuit an empty stage: an empty
        // batch asks the server for the branch's current coherence.
        let tmp = tempfile::TempDir::new().unwrap();
        Stage::empty().save(tmp.path()).unwrap();
        let body = prepare(&Stage::load(tmp.path()).unwrap()).unwrap();
        assert!(body.deltas.as_ref().unwrap().is_empty());
    }
}

#[cfg(test)]
mod open_code_tests {
    use super::*;
    use hydrate_wire::models::finding::Severity;

    /// The whole point of the server publishing `code` as an open string.
    ///
    /// The generated client used to close it into an enum with three variants
    /// and no fallback arm, so a validate response containing a code this
    /// build had never heard of failed to deserialize *entirely* — not the one
    /// unfamiliar finding, the whole response. Every additive server-side rule
    /// was therefore a coordinated release, and an installed CLI that was
    /// merely out of date became one that could not read a validate response
    /// at all.
    #[test]
    fn a_finding_code_this_build_has_never_heard_of_deserializes() {
        let raw = serde_json::json!({
            "code": "orphan_node",
            "severity": "warning",
            "locator": "a3f",
            "message": "this node carries no source decision",
        });
        let f: models::Finding =
            serde_json::from_value(raw).expect("an unknown code must not break the response");
        assert_eq!(f.code, "orphan_node");
        assert_eq!(f.severity, Severity::Warning);
    }

    /// …and it must reach the human output verbatim rather than being dropped
    /// or rendered as a placeholder. A finding the CLI cannot name is still a
    /// finding the user has to act on.
    #[test]
    fn an_unknown_code_is_rendered_verbatim() {
        let f = models::Finding {
            code: "orphan_node".to_string(),
            severity: Severity::Warning,
            locator: "a3f".to_string(),
            message: "no source decision".to_string(),
        };
        let line = finding_line(&f, &Locators::new(None, &crate::state::Stage::empty()));
        assert!(line.contains("orphan_node"), "{line}");
        assert!(line.contains("no source decision"), "{line}");
    }

    /// `severity` stays CLOSED on purpose — it is a two-valued verdict the CLI
    /// branches on to pick an exit code, so a third value is a real breaking
    /// change that deserves a coordinated release rather than a silent
    /// fall-through to "not an error".
    #[test]
    fn severity_stays_closed() {
        let raw = serde_json::json!({
            "code": "orphan_node",
            "severity": "advisory",
            "locator": "a3f",
            "message": "m",
        });
        assert!(serde_json::from_value::<models::Finding>(raw).is_err());
    }
}
