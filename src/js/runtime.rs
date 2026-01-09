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

/// Runs `f` with `event_loop` installed as the current JS-visible event loop.
pub fn with_event_loop<Host: 'static, R>(
  event_loop: &mut EventLoop<Host>,
  f: impl FnOnce() -> R,
) -> R {
  EVENT_LOOP_STACK.with(|stack| stack.borrow_mut().push(event_loop as *mut _ as *mut ()));
  let result = f();
  EVENT_LOOP_STACK.with(|stack| {
    stack
      .borrow_mut()
      .pop()
      .expect("event loop stack underflow");
  });
  result
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

