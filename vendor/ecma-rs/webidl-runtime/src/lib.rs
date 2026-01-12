//! Web IDL <-> JavaScript runtime adapter layer.
//!
//! Web IDL conversions and overload resolution are specified in terms of ECMAScript abstract
//! operations. This crate defines a small runtime trait boundary ([`JsRuntime`]/[`WebIdlJsRuntime`])
//! and provides a concrete implementation backed by `ecma-rs`'s `vm-js` value types.
//!
//! # `vendor/ecma-rs/webidl` integration
//!
//! This crate also depends on the `webidl` crate (in the same `vendor/ecma-rs` workspace). That
//! crate defines its own `webidl::JsRuntime` trait, which is used by spec-shaped helpers like:
//! - `webidl::convert_js_to_idl`
//! - `webidl::resolve_overload`
//!
//! `vm-js` GC handles are **not automatically rooted**, so this crate does **not** implement
//! `webidl::JsRuntime` directly for [`VmJsRuntime`]. Instead, callers must run `webidl` conversions
//! inside [`VmJsRuntime::with_webidl_cx`] using the provided [`VmJsWebIdlCx`] conversion context.
//! Note that callers still need to ensure any input `vm-js` handles passed into WebIDL algorithms
//! are rooted for the duration of the conversion (e.g. via [`VmJsRuntime::with_webidl_cx_rooted`]).
//!
//! Note: core WebIDL types like [`InterfaceId`], [`WebIdlHooks`], [`WebIdlLimits`], and
//! [`WebIdlLimits`] are re-exported from `vendor/ecma-rs/webidl` so FastRender does not maintain
//! duplicated definitions across crates.

pub mod conversions;
pub mod ecma_runtime;
pub mod overload_resolution;
pub mod runtime;
pub mod to_js;
pub mod vmjs;

pub use conversions::{convert_arguments, convert_to_idl, ArgumentSchema, ConvertedValue};
pub use ecma_runtime::{VmJsRuntime, VmJsWebIdlCx};
pub use overload_resolution::{
  resolve_overload, throw_no_matching_overload, ConvertedArgument, Optionality, OverloadArg,
  OverloadSig, ResolvedOverload, WebIdlValue,
};
pub use runtime::{
  interface_id_from_name, InterfaceId, IteratorRecord, JsOwnPropertyDescriptor, JsPropertyKind,
  JsRuntime, NativeHostFunction, WebIdlBindingsRuntime, WebIdlHooks, WebIdlJsRuntime, WebIdlLimits,
};
pub use to_js::{to_js, to_js_with_limits, ToJsLimits};

// Re-export the canonical runtime trait from `vendor/ecma-rs/webidl` under an explicit name so
// callers can migrate without conflicting with this crate's legacy runtime traits.
pub use webidl::JsRuntime as EcmaJsRuntime;
