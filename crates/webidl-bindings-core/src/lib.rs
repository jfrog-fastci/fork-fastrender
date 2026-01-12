#![forbid(unsafe_code)]

//! Runtime-agnostic WebIDL bindings algorithms.
//!
//! This crate contains the spec-shaped WebIDL algorithms used by FastRender's generated bindings:
//! - ECMAScript <-> IDL conversions
//! - runtime overload resolution
//!
//! The algorithms are written against a small runtime trait boundary (`JsRuntime` /
//! `WebIdlJsRuntime`) so they can be reused across JS backends without re-implementing the WebIDL
//! spec in codegen.

pub mod conversions;
pub mod overload_resolution;
pub mod runtime;

pub use conversions::{
  convert_arguments, convert_to_idl, ArgumentSchema, ConvertedValue, IntegerConversionAttrs,
};
pub use overload_resolution::{
  resolve_overload, throw_no_matching_overload, ConvertedArgument, Optionality, OverloadArg,
  OverloadSig, ResolvedOverload, WebIdlValue,
};
pub use runtime::{
  interface_id_from_name, InterfaceId, IteratorRecord, JsOwnPropertyDescriptor, JsPropertyKind,
  JsRuntime, WebIdlHooks, WebIdlJsRuntime, WebIdlLimits,
};
