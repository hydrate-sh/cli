//! Splitting a validate report into what your stage introduced and what it
//! inherited.
//!
//! The verdict the server returns covers the **resulting branch**, not your
//! change. On a branch that already carries findings — the normal state of a
//! large imported graph — that makes `hydrate validate && hydrate commit`
//! unusable however correct your change is, and gives a caller no way to tell
//! "I broke this" from "it was already like this".
//!
//! The split is computed from two server answers: the branch as it stands (an
//! empty batch), and the branch with your stage applied. Everything here is set
//! arithmetic over findings the server produced — the client never decides
//! whether something *is* a finding.
//!
//! ## Why this is fail-closed
//!
//! Two directions of error are possible, and they are not symmetric.
//!
//! * An inherited finding misread as **introduced** is loud: a spurious
//!   non-zero exit that the caller sees immediately and complains about.
//! * An introduced finding misread as **inherited** is **silent**: it still
//!   prints, filed among the ones you did not cause, the verdict reads clean,
//!   and the gate passes. Today that same finding stops the commit.
//!
//! So anything ambiguous is classified **introduced**. A caller who is stopped
//! wrongly can look; a caller who is waved through cannot.

use std::collections::HashMap;

use hydrate_wire::models::{self, ValidateResponse};

/// A finding's identity for set arithmetic: the server's `code` and its raw
/// `locator`, before any local path resolution.
///
/// Keyed on the **raw** locator deliberately. Path resolution is presentation —
/// it depends on the local index, can be partial, and two distinct ids can
/// resolve to one path when the index is stale. Diffing on rendered paths would
/// fold distinct findings together, which is exactly the silent direction.
type Key = (String, String);

fn key(f: &models::Finding) -> Key {
    // The code enum has no Display; its serde spelling is the stable name.
    let code = serde_json::to_value(f.code)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("{:?}", f.code));
    (code, f.locator.clone())
}

/// Count each key, rather than collecting a set.
///
/// A locator legitimately carries more than one finding — the server's own
/// resolver notes that a locator is not a join key. With a plain set, a locator
/// going from one finding to two would register as *no change*, silently hiding
/// the new one.
fn findings_of(r: &ValidateResponse) -> &[models::Finding] {
    r.findings.as_deref().unwrap_or_default()
}

fn tally(findings: &[models::Finding]) -> HashMap<Key, usize> {
    let mut counts = HashMap::new();
    for f in findings {
        *counts.entry(key(f)).or_insert(0) += 1;
    }
    counts
}

/// The three buckets, plus whether the split can be trusted.
#[derive(Debug, Default)]
pub struct Partition {
    /// Findings your stage caused. These, and only these, gate the commit.
    pub introduced: Vec<models::Finding>,
    /// Findings already on the branch before your stage.
    pub inherited: Vec<models::Finding>,
    /// Findings that were on the branch and your stage removed. Reported
    /// because "your change fixed four of these" is worth knowing and falls out
    /// of the same arithmetic.
    pub resolved: Vec<models::Finding>,
}

/// Why a partition could not be trusted, when it could not.
#[derive(Debug, PartialEq, Eq)]
pub enum Untrusted {
    /// The branch moved between the two reads, so the two answers describe
    /// different graphs. Attributing across them would blame your stage for
    /// someone else's commit, or hide one of yours behind theirs.
    BranchMoved { baseline: i32, staged: i32 },
    /// The buckets do not account for every finding. Should be unreachable;
    /// treated as a bug in this code rather than a fact about the branch.
    Conservation { detail: String },
}

/// Split `staged` against `baseline`.
///
/// `Err(Untrusted)` means the caller must fall back to the whole-branch verdict
/// and say so — never silently present a partition it cannot stand behind.
pub fn partition(
    baseline: &ValidateResponse,
    staged: &ValidateResponse,
) -> Result<Partition, Untrusted> {
    // The two answers must describe the same branch. Both responses carry the
    // version; comparing them is the whole guard.
    if baseline.branch.version != staged.branch.version {
        return Err(Untrusted::BranchMoved {
            baseline: baseline.branch.version,
            staged: staged.branch.version,
        });
    }

    let base_findings = findings_of(baseline);
    let staged_findings = findings_of(staged);
    let before = tally(base_findings);
    let mut remaining = before.clone();

    let mut out = Partition::default();
    for f in staged_findings {
        match remaining.get_mut(&key(f)) {
            // Present in the baseline too, and not yet accounted for: inherited.
            Some(n) if *n > 0 => {
                *n -= 1;
                out.inherited.push(f.clone());
            }
            // Either absent from the baseline, or the baseline's copies are
            // already spoken for — a second finding on a locator that had one.
            // Both are new.
            _ => out.introduced.push(f.clone()),
        }
    }

    // Whatever the baseline had that the staged run did not: your stage fixed it.
    for f in base_findings {
        if let Some(n) = remaining.get_mut(&key(f)) {
            if *n > 0 {
                *n -= 1;
                out.resolved.push(f.clone());
            }
        }
    }

    let staged_total = staged_findings.len();
    let baseline_total = base_findings.len();
    if out.introduced.len() + out.inherited.len() != staged_total {
        return Err(Untrusted::Conservation {
            detail: format!(
                "{} introduced + {} inherited != {staged_total} in the staged report",
                out.introduced.len(),
                out.inherited.len()
            ),
        });
    }
    if out.inherited.len() + out.resolved.len() != baseline_total {
        return Err(Untrusted::Conservation {
            detail: format!(
                "{} inherited + {} resolved != {baseline_total} in the baseline report",
                out.inherited.len(),
                out.resolved.len()
            ),
        });
    }

    Ok(out)
}

