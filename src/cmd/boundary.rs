//! `boundary flatten` — stage flattening a boundary (promote its children to its
//! parent and remove it). Nothing hits the server until `commit`.

use super::context::require_workdir;
use crate::cli::BoundaryFlattenArgs;
use crate::error::CliError;
use crate::output::OutputMode;
use crate::staging::{BoundaryFlattened, Changeset};
use crate::state::{Index, Stage};

pub fn flatten(args: BoundaryFlattenArgs, mode: OutputMode) -> Result<(), CliError> {
    let base = require_workdir()?;
    let mut changeset = Changeset::with_index(Stage::load(&base)?, Index::load(&base)?);
    let flattened = changeset.flatten_boundary(&args.path)?;
    changeset.into_stage().save(&base)?;

    println!("{}", render(&flattened, mode));
    Ok(())
}

fn render(f: &BoundaryFlattened, mode: OutputMode) -> String {
    match mode {
        OutputMode::Json => serde_json::json!({ "staged": { "flatten": f.path } }).to_string(),
        OutputMode::Human => format!("Staged flatten of boundary '{}'.", f.path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_names_the_boundary_in_both_modes() {
        let f = BoundaryFlattened {
            path: "Api".to_string(),
        };
        assert_eq!(
            render(&f, OutputMode::Human),
            "Staged flatten of boundary 'Api'."
        );
        let v: serde_json::Value = serde_json::from_str(&render(&f, OutputMode::Json)).unwrap();
        assert_eq!(v["staged"]["flatten"], "Api");
    }
}
