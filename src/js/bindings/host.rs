use std::collections::BTreeMap;

use crate::js::webidl::WebIdlBindingsRuntime;
use vm_js::{Scope, Value, VmError};

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
  FrozenArray(Vec<BindingValue<JsValue>>),
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

/// Host-defined behavior implementation for *realm-based* (`vm-js`) WebIDL bindings.
///
/// This mirrors [`WebHostBindings`], but is used by bindings installed directly into a `vm-js`
/// [`Realm`](vm_js::Realm) and invoked via [`vm_js::Vm::call_with_host_and_hooks`] /
/// [`vm_js::Vm::construct_with_host_and_hooks`].
///
/// Unlike `WebHostBindings`, this trait is **not** generic over a runtime adapter: it uses
/// `vm-js`'s native [`Value`] type directly.
pub trait VmJsBindingsHost {
  fn call_operation(
    &mut self,
    scope: &mut Scope<'_>,
    receiver: Option<Value>,
    interface: &'static str,
    operation: &'static str,
    overload: usize,
    args: Vec<BindingValue<Value>>,
  ) -> Result<BindingValue<Value>, VmError>;
}
