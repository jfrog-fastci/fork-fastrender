use crate::dom2;
use crate::js::CurrentScriptStateHandle;
use crate::js::host_document::{ActiveEventGuard, ActiveEventStack};
use std::ptr::NonNull;

/// Embedder-provided host context passed to `vm-js` native call/construct handlers.
///
/// `vm-js`'s native handler signatures include an opaque `&mut dyn vm_js::VmHost` parameter so
/// embedders can thread arbitrary state through VM→host boundaries without relying on thread-local
/// globals.
///
/// ## Safety contract
///
/// This context is created **fresh for each host→JS entry** ("call turn") and must not be stored
/// anywhere that could outlive that call (including inside GC-managed JS objects).
///
/// The raw pointers carried by this context are only valid for the duration of that call turn. In
/// particular, the `dom` pointer must not be retained across navigations or other host mutations
/// that could drop/replace the backing document.
#[derive(Default)]
pub struct VmJsHostContext {
  dom: Option<NonNull<dom2::Document>>,
  current_script_state: Option<CurrentScriptStateHandle>,
  active_events: ActiveEventStack,
}

impl VmJsHostContext {
  pub fn new(
    dom: Option<NonNull<dom2::Document>>,
    current_script_state: Option<CurrentScriptStateHandle>,
  ) -> Self {
    Self {
      dom,
      current_script_state,
      active_events: ActiveEventStack::default(),
    }
  }

  /// Returns the raw pointer to the current `dom2::Document` (if any).
  #[inline]
  pub fn dom_ptr(&self) -> Option<NonNull<dom2::Document>> {
    self.dom
  }

  /// Returns an immutable reference to the current `dom2::Document` (if any).
  ///
  /// See the type-level safety contract: the returned reference must not outlive the JS call turn
  /// that created this host context.
  #[inline]
  pub fn dom(&self) -> Option<&dom2::Document> {
    // SAFETY: `dom` is only set by the embedder when the pointer is valid for the duration of the
    // current JS call turn.
    self.dom.map(|ptr| unsafe { ptr.as_ref() })
  }

  /// Returns a mutable reference to the current `dom2::Document` (if any).
  ///
  /// See the type-level safety contract: the returned reference must not outlive the JS call turn
  /// that created this host context.
  #[inline]
  pub fn dom_mut(&mut self) -> Option<&mut dom2::Document> {
    // SAFETY: `dom` is only set by the embedder when the pointer is valid and uniquely owned for
    // the duration of the current JS call turn.
    self.dom.map(|mut ptr| unsafe { ptr.as_mut() })
  }

  #[inline]
  pub fn current_script_state(&self) -> Option<&CurrentScriptStateHandle> {
    self.current_script_state.as_ref()
  }

  pub(crate) fn push_active_event(
    &mut self,
    event_id: u64,
    event: &mut crate::web::events::Event,
  ) -> ActiveEventGuard {
    self.active_events.push(event_id, event)
  }

  pub(crate) fn with_active_event<R>(
    &mut self,
    event_id: u64,
    f: impl FnOnce(&mut crate::web::events::Event) -> R,
  ) -> Option<R> {
    self.active_events.with_event(event_id, f)
  }
}
