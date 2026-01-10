use std::cell::Cell;
use std::marker::PhantomData;
use vm_js::{
  GcObject, HostSlots, Intrinsics, PropertyDescriptor, PropertyKey, PropertyKind, Scope, Value, Vm,
  VmError, VmHost, VmHostHooks,
};

use webidl::WebIdlHooks;

/// A host function callback used by generated WebIDL bindings.
///
/// This matches the calling convention used by `crates/webidl-js-runtime`, but is implemented here
/// so new bindings can target the canonical `vendor/ecma-rs/webidl` + `crates/webidl-vm-js` stack.
pub type NativeHostFunction<R, Host> = fn(
  rt: &mut R,
  host: &mut Host,
  this: <R as WebIdlBindingsRuntime<Host>>::JsValue,
  args: &[<R as WebIdlBindingsRuntime<Host>>::JsValue],
) -> Result<<R as WebIdlBindingsRuntime<Host>>::JsValue, <R as WebIdlBindingsRuntime<Host>>::Error>;

/// Host-facing runtime API used by generated WebIDL bindings.
///
/// This trait is intentionally narrow: it exists to let generated glue install JS-visible
/// constructors/prototypes and invoke host-defined method bodies without depending on a specific
/// JS runtime implementation.
///
/// New WebIDL bindings should depend on this trait via `crate::js::webidl::WebIdlBindingsRuntime`.
pub trait WebIdlBindingsRuntime<Host>: Sized {
  /// ECMAScript value type.
  type JsValue: Copy;
  /// ECMAScript property key type (`String | Symbol`).
  type PropertyKey: Copy;
  /// Error type used by the runtime (usually the engine's throw/termination type).
  type Error;

  fn js_undefined(&self) -> Self::JsValue;
  fn js_null(&self) -> Self::JsValue;
  fn js_bool(&self, value: bool) -> Self::JsValue;
  fn js_number(&self, value: f64) -> Self::JsValue;
  fn js_string(&mut self, value: &str) -> Result<Self::JsValue, Self::Error>;

  fn js_string_to_rust_string(&mut self, value: Self::JsValue) -> Result<String, Self::Error>;

  fn is_undefined(&self, value: Self::JsValue) -> bool;
  fn is_null(&self, value: Self::JsValue) -> bool;
  fn is_object(&self, value: Self::JsValue) -> bool;
  fn is_boolean(&self, value: Self::JsValue) -> bool;

  fn to_boolean(&mut self, value: Self::JsValue) -> Result<bool, Self::Error>;
  fn to_number(&mut self, value: Self::JsValue) -> Result<f64, Self::Error>;
  fn to_string(&mut self, value: Self::JsValue) -> Result<Self::JsValue, Self::Error>;

  fn throw_type_error(&mut self, message: &str) -> Self::Error;
  fn throw_range_error(&mut self, message: &str) -> Self::Error;

  fn property_key(&mut self, name: &str) -> Result<Self::PropertyKey, Self::Error>;

  fn get(&mut self, obj: Self::JsValue, key: Self::PropertyKey) -> Result<Self::JsValue, Self::Error>;

  fn create_object(&mut self) -> Result<Self::JsValue, Self::Error>;

  fn create_function(&mut self, f: NativeHostFunction<Self, Host>) -> Result<Self::JsValue, Self::Error>;

  fn global_object(&mut self) -> Result<Self::JsValue, Self::Error>;

  fn define_data_property_str(
    &mut self,
    obj: Self::JsValue,
    name: &str,
    value: Self::JsValue,
    enumerable: bool,
  ) -> Result<(), Self::Error>;
}

/// Long-lived bindings state associated with a `vm-js` realm.
///
/// The state must outlive any JS function objects created by [`VmJsWebIdlBindingsCx::create_function`],
/// since a raw pointer to this state is stored in the function object's host slots for dispatch.
///
/// In practice, store this in a `Box` field on your realm/host struct.
pub struct VmJsWebIdlBindingsState<Host> {
  pub global_object: GcObject,
  pub limits: webidl::WebIdlLimits,
  pub hooks: Box<dyn WebIdlHooks<Value>>,
  native_call_id: Cell<Option<vm_js::NativeFunctionId>>,
  _phantom: PhantomData<fn(Host)>,
}

