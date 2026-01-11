use crate::error::Result;

use super::event_loop::EventLoop;
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
