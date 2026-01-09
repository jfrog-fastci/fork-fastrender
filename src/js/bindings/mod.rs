//! WebIDL-driven JavaScript bindings.
//!
//! - [`generated`] contains generic WebIDL-to-host glue (calls into [`host`]).
//! - [`dom_generated`] contains a temporary `VmJsRuntime`-backed DOM scaffold used for early
//!   integration/testing.
//!
//! Alongside the generated scaffolding we keep a small set of handwritten helpers (e.g.
//! `DOMException`) that provide spec-shaped behaviour needed by early bindings/tests.

pub mod dom_exception;
pub mod document;
pub mod dom_generated;
pub mod generated;
pub mod host;
mod scaffold_selectors;

pub use dom_exception::DomExceptionClass;
pub use document::install_document_query_selector_bindings;
pub use dom_generated::install_dom_bindings as install_dom_bindings_generated;
pub use generated::install_window_bindings;
pub use host::{BindingValue, WebHostBindings};

/// Host-provided hooks for DOM bindings.
///
/// For the MVP DOM scaffold we only require a handle to the global object that bindings should be
/// installed onto. Future work will extend this trait to allocate real platform objects and wire
/// method bodies to DOM implementations.
pub trait DomHost {
  fn global_object(&mut self) -> vm_js::Value;
}

pub fn install_dom_bindings(
  rt: &mut webidl_js_runtime::VmJsRuntime,
  host: &mut impl DomHost,
) -> Result<(), vm_js::VmError> {
  dom_generated::install_dom_bindings(rt, host)?;
  let global = host.global_object();
  let dom_exception = DomExceptionClass::install(rt, global)?;
  scaffold_selectors::install_scaffold_selector_bindings(rt, global, dom_exception)?;
  Ok(())
}