impl<Host> VmJsWebIdlBindingsState<Host> {
  pub fn new(
    global_object: GcObject,
    limits: webidl::WebIdlLimits,
    hooks: Box<dyn WebIdlHooks<Value>>,
  ) -> Self {
    Self {
      global_object,
      limits,
      hooks,
      native_call_id: Cell::new(None),
      _phantom: PhantomData,
    }
  }
}

/// Canonical WebIDL bindings runtime adapter for a real `vm-js` realm.
///
/// This is the preferred runtime for generated bindings: it installs real `vm-js` function objects
/// onto a realm global and performs conversions using `webidl` + [`webidl_vm_js::VmJsWebIdlCx`].
pub struct VmJsWebIdlBindingsCx<'a, Host> {
  state: &'a VmJsWebIdlBindingsState<Host>,
  cx: webidl_vm_js::VmJsWebIdlCx<'a>,
}

impl<'a, Host> VmJsWebIdlBindingsCx<'a, Host> {
  pub fn new(
    vm: &'a mut Vm,
    heap: &'a mut vm_js::Heap,
    state: &'a VmJsWebIdlBindingsState<Host>,
  ) -> Self {
    let cx = webidl_vm_js::VmJsWebIdlCx::new(vm, heap, state.limits, state.hooks.as_ref());
    Self { state, cx }
  }

  pub fn new_in_scope(
    vm: &'a mut Vm,
    scope: &'a mut Scope<'_>,
    state: &'a VmJsWebIdlBindingsState<Host>,
  ) -> Self {
    let cx = webidl_vm_js::VmJsWebIdlCx::new_in_scope(vm, scope, state.limits, state.hooks.as_ref());
    Self { state, cx }
  }

  fn intrinsics(&self) -> Result<Intrinsics, VmError> {
    self.cx.vm.intrinsics().ok_or(VmError::InvariantViolation(
      "vm-js intrinsics not installed; expected an initialized Realm",
    ))
  }
}

fn make_data_descriptor(value: Value, enumerable: bool) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable,
    configurable: true,
    kind: PropertyKind::Data {
      value,
      writable: true,
    },
  }
}

fn read_callee_slots(scope: &Scope<'_>, callee: GcObject) -> Result<HostSlots, VmError> {
  scope
    .heap()
    .object_host_slots(callee)?
    .ok_or(VmError::InvariantViolation(
      "WebIDL bindings function missing host slots",
    ))
}

fn host_from_vm_host<Host: 'static>(host: &mut dyn VmHost) -> Result<&mut Host, VmError> {
  host
    .as_any_mut()
    .downcast_mut::<Host>()
    .ok_or(VmError::TypeError(
      "WebIDL bindings host context type mismatch for native call",
    ))
}

fn dispatch_native_call<Host: 'static>(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let slots = read_callee_slots(scope, callee)?;
  let state_ptr = slots.a as *const VmJsWebIdlBindingsState<Host>;
  let f_ptr = slots.b as usize;
  if state_ptr.is_null() || f_ptr == 0 {
    return Err(VmError::InvariantViolation(
      "WebIDL bindings function has null dispatch metadata",
    ));
  }

  // SAFETY:
  // - `VmJsWebIdlBindingsState` is expected to be stored in a stable address (e.g. `Box`) for the
  //   lifetime of any JS function objects created by `create_function`.
  // - The stored function pointer is a plain Rust `fn` pointer.
  let state: &VmJsWebIdlBindingsState<Host> = unsafe { &*state_ptr };

  let host = host_from_vm_host::<Host>(host)?;

  let mut rt = VmJsWebIdlBindingsCx::new_in_scope(vm, scope, state);

  // SAFETY: function pointer lifetimes are erased; we rehydrate it at the call site.
  let f: NativeHostFunction<VmJsWebIdlBindingsCx<'_, Host>, Host> =
    unsafe { std::mem::transmute(f_ptr) };

  f(&mut rt, host, this, args)
}

impl<Host: 'static> WebIdlBindingsRuntime<Host> for VmJsWebIdlBindingsCx<'_, Host> {
  type JsValue = Value;
  type PropertyKey = PropertyKey;
  type Error = VmError;

