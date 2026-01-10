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

/// Derive a stable [`InterfaceId`] from an interface name.
///
/// This uses the 32-bit FNV-1a hash of the UTF-8 bytes, matching the helper used by the legacy
/// `crates/webidl-js-runtime` scaffolding. Generated bindings can use this for interface-like checks
/// (e.g. union conversion's platform object branch) before the bindings pipeline grows a dedicated
/// per-world interface ID registry.
pub fn interface_id_from_name(name: &str) -> InterfaceId {
  let mut hash: u32 = 0x811c_9dc5;
  for &b in name.as_bytes() {
    hash ^= b as u32;
    hash = hash.wrapping_mul(0x0100_0193);
  }
  InterfaceId(hash)
}

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
