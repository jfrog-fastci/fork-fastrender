use std::cell::{Cell, RefCell};
use std::marker::PhantomData;
use vm_js::{
  GcObject, HostSlots, Intrinsics, PropertyDescriptor, PropertyKey, PropertyKind, Scope, Value, Vm,
  VmError, VmHost, VmHostHooks,
};

use webidl::WebIdlHooks;
use webidl_vm_js::CallbackHandle;

/// Iterator state used by WebIDL `sequence<T>` / `FrozenArray<T>` conversions.
///
/// This mirrors the ECMAScript `IteratorRecord` shape: iterator object + cached `next` method +
/// `done` flag.
///
/// `vm-js` does not yet provide `%Array.prototype%[@@iterator]`, so generated bindings include a
/// minimal Array iteration fast-path so list arguments can accept arrays.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IteratorRecord<V: Copy> {
  pub iterator: V,
  pub next_method: V,
  pub done: bool,
  kind: IteratorRecordKind<V>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum IteratorRecordKind<V: Copy> {
  Protocol,
  Array {
    array: V,
    next_index: u32,
    length: u32,
  },
}

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

  /// Run `f` with `roots` treated as GC roots for the duration of the call.
  ///
  /// WebIDL conversion algorithms often keep VM values in local variables across allocations (for
  /// example iterator records when converting `sequence<T>`). GC-backed runtimes must ensure those
  /// values remain rooted so they cannot be collected while host code is still using them.
  fn with_stack_roots<R, F>(&mut self, roots: &[Self::JsValue], f: F) -> Result<R, Self::Error>
  where
    F: FnOnce(&mut Self) -> Result<R, Self::Error>;

  /// Conversion limits configured by the embedding.
  fn limits(&self) -> webidl::WebIdlLimits;

  fn js_undefined(&self) -> Self::JsValue;
  fn js_null(&self) -> Self::JsValue;
  fn js_bool(&self, value: bool) -> Self::JsValue;
  fn js_number(&self, value: f64) -> Self::JsValue;
  fn js_string(&mut self, value: &str) -> Result<Self::JsValue, Self::Error>;

  fn js_string_to_rust_string(&mut self, value: Self::JsValue) -> Result<String, Self::Error>;

  fn is_undefined(&self, value: Self::JsValue) -> bool;
  fn is_null(&self, value: Self::JsValue) -> bool;
  fn is_object(&self, value: Self::JsValue) -> bool;
  fn is_callable(&self, value: Self::JsValue) -> bool;
  fn is_boolean(&self, value: Self::JsValue) -> bool;
  fn is_number(&self, value: Self::JsValue) -> bool;
  fn is_bigint(&self, value: Self::JsValue) -> bool;
  fn is_string(&self, value: Self::JsValue) -> bool;
  fn is_string_object(&self, value: Self::JsValue) -> bool;
  fn is_symbol(&self, value: Self::JsValue) -> bool;

  /// Returns whether `value` is an embedding-defined platform object.
  fn is_platform_object(&self, value: Self::JsValue) -> bool;

  /// Returns whether `value` implements the given WebIDL interface.
  fn implements_interface(&self, value: Self::JsValue, interface: webidl::InterfaceId) -> bool;

  fn to_boolean(&mut self, value: Self::JsValue) -> Result<bool, Self::Error>;
  fn to_number(&mut self, value: Self::JsValue) -> Result<f64, Self::Error>;
  fn to_string(&mut self, value: Self::JsValue) -> Result<Self::JsValue, Self::Error>;

  fn throw_type_error(&mut self, message: &str) -> Self::Error;
  fn throw_range_error(&mut self, message: &str) -> Self::Error;
  fn throw_dom_exception(&mut self, name: &str, message: &str) -> Self::Error;

  fn property_key(&mut self, name: &str) -> Result<Self::PropertyKey, Self::Error>;

  /// Return the property key for `%Symbol.iterator%` in the active realm.
  fn symbol_iterator(&mut self) -> Result<Self::PropertyKey, Self::Error>;

  /// Return the property key for `%Symbol.asyncIterator%` in the active realm.
  fn symbol_async_iterator(&mut self) -> Result<Self::PropertyKey, Self::Error>;

  fn get(&mut self, obj: Self::JsValue, key: Self::PropertyKey) -> Result<Self::JsValue, Self::Error>;

  /// ECMAScript abstract operation `GetMethod ( V, P )`.
  fn get_method(
    &mut self,
    obj: Self::JsValue,
    key: Self::PropertyKey,
  ) -> Result<Option<Self::JsValue>, Self::Error>;

  /// ECMAScript abstract operation `GetIterator ( obj )`.
  fn get_iterator(
    &mut self,
    host: &mut Host,
    iterable: Self::JsValue,
  ) -> Result<IteratorRecord<Self::JsValue>, Self::Error>;

  fn get_iterator_from_method(
    &mut self,
    host: &mut Host,
    iterable: Self::JsValue,
    method: Self::JsValue,
  ) -> Result<IteratorRecord<Self::JsValue>, Self::Error>;

  fn iterator_step_value(
    &mut self,
    host: &mut Host,
    iterator_record: &mut IteratorRecord<Self::JsValue>,
  ) -> Result<Option<Self::JsValue>, Self::Error>;

  fn create_object(&mut self) -> Result<Self::JsValue, Self::Error>;

  /// Create a JavaScript `Array` object.
  ///
  /// The default implementation falls back to [`WebIdlBindingsRuntime::create_object`] so runtimes
  /// that do not yet support arrays can still compile generated bindings.
  fn create_array(&mut self, _len: usize) -> Result<Self::JsValue, Self::Error> {
    self.create_object()
  }

  /// Create a host-defined function object with WebIDL-visible metadata.
  ///
  /// - `name` is used for the function's `.name` property.
  /// - `length` is used for the function's `.length` property (required argument count).
  fn create_function(
    &mut self,
    name: &str,
    length: u32,
    f: NativeHostFunction<Self, Host>,
  ) -> Result<Self::JsValue, Self::Error>;

  /// Create a host-defined constructor function object with WebIDL-visible metadata.
  ///
  /// `vm-js` distinguishes between `[[Call]]` and `[[Construct]]` internal methods, so WebIDL
  /// interface objects with a `constructor(...)` member must provide both:
  /// - `call` is used for `Ctor(...)` and should generally throw a TypeError ("Illegal constructor").
  /// - `construct` is used for `new Ctor(...)`.
  fn create_constructor(
    &mut self,
    name: &str,
    length: u32,
    call: NativeHostFunction<Self, Host>,
    construct: NativeHostFunction<Self, Host>,
  ) -> Result<Self::JsValue, Self::Error>;

  /// Root and return a WebIDL callback function handle.
  ///
  /// Generated bindings use this to pass callbacks to the host in a GC-safe way.
  fn root_callback_function(&mut self, _value: Self::JsValue) -> Result<CallbackHandle, Self::Error> {
    Err(self.throw_type_error("Callback functions are not supported by this runtime"))
  }

  /// Root and return a WebIDL callback interface handle.
  ///
  /// Callback interfaces accept callable functions or objects with a callable `handleEvent` method.
  fn root_callback_interface(&mut self, _value: Self::JsValue) -> Result<CallbackHandle, Self::Error> {
    Err(self.throw_type_error("Callback interfaces are not supported by this runtime"))
  }

  fn global_object(&mut self) -> Result<Self::JsValue, Self::Error>;

  fn define_data_property_with_attrs(
    &mut self,
    obj: Self::JsValue,
    key: Self::PropertyKey,
    value: Self::JsValue,
    writable: bool,
    enumerable: bool,
    configurable: bool,
  ) -> Result<(), Self::Error>;

  fn define_accessor_property_with_attrs(
    &mut self,
    obj: Self::JsValue,
    key: Self::PropertyKey,
    get: Self::JsValue,
    set: Self::JsValue,
    enumerable: bool,
    configurable: bool,
  ) -> Result<(), Self::Error>;

  fn set_prototype(
    &mut self,
    obj: Self::JsValue,
    proto: Option<Self::JsValue>,
  ) -> Result<(), Self::Error>;

  fn define_data_property(
    &mut self,
    obj: Self::JsValue,
    key: Self::PropertyKey,
    value: Self::JsValue,
    enumerable: bool,
  ) -> Result<(), Self::Error> {
    self.define_data_property_with_attrs(obj, key, value, true, enumerable, true)
  }

  fn define_data_property_str(
    &mut self,
    obj: Self::JsValue,
    name: &str,
    value: Self::JsValue,
    enumerable: bool,
  ) -> Result<(), Self::Error> {
    let key = self.property_key(name)?;
    self.define_data_property(obj, key, value, enumerable)
  }

  fn define_data_property_str_with_attrs(
    &mut self,
    obj: Self::JsValue,
    name: &str,
    value: Self::JsValue,
    writable: bool,
    enumerable: bool,
    configurable: bool,
  ) -> Result<(), Self::Error> {
    let key = self.property_key(name)?;
    self.define_data_property_with_attrs(obj, key, value, writable, enumerable, configurable)
  }

  fn define_accessor_property_str_with_attrs(
    &mut self,
    obj: Self::JsValue,
    name: &str,
    get: Self::JsValue,
    set: Self::JsValue,
    enumerable: bool,
    configurable: bool,
  ) -> Result<(), Self::Error> {
    let key = self.property_key(name)?;
    self.define_accessor_property_with_attrs(obj, key, get, set, enumerable, configurable)
  }

  /// Defines a WebIDL operation method property.
  ///
  /// This follows the Web IDL JavaScript binding:
  /// - writable: true
  /// - configurable: true
  /// - enumerable: true
  fn define_method(
    &mut self,
    obj: Self::JsValue,
    name: &str,
    func: Self::JsValue,
  ) -> Result<(), Self::Error> {
    self.define_data_property_str_with_attrs(obj, name, func, true, true, true)
  }

  /// Defines a WebIDL attribute accessor property.
  ///
  /// - configurable: true
  /// - enumerable: true
  fn define_attribute_accessor(
    &mut self,
    obj: Self::JsValue,
    name: &str,
    get: Self::JsValue,
    set: Self::JsValue,
  ) -> Result<(), Self::Error> {
    self.define_accessor_property_str_with_attrs(obj, name, get, set, true, true)
  }

  /// Defines a WebIDL constant property.
  ///
  /// - writable: false
  /// - configurable: false
  /// - enumerable: true
  fn define_constant(
    &mut self,
    obj: Self::JsValue,
    name: &str,
    value: Self::JsValue,
  ) -> Result<(), Self::Error> {
    self.define_data_property_str_with_attrs(obj, name, value, false, true, false)
  }

  /// Defines an interface constructor, wiring `.prototype` and `prototype.constructor`.
  ///
  /// The property attributes are chosen to match WebIDL's requirements for interface objects and
  /// prototype objects:
  /// - `global[name]`: writable + configurable, non-enumerable
  /// - `ctor.prototype`: non-writable, non-enumerable, non-configurable
  /// - `proto.constructor`: writable + configurable, non-enumerable
  fn define_constructor(
    &mut self,
    global: Self::JsValue,
    name: &str,
    ctor: Self::JsValue,
    proto: Self::JsValue,
  ) -> Result<(), Self::Error> {
    self.define_data_property_str_with_attrs(global, name, ctor, true, false, true)?;
    self.define_data_property_str_with_attrs(ctor, "prototype", proto, false, false, false)?;
    self.define_data_property_str_with_attrs(proto, "constructor", ctor, true, false, true)?;
    Ok(())
  }
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
  native_construct_id: Cell<Option<vm_js::NativeConstructId>>,
  dispatch_records: RefCell<Vec<Box<NativeDispatchRecord>>>,
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
      native_construct_id: Cell::new(None),
      dispatch_records: RefCell::new(Vec::new()),
      _phantom: PhantomData,
    }
  }

  fn alloc_dispatch_record(&self, call: usize, construct: usize) -> *const NativeDispatchRecord {
    let mut records = self.dispatch_records.borrow_mut();
    records.push(Box::new(NativeDispatchRecord { call, construct }));
    // SAFETY: boxed value has a stable address even if `records` reallocates.
    records
      .last()
      .expect("push ensured an element exists")
      .as_ref() as *const NativeDispatchRecord
  }
}

