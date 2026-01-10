//! Compile-time regression test: `fastrender::js::dom_integration` must remain exported.
//!
//! The module lives in `src/js/legacy/dom_integration.rs` but is referenced by streaming pipeline
//! unit tests and some optional JS harness plumbing. A few refactors accidentally removed the
//! re-export from `src/js/mod.rs`, breaking downstream builds.

#[test]
fn dom_integration_module_is_exported() {
  #[allow(unused_imports)]
  use fastrender::js::dom_integration;
}

