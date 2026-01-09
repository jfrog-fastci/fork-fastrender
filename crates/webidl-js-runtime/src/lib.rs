//! Web IDL <-> JavaScript runtime adapter layer.
//!
//! Web IDL conversions and overload resolution are specified in terms of ECMAScript abstract
//! operations. This crate defines a small runtime trait boundary ([`JsRuntime`]/[`WebIdlJsRuntime`])
//! and provides a concrete implementation backed by `ecma-rs`'s `vm-js` value types.

pub mod ecma_runtime;
pub mod runtime;

pub use ecma_runtime::VmJsRuntime;
pub use runtime::{IteratorRecord, JsOwnPropertyDescriptor, JsRuntime, WebIdlJsRuntime};