/// Canonical WebIDL bindings runtime adapter for a real `vm-js` realm.
///
/// This is the preferred runtime for generated bindings: it installs real `vm-js` function objects
/// onto a realm global and performs conversions using `webidl` + [`webidl_vm_js::VmJsWebIdlCx`].
pub struct VmJsWebIdlBindingsCx<'a, Host> {
  state: &'a VmJsWebIdlBindingsState<Host>,
  cx: webidl_vm_js::VmJsWebIdlCx<'a>,
  cached_next_key: Option<PropertyKey>,
  cached_done_key: Option<PropertyKey>,
  cached_value_key: Option<PropertyKey>,
}

const ILLEGAL_CONSTRUCTOR_ERROR: &str = "Illegal constructor";

#[derive(Clone, Copy)]
struct NativeDispatchRecord {
  call: usize,
  construct: usize,
}

impl<'a, Host> VmJsWebIdlBindingsCx<'a, Host> {
  pub fn new(
    vm: &'a mut Vm,
    heap: &'a mut vm_js::Heap,
    state: &'a VmJsWebIdlBindingsState<Host>,
  ) -> Self {
    let cx = webidl_vm_js::VmJsWebIdlCx::new(vm, heap, state.limits, state.hooks.as_ref());
    Self {
      state,
      cx,
      cached_next_key: None,
      cached_done_key: None,
      cached_value_key: None,
    }
  }

