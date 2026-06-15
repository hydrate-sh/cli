//! Generated typed client — re-exported from the committed `hydrate_wire` crate
//! (produced by `openapi-generator` from the vendored `openapi.json`; regenerate
//! with `scripts/regen-wire.sh`).
//!
//! Never hand-edited; the committed crate is regenerated from the spec so the
//! wire types cannot drift from the contract. The hand-written ergonomics layer
//! ([`crate::client`]) wraps these; verb handlers use this surface, not the
//! generated names directly.

// Re-exported as the internal wire surface; consumed once the ergonomics layer
// (`client`) and the verb handlers wire up transport.
#[allow(unused_imports)]
pub use hydrate_wire::{apis, models};

#[cfg(test)]
mod tests {
    //! Smoke test: referencing generated delta types proves the committed wire
    //! crate compiles and still carries the delta vocabulary from the spec.
    //! Fails to compile if a type is dropped or renamed by a regen.
    fn _assert_is_type<T>() {}

    #[test]
    fn generated_delta_vocabulary_is_present() {
        _assert_is_type::<super::models::AddNodeDelta>();
        _assert_is_type::<super::models::AddEdgeDelta>();
        _assert_is_type::<super::models::DeltaFieldErrorBody>();
    }
}