  fn js_undefined(&self) -> Self::JsValue {
    Value::Undefined
  }

  fn js_null(&self) -> Self::JsValue {
    Value::Null
  }

  fn js_bool(&self, value: bool) -> Self::JsValue {
    Value::Bool(value)
  }

  fn js_number(&self, value: f64) -> Self::JsValue {
    Value::Number(value)
  }

  fn js_string(&mut self, value: &str) -> Result<Self::JsValue, Self::Error> {
    let s = self.cx.scope.alloc_string(value)?;
    self.cx.scope.push_root(Value::String(s))?;
    Ok(Value::String(s))
  }

  fn js_string_to_rust_string(&mut self, value: Self::JsValue) -> Result<String, Self::Error> {
    let Value::String(s) = value else {
      return Err(VmError::TypeError(
        "expected a string value for js_string_to_rust_string",
      ));
    };
    Ok(self.cx.scope.heap().get_string(s)?.to_utf8_lossy())
  }

  fn is_undefined(&self, value: Self::JsValue) -> bool {
    matches!(value, Value::Undefined)
  }

  fn is_null(&self, value: Self::JsValue) -> bool {
    matches!(value, Value::Null)
  }

  fn is_object(&self, value: Self::JsValue) -> bool {
    matches!(value, Value::Object(_))
  }

  fn is_boolean(&self, value: Self::JsValue) -> bool {
    matches!(value, Value::Bool(_))
  }

  fn to_boolean(&mut self, value: Self::JsValue) -> Result<bool, Self::Error> {
    use webidl::JsRuntime as _;
    self.cx.to_boolean(value)
  }

  fn to_number(&mut self, value: Self::JsValue) -> Result<f64, Self::Error> {
    use webidl::JsRuntime as _;
    self.cx.to_number(value)
  }

  fn to_string(&mut self, value: Self::JsValue) -> Result<Self::JsValue, Self::Error> {
    use webidl::JsRuntime as _;
    let s = self.cx.to_string(value)?;
    Ok(Value::String(s))
  }

  fn throw_type_error(&mut self, message: &str) -> Self::Error {
    let intr = match self.intrinsics() {
      Ok(intr) => intr,
      Err(err) => return err,
    };
    vm_js::throw_type_error(&mut self.cx.scope, intr, message)
  }

  fn throw_range_error(&mut self, message: &str) -> Self::Error {
    let intr = match self.intrinsics() {
      Ok(intr) => intr,
      Err(err) => return err,
    };
    match vm_js::new_range_error(&mut self.cx.scope, intr, message) {
      Ok(value) => VmError::Throw(value),
      Err(err) => err,
    }
  }

  fn property_key(&mut self, name: &str) -> Result<Self::PropertyKey, Self::Error> {
    let s = self.cx.scope.alloc_string(name)?;
    self.cx.scope.push_root(Value::String(s))?;
    Ok(PropertyKey::from_string(s))
  }

  fn get(&mut self, obj: Self::JsValue, key: Self::PropertyKey) -> Result<Self::JsValue, Self::Error> {
    let Value::Object(obj) = obj else {
      return Err(self.throw_type_error("get: expected object receiver"));
    };
    // Root the receiver + key while running `[[Get]]`.
    match key {
      PropertyKey::String(s) => {
        self.cx.scope.push_root(Value::String(s))?;
      }
      PropertyKey::Symbol(s) => {
        self.cx.scope.push_root(Value::Symbol(s))?;
      }
    }
    self.cx.scope.push_root(Value::Object(obj))?;

    let value = self.cx.vm.get(&mut self.cx.scope, obj, key)?;
    self.cx.scope.push_root(value)?;
    Ok(value)
  }

  fn create_object(&mut self) -> Result<Self::JsValue, Self::Error> {
    use webidl::JsRuntime as _;
    let obj = self.cx.alloc_object()?;
    Ok(Value::Object(obj))
  }

