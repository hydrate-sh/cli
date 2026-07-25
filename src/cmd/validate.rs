//! `validate` — dry-run the staged changeset and report coherence findings.
//!
//! Lowers the current stage to the SAME typed delta batch `commit` builds
//! (`staging::lower`), POSTs it to the branch's validate endpoint, and prints the
//! server's coherence report. Unlike `commit`, it is non-mutating: it never
//! applies the batch and — critically — **never clears the stage**. It exists so
//! an agent can gate a loop, `hydrate validate && hydrate commit`.
//!
//! Exit code: `0` when the server's authoritative `valid` verdict is true,
//! [`exit::VALIDATION`] when it is false. The server is the sole authority for
//! validation; this client never re-derives the verdict from the findings. The
//! findings themselves always print (in both modes); the exit code is only the
//! pass/fail signal. A transport or parse failure keeps its own existing code (it
//! never masquerades as "found errors").

use hydrate_wire::models::{self, ValidateResponse};

use super::context::require_workdir;
use crate::client::Client;
use crate::config::Config;
use crate::error::CliError;
use crate::exit;
use crate::output::OutputMode;
use crate::staging::lower;
use crate::state::{Binding, Stage};

/// Run `validate`: prepare the request from the stage (read-only), POST it, print
/// the findings, and return the process exit code (`0` when the server verdict is
/// `valid`, [`exit::VALIDATION`] when it is not). Returns `Err` only for a real
/// failure (no workdir, unbound, transport, parse) — those keep their own exit
/// codes.
pub fn run(mode: OutputMode) -> Result<u8, CliError> {
    let base = require_workdir()?;
    let binding = Binding::load(&base)?.ok_or_else(|| {
        CliError::Other(
            "this working copy is not bound to a branch; run `hydrate fork`".to_string(),
        )
    })?;

    // Read the stage into the delta batch. This never writes the stage back —
    // validate is a dry-run, so the staged work must survive it untouched.
    let body = prepare(&base)?;

    let config = Config::load()?;
    let client = Client::new(&config)?;
    let response = client.validate_deltas(binding.branch_id, body)?;

    // Fail loud on a server-verdict / severity disagreement rather than silently
    // trusting one side. It goes to stderr so it is loud in BOTH output modes
    // without polluting the stdout contract (the verbatim JSON, or the human
    // report). The server verdict still governs the exit code below.
    if let Some(warning) = disagreement_warning(&response) {
        eprintln!("{warning}");
    }

    println!("{}", render(&response, &binding, mode));
    Ok(exit_code(&response))
}

/// Lower the current stage into the validate request body — a read of the stage,
/// never a write. An empty stage lowers to an empty batch, which the server
/// answers with the branch's *current* coherence (a useful "is it coherent now?"
/// probe), so — unlike `commit` — there is no "nothing staged" short-circuit.
fn prepare(base: &std::path::Path) -> Result<models::V1ValidateBody, CliError> {
    let stage = Stage::load(base)?;
    let deltas = lower(&stage)?;
    Ok(models::V1ValidateBody {
        deltas: Some(deltas),
    })
}

/// The error-severity findings in the report. Used only for the displayed count
/// and to cross-check the server verdict — NOT to derive it. `warning`-severity
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

