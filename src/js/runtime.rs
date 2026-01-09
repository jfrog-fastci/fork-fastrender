use crate::error::Result;

use super::event_loop::EventLoop;
use super::window_timers::JsValue;

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