  fn create_function(&mut self, f: NativeHostFunction<Self, Host>) -> Result<Self::JsValue, Self::Error> {
    let call_id = if let Some(id) = self.state.native_call_id.get() {
      id
    } else {
      let id = self.cx.vm.register_native_call(dispatch_native_call::<Host>)?;
      self.state.native_call_id.set(Some(id));
      id
    };

    let intr = self.intrinsics()?;

    // Root the name across allocation of the function object.
    let name = self.cx.scope.alloc_string("webidl binding")?;
    self.cx.scope.push_root(Value::String(name))?;

    let func = self.cx.scope.alloc_native_function(call_id, None, name, 0)?;
    self.cx.scope.push_root(Value::Object(func))?;
    self.cx
      .scope
      .heap_mut()
      .object_set_prototype(func, Some(intr.function_prototype()))?;

    let slots = HostSlots {
      a: (self.state as *const VmJsWebIdlBindingsState<Host>) as u64,
      b: f as usize as u64,
    };
    self.cx.scope.heap_mut().object_set_host_slots(func, slots)?;

    Ok(Value::Object(func))
  }

  fn global_object(&mut self) -> Result<Self::JsValue, Self::Error> {
    self.cx.scope.push_root(Value::Object(self.state.global_object))?;
    Ok(Value::Object(self.state.global_object))
  }

  fn define_data_property_str(
    &mut self,
    obj: Self::JsValue,
    name: &str,
    value: Self::JsValue,
    enumerable: bool,
  ) -> Result<(), Self::Error> {
    let Value::Object(obj) = obj else {
      return Err(self.throw_type_error("define_data_property_str: expected object receiver"));
    };

    // Temporarily root `obj` and `value` while allocating the property key: allocating can trigger
    // a GC cycle and would invalidate unrooted GC handles.
    let mut scope = self.cx.scope.reborrow();
    scope.push_root(Value::Object(obj))?;
    scope.push_root(value)?;

    let key_s = scope.alloc_string(name)?;
    scope.push_root(Value::String(key_s))?;
    let key = PropertyKey::from_string(key_s);

    scope.define_property(obj, key, make_data_descriptor(value, enumerable))
  }
}