  pub fn new_in_scope(
    vm: &'a mut Vm,
    scope: &'a mut Scope<'_>,
    state: &'a VmJsWebIdlBindingsState<Host>,
  ) -> Self {
    let cx = webidl_vm_js::VmJsWebIdlCx::new_in_scope(vm, scope, state.limits, state.hooks.as_ref());
    Self {
      state,
      cx,
      cached_next_key: None,
      cached_done_key: None,
      cached_value_key: None,
    }
  }

  fn intrinsics(&self) -> Result<Intrinsics, VmError> {
    self.cx.vm.intrinsics().ok_or(VmError::InvariantViolation(
      "vm-js intrinsics not installed; expected an initialized Realm",
    ))
  }

  fn dom_exception_class_from_global(
    &mut self,
  ) -> Result<Option<crate::js::bindings::DomExceptionClassVmJs>, VmError> {
    // Prefer the real `DOMException` class when available. This is installed by `WindowRealm`
    // initialization (and other embeddings) on the global object.
    //
    // When missing, fall back to throwing an `Error`-like object so WebIDL conversions can still
    // report failures in non-DOM contexts.
    let mut scope = self.cx.scope.reborrow();
    let global = self.state.global_object;
    scope.push_root(Value::Object(global))?;

    let key_dom_exception_s = scope.alloc_string("DOMException")?;
    scope.push_root(Value::String(key_dom_exception_s))?;
    let key_dom_exception = PropertyKey::from_string(key_dom_exception_s);
    let constructor = match scope
      .heap()
      .object_get_own_data_property_value(global, &key_dom_exception)
    {
      Ok(Some(Value::Object(constructor))) => constructor,
      Ok(_) | Err(VmError::PropertyNotData) => return Ok(None),
      Err(err) => return Err(err),
    };
    scope.push_root(Value::Object(constructor))?;

    let key_prototype_s = scope.alloc_string("prototype")?;
    scope.push_root(Value::String(key_prototype_s))?;
    let key_prototype = PropertyKey::from_string(key_prototype_s);
    let prototype = match scope
      .heap()
      .object_get_own_data_property_value(constructor, &key_prototype)
    {
      Ok(Some(Value::Object(prototype))) => prototype,
      Ok(_) | Err(VmError::PropertyNotData) => return Ok(None),
      Err(err) => return Err(err),
    };

    Ok(Some(crate::js::bindings::DomExceptionClassVmJs {
      constructor,
      prototype,
    }))
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
  let dispatch_ptr = slots.b as *const NativeDispatchRecord;
  if state_ptr.is_null() || dispatch_ptr.is_null() {
    return Err(VmError::InvariantViolation(
      "WebIDL bindings function has null dispatch metadata",
    ));
  }

  // SAFETY:
  // - `VmJsWebIdlBindingsState` is expected to be stored in a stable address (e.g. `Box`) for the
  //   lifetime of any JS function objects created by `create_function`.
  // - `dispatch_ptr` points to a `NativeDispatchRecord` owned by the same state.
  let state: &VmJsWebIdlBindingsState<Host> = unsafe { &*state_ptr };
  let dispatch: &NativeDispatchRecord = unsafe { &*dispatch_ptr };
  if dispatch.call == 0 {
    return Err(VmError::InvariantViolation(
      "WebIDL bindings function missing [[Call]] dispatch entry",
    ));
  }

  let host = host_from_vm_host::<Host>(host)?;

  let mut rt = VmJsWebIdlBindingsCx::new_in_scope(vm, scope, state);

  // SAFETY: function pointer lifetimes are erased; we rehydrate it at the call site.
  let f: NativeHostFunction<VmJsWebIdlBindingsCx<'_, Host>, Host> =
    unsafe { std::mem::transmute(dispatch.call) };

  f(&mut rt, host, this, args)
}

fn dispatch_native_construct<Host: 'static>(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  args: &[Value],
  new_target: Value,
) -> Result<Value, VmError> {
  // For now, enforce `new_target === callee` so we can be deterministic while we do not support
  // subclassing semantics in the WebIDL bindings layer.
  if new_target != Value::Object(callee) {
    let intr = vm.intrinsics().ok_or(VmError::InvariantViolation(
      "vm-js intrinsics not installed; expected an initialized Realm",
    ))?;
    return Err(vm_js::throw_type_error(
      scope,
      intr,
      ILLEGAL_CONSTRUCTOR_ERROR,
    ));
  }

  let slots = read_callee_slots(scope, callee)?;
  let state_ptr = slots.a as *const VmJsWebIdlBindingsState<Host>;
  let dispatch_ptr = slots.b as *const NativeDispatchRecord;
  if state_ptr.is_null() || dispatch_ptr.is_null() {
    return Err(VmError::InvariantViolation(
      "WebIDL bindings function has null dispatch metadata",
    ));
  }

  // SAFETY: see `dispatch_native_call`.
  let state: &VmJsWebIdlBindingsState<Host> = unsafe { &*state_ptr };
  let dispatch: &NativeDispatchRecord = unsafe { &*dispatch_ptr };
  if dispatch.construct == 0 {
    return Err(VmError::InvariantViolation(
      "WebIDL bindings function missing [[Construct]] dispatch entry",
    ));
  }

  let host = host_from_vm_host::<Host>(host)?;
  let mut rt = VmJsWebIdlBindingsCx::new_in_scope(vm, scope, state);

  // SAFETY: function pointer lifetimes are erased; we rehydrate it at the call site.
  let f: NativeHostFunction<VmJsWebIdlBindingsCx<'_, Host>, Host> =
    unsafe { std::mem::transmute(dispatch.construct) };
  f(&mut rt, host, Value::Undefined, args)
}

