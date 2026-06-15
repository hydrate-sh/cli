use std::process::ExitCode;

use super::unimplemented;
use crate::cli::NodeAddArgs;

pub fn add(_args: NodeAddArgs) -> ExitCode {
    unimplemented("node add")
}