/// Compatibility shim: allow existing generated bindings to continue compiling against the legacy
/// `crates/webidl-js-runtime::VmJsRuntime` during migration.
impl<Host: 'static> WebIdlBindingsRuntime<Host> for webidl_js_runtime::VmJsRuntime {
  type JsValue = Value;
  type PropertyKey = PropertyKey;
  type Error = VmError;

  fn js_undefined(&self) -> Self::JsValue {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::js_undefined(self)
  }

  fn js_null(&self) -> Self::JsValue {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::js_null(self)
  }

  fn js_bool(&self, value: bool) -> Self::JsValue {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::js_boolean(self, value)
  }

  fn js_number(&self, value: f64) -> Self::JsValue {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::js_number(self, value)
  }

  fn js_string(&mut self, value: &str) -> Result<Self::JsValue, Self::Error> {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::alloc_string(self, value)
  }

  fn js_string_to_rust_string(&mut self, value: Self::JsValue) -> Result<String, Self::Error> {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::string_to_utf8_lossy(
      self, value,
    )
  }

  fn is_undefined(&self, value: Self::JsValue) -> bool {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::is_undefined(self, value)
  }

  fn is_null(&self, value: Self::JsValue) -> bool {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::is_null(self, value)
  }

  fn is_object(&self, value: Self::JsValue) -> bool {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::is_object(self, value)
  }

  fn is_boolean(&self, value: Self::JsValue) -> bool {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::is_boolean(self, value)
  }

  fn to_boolean(&mut self, value: Self::JsValue) -> Result<bool, Self::Error> {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::to_boolean(self, value)
  }

  fn to_number(&mut self, value: Self::JsValue) -> Result<f64, Self::Error> {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::to_number(self, value)
  }

  fn to_string(&mut self, value: Self::JsValue) -> Result<Self::JsValue, Self::Error> {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::to_string(self, value)
  }

  fn throw_type_error(&mut self, message: &str) -> Self::Error {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::WebIdlJsRuntime>::throw_type_error(
      self, message,
    )
  }

  fn throw_range_error(&mut self, message: &str) -> Self::Error {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::WebIdlJsRuntime>::throw_range_error(
      self, message,
    )
  }

  fn property_key(&mut self, name: &str) -> Result<Self::PropertyKey, Self::Error> {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::property_key_from_str(
      self, name,
    )
  }

  fn get(&mut self, obj: Self::JsValue, key: Self::PropertyKey) -> Result<Self::JsValue, Self::Error> {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::get(self, obj, key)
  }

  fn create_object(&mut self) -> Result<Self::JsValue, Self::Error> {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::WebIdlBindingsRuntime<Host>>::create_object(
      self,
    )
  }

  fn create_function(&mut self, f: NativeHostFunction<Self, Host>) -> Result<Self::JsValue, Self::Error> {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::WebIdlBindingsRuntime<Host>>::create_function(self, f)
  }

  fn global_object(&mut self) -> Result<Self::JsValue, Self::Error> {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::WebIdlBindingsRuntime<Host>>::global_object(self)
  }

  fn define_data_property_str(
    &mut self,
    obj: Self::JsValue,
    name: &str,
    value: Self::JsValue,
    enumerable: bool,
  ) -> Result<(), Self::Error> {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::WebIdlBindingsRuntime<Host>>::define_data_property_str(
      self,
      obj,
      name,
      value,
      enumerable,
    )
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use vm_js::{Heap, HeapLimits, JsRuntime as VmJsRuntime, VmOptions};
  use webidl::{JsRuntime as _, InterfaceId, WebIdlHooks, WebIdlLimits};

  struct NoHooks;

  impl WebIdlHooks<Value> for NoHooks {
    fn is_platform_object(&self, _value: Value) -> bool {
      false
    }

    fn implements_interface(&self, _value: Value, _interface: InterfaceId) -> bool {
      false
    }
  }

  #[derive(Default)]
  struct TestHost;

  fn add<'a>(
    rt: &mut VmJsWebIdlBindingsCx<'a, TestHost>,
    _host: &mut TestHost,
    _this: Value,
    args: &[Value],
  ) -> Result<Value, VmError> {
    let a = args.get(0).copied().unwrap_or(Value::Undefined);
    let b = args.get(1).copied().unwrap_or(Value::Undefined);
    let a = rt.to_number(a)?;
    let b = rt.to_number(b)?;
    Ok(rt.js_number(a + b))
  }

  #[test]
  fn vmjs_bindings_runtime_installs_and_dispatches_host_function() -> Result<(), VmError> {
    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(HeapLimits::new(16 * 1024 * 1024, 8 * 1024 * 1024));
    let mut runtime = VmJsRuntime::new(vm, heap)?;

    let state = Box::new(VmJsWebIdlBindingsState::<TestHost>::new(
      runtime.realm().global_object(),
      WebIdlLimits::default(),
      Box::new(NoHooks),
    ));

    {
      let (vm, heap, _realm) = webidl_vm_js::split_js_runtime(&mut runtime);
      let mut cx = VmJsWebIdlBindingsCx::new(vm, heap, &state);

      let func = cx.create_function(add)?;
      let global = cx.global_object()?;
      cx.define_data_property_str(global, "add", func, true)?;
    }

    let mut host = TestHost::default();
    let out = runtime.exec_script_with_host(&mut host, "add(1, 2)")?;
    assert!(matches!(out, Value::Number(n) if (n - 3.0).abs() < f64::EPSILON));
    Ok(())
  }

  #[test]
  fn vmjs_webidl_cx_sets_array_prototype_when_intrinsics_installed() -> Result<(), VmError> {
    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(HeapLimits::new(16 * 1024 * 1024, 8 * 1024 * 1024));
    let mut runtime = VmJsRuntime::new(vm, heap)?;

    let (vm, heap, realm) = webidl_vm_js::split_js_runtime(&mut runtime);
    let hooks = NoHooks;
    let limits = WebIdlLimits::default();
    let mut cx = webidl_vm_js::VmJsWebIdlCx::new(vm, heap, limits, &hooks);

    let arr =
      webidl::sequence_to_js_array(&mut cx, &limits, &[1u32, 2u32, 3u32]).expect("sequence_to_js_array");

    let proto = cx.scope.heap().object_prototype(arr)?;
    assert_eq!(proto, Some(realm.intrinsics().array_prototype()));
    Ok(())
  }
}
