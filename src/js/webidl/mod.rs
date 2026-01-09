//! Web IDL scaffolding for JS bindings.
//!
//! The actual runtime adapter lives in the `webidl-js-runtime` crate; FastRender re-exports the
//! trait boundary here so the eventual DOM bindings can depend on `fastrender::js::webidl` without
//! pulling in additional crates directly.

pub use webidl_js_runtime::{
  InterfaceId, IteratorRecord, JsOwnPropertyDescriptor, JsPropertyKind, JsRuntime, VmJsRuntime,
  WebIdlHooks, WebIdlJsRuntime, WebIdlLimits,
};
