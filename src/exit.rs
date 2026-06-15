//! Stable process exit codes — few codes, with retry-relevant cases distinct.
//!
//! Richer machine detail rides in the `--json` output's `error.kind`; these
//! codes are the coarse, scriptable signal.

/// Generic failure (a bug, a malformed request, an unhandled case).
pub const GENERIC: u8 = 1;

/// Optimistic-concurrency conflict — the branch moved under us. Retryable.
pub const CONFLICT: u8 = 4;

/// Network / transport failure reaching the service. Retryable.
pub const NETWORK: u8 = 6;
