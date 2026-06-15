use std::process::ExitCode;

use super::not_implemented;
use crate::cli::NodeAddArgs;

pub fn add(_args: NodeAddArgs) -> ExitCode {
    not_implemented("node add")
}
