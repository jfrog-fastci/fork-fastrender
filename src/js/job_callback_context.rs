//! Thread-local incumbent context for HTML `HostMakeJobCallback` / `HostCallJobCallback`.
//!
//! `vm-js` exposes a host hook surface for wrapping callbacks into [`vm_js::JobCallback`] records.
//! HTML uses these records to capture and restore the "incumbent settings object" when Promise
//! jobs/microtasks run.
//!
//! FastRender uses this to carry a minimal snapshot of the current script/realm through Promise
//! reactions so operations like dynamic `import()` can resolve relative to the correct classic
//! script URL even after the originating script has finished running.

use std::cell::RefCell;
use vm_js::{RealmId, ScriptId};

/// Minimal host-defined data attached to a `vm_js::JobCallback`.
///
/// This is intentionally an owned, Send+Sync record (no raw pointers).
#[derive(Clone, Debug, Default)]
pub(crate) struct JobCallbackContext {
  pub realm: Option<RealmId>,
  pub window_id: Option<u64>,
  pub script_id: Option<ScriptId>,
  pub script_url: Option<String>,
}

thread_local! {
  static CONTEXT_STACK: RefCell<Vec<JobCallbackContext>> = const { RefCell::new(Vec::new()) };
}

#[must_use]
pub(crate) struct JobCallbackContextGuard {
  _private: (),
}

/// Push a new job-callback context onto the thread-local stack.
pub(crate) fn push_job_callback_context(ctx: JobCallbackContext) -> JobCallbackContextGuard {
  CONTEXT_STACK.with(|stack| stack.borrow_mut().push(ctx));
  JobCallbackContextGuard { _private: () }
}

impl Drop for JobCallbackContextGuard {
  fn drop(&mut self) {
    CONTEXT_STACK.with(|stack| {
      let popped = stack.borrow_mut().pop();
      debug_assert!(popped.is_some(), "job callback context stack underflow");
    });
  }
}

/// Snapshot the current top-of-stack context.
pub(crate) fn current_job_callback_context() -> JobCallbackContext {
  CONTEXT_STACK.with(|stack| stack.borrow().last().cloned().unwrap_or_default())
}

/// Best-effort lookup for a script URL captured in the incumbent context stack.
///
/// This is used as a fallback by the module loader when the per-realm script-id URL map has been
/// removed after script evaluation, but a Promise microtask is still executing code originating
/// from that script.
pub(crate) fn script_url_for_script_id(script_id: ScriptId) -> Option<String> {
  CONTEXT_STACK.with(|stack| {
    stack
      .borrow()
      .iter()
      .rev()
      .find(|ctx| ctx.script_id == Some(script_id))
      .and_then(|ctx| ctx.script_url.clone())
  })
}