impl<Host: 'static> WebIdlBindingsRuntime<Host> for VmJsWebIdlBindingsCx<'_, Host> {
  type JsValue = Value;
  type PropertyKey = PropertyKey;
  type Error = VmError;

  fn with_stack_roots<R, F>(&mut self, roots: &[Self::JsValue], f: F) -> Result<R, Self::Error>
  where
    F: FnOnce(&mut Self) -> Result<R, Self::Error>,
  {
    let base = self.cx.scope.heap().stack_root_len();
    // `push_stack_roots` ensures `roots` are treated as extra roots if growing the root stack
    // triggers GC.
    self.cx.scope.heap_mut().push_stack_roots(roots)?;
    let out = f(self);
    self.cx.scope.heap_mut().truncate_stack_roots(base);
    out
  }

  fn limits(&self) -> webidl::WebIdlLimits {
    self.state.limits
  }

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

  fn is_callable(&self, value: Self::JsValue) -> bool {
    self.cx.scope.heap().is_callable(value).unwrap_or(false)
  }

  fn is_boolean(&self, value: Self::JsValue) -> bool {
    matches!(value, Value::Bool(_))
  }

  fn is_number(&self, value: Self::JsValue) -> bool {
    matches!(value, Value::Number(_))
  }

  fn is_bigint(&self, value: Self::JsValue) -> bool {
    matches!(value, Value::BigInt(_))
  }

  fn is_string(&self, value: Self::JsValue) -> bool {
    matches!(value, Value::String(_))
  }

  fn is_string_object(&self, _value: Self::JsValue) -> bool {
    // `vm-js` does not yet model boxed String objects.
    false
  }

  fn is_symbol(&self, value: Self::JsValue) -> bool {
    matches!(value, Value::Symbol(_))
  }

  fn is_platform_object(&self, value: Self::JsValue) -> bool {
    self.state.hooks.is_platform_object(value)
  }

