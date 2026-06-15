use std::process::ExitCode;

use super::unimplemented;
use crate::cli::ForkArgs;

pub fn run(_args: ForkArgs) -> ExitCode {
    unimplemented("fork")
}
