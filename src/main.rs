//! Canonical `hydrate` binary — a thin wrapper over [`hydrate::run`].

use std::process::ExitCode;

fn main() -> ExitCode {
    hydrate::run()
}