/// The machine token for a finding's code (its serde wire spelling), so the human
/// output names the same code the JSON carries.
fn code_str(code: models::finding::Code) -> &'static str {
    match code {
        models::finding::Code::UnsatisfiedInput => "unsatisfied_input",
        models::finding::Code::DanglingWire => "dangling_wire",
        models::finding::Code::TypeMismatch => "type_mismatch",
    }
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
fn render(response: &ValidateResponse, binding: &Binding, mode: OutputMode) -> String {
    // Borrow, don't clone: normalize `null` to `[]` without copying the vec.
    let findings: &[models::Finding] = response.findings.as_deref().unwrap_or_default();
    match mode {
        OutputMode::Json => serde_json::json!({
            "valid": response.valid,
            "findings": findings,
        })
        .to_string(),
        OutputMode::Human => {
            let mut out = String::new();
            if findings.is_empty() {
                out.push_str("No coherence findings.");
            } else {
                out.push_str(&format!("{}:", plural(findings.len(), "coherence finding")));
                for f in findings {
                    out.push_str(&format!(
                        "\n  [{}] {}  {}: {}",
                        severity_str(f.severity),
                        code_str(f.code),
                        f.locator,
                        f.message,
                    ));
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
                        "\nInvalid: {} on branch '{}'; not safe to commit.",
                        plural(errors, "coherence error"),
                        binding.branch_name
                    ));
                } else {
                    // Server said invalid with no error-severity finding to show.
                    out.push_str(&format!(
                        "\nInvalid: branch '{}' is not coherent; not safe to commit.",
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
    use hydrate_wire::models::finding::{Code, Severity};
    use uuid::Uuid;

    fn binding() -> Binding {
        Binding {
            project_id: Uuid::from_u128(1),
            project_name: "p".to_string(),
            branch_id: Uuid::from_u128(2),
            branch_name: "spicy".to_string(),
        }
    }

    fn finding(code: Code, severity: Severity, locator: &str, message: &str) -> models::Finding {
        models::Finding {
            code,
            severity,
            locator: locator.to_string(),
            message: message.to_string(),
        }
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

        let human = render(&r, &binding(), OutputMode::Human);
        assert!(human.contains("Valid"), "{human}");
        assert!(human.contains("branch 'spicy'"), "{human}");

        let v: serde_json::Value =
            serde_json::from_str(&render(&r, &binding(), OutputMode::Json)).unwrap();
        assert_eq!(v["valid"], true);
        assert!(v["findings"].as_array().unwrap().is_empty());
    }

    #[test]
    fn error_findings_exit_nonzero_with_the_distinct_validation_code() {
        let r = response(
            false,
            vec![finding(
                Code::UnsatisfiedInput,
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
                Code::TypeMismatch,
                Severity::Error,
                "edge-9",
                "endpoint types differ: HotDog vs Rating",
            )],
        );
        // Human: the finding is spelled out (code, severity, locator, message) and
        // the verdict says invalid.
        let human = render(&r, &binding(), OutputMode::Human);
        assert!(human.contains("type_mismatch"), "{human}");
        assert!(human.contains("error"), "{human}");
        assert!(human.contains("edge-9"), "{human}");
        assert!(human.contains("endpoint types differ"), "{human}");
        assert!(human.contains("Invalid"), "{human}");

        // JSON: the verbatim {valid, findings[]} contract, every field intact.
        let v: serde_json::Value =
            serde_json::from_str(&render(&r, &binding(), OutputMode::Json)).unwrap();
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
            vec![finding(
                Code::UnsatisfiedInput,
                Severity::Warning,
                "node-7",
                "advisory only",
            )],
        );
        assert_eq!(exit_code(&r), exit::SUCCESS);
        let human = render(&r, &binding(), OutputMode::Human);
        assert!(human.contains("advisory only"), "{human}");
        assert!(human.contains("Valid"), "{human}");
    }

    #[test]
    fn mixed_findings_gate_on_the_error_and_still_show_the_warning() {
        let r = response(
            false,
            vec![
                finding(
                    Code::UnsatisfiedInput,
                    Severity::Warning,
                    "node-7",
                    "advisory",
                ),
                finding(
                    Code::DanglingWire,
                    Severity::Error,
                    "edge-2",
                    "wire to a missing port",
                ),
            ],
        );
        assert_eq!(exit_code(&r), exit::VALIDATION);
        let v: serde_json::Value =
            serde_json::from_str(&render(&r, &binding(), OutputMode::Json)).unwrap();
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
    fn prepare_lowers_the_stage_without_clearing_it() {
        let tmp = staged_dir();
        let before = Stage::load(tmp.path()).unwrap();

        let body = prepare(tmp.path()).unwrap();
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

        let human = render(&r, &binding(), OutputMode::Human);
        assert!(human.contains("Invalid"), "{human}");
        assert!(human.contains("branch 'spicy'"), "{human}");

        let v: serde_json::Value =
            serde_json::from_str(&render(&r, &binding(), OutputMode::Json)).unwrap();
        assert_eq!(v["valid"], false);
    }

    #[test]
    fn server_invalid_verdict_exits_five_with_warning_only_findings() {
        // valid:false but only warning-severity findings: the server verdict, not
        // the local error-severity scan, governs. Exit VALIDATION, read "Invalid".
        let r = response(
            false,
            vec![finding(
                Code::UnsatisfiedInput,
                Severity::Warning,
                "node-7",
                "advisory only",
            )],
        );
        assert_eq!(exit_code(&r), exit::VALIDATION);
        let human = render(&r, &binding(), OutputMode::Human);
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
        let r = response(
            true,
            vec![finding(Code::DanglingWire, Severity::Error, "e", "m")],
        );
        assert!(disagreement_warning(&r).is_some());
    }

    #[test]
    fn disagreement_warning_silent_when_verdict_and_severities_agree() {
        // valid:true + no error findings → agree.
        assert!(disagreement_warning(&response(true, vec![])).is_none());
        // valid:false + an error finding → agree.
        let r = response(
            false,
            vec![finding(Code::DanglingWire, Severity::Error, "e", "m")],
        );
        assert!(disagreement_warning(&r).is_none());
    }

    #[test]
    fn prepare_on_an_empty_stage_sends_an_empty_batch() {
        // Unlike commit, validate does not short-circuit an empty stage: an empty
        // batch asks the server for the branch's current coherence.
        let tmp = tempfile::TempDir::new().unwrap();
        Stage::empty().save(tmp.path()).unwrap();
        let body = prepare(tmp.path()).unwrap();
        assert!(body.deltas.as_ref().unwrap().is_empty());
    }
}
