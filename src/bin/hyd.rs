//! `hyd` — short alias for the `hydrate` binary; same entry point.

use std::process::ExitCode;

fn main() -> ExitCode {
    hydrate::run()
}
