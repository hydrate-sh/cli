use std::process::ExitCode;

use super::not_implemented;
use crate::cli::EdgeAddArgs;

pub fn add(_args: EdgeAddArgs) -> ExitCode {
    not_implemented("edge add")
}
