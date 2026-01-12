//! `ecma-rs/vm-js` backend.
//!
//! This crate's primary `vm-js` runtime implementation is [`VmJsRuntime`]. Some downstream code
//! prefers importing backend-specific types via a dedicated module (e.g. `crate::vmjs::...`), so
//! keep a thin `vmjs` module that re-exports the canonical runtime type.

pub use crate::ecma_runtime::VmJsRuntime;
