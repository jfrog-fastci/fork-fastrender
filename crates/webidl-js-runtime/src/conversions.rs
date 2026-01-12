//! WebIDL <-> JavaScript value conversion algorithms.
//!
//! This module is a thin re-export of the runtime-agnostic algorithms in the vendored `webidl`
//! crate (`vendor/ecma-rs/webidl`).

pub use webidl::conversions::*;