  fn implements_interface(&self, value: Self::JsValue, interface: webidl::InterfaceId) -> bool {
    self.state.hooks.implements_interface(value, interface)
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

  fn throw_dom_exception(&mut self, name: &str, message: &str) -> Self::Error {
    // Prefer throwing a real `DOMException` object when the constructor is installed on the global
    // object (as `WindowRealm` does). If it is missing or has been clobbered by user code, fall
    // back to an `Error`-like object with a matching `.name` so bindings still surface something
    // spec-shaped.
    match self.dom_exception_class_from_global() {
      Ok(Some(dom_exception)) => {
        return crate::js::bindings::throw_dom_exception(&mut self.cx.scope, dom_exception, name, message);
      }
      Ok(None) => {}
      Err(err) => return err,
    }
    let intr = match self.intrinsics() {
      Ok(intr) => intr,
      Err(err) => return err,
    };
    crate::js::bindings::dom_exception_vmjs::throw_dom_exception_like_error(&mut self.cx.scope, intr, name, message)
  }

  fn property_key(&mut self, name: &str) -> Result<Self::PropertyKey, Self::Error> {
    // WebIDL conversions can call `property_key("done")`/`property_key("value")` inside tight loops
    // (e.g. `sequence<T>` from an iterator). Cache the hottest iterator protocol keys to avoid
    // allocating and rooting millions of duplicate strings in a single conversion.
    match name {
      "next" => {
        if let Some(key) = self.cached_next_key {
          return Ok(key);
        }
        let s = self.cx.scope.alloc_string("next")?;
        self.cx.scope.push_root(Value::String(s))?;
        let key = PropertyKey::from_string(s);
        self.cached_next_key = Some(key);
        Ok(key)
      }
      "done" => {
        if let Some(key) = self.cached_done_key {
          return Ok(key);
        }
        let s = self.cx.scope.alloc_string("done")?;
        self.cx.scope.push_root(Value::String(s))?;
        let key = PropertyKey::from_string(s);
        self.cached_done_key = Some(key);
        Ok(key)
      }
      "value" => {
        if let Some(key) = self.cached_value_key {
          return Ok(key);
        }
        let s = self.cx.scope.alloc_string("value")?;
        self.cx.scope.push_root(Value::String(s))?;
        let key = PropertyKey::from_string(s);
        self.cached_value_key = Some(key);
        Ok(key)
      }
      _ => {
        let s = self.cx.scope.alloc_string(name)?;
        self.cx.scope.push_root(Value::String(s))?;
        Ok(PropertyKey::from_string(s))
      }
    }
  }

  fn symbol_iterator(&mut self) -> Result<Self::PropertyKey, Self::Error> {
    let intr = self.intrinsics()?;
    Ok(PropertyKey::from_symbol(intr.well_known_symbols().iterator))
  }

  fn symbol_async_iterator(&mut self) -> Result<Self::PropertyKey, Self::Error> {
    let intr = self.intrinsics()?;
    Ok(PropertyKey::from_symbol(intr.well_known_symbols().async_iterator))
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

  fn get_method(
    &mut self,
    obj: Self::JsValue,
    key: Self::PropertyKey,
  ) -> Result<Option<Self::JsValue>, Self::Error> {
    let func = self.get(obj, key)?;
    if matches!(func, Value::Undefined | Value::Null) {
      return Ok(None);
    }
    if !self.cx.scope.heap().is_callable(func)? {
      return Err(self.throw_type_error("GetMethod: property is not callable"));
    }
    Ok(Some(func))
  }

  fn get_iterator(
    &mut self,
    host: &mut Host,
    iterable: Self::JsValue,
  ) -> Result<IteratorRecord<Self::JsValue>, Self::Error> {
    let Value::Object(obj) = iterable else {
      return Err(self.throw_type_error("GetIterator: expected object"));
    };

    self.with_stack_roots(&[iterable], |rt| {
      // Minimal Array fast-path: `vm-js` does not yet expose `%Array.prototype%[@@iterator]` on the
      // intrinsic graph, but arrays should still be accepted as iterable inputs for `sequence<T>`.
      if let Ok(intr) = rt.intrinsics() {
        if rt.cx.scope.heap().object_prototype(obj)? == Some(intr.array_prototype()) {
          let length_key = rt.property_key("length")?;
          let len_value = rt.get(iterable, length_key)?;
          let len = rt.to_number(len_value)?;
          if !len.is_finite() || len < 0.0 {
            return Err(rt.throw_type_error(
              "GetIterator: array length is not a non-negative finite number",
            ));
          }
          let length = len as u32;
          return Ok(IteratorRecord {
            iterator: iterable,
            next_method: Value::Undefined,
            done: false,
            kind: IteratorRecordKind::Array {
              array: iterable,
              next_index: 0,
              length,
            },
          });
        }
      }

      let iterator_key = rt.symbol_iterator()?;
      let Some(method) = rt.get_method(iterable, iterator_key)? else {
        return Err(rt.throw_type_error("GetIterator: value is not iterable"));
      };
      rt.get_iterator_from_method(host, iterable, method)
    })
  }

  fn get_iterator_from_method(
    &mut self,
    host: &mut Host,
    iterable: Self::JsValue,
    method: Self::JsValue,
  ) -> Result<IteratorRecord<Self::JsValue>, Self::Error> {
    let iterator = self.cx.vm.call(host, &mut self.cx.scope, method, iterable, &[])?;
    if !self.is_object(iterator) {
      return Err(self.throw_type_error("Iterator method did not return an object"));
    }

    self.with_stack_roots(&[iterator], |rt| {
      let next_key = rt.property_key("next")?;
      let next = rt.get(iterator, next_key)?;
      if !rt.cx.scope.heap().is_callable(next)? {
        return Err(rt.throw_type_error("Iterator.next is not callable"));
      }
      Ok(IteratorRecord {
        iterator,
        next_method: next,
        done: false,
        kind: IteratorRecordKind::Protocol,
      })
    })
  }

  fn iterator_step_value(
    &mut self,
    host: &mut Host,
    iterator_record: &mut IteratorRecord<Self::JsValue>,
  ) -> Result<Option<Self::JsValue>, Self::Error> {
    if iterator_record.done {
      return Ok(None);
    }

    match &mut iterator_record.kind {
      IteratorRecordKind::Array {
        array,
        next_index,
        length,
      } => {
        if *next_index >= *length {
          iterator_record.done = true;
          return Ok(None);
        }
        let idx = *next_index;
        *next_index = next_index.saturating_add(1);
        self.with_stack_roots(&[*array], |rt| {
          let key_s = rt.cx.scope.alloc_string(&idx.to_string())?;
          let key = PropertyKey::from_string(key_s);
          let value = rt.get(*array, key)?;
          Ok(Some(value))
        })
      }
      IteratorRecordKind::Protocol => {
        let iterator = iterator_record.iterator;
        let next_method = iterator_record.next_method;
        let result = self
          .cx
          .vm
          .call(host, &mut self.cx.scope, next_method, iterator, &[])?;
        if !self.is_object(result) {
          return Err(self.throw_type_error("Iterator.next() did not return an object"));
        }

        self.with_stack_roots(&[result], |rt| {
          let done_key = rt.property_key("done")?;
          let done = rt.get(result, done_key)?;
          let done = rt.to_boolean(done)?;
          if done {
            iterator_record.done = true;
            return Ok(None);
          }

          let value_key = rt.property_key("value")?;
          let value = rt.get(result, value_key)?;
          Ok(Some(value))
        })
      }
    }
  }

  fn create_object(&mut self) -> Result<Self::JsValue, Self::Error> {
    use webidl::JsRuntime as _;
    let obj = self.cx.alloc_object()?;
    Ok(Value::Object(obj))
  }

  fn create_array(&mut self, len: usize) -> Result<Self::JsValue, Self::Error> {
    use webidl::JsRuntime as _;
    let obj = self.cx.alloc_array(len)?;
    Ok(Value::Object(obj))
  }

  fn create_function(
    &mut self,
    name: &str,
    length: u32,
    f: NativeHostFunction<Self, Host>,
  ) -> Result<Self::JsValue, Self::Error> {
    let call_id = if let Some(id) = self.state.native_call_id.get() {
      id
    } else {
      let id = self.cx.vm.register_native_call(dispatch_native_call::<Host>)?;
      self.state.native_call_id.set(Some(id));
      id
    };

    let intr = self.intrinsics()?;

    // Root the name across allocation of the function object.
    let name_s = self.cx.scope.alloc_string(name)?;
    self.cx.scope.push_root(Value::String(name_s))?;

    let func = self
      .cx
      .scope
      .alloc_native_function(call_id, None, name_s, length)?;
    self.cx.scope.push_root(Value::Object(func))?;
    self.cx
      .scope
      .heap_mut()
      .object_set_prototype(func, Some(intr.function_prototype()))?;

    let dispatch_ptr = self.state.alloc_dispatch_record(f as usize, 0);
    let slots = HostSlots {
      a: (self.state as *const VmJsWebIdlBindingsState<Host>) as u64,
      b: dispatch_ptr as u64,
    };
    self.cx.scope.heap_mut().object_set_host_slots(func, slots)?;

    Ok(Value::Object(func))
  }

  fn create_constructor(
    &mut self,
    name: &str,
    length: u32,
    call: NativeHostFunction<Self, Host>,
    construct: NativeHostFunction<Self, Host>,
  ) -> Result<Self::JsValue, Self::Error> {
    let call_id = if let Some(id) = self.state.native_call_id.get() {
      id
    } else {
      let id = self.cx.vm.register_native_call(dispatch_native_call::<Host>)?;
      self.state.native_call_id.set(Some(id));
      id
    };

    let construct_id = if let Some(id) = self.state.native_construct_id.get() {
      id
    } else {
      let id = self
        .cx
        .vm
        .register_native_construct(dispatch_native_construct::<Host>)?;
      self.state.native_construct_id.set(Some(id));
      id
    };

    let intr = self.intrinsics()?;

    // Root the name across allocation of the function object.
    let name_s = self.cx.scope.alloc_string(name)?;
    self.cx.scope.push_root(Value::String(name_s))?;

    let func = self
      .cx
      .scope
      .alloc_native_function(call_id, Some(construct_id), name_s, length)?;
    self.cx.scope.push_root(Value::Object(func))?;
    self.cx
      .scope
      .heap_mut()
      .object_set_prototype(func, Some(intr.function_prototype()))?;

    let dispatch_ptr = self
      .state
      .alloc_dispatch_record(call as usize, construct as usize);
    let slots = HostSlots {
      a: (self.state as *const VmJsWebIdlBindingsState<Host>) as u64,
      b: dispatch_ptr as u64,
    };
    self.cx.scope.heap_mut().object_set_host_slots(func, slots)?;

    Ok(Value::Object(func))
  }

  fn root_callback_function(&mut self, value: Self::JsValue) -> Result<CallbackHandle, Self::Error> {
    let handle = {
      let heap = self.cx.scope.heap_mut();
      CallbackHandle::from_callback_function(&*self.cx.vm, heap, value, false)?
    };
    handle.ok_or_else(|| self.throw_type_error("Callback function is null or undefined"))
  }

  fn root_callback_interface(&mut self, value: Self::JsValue) -> Result<CallbackHandle, Self::Error> {
    let handle = {
      let vm = &mut *self.cx.vm;
      let heap = self.cx.scope.heap_mut();
      CallbackHandle::from_callback_interface(vm, heap, value, false)?
    };
    handle.ok_or_else(|| self.throw_type_error("Callback interface is null or undefined"))
  }

  fn global_object(&mut self) -> Result<Self::JsValue, Self::Error> {
    self.cx.scope.push_root(Value::Object(self.state.global_object))?;
    Ok(Value::Object(self.state.global_object))
  }

  fn define_data_property_with_attrs(
    &mut self,
    obj: Self::JsValue,
    key: Self::PropertyKey,
    value: Self::JsValue,
    writable: bool,
    enumerable: bool,
    configurable: bool,
  ) -> Result<(), Self::Error> {
    let Value::Object(obj) = obj else {
      return Err(self.throw_type_error("define_data_property_with_attrs: expected object receiver"));
    };

    // Root `obj`, `key`, and `value` across `define_property`, which may allocate and GC.
    let mut scope = self.cx.scope.reborrow();
    scope.push_root(Value::Object(obj))?;
    match key {
      PropertyKey::String(s) => scope.push_root(Value::String(s))?,
      PropertyKey::Symbol(s) => scope.push_root(Value::Symbol(s))?,
    };
    scope.push_root(value)?;

    scope.define_property(
      obj,
      key,
      PropertyDescriptor {
        enumerable,
        configurable,
        kind: PropertyKind::Data { value, writable },
      },
    )
  }

  fn define_accessor_property_with_attrs(
    &mut self,
    obj: Self::JsValue,
    key: Self::PropertyKey,
    get: Self::JsValue,
    set: Self::JsValue,
    enumerable: bool,
    configurable: bool,
  ) -> Result<(), Self::Error> {
    let Value::Object(obj) = obj else {
      return Err(self.throw_type_error(
        "define_accessor_property_with_attrs: expected object receiver",
      ));
    };

    // Root `obj`, `key`, `get`, and `set` across `define_property`, which may allocate and GC.
    let mut scope = self.cx.scope.reborrow();
    scope.push_root(Value::Object(obj))?;
    match key {
      PropertyKey::String(s) => scope.push_root(Value::String(s))?,
      PropertyKey::Symbol(s) => scope.push_root(Value::Symbol(s))?,
    };
    scope.push_root(get)?;
    scope.push_root(set)?;

    scope.define_property(
      obj,
      key,
      PropertyDescriptor {
        enumerable,
        configurable,
        kind: PropertyKind::Accessor { get, set },
      },
    )
  }

  fn set_prototype(
    &mut self,
    obj: Self::JsValue,
    proto: Option<Self::JsValue>,
  ) -> Result<(), Self::Error> {
    let Value::Object(obj) = obj else {
      return Err(self.throw_type_error("set_prototype: expected object receiver"));
    };
    let proto_obj = match proto {
      None => None,
      Some(Value::Null) => None,
      Some(Value::Object(o)) => Some(o),
      Some(_) => return Err(self.throw_type_error("set_prototype: expected object or null prototype")),
    };

    let mut scope = self.cx.scope.reborrow();
    scope.push_root(Value::Object(obj))?;
    if let Some(proto_obj) = proto_obj {
      scope.push_root(Value::Object(proto_obj))?;
    }

    scope.heap_mut().object_set_prototype(obj, proto_obj)?;
    Ok(())
  }

  fn define_data_property(
    &mut self,
    obj: Self::JsValue,
    key: Self::PropertyKey,
    value: Self::JsValue,
    enumerable: bool,
  ) -> Result<(), Self::Error> {
    let Value::Object(obj) = obj else {
      return Err(self.throw_type_error("define_data_property: expected object receiver"));
    };

    // Root `obj`, `key`, and `value` across `define_property`, which may allocate and GC.
    let mut scope = self.cx.scope.reborrow();
    scope.push_root(Value::Object(obj))?;
    match key {
      PropertyKey::String(s) => scope.push_root(Value::String(s))?,
      PropertyKey::Symbol(s) => scope.push_root(Value::Symbol(s))?,
    };
    scope.push_root(value)?;

    scope.define_property(obj, key, make_data_descriptor(value, enumerable))
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

  fn with_stack_roots<R, F>(&mut self, roots: &[Self::JsValue], f: F) -> Result<R, Self::Error>
  where
    F: FnOnce(&mut Self) -> Result<R, Self::Error>,
  {
    <Self as webidl_js_runtime::JsRuntime>::with_stack_roots(self, roots, f)
  }

  fn limits(&self) -> webidl::WebIdlLimits {
    <Self as webidl_js_runtime::WebIdlJsRuntime>::limits(self)
  }

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

  fn is_callable(&self, value: Self::JsValue) -> bool {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::is_callable(self, value)
  }

  fn is_boolean(&self, value: Self::JsValue) -> bool {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::is_boolean(self, value)
  }

  fn is_number(&self, value: Self::JsValue) -> bool {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::is_number(self, value)
  }

  fn is_bigint(&self, value: Self::JsValue) -> bool {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::is_bigint(self, value)
  }

  fn is_string(&self, value: Self::JsValue) -> bool {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::is_string(self, value)
  }

  fn is_string_object(&self, value: Self::JsValue) -> bool {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::WebIdlJsRuntime>::is_string_object(self, value)
  }

  fn is_symbol(&self, value: Self::JsValue) -> bool {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::is_symbol(self, value)
  }

  fn is_platform_object(&self, value: Self::JsValue) -> bool {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::WebIdlJsRuntime>::is_platform_object(self, value)
  }

  fn implements_interface(&self, value: Self::JsValue, interface: webidl::InterfaceId) -> bool {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::WebIdlJsRuntime>::implements_interface(self, value, interface)
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

  fn throw_dom_exception(&mut self, name: &str, message: &str) -> Self::Error {
    use webidl_js_runtime::JsRuntime as _;
    let obj = match self.alloc_object_value() {
      Ok(obj) => obj,
      Err(err) => return err,
    };
    let Value::Object(_) = obj else {
      return VmError::Throw(Value::Undefined);
    };

    // Root the object while allocating the name/message strings and property keys: allocations may
    // trigger GC, and `vm-js` does not automatically trace Rust locals.
    let name_value = match <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::with_stack_roots(
      self,
      &[obj],
      |rt| rt.alloc_string(name),
    ) {
      Ok(v) => v,
      Err(err) => return err,
    };
    let message_value = match <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::with_stack_roots(
      self,
      &[obj],
      |rt| rt.alloc_string(message),
    ) {
      Ok(v) => v,
      Err(err) => return err,
    };

    let _ = <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::with_stack_roots(
      self,
      &[obj, name_value, message_value],
      |rt| {
        let name_key =
          <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::property_key_from_str(
            rt, "name",
          )?;
        let message_key =
          <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::property_key_from_str(
            rt, "message",
          )?;

        let name_key_root = match name_key {
          PropertyKey::String(s) => Value::String(s),
          PropertyKey::Symbol(s) => Value::Symbol(s),
        };
        let message_key_root = match message_key {
          PropertyKey::String(s) => Value::String(s),
          PropertyKey::Symbol(s) => Value::Symbol(s),
        };

        <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::with_stack_roots(
          rt,
          &[obj, name_value, message_value, name_key_root, message_key_root],
          |rt| {
            <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::define_data_property(
              rt,
              obj,
              name_key,
              name_value,
              false,
            )?;
            <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::define_data_property(
              rt,
              obj,
              message_key,
              message_value,
              false,
            )?;
            Ok(())
          },
        )
      },
    );

    VmError::Throw(obj)
  }

  fn property_key(&mut self, name: &str) -> Result<Self::PropertyKey, Self::Error> {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::property_key_from_str(
      self, name,
    )
  }

  fn symbol_iterator(&mut self) -> Result<Self::PropertyKey, Self::Error> {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::WebIdlJsRuntime>::symbol_iterator(self)
  }

  fn symbol_async_iterator(&mut self) -> Result<Self::PropertyKey, Self::Error> {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::WebIdlJsRuntime>::symbol_async_iterator(self)
  }

  fn get(&mut self, obj: Self::JsValue, key: Self::PropertyKey) -> Result<Self::JsValue, Self::Error> {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::get(self, obj, key)
  }

  fn get_method(
    &mut self,
    obj: Self::JsValue,
    key: Self::PropertyKey,
  ) -> Result<Option<Self::JsValue>, Self::Error> {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::get_method(self, obj, key)
  }

  fn get_iterator(
    &mut self,
    host: &mut Host,
    iterable: Self::JsValue,
  ) -> Result<IteratorRecord<Self::JsValue>, Self::Error> {
    let iterator_key =
      <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::WebIdlJsRuntime>::symbol_iterator(self)?;
    let Some(method) =
      <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::get_method(self, iterable, iterator_key)?
    else {
      return Err(<webidl_js_runtime::VmJsRuntime as webidl_js_runtime::WebIdlJsRuntime>::throw_type_error(
        self,
        "GetIterator: value is not iterable",
      ));
    };
    self.get_iterator_from_method(host, iterable, method)
  }

  fn get_iterator_from_method(
    &mut self,
    _host: &mut Host,
    iterable: Self::JsValue,
    method: Self::JsValue,
  ) -> Result<IteratorRecord<Self::JsValue>, Self::Error> {
    let record =
      <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::get_iterator_from_method(
        self, iterable, method,
      )?;
    Ok(IteratorRecord {
      iterator: record.iterator,
      next_method: record.next_method,
      done: record.done,
      kind: IteratorRecordKind::Protocol,
    })
  }

  fn iterator_step_value(
    &mut self,
    _host: &mut Host,
    iterator_record: &mut IteratorRecord<Self::JsValue>,
  ) -> Result<Option<Self::JsValue>, Self::Error> {
    // Bridge through the legacy iterator record type.
    let mut record = webidl_js_runtime::IteratorRecord {
      iterator: iterator_record.iterator,
      next_method: iterator_record.next_method,
      done: iterator_record.done,
    };
    let out =
      <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::iterator_step_value(
        self, &mut record,
      )?;
    iterator_record.done = record.done;
    Ok(out)
  }

  fn create_object(&mut self) -> Result<Self::JsValue, Self::Error> {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::WebIdlBindingsRuntime<Host>>::create_object(
      self,
    )
  }

  fn create_array(&mut self, len: usize) -> Result<Self::JsValue, Self::Error> {
    let obj = {
      let mut scope = self.heap_mut().scope();
      scope.alloc_array(len)?
    };
    Ok(Value::Object(obj))
  }

  fn create_function(
    &mut self,
    name: &str,
    length: u32,
    f: NativeHostFunction<Self, Host>,
  ) -> Result<Self::JsValue, Self::Error> {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::WebIdlBindingsRuntime<Host>>::create_function(
      self, name, length, f,
    )
  }

  fn create_constructor(
    &mut self,
    name: &str,
    length: u32,
    _call: NativeHostFunction<Self, Host>,
    construct: NativeHostFunction<Self, Host>,
  ) -> Result<Self::JsValue, Self::Error> {
    // The legacy heap-only runtime does not model `[[Construct]]` separately. Preserve existing
    // behavior by treating constructors as plain callables and ignoring the "illegal call" stub.
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::WebIdlBindingsRuntime<Host>>::create_function(
      self, name, length, construct,
    )
  }

  fn root_callback_function(&mut self, value: Self::JsValue) -> Result<CallbackHandle, Self::Error> {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::WebIdlBindingsRuntime<Host>>::root_callback_function(
      self, value,
    )
  }

  fn root_callback_interface(&mut self, value: Self::JsValue) -> Result<CallbackHandle, Self::Error> {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::WebIdlBindingsRuntime<Host>>::root_callback_interface(
      self, value,
    )
  }

  fn global_object(&mut self) -> Result<Self::JsValue, Self::Error> {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::WebIdlBindingsRuntime<Host>>::global_object(self)
  }

  fn define_data_property_with_attrs(
    &mut self,
    obj: Self::JsValue,
    key: Self::PropertyKey,
    value: Self::JsValue,
    writable: bool,
    enumerable: bool,
    configurable: bool,
  ) -> Result<(), Self::Error> {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::WebIdlBindingsRuntime<Host>>::define_data_property_with_attrs(
      self,
      obj,
      key,
      value,
      writable,
      enumerable,
      configurable,
    )
  }

  fn define_accessor_property_with_attrs(
    &mut self,
    obj: Self::JsValue,
    key: Self::PropertyKey,
    get: Self::JsValue,
    set: Self::JsValue,
    enumerable: bool,
    configurable: bool,
  ) -> Result<(), Self::Error> {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::WebIdlBindingsRuntime<Host>>::define_accessor_property_with_attrs(
      self,
      obj,
      key,
      get,
      set,
      enumerable,
      configurable,
    )
  }

  fn set_prototype(
    &mut self,
    obj: Self::JsValue,
    proto: Option<Self::JsValue>,
  ) -> Result<(), Self::Error> {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::WebIdlBindingsRuntime<Host>>::set_prototype(
      self, obj, proto,
    )
  }

  fn define_data_property(
    &mut self,
    obj: Self::JsValue,
    key: Self::PropertyKey,
    value: Self::JsValue,
    enumerable: bool,
  ) -> Result<(), Self::Error> {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::define_data_property(
      self, obj, key, value, enumerable,
    )
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
  use crate::js::bindings::DomExceptionClassVmJs;
  use vm_js::{Heap, HeapLimits, JsRuntime as VmJsRuntime, VmOptions};
  use webidl::{InterfaceId, JsRuntime as _, WebIdlHooks, WebIdlLimits};

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

      let func = cx.create_function("add", 2, add)?;
      let global = cx.global_object()?;
      cx.define_data_property_str(global, "add", func, true)?;
    }

    let mut host = TestHost::default();
    let out = runtime.exec_script_with_host(&mut host, "add(1, 2)")?;
    assert!(matches!(out, Value::Number(n) if (n - 3.0).abs() < f64::EPSILON));
    Ok(())
  }

  #[test]
  fn vmjs_bindings_runtime_throw_dom_exception_sets_name_and_message() -> Result<(), VmError> {
    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(HeapLimits::new(16 * 1024 * 1024, 8 * 1024 * 1024));
    let mut runtime = VmJsRuntime::new(vm, heap)?;

    let state = Box::new(VmJsWebIdlBindingsState::<TestHost>::new(
      runtime.realm().global_object(),
      WebIdlLimits::default(),
      Box::new(NoHooks),
    ));

    let (vm, heap, realm) = webidl_vm_js::split_js_runtime(&mut runtime);

    // Before `DOMException` is installed, the bindings runtime should still throw an Error-shaped
    // object with the requested name/message.
    {
      let mut cx = VmJsWebIdlBindingsCx::new(vm, heap, &state);

      let err = cx.throw_dom_exception("SyntaxError", "m");
      let VmError::Throw(thrown) = err else {
        return Err(VmError::TypeError("expected VmError::Throw"));
      };
      // Root the thrown value before any further allocations.
      cx.cx.scope.push_root(thrown)?;
      let Value::Object(obj) = thrown else {
        return Err(VmError::TypeError("expected thrown value to be an object"));
      };
      // Root the thrown object while allocating strings/property keys below.
      cx.cx.scope.push_root(Value::Object(obj))?;

      let proto = cx.cx.scope.heap().object_prototype(obj)?;
      assert_eq!(proto, Some(realm.intrinsics().error_prototype()));

      let name_key_s = cx.cx.scope.alloc_string("name")?;
      cx.cx.scope.push_root(Value::String(name_key_s))?;
      let name_key = PropertyKey::from_string(name_key_s);
      let name_value = cx
        .cx
        .scope
        .heap()
        .object_get_own_data_property_value(obj, &name_key)?
        .unwrap_or(Value::Undefined);
      assert_eq!(cx.js_string_to_rust_string(name_value)?, "SyntaxError");

      let message_key_s = cx.cx.scope.alloc_string("message")?;
      cx.cx.scope.push_root(Value::String(message_key_s))?;
      let message_key = PropertyKey::from_string(message_key_s);
      let message_value = cx
        .cx
        .scope
        .heap()
        .object_get_own_data_property_value(obj, &message_key)?
        .unwrap_or(Value::Undefined);
      assert_eq!(cx.js_string_to_rust_string(message_value)?, "m");
    }

    // Once `DOMException` is installed on the global object, `throw_dom_exception` should create a
    // real DOMException instance (with `.prototype` on its prototype chain).
    let dom_exception = {
      let mut scope = heap.scope();
      DomExceptionClassVmJs::install(vm, &mut scope, realm)?
    };

    {
      let mut cx = VmJsWebIdlBindingsCx::new(vm, heap, &state);

      let err = cx.throw_dom_exception("SyntaxError", "m");
      let VmError::Throw(thrown) = err else {
        return Err(VmError::TypeError("expected VmError::Throw"));
      };
      // Root the thrown value before any further allocations.
      cx.cx.scope.push_root(thrown)?;
      let Value::Object(obj) = thrown else {
        return Err(VmError::TypeError("expected thrown value to be an object"));
      };
      // Root the thrown object while allocating strings/property keys below.
      cx.cx.scope.push_root(Value::Object(obj))?;

      let proto = cx.cx.scope.heap().object_prototype(obj)?;
      assert_eq!(proto, Some(dom_exception.prototype));

      let name_key_s = cx.cx.scope.alloc_string("name")?;
      cx.cx.scope.push_root(Value::String(name_key_s))?;
      let name_key = PropertyKey::from_string(name_key_s);
      let name_value = cx
        .cx
        .scope
        .heap()
        .object_get_own_data_property_value(obj, &name_key)?
        .unwrap_or(Value::Undefined);
      assert_eq!(cx.js_string_to_rust_string(name_value)?, "SyntaxError");

      let message_key_s = cx.cx.scope.alloc_string("message")?;
      cx.cx.scope.push_root(Value::String(message_key_s))?;
      let message_key = PropertyKey::from_string(message_key_s);
      let message_value = cx
        .cx
        .scope
        .heap()
        .object_get_own_data_property_value(obj, &message_key)?
        .unwrap_or(Value::Undefined);
      assert_eq!(cx.js_string_to_rust_string(message_value)?, "m");
    }

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
