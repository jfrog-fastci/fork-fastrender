//! Web IDL scaffolding for JS bindings.
//!
//! FastRender uses the `vendor/ecma-rs/webidl` crate as the authoritative source of:
//! - WebIDL conversion helpers (e.g. `DOMString`)
//! - the `JsRuntime` trait boundary used by those helpers
//!
//! This module re-exports that API surface as `fastrender::js::webidl` so generated bindings can
//! depend on a single in-crate path without duplicating conversion logic.

pub use webidl::*;

/// Canonical `vm-js` adapter for the `webidl` conversion/runtime traits.
pub use webidl_vm_js::VmJsWebIdlCx;

/// Standard data-property attribute presets used by WebIDL bindings installation code.
pub use webidl_vm_js::bindings_runtime::DataPropertyAttributes;

pub mod conversions;

/// Canonical bindings runtime for installing WebIDL-generated APIs onto a real `vm-js` realm.
pub use crate::js::webidl_runtime_vmjs::{
  IteratorRecord, NativeHostFunction, VmJsWebIdlBindingsCx, VmJsWebIdlBindingsState, WebIdlBindingsRuntime,
};

#[deprecated(note = "Use fastrender::js::webidl::legacy::VmJsRuntime instead.")]
pub use legacy::VmJsRuntime;

/// Legacy bindings runtime that operates on a heap-only value model.
///
/// This runtime cannot execute author scripts and exists only as a temporary compatibility layer
/// while FastRender migrates bindings onto real `vm-js` realms.
pub mod legacy {
  pub use webidl_js_runtime::{NativeHostFunction, VmJsRuntime, WebIdlBindingsRuntime};
}
