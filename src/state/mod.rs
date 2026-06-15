//! On-disk working-directory state under `.hydrate/`.
//!
//! `config.toml` binds this directory to a project + branch (human-edited,
//! footgun-free); `stage.json` holds the staged changeset + the path→UUID alias
//! table (wire-native JSON). Populated when branch binding + staging land.
