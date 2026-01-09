use std::collections::BTreeMap;

use crate::js::webidl::WebIdlBindingsRuntime;

/// A minimally-typed value container used by the generated binding shims when crossing into the
/// host.
///
/// This is intentionally small: it is *not* a full JS value model. Objects are passed through as
/// opaque `JsValue` handles, while primitives/dictionaries are converted to Rust-owned values.
#[derive(Debug, Clone, PartialEq)]
pub enum BindingValue<JsValue: Copy> {
  Undefined,
  Null,
  Bool(bool),
  Number(f64),
  String(String),
  Object(JsValue),
  Sequence(Vec<BindingValue<JsValue>>),
  Dictionary(BTreeMap<String, BindingValue<JsValue>>),
}

/// Host-defined behavior implementation for WebIDL bindings.
///
/// The generated bindings are responsible for:
/// - overload resolution
/// - argument conversion
/// - return value conversion
///
/// The host is responsible for implementing the actual DOM/Web API behavior and for maintaining
/// any per-object state associated with `JsValue` handles.
pub trait WebHostBindings<R>: Sized
where
  R: WebIdlBindingsRuntime<Self>,
{
  fn call_operation(
    &mut self,
    rt: &mut R,
    receiver: Option<R::JsValue>,
    interface: &'static str,
    operation: &'static str,
    overload: usize,
    args: Vec<BindingValue<R::JsValue>>,
  ) -> Result<BindingValue<R::JsValue>, R::Error>;
}
