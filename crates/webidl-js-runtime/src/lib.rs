//! Web IDL <-> JavaScript runtime adapter layer.
//!
//! Web IDL conversions and overload resolution are specified in terms of ECMAScript abstract
//! operations. This crate defines a small runtime trait boundary ([`JsRuntime`]/[`WebIdlJsRuntime`])
//! and provides a concrete implementation backed by `ecma-rs`'s `vm-js` value types.
//!
//! Note: core WebIDL types like [`InterfaceId`], [`WebIdlHooks`], [`WebIdlLimits`], and
//! [`WebIdlLimits`] are re-exported from `engines/ecma-rs/webidl` so FastRender does not maintain
//! duplicated definitions across crates.

pub mod conversions;
pub mod ecma_runtime;
pub mod overload_resolution;
pub mod runtime;
pub mod to_js;

pub use conversions::{convert_arguments, convert_to_idl, ArgumentSchema, ConvertedValue};
pub use ecma_runtime::VmJsRuntime;
pub use overload_resolution::{
  resolve_overload, throw_no_matching_overload, ConvertedArgument, OverloadArg, OverloadSig,
  Optionality, ResolvedOverload, WebIdlValue,
};
pub use runtime::{
  interface_id_from_name, InterfaceId, IteratorRecord, JsOwnPropertyDescriptor, JsPropertyKind,
  JsRuntime, WebIdlHooks, WebIdlJsRuntime, WebIdlLimits, NativeHostFunction, WebIdlBindingsRuntime,
};
pub use to_js::{to_js, to_js_with_limits, ToJsLimits};

// Re-export the canonical runtime trait from `engines/ecma-rs/webidl` under an explicit name so
// callers can migrate without conflicting with this crate's legacy runtime traits.
pub use webidl::JsRuntime as EcmaJsRuntime;
