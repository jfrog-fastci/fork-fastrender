use crate::error::Result;

use super::event_loop::EventLoop;
use std::cell::RefCell;
use vm_js::Value;

/// JS value type used by FastRender's `vm-js` embedding.
///
/// This replaces the old placeholder `JsValue` enum previously defined in `window_timers.rs`.
pub type JsValue = Value;

pub type NativeFunction<Host> =
  Box<dyn Fn(&mut Host, &mut EventLoop<Host>) -> Result<JsValue> + 'static>;

pub trait JsObject<Host: 'static> {
  fn define_method(&mut self, name: &str, func: NativeFunction<Host>);
}

pub trait JsRuntime<Host: 'static> {
  type Object: JsObject<Host>;

  fn global_object(&mut self, name: &str) -> &mut Self::Object;

  fn define_global_function(&mut self, name: &str, func: NativeFunction<Host>);
}

thread_local! {
  /// A stack of pointers to the currently executing [`EventLoop`].
  ///
  /// `vm-js` native functions do not receive an `&mut EventLoop`, but Web APIs like `setTimeout`
  /// need to schedule work on the currently executing event loop. The embedding installs the event
  /// loop pointer before calling into JS.
  static EVENT_LOOP_STACK: RefCell<Vec<*mut ()>> = RefCell::new(Vec::new());
}

struct EventLoopStackGuard {
  expected_ptr: *mut (),
}

impl Drop for EventLoopStackGuard {
  fn drop(&mut self) {
    EVENT_LOOP_STACK.with(|stack| {
      let popped = stack.borrow_mut().pop();
      debug_assert!(popped.is_some(), "event loop stack underflow");
      if let Some(popped) = popped {
        debug_assert_eq!(
          popped, self.expected_ptr,
          "event loop stack corruption (expected different pointer)"
        );
      }
    });
  }
}

/// Push `event_loop` onto the thread-local "current event loop" stack and return an RAII guard
/// that will pop the pointer on drop.
///
/// # Safety
///
/// `event_loop` must be a valid pointer to an `EventLoop<Host>` for the duration of the returned
/// guard, and the caller must ensure the `Host` type parameter matches the event loop that will
/// later be retrieved via [`current_event_loop_mut`].
unsafe fn push_event_loop_ptr<Host: 'static>(event_loop: *mut EventLoop<Host>) -> EventLoopStackGuard {
  let ptr = event_loop as *mut ();
  EVENT_LOOP_STACK.with(|stack| stack.borrow_mut().push(ptr));
  EventLoopStackGuard { expected_ptr: ptr }
}

/// RAII guard that temporarily moves the `EventLoop` out of a `&mut` reference while JS is
/// executing.
///
/// This keeps `current_event_loop_mut()` sound: native bindings can obtain an `&mut EventLoop`
/// without aliasing the caller's `&mut EventLoop` borrow.
struct EventLoopSwapGuard<'a, Host: 'static> {
  slot: &'a mut EventLoop<Host>,
  owned: EventLoop<Host>,
}

impl<'a, Host: 'static> EventLoopSwapGuard<'a, Host> {
  fn new(slot: &'a mut EventLoop<Host>) -> Self {
    let owned = std::mem::take(slot);
    Self { slot, owned }
  }

  fn owned_ptr(&mut self) -> *mut EventLoop<Host> {
    &mut self.owned as *mut EventLoop<Host>
  }
}

impl<Host: 'static> Drop for EventLoopSwapGuard<'_, Host> {
  fn drop(&mut self) {
    // Always restore the event loop, even if JS panics.
    *self.slot = std::mem::take(&mut self.owned);
  }
}

/// Runs `f` with `event_loop` installed as the current JS-visible event loop.
pub fn with_event_loop<Host: 'static, R>(
  event_loop: &mut EventLoop<Host>,
  f: impl FnOnce() -> R,
) -> R {
  let mut swap = EventLoopSwapGuard::new(event_loop);
  // SAFETY: `swap` owns the moved-out event loop for the duration of this call. The pointer is
  // removed from the TLS stack before `swap` restores the event loop back into `event_loop`.
  let _guard = unsafe { push_event_loop_ptr::<Host>(swap.owned_ptr()) };
  f()
}

/// Returns the currently installed event loop pointer.
pub(crate) fn current_event_loop_mut<Host: 'static>() -> Option<&'static mut EventLoop<Host>> {
  let ptr = EVENT_LOOP_STACK.with(|stack| stack.borrow().last().copied());
  let ptr = ptr?;
  // SAFETY: `with_event_loop` installs a valid pointer for the duration of a JS call. The pointer
  // is only used during that dynamic extent. Callers must ensure the `Host` type parameter matches
  // the installed event loop.
  Some(unsafe { &mut *(ptr as *mut EventLoop<Host>) })
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::panic::{catch_unwind, AssertUnwindSafe};

  #[test]
  fn with_event_loop_restores_event_loop_and_tls_stack_on_panic() -> crate::Result<()> {
    // Use `()` as a minimal host type.
    let mut host = ();
    let mut event_loop = EventLoop::<()>::new();

    let result = catch_unwind(AssertUnwindSafe(|| {
      with_event_loop(&mut event_loop, || {
        // Ensure the pointer is visible inside the closure.
        let loop_ref = current_event_loop_mut::<()>().expect("expected current event loop");
        loop_ref.queue_microtask(|_, _| Ok(())).expect("queue microtask");

        panic!("boom");
      });
    }));
    assert!(result.is_err());

    // The TLS stack must be cleaned up even on panic.
    assert!(current_event_loop_mut::<()>().is_none());

    // And the moved-out event loop must be restored so the queued microtask still runs.
    event_loop.perform_microtask_checkpoint(&mut host)?;
    Ok(())
  }
}
