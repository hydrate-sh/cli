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
    // `code` is a plain `String` — the server publishes it as an OPEN set, so
    // there is no enum to spell out. This used to round-trip a generated enum
    // through serde to recover its wire name; the round-trip is gone with the
    // enum, and so is the silent failure it papered over (an unrecognized code
    // could not be represented at all, so the whole response failed to
    // deserialize before this function ever ran).
    (f.code.clone(), f.locator.clone())
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

    check_conservation(&out, base_findings, staged_findings)?;
    Ok(out)
}

/// Every finding in each report must be accounted for by the buckets, key for
/// key.
///
/// Compared against **independently re-derived** tallies. Comparing bucket
/// *lengths* to input lengths would be true by construction — the loops above
/// push each finding exactly once — so it would detect nothing at all; a review
/// proved exactly that by rewriting the old check to `> total + 9999` with every
/// test still passing.
///
/// What it catches is a finding dropped, duplicated, or emitted with a key that
/// is in neither report. It deliberately does NOT catch a finding moved between
/// `introduced` and `inherited`: those two are summed against the staged report
/// together, and attribution between them is what the bucket tests cover. Two
/// different questions, two different guards.
fn check_conservation(
    out: &Partition,
    baseline: &[models::Finding],
    staged: &[models::Finding],
) -> Result<(), Untrusted> {
    let mut got_staged: HashMap<Key, usize> = HashMap::new();
    for f in out.introduced.iter().chain(out.inherited.iter()) {
        *got_staged.entry(key(f)).or_insert(0) += 1;
    }
    if got_staged != tally(staged) {
        return Err(Untrusted::Conservation {
            detail: "the introduced and inherited buckets do not reconstruct the staged report"
                .to_string(),
        });
    }

    let mut got_baseline: HashMap<Key, usize> = HashMap::new();
    for f in out.inherited.iter().chain(out.resolved.iter()) {
        *got_baseline.entry(key(f)).or_insert(0) += 1;
    }
    if got_baseline != tally(baseline) {
        return Err(Untrusted::Conservation {
            detail: "the inherited and resolved buckets do not reconstruct the baseline report"
                .to_string(),
        });
    }
    Ok(())
}

impl Partition {
    /// Everything on the branch, attributed to nobody's change.
    ///
    /// The answer when the stage is empty: an empty changeset cannot introduce
    /// a finding, so the split is known without asking the server twice.
    pub fn all_inherited(response: &ValidateResponse) -> Partition {
        Partition {
            introduced: Vec::new(),
            inherited: findings_of(response).to_vec(),
            resolved: Vec::new(),
        }
    }

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

    fn finding(code: &str, locator: &str) -> models::Finding {
        models::Finding {
            code: code.to_string(),
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

    const UNSAT: &str = "unsatisfied_input";
    const MISMATCH: &str = "type_mismatch";

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
        //
        // The ORDER matters for what this proves. With base=[UNSAT@a] and
        // staged=[UNSAT@a, MISMATCH@a], multiset counting hands the second
        // entry to `introduced` whichever key is used — so that arrangement
        // passes even with `code` dropped entirely, which a review demonstrated.
        // The discriminating case swaps the code between the two reports: with
        // the real key that is one resolved and one introduced; keyed on the
        // locator alone it collapses to a single *inherited* finding, which is
        // the silent direction.
        let base = response(7, vec![finding(MISMATCH, "a")]);
        let staged = response(7, vec![finding(UNSAT, "a")]);
        let p = partition(&base, &staged).unwrap();
        assert_eq!(p.introduced.len(), 1, "the new code must be introduced");
        assert_eq!(p.introduced[0].code, UNSAT);
        assert_eq!(p.resolved.len(), 1, "the old code must be resolved");
        assert_eq!(p.resolved[0].code, MISMATCH);
        assert!(
            p.inherited.is_empty(),
            "a different code on the same locator is not the same finding"
        );

        // And the additive arrangement still behaves.
        let base = response(7, vec![finding(UNSAT, "a")]);
        let staged = response(7, vec![finding(UNSAT, "a"), finding(MISMATCH, "a")]);
        let p = partition(&base, &staged).unwrap();
        assert_eq!(p.introduced.len(), 1);
        assert_eq!(p.introduced[0].code, MISMATCH);
    }

    #[test]
    fn a_duplicate_in_the_baseline_is_counted_too() {
        // The multiset must count both sides. Keyed as a set on the baseline,
        // two inherited findings on one locator would leave one of them looking
        // resolved when it was not.
        let base = response(7, vec![finding(UNSAT, "a"), finding(UNSAT, "a")]);
        let staged = response(7, vec![finding(UNSAT, "a"), finding(UNSAT, "a")]);
        let p = partition(&base, &staged).unwrap();
        assert_eq!(p.inherited.len(), 2, "both copies are inherited");
        assert!(p.resolved.is_empty(), "nothing was resolved");
        assert!(p.introduced.is_empty());
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
    fn conservation_catches_a_dropped_or_invented_finding() {
        // The old check compared bucket lengths to input lengths, which the
        // loops make true by construction — a review rewrote it to
        // `> total + 9999` and nothing failed. This exercises it directly, with
        // buckets that do not reconstruct the reports.
        let a = finding(UNSAT, "a");
        let b = finding(UNSAT, "b");

        // Dropped: staged had two, the buckets account for one.
        let dropped = Partition {
            introduced: vec![a.clone()],
            inherited: vec![],
            resolved: vec![],
        };
        let err = check_conservation(&dropped, &[], &[a.clone(), b.clone()]).unwrap_err();
        assert!(matches!(err, Untrusted::Conservation { .. }), "{err:?}");
        assert!(err.to_string().contains("does not add up"), "{err}");

        // Invented: a finding in no report at all.
        let invented = Partition {
            introduced: vec![a.clone(), b.clone()],
            inherited: vec![],
            resolved: vec![],
        };
        assert!(check_conservation(&invented, &[], std::slice::from_ref(&a)).is_err());

        // Baseline side: resolved claims something the baseline never had.
        let bogus = Partition {
            introduced: vec![],
            inherited: vec![],
            resolved: vec![b.clone()],
        };
        assert!(check_conservation(&bogus, std::slice::from_ref(&a), &[]).is_err());

        // And a correct split passes. Note an INHERITED finding appears in both
        // reports by definition, so the staged side must list it too — the
        // check is strict about that, which is the point.
        let good = Partition {
            introduced: vec![b.clone()],
            inherited: vec![a.clone()],
            resolved: vec![],
        };
        let base_one = [a.clone()];
        assert!(check_conservation(&good, &base_one, &[a, b]).is_ok());
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
