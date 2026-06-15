use std::process::ExitCode;

use super::not_implemented;
use crate::cli::ForkArgs;

pub fn run(_args: ForkArgs) -> ExitCode {
    not_implemented("fork")
}
