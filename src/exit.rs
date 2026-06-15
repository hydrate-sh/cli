//! Stable process exit codes — few codes, with retry-relevant cases distinct.
//!
//! Success is `0` (std `ExitCode::SUCCESS`); the codes below are the failure
//! signal. Richer machine detail rides in the `--json` output's `error.kind`,
//! not in new codes.
//!
//! `CONFLICT` and `NETWORK` are the reserved contract — they are wired when
//! transport lands, so they are not yet referenced.
#![allow(dead_code)]

/// Generic failure (a bug, a malformed request, an unhandled case).
pub const GENERIC: u8 = 1;

/// Optimistic-concurrency conflict — the branch moved under us. Retryable.
pub const CONFLICT: u8 = 4;

/// Network / transport failure reaching the service. Retryable.
pub const NETWORK: u8 = 6;
