//! Compatibility wrapper around the vendored `ecma-rs` WebIDL runtime.
//!
//! This crate preserves the historical package/crate naming (`webidl-js-runtime` /
//! `webidl_js_runtime`) while the implementation lives in `vendor/ecma-rs/webidl-runtime`.
//!
//! New code should prefer depending on `webidl-runtime` directly.

pub use webidl_runtime::*;
