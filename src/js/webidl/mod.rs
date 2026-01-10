//! Web IDL scaffolding for JS bindings.
//!
//! FastRender uses the `vendor/ecma-rs/webidl` crate as the authoritative source of:
//! - WebIDL conversion helpers (e.g. `DOMString`)
//! - the `JsRuntime` trait boundary used by those helpers
//!
//! This module re-exports that API surface as `fastrender::js::webidl` so generated bindings can
//! depend on a single in-crate path without duplicating conversion logic.

pub use webidl::*;

// FastRender-specific VM/runtime scaffolding used by generated bindings and host shims.
pub use webidl_js_runtime::{
  NativeHostFunction, VmJsRuntime, WebIdlBindingsRuntime,
};