impl Partition {
    /// The error-severity findings your stage introduced — what a
    /// changeset-relative verdict is a function of.
    pub fn introduced_errors(&self) -> Vec<&models::Finding> {
        self.introduced
            .iter()
            .filter(|f| matches!(f.severity, models::finding::Severity::Error))
            .collect()
    }
}

impl std::fmt::Display for Untrusted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Untrusted::BranchMoved { baseline, staged } => write!(
                f,
                "the branch moved between the two reads (version {baseline} then {staged}), \
                 so findings cannot be attributed to your stage; showing the whole-branch \
                 verdict instead"
            ),
            Untrusted::Conservation { detail } => write!(
                f,
                "the findings split does not add up ({detail}); showing the whole-branch \
                 verdict instead"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn finding(code: models::finding::Code, locator: &str) -> models::Finding {
        models::Finding {
            code,
            locator: locator.to_string(),
            message: format!("about {locator}"),
            severity: models::finding::Severity::Error,
        }
    }

    fn response(version: i32, findings: Vec<models::Finding>) -> ValidateResponse {
        ValidateResponse {
            valid: findings.is_empty(),
            findings: Some(findings),
            branch: Box::new(models::BranchRef {
                id: Uuid::from_u128(1),
                version,
            }),
            project_id: Uuid::from_u128(9),
            version: "v1".to_string(),
        }
    }

    const UNSAT: models::finding::Code = models::finding::Code::UnsatisfiedInput;
    const MISMATCH: models::finding::Code = models::finding::Code::TypeMismatch;

    #[test]
    fn a_finding_in_both_reports_is_inherited() {
        let base = response(7, vec![finding(UNSAT, "a")]);
        let staged = response(7, vec![finding(UNSAT, "a")]);
        let p = partition(&base, &staged).unwrap();
        assert_eq!(p.inherited.len(), 1);
        assert!(p.introduced.is_empty());
        assert!(p.resolved.is_empty());
    }

    #[test]
    fn a_finding_only_in_the_staged_report_is_introduced() {
        let base = response(7, vec![finding(UNSAT, "a")]);
        let staged = response(7, vec![finding(UNSAT, "a"), finding(UNSAT, "b")]);
        let p = partition(&base, &staged).unwrap();
        assert_eq!(p.introduced.len(), 1);
        assert_eq!(p.introduced[0].locator, "b");
        assert_eq!(p.inherited.len(), 1);
    }

    #[test]
    fn a_finding_only_in_the_baseline_was_resolved_by_the_stage() {
        let base = response(7, vec![finding(UNSAT, "a"), finding(UNSAT, "b")]);
        let staged = response(7, vec![finding(UNSAT, "a")]);
        let p = partition(&base, &staged).unwrap();
        assert_eq!(p.resolved.len(), 1);
        assert_eq!(p.resolved[0].locator, "b");
        assert!(p.introduced.is_empty());
    }

    #[test]
    fn the_same_code_and_locator_twice_is_counted_twice() {
        // The dangerous case for a set-based diff. One finding on a locator
        // becoming two is a NEW finding; a set would see the key already
        // present and report no change — silently, in the direction that
        // waves a caller through.
        let base = response(7, vec![finding(UNSAT, "a")]);
        let staged = response(7, vec![finding(UNSAT, "a"), finding(UNSAT, "a")]);
        let p = partition(&base, &staged).unwrap();
        assert_eq!(
            p.introduced.len(),
            1,
            "a second finding on one locator is new"
        );
        assert_eq!(p.inherited.len(), 1);
    }

    #[test]
    fn two_codes_on_one_locator_are_distinct_findings() {
        // Identity is (code, locator), not locator alone: the server can report
        // two different problems about the same port.
        let base = response(7, vec![finding(UNSAT, "a")]);
        let staged = response(7, vec![finding(UNSAT, "a"), finding(MISMATCH, "a")]);
        let p = partition(&base, &staged).unwrap();
        assert_eq!(p.introduced.len(), 1);
        assert_eq!(p.introduced[0].code, MISMATCH);
    }

    #[test]
    fn a_moved_branch_refuses_to_partition() {
        // The two answers describe different graphs. Attributing across them
        // would blame your stage for someone else's commit — or, worse, hide
        // one of yours behind theirs.
        let base = response(7, vec![finding(UNSAT, "a")]);
        let staged = response(8, vec![finding(UNSAT, "b")]);
        let err = partition(&base, &staged).unwrap_err();
        assert_eq!(
            err,
            Untrusted::BranchMoved {
                baseline: 7,
                staged: 8
            }
        );
        assert!(err.to_string().contains("moved between the two reads"));
        assert!(err.to_string().contains("whole-branch verdict"));
    }

    #[test]
    fn only_introduced_errors_gate() {
        let base = response(7, vec![]);
        let mut warn = finding(UNSAT, "w");
        warn.severity = models::finding::Severity::Warning;
        let staged = response(7, vec![finding(UNSAT, "e"), warn]);
        let p = partition(&base, &staged).unwrap();
        assert_eq!(p.introduced.len(), 2);
        assert_eq!(p.introduced_errors().len(), 1);
        assert_eq!(p.introduced_errors()[0].locator, "e");
    }

    #[test]
    fn an_empty_stage_against_a_dirty_branch_introduces_nothing() {
        // The acceptance case: a branch carrying findings, a stage that changes
        // nothing. Today this exits 5 reporting findings the caller did not
        // cause.
        let dirty: Vec<models::Finding> =
            (0..99).map(|i| finding(UNSAT, &format!("p{i}"))).collect();
        let base = response(7, dirty.clone());
        let staged = response(7, dirty);
        let p = partition(&base, &staged).unwrap();
        assert!(p.introduced.is_empty(), "empty stage introduced nothing");
        assert!(p.introduced_errors().is_empty());
        assert_eq!(p.inherited.len(), 99);
    }
}
