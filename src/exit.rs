//! Stable process exit codes — few codes, with retry-relevant cases distinct.
//!
//! Success is `0` (std `ExitCode::SUCCESS`); the codes below are the failure
//! signal. Richer machine detail rides in the `--json` output's `error.kind`,
//! not in new codes.
/// Success — no failure and (for `validate`) no error-severity findings.
pub const SUCCESS: u8 = 0;

/// Generic failure (a bug, a malformed request, an unhandled case).
pub const GENERIC: u8 = 1;

/// Optimistic-concurrency conflict — the branch moved under us. Retryable.
pub const CONFLICT: u8 = 4;

/// `hydrate validate` found error-severity coherence findings in the staged
/// change. Distinct from a transport/parse failure so an agent can gate a loop
/// (`hydrate validate && hydrate commit`): the findings themselves still print;
/// this code is only the pass/fail signal, not an error reaching the service.
pub const VALIDATION: u8 = 5;

/// Network / transport failure reaching the service. Retryable.
pub const NETWORK: u8 = 6;
