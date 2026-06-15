use std::process::ExitCode;

use super::unimplemented;
use crate::cli::EdgeAddArgs;

pub fn add(_args: EdgeAddArgs) -> ExitCode {
    unimplemented("edge add")
}
