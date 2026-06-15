//! Hand-written ergonomics layer over the generated [`crate::wire`] client.
//!
//! Keeps the generated wire types untouched (DRY, regenerated from the spec)
//! while exposing the small, friendly surface the verb handlers call. Populated
//! once transport lands.
