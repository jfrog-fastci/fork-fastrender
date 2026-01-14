use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::marker::PhantomData;
use vm_js::{
  GcObject, HostSlots, Intrinsics, PropertyDescriptor, PropertyKey, PropertyKind, Scope, Value, Vm,
  VmError, VmHost, VmHostHooks,
};

/// Fallible `Box::new` that returns `VmError::OutOfMemory` instead of aborting the process.
///
/// WebIDL binding installation is reachable from untrusted JS (e.g. realm creation) and uses
/// fallible `Result<_, VmError>` APIs. Rust's default `Box::new` aborts the process on allocator
/// OOM, so use a manual allocation.
#[inline]
fn box_try_new_vm<T>(value: T) -> Result<Box<T>, VmError> {
  // `Box::new` does not allocate for ZSTs, so it cannot fail with OOM.
  if std::mem::size_of::<T>() == 0 {
    return Ok(Box::new(value));
  }

  let layout = std::alloc::Layout::new::<T>();
  // SAFETY: `alloc` returns either a suitably aligned block of memory for `T` or null on OOM. We
  // write `value` into it and transfer ownership to `Box`.
  unsafe {
    let ptr = std::alloc::alloc(layout) as *mut T;
    if ptr.is_null() {
      return Err(VmError::OutOfMemory);
    }
    ptr.write(value);
    Ok(Box::from_raw(ptr))
  }
}

use webidl::WebIdlHooks;
use webidl_vm_js::bindings_runtime::DataPropertyAttributes;
use webidl_vm_js::CallbackHandle;
use webidl_vm_js::VmJsHostHooksPayload;

#[derive(Default)]
struct NoopVmHostHooks;

impl VmHostHooks for NoopVmHostHooks {
  fn host_enqueue_promise_job(&mut self, _job: vm_js::Job, _realm: Option<vm_js::RealmId>) {}
}
#[derive(Debug, Clone, Copy)]
pub struct JsOwnPropertyDescriptor {
  pub enumerable: bool,
}

/// Iterator state used by WebIDL `sequence<T>` / `FrozenArray<T>` conversions.
///
/// This mirrors the ECMAScript `IteratorRecord` shape: iterator object + cached `next` method +
/// `done` flag.
///
/// For a real `vm-js` realm runtime ([`VmJsWebIdlBindingsCx`]), we delegate to the canonical
/// `vm_js::iterator` module (which includes an Array fast-path while
/// `%Array.prototype%[@@iterator]` is not implemented).
///
/// For the legacy heap-only runtime (`vendor/ecma-rs/webidl-runtime`) we keep a small in-tree iterator
/// record implementation so WebIDL `sequence<T>` conversions can still accept arrays.
#[derive(Debug, Clone, Copy)]
pub struct IteratorRecord<V: Copy> {
  pub iterator: V,
  pub next_method: V,
  pub done: bool,
  kind: IteratorRecordKind<V>,
}

#[derive(Debug, Clone, Copy)]
enum IteratorRecordKind<V: Copy> {
  VmJs(vm_js::iterator::IteratorRecord),
  Protocol,
  Array {
    array: V,
    next_index: u32,
    length: u32,
  },
}

/// A host function callback used by generated WebIDL bindings.
///
/// This matches the calling convention used by the legacy heap-only runtime, but is implemented
/// here so new bindings can target the canonical `vendor/ecma-rs/webidl` + `vendor/ecma-rs/webidl-vm-js`
/// stack.
pub type NativeHostFunction<R, Host> = fn(
  rt: &mut R,
  host: &mut Host,
  this: <R as WebIdlBindingsRuntime<Host>>::JsValue,
  args: &[<R as WebIdlBindingsRuntime<Host>>::JsValue],
) -> Result<
  <R as WebIdlBindingsRuntime<Host>>::JsValue,
  <R as WebIdlBindingsRuntime<Host>>::Error,
>;

/// Host-facing runtime API used by generated WebIDL bindings.
///
/// This trait is intentionally narrow: it exists to let generated glue install JS-visible
/// constructors/prototypes and invoke host-defined method bodies without depending on a specific
/// JS runtime implementation.
///
/// Note: the current generated bindings backend targets
/// `webidl_js_runtime::WebIdlBindingsRuntime` for spec-shaped conversions and overload resolution.
/// This trait remains as the realm-based (`vm-js`) adapter surface.
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

  /// ECMAScript abstract operation `ToObject ( argument )`.
  ///
  /// Implementations must throw a `TypeError` when `value` is `null` or `undefined`.
  fn to_object(&mut self, value: Self::JsValue) -> Result<Self::JsValue, Self::Error>;

  fn to_boolean(&mut self, value: Self::JsValue) -> Result<bool, Self::Error>;
  fn to_number(&mut self, host: &mut Host, value: Self::JsValue) -> Result<f64, Self::Error>;
  fn to_string(
    &mut self,
    host: &mut Host,
    value: Self::JsValue,
  ) -> Result<Self::JsValue, Self::Error>;

  fn throw_type_error(&mut self, message: &str) -> Self::Error;
  fn throw_range_error(&mut self, message: &str) -> Self::Error;
  fn throw_dom_exception(&mut self, name: &str, message: &str) -> Self::Error;

  fn property_key(&mut self, name: &str) -> Result<Self::PropertyKey, Self::Error>;

  /// Return the property key for `%Symbol.iterator%` in the active realm.
  fn symbol_iterator(&mut self) -> Result<Self::PropertyKey, Self::Error>;

  /// Return the property key for `%Symbol.asyncIterator%` in the active realm.
  fn symbol_async_iterator(&mut self) -> Result<Self::PropertyKey, Self::Error>;

  fn get(
    &mut self,
    host: &mut Host,
    obj: Self::JsValue,
    key: Self::PropertyKey,
  ) -> Result<Self::JsValue, Self::Error>;

  /// Returns the receiver's own property keys (both String and Symbol keys).
  fn own_property_keys(
    &mut self,
    obj: Self::JsValue,
  ) -> Result<Vec<Self::PropertyKey>, Self::Error>;

  /// Returns an own property descriptor for `key` if present.
  fn get_own_property(
    &mut self,
    obj: Self::JsValue,
    key: Self::PropertyKey,
  ) -> Result<Option<JsOwnPropertyDescriptor>, Self::Error>;

  /// Returns true if `key` is a Symbol.
  fn property_key_is_symbol(&self, key: Self::PropertyKey) -> bool;

  /// Converts a property key into a JS string value.
  ///
  /// Per ECMAScript `ToString`, this must throw a TypeError if `key` is a Symbol.
  fn property_key_to_js_string(
    &mut self,
    key: Self::PropertyKey,
  ) -> Result<Self::JsValue, Self::Error>;

  /// Returns true if `value` is a JavaScript `Array` object.
  ///
  /// `vm-js` does not yet expose `%Array.prototype%[@@iterator]`, so bindings use an Array
  /// fast-path when converting `sequence<T>`. Union conversions need to detect arrays during
  /// member-type selection so `sequence<T>` unions work with arrays.
  fn is_array(&mut self, _value: Self::JsValue) -> Result<bool, Self::Error> {
    Ok(false)
  }

  /// ECMAScript abstract operation `GetMethod ( V, P )`.
  fn get_method(
    &mut self,
    host: &mut Host,
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

  /// ECMAScript abstract operation `Call ( F, V, argumentsList )`.
  fn call(
    &mut self,
    host: &mut Host,
    callee: Self::JsValue,
    this: Self::JsValue,
    args: &[Self::JsValue],
  ) -> Result<Self::JsValue, Self::Error>;

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
  fn root_callback_function(
    &mut self,
    _value: Self::JsValue,
  ) -> Result<CallbackHandle, Self::Error> {
    Err(self.throw_type_error("Callback functions are not supported by this runtime"))
  }

  /// Root and return a WebIDL callback interface handle.
  ///
  /// Callback interfaces accept callable functions or objects with a callable `handleEvent` method.
  fn root_callback_interface(
    &mut self,
    _value: Self::JsValue,
  ) -> Result<CallbackHandle, Self::Error> {
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
    attrs: DataPropertyAttributes,
  ) -> Result<(), Self::Error> {
    self.define_data_property_with_attrs(
      obj,
      key,
      value,
      attrs.writable,
      attrs.enumerable,
      attrs.configurable,
    )
  }

  fn define_data_property_str(
    &mut self,
    obj: Self::JsValue,
    name: &str,
    value: Self::JsValue,
    attrs: DataPropertyAttributes,
  ) -> Result<(), Self::Error> {
    self.with_stack_roots(&[obj, value], |rt| {
      let key = rt.property_key(name)?;
      rt.define_data_property(obj, key, value, attrs)
    })
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
    self.define_data_property_str(
      obj,
      name,
      value,
      DataPropertyAttributes::new(writable, enumerable, configurable),
    )
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
    self.with_stack_roots(&[obj, get, set], |rt| {
      let key = rt.property_key(name)?;
      rt.define_accessor_property_with_attrs(obj, key, get, set, enumerable, configurable)
    })
  }

  /// Defines a WebIDL operation method property.
  ///
  /// This follows the Web IDL JavaScript binding:
  /// - writable: true
  /// - configurable: true
  /// - enumerable: false
  fn define_method(
    &mut self,
    obj: Self::JsValue,
    name: &str,
    func: Self::JsValue,
  ) -> Result<(), Self::Error> {
    self.define_data_property_str(obj, name, func, DataPropertyAttributes::METHOD)
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
    self.define_data_property_str(obj, name, value, DataPropertyAttributes::CONST)
  }

  /// Defines an interface constructor, wiring `.prototype` and `prototype.constructor`.
  ///
  /// The property attributes are chosen to match WebIDL's requirements for interface objects and
  /// prototype objects:
  /// - `global[name]`: writable + configurable, non-enumerable
  /// - `ctor.prototype`: non-writable, non-enumerable, non-configurable
  /// - `proto.constructor`: non-writable, non-enumerable, non-configurable
  fn define_constructor(
    &mut self,
    global: Self::JsValue,
    name: &str,
    ctor: Self::JsValue,
    proto: Self::JsValue,
  ) -> Result<(), Self::Error> {
    self.define_data_property_str(global, name, ctor, DataPropertyAttributes::CONSTRUCTOR)?;
    self.define_data_property_str(
      ctor,
      "prototype",
      proto,
      DataPropertyAttributes::CONSTRUCTOR_PROTOTYPE,
    )?;
    self.define_data_property_str(
      proto,
      "constructor",
      ctor,
      DataPropertyAttributes::PROTOTYPE_CONSTRUCTOR,
    )?;
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
  constructor_default_protos: RefCell<HashMap<GcObject, GcObject>>,
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
      constructor_default_protos: RefCell::new(HashMap::new()),
      dispatch_records: RefCell::new(Vec::new()),
      _phantom: PhantomData,
    }
  }

  fn alloc_dispatch_record(
    &self,
    call: usize,
    construct: usize,
  ) -> Result<*const NativeDispatchRecord, VmError> {
    let mut records = self.dispatch_records.borrow_mut();
    records.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
    let record = box_try_new_vm(NativeDispatchRecord { call, construct })?;
    let ptr: *const NativeDispatchRecord = record.as_ref();
    // SAFETY: boxed value has a stable address even if `records` reallocates.
    records.push(record);
    Ok(ptr)
  }
}

/// Canonical WebIDL bindings runtime adapter for a real `vm-js` realm.
///
/// This is the preferred runtime for generated bindings: it installs real `vm-js` function objects
/// onto a realm global and performs conversions using `webidl` + [`webidl_vm_js::VmJsWebIdlCx`].
pub struct VmJsWebIdlBindingsCx<'a, Host> {
  state: &'a VmJsWebIdlBindingsState<Host>,
  cx: webidl_vm_js::VmJsWebIdlCx<'a>,
  vm_host_hooks: Option<&'a mut dyn VmHostHooks>,
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
      vm_host_hooks: None,
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
    let cx =
      webidl_vm_js::VmJsWebIdlCx::new_in_scope(vm, scope, state.limits, state.hooks.as_ref());
    Self {
      state,
      cx,
      vm_host_hooks: None,
      cached_next_key: None,
      cached_done_key: None,
      cached_value_key: None,
    }
  }

  pub fn from_native_call(
    vm: &'a mut Vm,
    scope: &'a mut Scope<'_>,
    hooks: &'a mut dyn VmHostHooks,
    state: &'a VmJsWebIdlBindingsState<Host>,
  ) -> Self {
    let cx =
      webidl_vm_js::VmJsWebIdlCx::new_in_scope(vm, scope, state.limits, state.hooks.as_ref());
    Self {
      state,
      cx,
      vm_host_hooks: Some(hooks),
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

fn make_data_descriptor(value: Value, attrs: DataPropertyAttributes) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: attrs.enumerable,
    configurable: attrs.configurable,
    kind: PropertyKind::Data {
      value,
      writable: attrs.writable,
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

const WEBIDL_BINDINGS_HOST_CONTEXT_TYPE_MISMATCH: &str =
  "WebIDL bindings host context type mismatch for native call";

fn host_ptr_from_vm_host_or_hooks<Host: 'static>(
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
) -> Result<*mut Host, VmError> {
  // Prefer the explicit `VmHost` argument when it contains the host. This avoids creating two
  // aliasing `&mut Host` references if the embedding also stores a pointer to the same host in the
  // hooks payload.
  if let Some(host) = host.as_any_mut().downcast_mut::<Host>() {
    return Ok(host as *mut Host);
  }

  let Some(any) = hooks.as_any_mut() else {
    return Err(VmError::TypeError(
      WEBIDL_BINDINGS_HOST_CONTEXT_TYPE_MISMATCH,
    ));
  };

  // `Any::downcast_mut` returns a mutable borrow of `any`. We want to:
  // - try multiple recovery strategies, and
  // - return a raw pointer so the borrow of `hooks` ends before `VmJsWebIdlBindingsCx` stores a
  //   `&mut dyn VmHostHooks` for nested JS calls.
  //
  // Use a raw pointer to avoid borrow checker issues while downcasting.
  let any_ptr: *mut dyn std::any::Any = any;
  // SAFETY: `any_ptr` is derived from `hooks.as_any_mut()` and is only used within this function.
  unsafe {
    let Some(payload) = (&mut *any_ptr).downcast_mut::<VmJsHostHooksPayload>() else {
      return Err(VmError::TypeError(
        WEBIDL_BINDINGS_HOST_CONTEXT_TYPE_MISMATCH,
      ));
    };

    // FastRender standard: prefer the explicit embedder state pointer.
    if let Some(host) = payload.embedder_state_mut::<Host>() {
      return Ok(host as *mut Host);
    }

    // Fallback: recover the `VmHost` pointer stored in the payload, then downcast.
    if let Some(vm_host) = payload.vm_host_mut() {
      if let Some(host) = vm_host.as_any_mut().downcast_mut::<Host>() {
        return Ok(host as *mut Host);
      }
    }
  }

  Err(VmError::TypeError(
    WEBIDL_BINDINGS_HOST_CONTEXT_TYPE_MISMATCH,
  ))
}

fn dispatch_native_call<Host: 'static>(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
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

  // WebIDL conversions like `ToNumber` can call back into JS (e.g. `valueOf()`), so nested calls
  // must observe the real embedder host context + hooks.
  //
  // Soundness: we downcast `host` to `&mut Host` and pass it to the host function body. The
  // `VmJsWebIdlBindingsCx` does **not** store a `&mut dyn VmHost`; instead JS-calling runtime
  // methods (`to_number`, `to_string`, `get`, `get_method`) take `&mut Host` explicitly and reborrow
  // it for the duration of the nested JS call. This avoids having two aliasing `&mut` references to
  // the same host object live at once.
  let host_ptr = host_ptr_from_vm_host_or_hooks::<Host>(host, hooks)?;
  let mut rt = VmJsWebIdlBindingsCx::from_native_call(vm, scope, hooks, state);
  // SAFETY: `host_ptr` is derived from either the explicit `VmHost` argument or from a
  // `VmJsHostHooksPayload` stored behind `hooks.as_any_mut()`. The embedding is responsible for
  // ensuring the recovered pointer is valid for the duration of this native call.
  let host: &mut Host = unsafe { &mut *host_ptr };

  // SAFETY: function pointer lifetimes are erased; we rehydrate it at the call site.
  let f: NativeHostFunction<VmJsWebIdlBindingsCx<'_, Host>, Host> =
    unsafe { std::mem::transmute(dispatch.call) };

  f(&mut rt, host, this, args)
}

fn dispatch_native_construct<Host: 'static>(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  args: &[Value],
  new_target: Value,
) -> Result<Value, VmError> {
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
    let intr = vm.intrinsics().ok_or(VmError::InvariantViolation(
      "vm-js intrinsics not installed; expected an initialized Realm",
    ))?;
    return Err(vm_js::throw_type_error(
      scope,
      intr,
      ILLEGAL_CONSTRUCTOR_ERROR,
    ));
  }

  let default_proto = state
    .constructor_default_protos
    .borrow()
    .get(&callee)
    .copied()
    .ok_or(VmError::InvariantViolation(
      "WebIDL constructor missing default prototype mapping",
    ))?;

  // Root the inputs across `GetPrototypeFromConstructor` and wrapper allocation.
  scope.push_root(new_target)?;
  scope.push_root(Value::Object(default_proto))?;

  // WebIDL constructors use `GetPrototypeFromConstructor(newTarget, defaultProto)` so that
  // subclassing can override the wrapper's `[[Prototype]]`.
  let proto = vm_js::get_prototype_from_constructor(vm, scope, new_target, default_proto)?;
  scope.push_root(Value::Object(proto))?;

  // Allocate a fresh wrapper and brand it by setting the prototype.
  let obj = scope.alloc_object_with_prototype(Some(proto))?;
  scope.push_root(Value::Object(obj))?;

  // WebIDL conversions like `ToNumber` can call back into JS (e.g. `valueOf()`), so nested calls
  // must observe the real embedder host context + hooks.
  //
  // Soundness: we downcast `host` to `&mut Host` and pass it to the host function body. The
  // `VmJsWebIdlBindingsCx` does **not** store a `&mut dyn VmHost`; instead JS-calling runtime
  // methods (`to_number`, `to_string`, `get`, `get_method`) take `&mut Host` explicitly and reborrow
  // it for the duration of the nested JS call. This avoids having two aliasing `&mut` references to
  // the same host object live at once.
  let host_ptr = host_ptr_from_vm_host_or_hooks::<Host>(host, hooks)?;
  let mut rt = VmJsWebIdlBindingsCx::from_native_call(vm, scope, hooks, state);
  // SAFETY: see `dispatch_native_call`.
  let host: &mut Host = unsafe { &mut *host_ptr };

  // SAFETY: function pointer lifetimes are erased; we rehydrate it at the call site.
  let f: NativeHostFunction<VmJsWebIdlBindingsCx<'_, Host>, Host> =
    unsafe { std::mem::transmute(dispatch.construct) };

  // Constructors initialize the pre-allocated wrapper (`this`). The return value is ignored and
  // the wrapper is returned to JS.
  let _ = f(&mut rt, host, Value::Object(obj), args)?;
  Ok(Value::Object(obj))
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
    let js = self.cx.scope.heap().get_string(s)?;
    if js.len_code_units() > self.limits().max_string_code_units {
      return Err(self.throw_range_error("string exceeds maximum length"));
    }
    Ok(js.to_utf8_lossy())
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

  fn is_string_object(&self, value: Self::JsValue) -> bool {
    let Value::Object(obj) = value else {
      return false;
    };
    let intr = match self.intrinsics() {
      Ok(intr) => intr,
      Err(_) => return false,
    };
    let string_proto = intr.string_prototype();
    let mut current = match self.cx.scope.heap().object_prototype(obj) {
      Ok(v) => v,
      Err(_) => return false,
    };
    while let Some(proto) = current {
      if proto == string_proto {
        return true;
      }
      current = match self.cx.scope.heap().object_prototype(proto) {
        Ok(v) => v,
        Err(_) => return false,
      };
    }
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

  fn to_object(&mut self, value: Self::JsValue) -> Result<Self::JsValue, Self::Error> {
    match value {
      Value::Object(_) => Ok(value),
      Value::Undefined | Value::Null => {
        Err(self.throw_type_error("ToObject: cannot convert null or undefined to object"))
      }
      other => {
        let intr = self.intrinsics()?;
        let object_ctor = Value::Object(intr.object_constructor());
        // Root the input value + callee across the internal boxing call (which can allocate).
        let boxed = self.with_stack_roots(&[other, object_ctor], |rt| {
          rt.cx
            .vm
            .call_without_host(&mut rt.cx.scope, object_ctor, Value::Undefined, &[other])
        })?;
        if !self.is_object(boxed) {
          return Err(self.throw_type_error("ToObject internal boxing returned non-object"));
        }
        // Keep the boxed wrapper alive for the lifetime of this conversion context.
        self.cx.scope.push_root(boxed)?;
        Ok(boxed)
      }
    }
  }

  fn to_boolean(&mut self, value: Self::JsValue) -> Result<bool, Self::Error> {
    use webidl::JsRuntime as _;
    self.cx.to_boolean(value)
  }

  fn to_number(&mut self, host: &mut Host, value: Self::JsValue) -> Result<f64, Self::Error> {
    if let Some(hooks) = self.vm_host_hooks.as_deref_mut() {
      return self
        .cx
        .scope
        .to_number(&mut *self.cx.vm, host, hooks, value);
    }
    // No host hooks are available; fall back to dummy-host conversions.
    use webidl::JsRuntime as _;
    self.cx.to_number(value)
  }

  fn to_string(
    &mut self,
    host: &mut Host,
    value: Self::JsValue,
  ) -> Result<Self::JsValue, Self::Error> {
    if let Some(hooks) = self.vm_host_hooks.as_deref_mut() {
      let s = self
        .cx
        .scope
        .to_string(&mut *self.cx.vm, host, hooks, value)?;
      let value = Value::String(s);
      self.cx.scope.push_root(value)?;
      return Ok(value);
    }
    // No host hooks are available; fall back to dummy-host conversions.
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
        return crate::js::bindings::throw_dom_exception(
          &mut self.cx.scope,
          dom_exception,
          name,
          message,
        );
      }
      Ok(None) => {}
      Err(err) => return err,
    }
    let intr = match self.intrinsics() {
      Ok(intr) => intr,
      Err(err) => return err,
    };
    crate::js::bindings::dom_exception_vmjs::throw_dom_exception_like_error(
      &mut self.cx.scope,
      intr,
      name,
      message,
    )
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
    Ok(PropertyKey::from_symbol(
      intr.well_known_symbols().async_iterator,
    ))
  }

  fn get(
    &mut self,
    host: &mut Host,
    obj: Self::JsValue,
    key: Self::PropertyKey,
  ) -> Result<Self::JsValue, Self::Error> {
    let Value::Object(obj) = obj else {
      return Err(self.throw_type_error("get: expected object receiver"));
    };
    // Root the receiver + key while running `[[Get]]`: rooting can allocate/GC, so treat both as
    // live simultaneously.
    let key_root = match key {
      PropertyKey::String(s) => Value::String(s),
      PropertyKey::Symbol(s) => Value::Symbol(s),
    };
    self.cx.scope.push_roots(&[Value::Object(obj), key_root])?;

    let value = if let Some(hooks) = self.vm_host_hooks.as_deref_mut() {
      self.cx.scope.ordinary_get_with_host_and_hooks(
        &mut *self.cx.vm,
        host,
        hooks,
        obj,
        key,
        Value::Object(obj),
      )?
    } else {
      // Fallback to `Vm::get` when host hooks are unavailable.
      self.cx.vm.get(&mut self.cx.scope, obj, key)?
    };
    self.cx.scope.push_root(value)?;
    Ok(value)
  }

  fn own_property_keys(
    &mut self,
    obj: Self::JsValue,
  ) -> Result<Vec<Self::PropertyKey>, Self::Error> {
    let Value::Object(obj) = obj else {
      return Err(self.throw_type_error("own_property_keys: expected object receiver"));
    };
    // Root `obj` across the `[[OwnPropertyKeys]]` operation: string objects/typed arrays may
    // allocate new index-key strings while building the key list.
    self.with_stack_roots(&[Value::Object(obj)], |rt| {
      rt.cx.scope.ordinary_own_property_keys(obj)
    })
  }

  fn get_own_property(
    &mut self,
    obj: Self::JsValue,
    key: Self::PropertyKey,
  ) -> Result<Option<JsOwnPropertyDescriptor>, Self::Error> {
    let Value::Object(obj) = obj else {
      return Err(self.throw_type_error("get_own_property: expected object receiver"));
    };
    let key_root = match key {
      PropertyKey::String(s) => Value::String(s),
      PropertyKey::Symbol(s) => Value::Symbol(s),
    };
    self.with_stack_roots(&[Value::Object(obj), key_root], |rt| {
      let desc = rt.cx.scope.ordinary_get_own_property(obj, key)?;
      Ok(desc.map(|d| JsOwnPropertyDescriptor {
        enumerable: d.enumerable,
      }))
    })
  }

  fn property_key_is_symbol(&self, key: Self::PropertyKey) -> bool {
    matches!(key, PropertyKey::Symbol(_))
  }

  fn property_key_to_js_string(
    &mut self,
    key: Self::PropertyKey,
  ) -> Result<Self::JsValue, Self::Error> {
    match key {
      PropertyKey::String(s) => Ok(Value::String(s)),
      PropertyKey::Symbol(_) => {
        Err(self.throw_type_error("Cannot convert a Symbol value to a string"))
      }
    }
  }

  fn is_array(&mut self, value: Self::JsValue) -> Result<bool, Self::Error> {
    let Value::Object(obj) = value else {
      return Ok(false);
    };
    Ok(self.cx.scope.heap().object_is_array(obj)?)
  }

  fn get_method(
    &mut self,
    host: &mut Host,
    obj: Self::JsValue,
    key: Self::PropertyKey,
  ) -> Result<Option<Self::JsValue>, Self::Error> {
    let func = self.get(host, obj, key)?;
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
    // Minimal Array fast-path: `vm-js` does not currently expose `%Array.prototype%[@@iterator]` on
    // the intrinsic graph, so arrays need special handling for WebIDL `sequence<T>` conversions.
    //
    // We intentionally keep this fast-path even when host hooks are available: `GetIterator` for
    // arrays should not depend on a prototype-chain `@@iterator` lookup (which `vm-js` does not yet
    // implement).
    if let Value::Object(obj) = iterable {
      if self.cx.scope.heap().object_is_array(obj)? {
        return self.with_stack_roots::<IteratorRecord<Self::JsValue>, _>(&[iterable], |rt| {
          let length_key = rt.property_key("length")?;
          let len_value = rt.get(host, iterable, length_key)?;
          let len = rt.to_number(host, len_value)?;
          if !len.is_finite() || len < 0.0 {
            return Err(
              rt.throw_type_error("GetIterator: array length is not a non-negative finite number"),
            );
          }
          let length = len as u32;
          Ok(IteratorRecord {
            iterator: iterable,
            next_method: Value::Undefined,
            done: false,
            kind: IteratorRecordKind::Array {
              array: iterable,
              next_index: 0,
              length,
            },
          })
        });
      }
    }

    // Prefer the engine's canonical iterator implementation when host hooks are available (native
    // call/construct). This avoids drift between the bindings runtime and `vm-js` iterator
    // semantics (iterator protocol errors, and host-hook propagation).
    if let Some(hooks) = self.vm_host_hooks.as_deref_mut() {
      let record =
        vm_js::iterator::get_iterator(&mut *self.cx.vm, host, hooks, &mut self.cx.scope, iterable)?;
      return Ok(IteratorRecord {
        iterator: record.iterator,
        next_method: record.next_method,
        done: record.done,
        kind: IteratorRecordKind::VmJs(record),
      });
    }

    // Fallback for contexts that do not have `VmHostHooks` (e.g. bindings installation). This path
    // is intentionally small; most conversion-heavy code runs inside native-call dispatch where we
    // take the canonical `vm-js` iterator path above.
    let Value::Object(obj) = iterable else {
      return Err(self.throw_type_error("GetIterator: expected object"));
    };

    self.with_stack_roots::<IteratorRecord<Self::JsValue>, _>(&[iterable], |rt| {
      // Minimal Array fast-path: `vm-js` does not yet expose `%Array.prototype%[@@iterator]` on the
      // intrinsic graph, but arrays should still be accepted as iterable inputs for `sequence<T>`.
      if rt.cx.scope.heap().object_is_array(obj)? {
        let length_key = rt.property_key("length")?;
        let len_value = rt.get(host, iterable, length_key)?;
        let len = rt.to_number(host, len_value)?;
        if !len.is_finite() || len < 0.0 {
          return Err(
            rt.throw_type_error("GetIterator: array length is not a non-negative finite number"),
          );
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

      let iterator_key = rt.symbol_iterator()?;
      let Some(method) = rt.get_method(host, iterable, iterator_key)? else {
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
    if let Some(hooks) = self.vm_host_hooks.as_deref_mut() {
      let record = vm_js::iterator::get_iterator_from_method(
        &mut *self.cx.vm,
        host,
        hooks,
        &mut self.cx.scope,
        iterable,
        method,
      )?;
      return Ok(IteratorRecord {
        iterator: record.iterator,
        next_method: record.next_method,
        done: record.done,
        kind: IteratorRecordKind::VmJs(record),
      });
    }

    let iterator = self
      .cx
      .vm
      .call(host, &mut self.cx.scope, method, iterable, &[])?;
    if !self.is_object(iterator) {
      return Err(self.throw_type_error("Iterator method did not return an object"));
    }

    self.with_stack_roots(&[iterator], |rt| {
      let next_key = rt.property_key("next")?;
      let next = rt.get(host, iterator, next_key)?;
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
      IteratorRecordKind::VmJs(record) => {
        let Some(hooks) = self.vm_host_hooks.as_deref_mut() else {
          return Err(
            self
              .throw_type_error("IteratorStepValue: missing host hooks for vm-js iterator record"),
          );
        };
        let out = vm_js::iterator::iterator_step_value(
          &mut *self.cx.vm,
          host,
          hooks,
          &mut self.cx.scope,
          record,
        )?;
        iterator_record.iterator = record.iterator;
        iterator_record.next_method = record.next_method;
        iterator_record.done = record.done;
        Ok(out)
      }
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
          let value = rt.get(host, *array, key)?;
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
          let done = rt.get(host, result, done_key)?;
          let done = rt.to_boolean(done)?;
          if done {
            iterator_record.done = true;
            return Ok(None);
          }

          let value_key = rt.property_key("value")?;
          let value = rt.get(host, result, value_key)?;
          Ok(Some(value))
        })
      }
    }
  }

  fn call(
    &mut self,
    host: &mut Host,
    callee: Self::JsValue,
    this: Self::JsValue,
    args: &[Self::JsValue],
  ) -> Result<Self::JsValue, Self::Error> {
    self
      .cx
      .vm
      .call(host, &mut self.cx.scope, callee, this, args)
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
      let id = self
        .cx
        .vm
        .register_native_call(dispatch_native_call::<Host>)?;
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

    // `vm-js` distinguishes between `[[Call]]` and `[[Construct]]`. The legacy bindings generator
    // (which targets this runtime trait) assumes plain functions are constructable (matching
    // JavaScript's default `Function` semantics) and expects interface objects like `URL` /
    // `URLSearchParams` / `Node` to support `new`.
    //
    // To keep those bindings working while we transition to the `webidl-vm-js` codegen backend, we
    // install the same Rust callback for both `[[Call]]` and `[[Construct]]`.
    let func = self
      .cx
      .scope
      .alloc_native_function(call_id, Some(construct_id), name_s, length)?;
    self.cx.scope.push_root(Value::Object(func))?;
    self
      .cx
      .scope
      .heap_mut()
      .object_set_prototype(func, Some(intr.function_prototype()))?;

    let dispatch_ptr = self.state.alloc_dispatch_record(f as usize, f as usize)?;
    let slots = HostSlots {
      a: (self.state as *const VmJsWebIdlBindingsState<Host>) as u64,
      b: dispatch_ptr as u64,
    };
    self
      .cx
      .scope
      .heap_mut()
      .object_set_host_slots(func, slots)?;

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
      let id = self
        .cx
        .vm
        .register_native_call(dispatch_native_call::<Host>)?;
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
    self
      .cx
      .scope
      .heap_mut()
      .object_set_prototype(func, Some(intr.function_prototype()))?;

    let dispatch_ptr = self.state.alloc_dispatch_record(
      call as usize,
      if call as usize == construct as usize {
        0
      } else {
        construct as usize
      },
    )?;
    let slots = HostSlots {
      a: (self.state as *const VmJsWebIdlBindingsState<Host>) as u64,
      b: dispatch_ptr as u64,
    };
    self
      .cx
      .scope
      .heap_mut()
      .object_set_host_slots(func, slots)?;

    Ok(Value::Object(func))
  }

  fn root_callback_function(
    &mut self,
    value: Self::JsValue,
  ) -> Result<CallbackHandle, Self::Error> {
    let handle = {
      let heap = self.cx.scope.heap_mut();
      CallbackHandle::from_callback_function(&*self.cx.vm, heap, value, false)?
    };
    handle.ok_or_else(|| self.throw_type_error("Callback function is null or undefined"))
  }

  fn root_callback_interface(
    &mut self,
    value: Self::JsValue,
  ) -> Result<CallbackHandle, Self::Error> {
    let handle = {
      let vm = &mut *self.cx.vm;
      let heap = self.cx.scope.heap_mut();
      CallbackHandle::from_callback_interface(vm, heap, value, false)?
    };
    handle.ok_or_else(|| self.throw_type_error("Callback interface is null or undefined"))
  }

  fn global_object(&mut self) -> Result<Self::JsValue, Self::Error> {
    self
      .cx
      .scope
      .push_root(Value::Object(self.state.global_object))?;
    Ok(Value::Object(self.state.global_object))
  }

  fn define_constructor(
    &mut self,
    global: Self::JsValue,
    name: &str,
    ctor: Self::JsValue,
    proto: Self::JsValue,
  ) -> Result<(), Self::Error> {
    let Value::Object(ctor_obj) = ctor else {
      return Err(self.throw_type_error("define_constructor: expected object constructor"));
    };
    let Value::Object(proto_obj) = proto else {
      return Err(self.throw_type_error("define_constructor: expected object prototype"));
    };

    self
      .state
      .constructor_default_protos
      .borrow_mut()
      .insert(ctor_obj, proto_obj);

    // Follow the Web IDL JavaScript binding:
    // - `global[name]`: writable + configurable, non-enumerable
    // - `ctor.prototype`: non-writable, non-enumerable, non-configurable
    // - `proto.constructor`: non-writable, non-enumerable, non-configurable
    self.define_data_property_str_with_attrs(global, name, ctor, true, false, true)?;
    self.define_data_property_str_with_attrs(ctor, "prototype", proto, false, false, false)?;
    self.define_data_property_str_with_attrs(proto, "constructor", ctor, false, false, false)?;
    Ok(())
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
      return Err(
        self.throw_type_error("define_data_property_with_attrs: expected object receiver"),
      );
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
      return Err(
        self.throw_type_error("define_accessor_property_with_attrs: expected object receiver"),
      );
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
      Some(_) => {
        return Err(self.throw_type_error("set_prototype: expected object or null prototype"))
      }
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
    attrs: DataPropertyAttributes,
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

    scope.define_property(obj, key, make_data_descriptor(value, attrs))
  }

  fn define_data_property_str(
    &mut self,
    obj: Self::JsValue,
    name: &str,
    value: Self::JsValue,
    attrs: DataPropertyAttributes,
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

    scope.define_property(obj, key, make_data_descriptor(value, attrs))
  }
}

impl<Host: 'static> webidl_js_runtime::JsRuntime for VmJsWebIdlBindingsCx<'_, Host> {
  type JsValue = Value;
  type PropertyKey = PropertyKey;
  type Error = VmError;

  fn with_stack_roots<R, F>(&mut self, roots: &[Self::JsValue], f: F) -> Result<R, Self::Error>
  where
    F: FnOnce(&mut Self) -> Result<R, Self::Error>,
  {
    let base = self.cx.scope.heap().stack_root_len();
    self.cx.scope.heap_mut().push_stack_roots(roots)?;
    let out = f(self);
    self.cx.scope.heap_mut().truncate_stack_roots(base);
    out
  }

  fn js_undefined(&self) -> Self::JsValue {
    Value::Undefined
  }

  fn js_null(&self) -> Self::JsValue {
    Value::Null
  }

  fn js_boolean(&self, value: bool) -> Self::JsValue {
    Value::Bool(value)
  }

  fn js_number(&self, value: f64) -> Self::JsValue {
    Value::Number(value)
  }

  fn alloc_string(&mut self, value: &str) -> Result<Self::JsValue, Self::Error> {
    let s = self.cx.scope.alloc_string(value)?;
    self.cx.scope.push_root(Value::String(s))?;
    Ok(Value::String(s))
  }

  fn alloc_string_from_code_units(&mut self, units: &[u16]) -> Result<Self::JsValue, Self::Error> {
    let s = self.cx.scope.alloc_string_from_code_units(units)?;
    self.cx.scope.push_root(Value::String(s))?;
    Ok(Value::String(s))
  }

  fn is_undefined(&self, value: Self::JsValue) -> bool {
    matches!(value, Value::Undefined)
  }

  fn is_null(&self, value: Self::JsValue) -> bool {
    matches!(value, Value::Null)
  }

  fn with_string_code_units<R>(
    &mut self,
    string: Self::JsValue,
    f: impl FnOnce(&[u16]) -> R,
  ) -> Result<R, Self::Error> {
    let Value::String(s) = string else {
      return Err(webidl_js_runtime::WebIdlJsRuntime::throw_type_error(
        self,
        "expected a string value",
      ));
    };
    Ok(f(self.cx.scope.heap().get_string(s)?.as_code_units()))
  }

  fn property_key_from_str(&mut self, s: &str) -> Result<Self::PropertyKey, Self::Error> {
    // Mirror the key caching used by the bindings runtime `property_key` helper to avoid allocating
    // repeated iterator-protocol strings in tight loops.
    match s {
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
        let handle = self.cx.scope.alloc_string(s)?;
        self.cx.scope.push_root(Value::String(handle))?;
        Ok(PropertyKey::from_string(handle))
      }
    }
  }

  fn property_key_from_u32(&mut self, index: u32) -> Result<Self::PropertyKey, Self::Error> {
    self.property_key_from_str(&index.to_string())
  }

  fn property_key_is_symbol(&self, key: Self::PropertyKey) -> bool {
    matches!(key, PropertyKey::Symbol(_))
  }

  fn property_key_is_string(&self, key: Self::PropertyKey) -> bool {
    matches!(key, PropertyKey::String(_))
  }

  fn property_key_to_js_string(
    &mut self,
    key: Self::PropertyKey,
  ) -> Result<Self::JsValue, Self::Error> {
    match key {
      PropertyKey::String(s) => {
        self.cx.scope.push_root(Value::String(s))?;
        Ok(Value::String(s))
      }
      PropertyKey::Symbol(sym) => {
        let s = self.cx.scope.heap_mut().to_string(Value::Symbol(sym))?;
        self.cx.scope.push_root(Value::String(s))?;
        Ok(Value::String(s))
      }
    }
  }

  fn alloc_object(&mut self) -> Result<Self::JsValue, Self::Error> {
    let obj = self.cx.scope.alloc_object()?;
    self.cx.scope.push_root(Value::Object(obj))?;
    // When a realm is initialized, prefer `%Object.prototype%` so the result behaves like a normal
    // JavaScript object (e.g. has standard methods).
    if let Some(intrinsics) = self.cx.vm.intrinsics() {
      let proto = intrinsics.object_prototype();
      self.cx.scope.push_root(Value::Object(proto))?;
      self.cx.scope.object_set_prototype(obj, Some(proto))?;
    }
    Ok(Value::Object(obj))
  }

  fn alloc_array(&mut self) -> Result<Self::JsValue, Self::Error> {
    let obj = self.cx.scope.alloc_array(0)?;
    self.cx.scope.push_root(Value::Object(obj))?;
    // When a realm is initialized, prefer `%Array.prototype%` so the result behaves like a normal
    // JavaScript array (e.g. is iterable, has standard methods).
    if let Some(intrinsics) = self.cx.vm.intrinsics() {
      let proto = intrinsics.array_prototype();
      self.cx.scope.push_root(Value::Object(proto))?;
      self.cx.scope.object_set_prototype(obj, Some(proto))?;
    }
    Ok(Value::Object(obj))
  }

  fn define_data_property(
    &mut self,
    obj: Self::JsValue,
    key: Self::PropertyKey,
    value: Self::JsValue,
    enumerable: bool,
  ) -> Result<(), Self::Error> {
    let Value::Object(obj) = obj else {
      return Err(webidl_js_runtime::WebIdlJsRuntime::throw_type_error(
        self,
        "define_data_property: receiver is not an object",
      ));
    };
    self.cx.scope.define_property(
      obj,
      key,
      PropertyDescriptor {
        enumerable,
        configurable: true,
        kind: PropertyKind::Data {
          value,
          writable: true,
        },
      },
    )
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

  fn is_symbol(&self, value: Self::JsValue) -> bool {
    matches!(value, Value::Symbol(_))
  }

  fn to_object(&mut self, value: Self::JsValue) -> Result<Self::JsValue, Self::Error> {
    let mut dummy_host = ();
    let mut dummy_hooks = NoopVmHostHooks;

    let obj = if let Some(hooks) = self.vm_host_hooks.as_deref_mut() {
      self
        .cx
        .scope
        .to_object(&mut *self.cx.vm, &mut dummy_host, hooks, value)?
    } else {
      self
        .cx
        .scope
        .to_object(&mut *self.cx.vm, &mut dummy_host, &mut dummy_hooks, value)?
    };
    self.cx.scope.push_root(Value::Object(obj))?;
    Ok(Value::Object(obj))
  }

  fn call(
    &mut self,
    callee: Self::JsValue,
    this: Self::JsValue,
    args: &[Self::JsValue],
  ) -> Result<Self::JsValue, Self::Error> {
    if let Some(hooks) = self.vm_host_hooks.as_deref_mut() {
      return self
        .cx
        .vm
        .call_with_host(&mut self.cx.scope, hooks, callee, this, args);
    }
    self.cx.vm.call_without_host(&mut self.cx.scope, callee, this, args)
  }

  fn to_boolean(&mut self, value: Self::JsValue) -> Result<bool, Self::Error> {
    self.cx.scope.heap().to_boolean(value)
  }

  fn to_number(&mut self, value: Self::JsValue) -> Result<f64, Self::Error> {
    let mut dummy_host = ();
    let mut dummy_hooks = NoopVmHostHooks;

    if let Some(hooks) = self.vm_host_hooks.as_deref_mut() {
      self
        .cx
        .scope
        .to_number(&mut *self.cx.vm, &mut dummy_host, hooks, value)
    } else {
      self
        .cx
        .scope
        .to_number(&mut *self.cx.vm, &mut dummy_host, &mut dummy_hooks, value)
    }
  }

  fn to_string(&mut self, value: Self::JsValue) -> Result<Self::JsValue, Self::Error> {
    let mut dummy_host = ();
    let mut dummy_hooks = NoopVmHostHooks;

    let s = if let Some(hooks) = self.vm_host_hooks.as_deref_mut() {
      self
        .cx
        .scope
        .to_string(&mut *self.cx.vm, &mut dummy_host, hooks, value)?
    } else {
      self
        .cx
        .scope
        .to_string(&mut *self.cx.vm, &mut dummy_host, &mut dummy_hooks, value)?
    };
    self.cx.scope.push_root(Value::String(s))?;
    Ok(Value::String(s))
  }

  fn string_to_utf8_lossy(&mut self, string: Self::JsValue) -> Result<String, Self::Error> {
    let string = webidl_js_runtime::JsRuntime::to_string(self, string)?;
    let Value::String(s) = string else {
      return Err(VmError::InvariantViolation("ToString returned non-string"));
    };
    Ok(self.cx.scope.heap().get_string(s)?.to_utf8_lossy())
  }

  fn to_bigint(&mut self, value: Self::JsValue) -> Result<Self::JsValue, Self::Error> {
    webidl::WebIdlJsRuntime::to_bigint(&mut self.cx, value)
  }

  fn to_numeric(&mut self, value: Self::JsValue) -> Result<Self::JsValue, Self::Error> {
    webidl::WebIdlJsRuntime::to_numeric(&mut self.cx, value)
  }

  fn get(
    &mut self,
    obj: Self::JsValue,
    key: Self::PropertyKey,
  ) -> Result<Self::JsValue, Self::Error> {
    let Value::Object(obj) = obj else {
      return Err(webidl_js_runtime::WebIdlJsRuntime::throw_type_error(
        self,
        "Get: receiver is not an object",
      ));
    };
    let mut dummy_host = ();
    let mut dummy_hooks = NoopVmHostHooks;

    let value = if let Some(hooks) = self.vm_host_hooks.as_deref_mut() {
      self.cx.scope.get_with_host_and_hooks(
        &mut *self.cx.vm,
        &mut dummy_host,
        hooks,
        obj,
        key,
        Value::Object(obj),
      )?
    } else {
      self.cx.scope.get_with_host_and_hooks(
        &mut *self.cx.vm,
        &mut dummy_host,
        &mut dummy_hooks,
        obj,
        key,
        Value::Object(obj),
      )?
    };
    self.cx.scope.push_root(value)?;
    Ok(value)
  }

  fn own_property_keys(
    &mut self,
    obj: Self::JsValue,
  ) -> Result<Vec<Self::PropertyKey>, Self::Error> {
    let Value::Object(obj) = obj else {
      return Err(webidl_js_runtime::WebIdlJsRuntime::throw_type_error(
        self,
        "OwnPropertyKeys: receiver is not an object",
      ));
    };
    let mut dummy_host = ();
    let mut dummy_hooks = NoopVmHostHooks;
    let mut tick = Vm::tick;
    if let Some(hooks) = self.vm_host_hooks.as_deref_mut() {
      self.cx.scope.own_property_keys_with_host_and_hooks_with_tick(
        &mut *self.cx.vm,
        &mut dummy_host,
        hooks,
        obj,
        &mut tick,
      )
    } else {
      self.cx.scope.own_property_keys_with_host_and_hooks_with_tick(
        &mut *self.cx.vm,
        &mut dummy_host,
        &mut dummy_hooks,
        obj,
        &mut tick,
      )
    }
  }

  fn get_own_property(
    &mut self,
    obj: Self::JsValue,
    key: Self::PropertyKey,
  ) -> Result<Option<webidl_js_runtime::JsOwnPropertyDescriptor<Self::JsValue>>, Self::Error> {
    let Value::Object(obj) = obj else {
      return Err(webidl_js_runtime::WebIdlJsRuntime::throw_type_error(
        self,
        "GetOwnProperty: receiver is not an object",
      ));
    };
    let mut dummy_host = ();
    let mut dummy_hooks = NoopVmHostHooks;
    let mut tick = Vm::tick;
    let desc = if let Some(hooks) = self.vm_host_hooks.as_deref_mut() {
      self.cx.scope.get_own_property_with_host_and_hooks_with_tick(
        &mut *self.cx.vm,
        &mut dummy_host,
        hooks,
        obj,
        key,
        &mut tick,
      )?
    } else {
      self.cx.scope.get_own_property_with_host_and_hooks_with_tick(
        &mut *self.cx.vm,
        &mut dummy_host,
        &mut dummy_hooks,
        obj,
        key,
        &mut tick,
      )?
    };
    let Some(desc) = desc else {
      return Ok(None);
    };

    let kind = match desc.kind {
      PropertyKind::Data { value, .. } => webidl_js_runtime::JsPropertyKind::Data { value },
      PropertyKind::Accessor { get, set } => webidl_js_runtime::JsPropertyKind::Accessor { get, set },
    };
    Ok(Some(webidl_js_runtime::JsOwnPropertyDescriptor {
      enumerable: desc.enumerable,
      kind,
    }))
  }

  fn get_method(
    &mut self,
    obj: Self::JsValue,
    key: Self::PropertyKey,
  ) -> Result<Option<Self::JsValue>, Self::Error> {
    let value = webidl_js_runtime::JsRuntime::get(self, obj, key)?;
    if matches!(value, Value::Undefined | Value::Null) {
      // Array iterator fast-path: use a sentinel `undefined` method value to request array
      // iteration from `get_iterator_from_method` when the runtime does not expose
      // `%Array.prototype%[@@iterator]`.
      if let Value::Object(obj_handle) = obj {
        if let Ok(intr) = self.intrinsics() {
          if key == PropertyKey::Symbol(intr.well_known_symbols().iterator)
            && self.cx.scope.heap().object_is_array(obj_handle)?
          {
            return Ok(Some(Value::Undefined));
          }
        }
      }
      return Ok(None);
    }

    if !webidl_js_runtime::JsRuntime::is_callable(self, value) {
      return Err(VmError::TypeError("GetMethod: target is not callable"));
    }
    Ok(Some(value))
  }

  fn get_iterator_from_method(
    &mut self,
    iterable: Self::JsValue,
    method: Self::JsValue,
  ) -> Result<webidl_js_runtime::IteratorRecord<Self::JsValue>, Self::Error> {
    // Array iterator fast-path: see `get_method`.
    if matches!(method, Value::Undefined) {
      if let Value::Object(obj_handle) = iterable {
        if self.cx.scope.heap().object_is_array(obj_handle)? {
          // Sentinel iterator record: `next_method` stores the next index as a number; we re-read
          // `array.length` per-step (matching `%ArrayIteratorPrototype%.next` semantics).
          return Ok(webidl_js_runtime::IteratorRecord {
            iterator: iterable,
            next_method: Value::Number(0.0),
            done: false,
          });
        }
      }
    }

    let iterator = webidl_js_runtime::JsRuntime::call(self, method, iterable, &[])?;
    if !webidl_js_runtime::JsRuntime::is_object(self, iterator) {
      return Err(webidl_js_runtime::WebIdlJsRuntime::throw_type_error(
        self,
        "Iterator method did not return an object",
      ));
    }

    webidl_js_runtime::JsRuntime::with_stack_roots(self, &[iterable, iterator], |rt| {
      let next_key = webidl_js_runtime::JsRuntime::property_key_from_str(rt, "next")?;
      let next = webidl_js_runtime::JsRuntime::get(rt, iterator, next_key)?;
      if !webidl_js_runtime::JsRuntime::is_callable(rt, next) {
        return Err(webidl_js_runtime::WebIdlJsRuntime::throw_type_error(
          rt,
          "Iterator.next is not callable",
        ));
      }
      Ok(webidl_js_runtime::IteratorRecord {
        iterator,
        next_method: next,
        done: false,
      })
    })
  }

  fn iterator_step_value(
    &mut self,
    iterator_record: &mut webidl_js_runtime::IteratorRecord<Self::JsValue>,
  ) -> Result<Option<Self::JsValue>, Self::Error> {
    if iterator_record.done {
      return Ok(None);
    }

    // Array iteration fast-path (see `get_iterator_from_method`).
    if let (Value::Object(obj_handle), Value::Number(next_index)) =
      (iterator_record.iterator, iterator_record.next_method)
    {
      // Only treat this as an array iterator if the receiver is actually an intrinsic array.
      if self.cx.scope.heap().object_is_array(obj_handle)? {
        return webidl_js_runtime::JsRuntime::with_stack_roots(
          self,
          &[iterator_record.iterator],
          |rt| {
            let next_index = if next_index.is_finite() && next_index >= 0.0 {
              next_index as u32
            } else {
              0
            };

            let length_key = webidl_js_runtime::JsRuntime::property_key_from_str(rt, "length")?;
            let len_value =
              webidl_js_runtime::JsRuntime::get(rt, iterator_record.iterator, length_key)?;
            let len = webidl_js_runtime::JsRuntime::to_number(rt, len_value)?;
            if !len.is_finite() || len < 0.0 {
              return Err(webidl_js_runtime::WebIdlJsRuntime::throw_type_error(
                rt,
                "GetIterator: array length is not a non-negative finite number",
              ));
            }
            let length = len as u32;

            if next_index >= length {
              iterator_record.done = true;
              return Ok(None);
            }

            // Read element at `next_index` via ordinary property access (holes yield `undefined`).
            let key = rt.property_key_from_u32(next_index)?;
            let value = webidl_js_runtime::JsRuntime::get(rt, iterator_record.iterator, key)?;

            iterator_record.next_method = Value::Number(next_index.saturating_add(1) as f64);
            Ok(Some(value))
          },
        );
      }
    }

    let iterator = iterator_record.iterator;
    let next_method = iterator_record.next_method;
    webidl_js_runtime::JsRuntime::with_stack_roots(self, &[iterator, next_method], |rt| {
      let result = webidl_js_runtime::JsRuntime::call(rt, next_method, iterator, &[])?;
      if !webidl_js_runtime::JsRuntime::is_object(rt, result) {
        return Err(webidl_js_runtime::WebIdlJsRuntime::throw_type_error(
          rt,
          "Iterator.next() did not return an object",
        ));
      }

      webidl_js_runtime::JsRuntime::with_stack_roots(rt, &[result], |rt| {
        let done_key = webidl_js_runtime::JsRuntime::property_key_from_str(rt, "done")?;
        let done = webidl_js_runtime::JsRuntime::get(rt, result, done_key)?;
        let done = webidl_js_runtime::JsRuntime::to_boolean(rt, done)?;
        if done {
          iterator_record.done = true;
          return Ok(None);
        }

        let value_key = webidl_js_runtime::JsRuntime::property_key_from_str(rt, "value")?;
        let value = webidl_js_runtime::JsRuntime::get(rt, result, value_key)?;
        Ok(Some(value))
      })
    })
  }
}

impl<Host: 'static> webidl_js_runtime::WebIdlJsRuntime for VmJsWebIdlBindingsCx<'_, Host> {
  fn limits(&self) -> webidl::WebIdlLimits {
    self.state.limits
  }

  fn hooks(&self) -> &dyn WebIdlHooks<Self::JsValue> {
    self.state.hooks.as_ref()
  }

  fn promise_resolve(&mut self, value: Self::JsValue) -> Result<Self::JsValue, Self::Error> {
    // Spec: https://tc39.es/ecma262/#sec-promise-resolve
    //
    // Delegate to `vm-js`'s spec-shaped helper so:
    // - Promise objects are returned directly when already from the intrinsic %Promise% constructor,
    // - thenables are assimilated via their `then` method, and
    // - the returned value is always a Promise object.
    //
    // Root `value` across Promise resolution: `PromiseResolve` can allocate and (when resolving
    // thenables) invoke user code.
    //
    // Promise resolution can also enqueue jobs; prefer calling through the real host hooks when
    // available (native calls). When unavailable (e.g. conversion-only contexts), fall back to a
    // no-op host hook implementation.
    let mut noop_hooks = NoopVmHostHooks;
    let promise = webidl_js_runtime::JsRuntime::with_stack_roots(self, &[value], |rt| {
      if let Some(hooks) = rt.vm_host_hooks.as_deref_mut() {
        vm_js::promise_resolve(&mut *rt.cx.vm, &mut rt.cx.scope, hooks, value)
      } else {
        vm_js::promise_resolve(&mut *rt.cx.vm, &mut rt.cx.scope, &mut noop_hooks, value)
      }
    })?;
    self.cx.scope.push_root(promise)?;
    Ok(promise)
  }

  fn symbol_iterator(&mut self) -> Result<Self::PropertyKey, Self::Error> {
    let sym =
      webidl::JsRuntime::well_known_symbol(&mut self.cx, webidl::WellKnownSymbol::Iterator)?;
    Ok(PropertyKey::from_symbol(sym))
  }

  fn symbol_async_iterator(&mut self) -> Result<Self::PropertyKey, Self::Error> {
    let sym =
      webidl::JsRuntime::well_known_symbol(&mut self.cx, webidl::WellKnownSymbol::AsyncIterator)?;
    Ok(PropertyKey::from_symbol(sym))
  }

  fn symbol_to_property_key(
    &mut self,
    symbol: Self::JsValue,
  ) -> Result<Self::PropertyKey, Self::Error> {
    let Value::Symbol(sym) = symbol else {
      return Err(webidl_js_runtime::WebIdlJsRuntime::throw_type_error(
        self,
        "expected a Symbol value",
      ));
    };
    self.cx.scope.push_root(Value::Symbol(sym))?;
    Ok(PropertyKey::from_symbol(sym))
  }

  fn platform_object_opaque(&self, value: Self::JsValue) -> Option<u64> {
    if !webidl_js_runtime::WebIdlJsRuntime::is_platform_object(self, value) {
      return None;
    }
    let Value::Object(obj) = value else {
      return None;
    };
    Some((obj.index() as u64) | ((obj.generation() as u64) << 32))
  }

  fn is_string_object(&self, value: Self::JsValue) -> bool {
    webidl::JsRuntime::is_string_object(&self.cx, value)
  }

  fn is_array_buffer(&self, value: Self::JsValue) -> bool {
    if let Value::Object(obj) = value {
      if self.cx.scope.heap().is_array_buffer_object(obj) {
        return true;
      }
    }
    // Allow embedder hooks to treat host objects as ArrayBuffers (e.g. external buffers).
    self.state.hooks.is_array_buffer(value)
  }

  fn is_shared_array_buffer(&self, value: Self::JsValue) -> bool {
    let _ = value;
    false
  }

  fn is_data_view(&self, value: Self::JsValue) -> bool {
    let Value::Object(obj) = value else {
      return false;
    };
    self.cx.scope.heap().is_data_view_object(obj)
  }

  fn typed_array_name(&self, value: Self::JsValue) -> Option<&'static str> {
    let Value::Object(obj) = value else {
      return None;
    };
    self.cx.scope.heap().typed_array_name(obj)
  }

  fn platform_object_to_js_value(
    &mut self,
    value: &webidl::ir::PlatformObject,
  ) -> Option<Self::JsValue> {
    value.downcast_ref::<Value>().copied()
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
}

/// Compatibility shim: allow existing generated bindings to continue compiling against the legacy
/// `webidl_js_runtime::VmJsRuntime` during migration.
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
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::WebIdlJsRuntime>::is_string_object(
      self, value,
    )
  }

  fn is_symbol(&self, value: Self::JsValue) -> bool {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::is_symbol(self, value)
  }

  fn is_platform_object(&self, value: Self::JsValue) -> bool {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::WebIdlJsRuntime>::is_platform_object(
      self, value,
    )
  }

  fn implements_interface(&self, value: Self::JsValue, interface: webidl::InterfaceId) -> bool {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::WebIdlJsRuntime>::implements_interface(
      self, value, interface,
    )
  }

  fn to_object(&mut self, value: Self::JsValue) -> Result<Self::JsValue, Self::Error> {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::to_object(self, value)
  }

  fn to_boolean(&mut self, value: Self::JsValue) -> Result<bool, Self::Error> {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::to_boolean(self, value)
  }

  fn to_number(&mut self, _host: &mut Host, value: Self::JsValue) -> Result<f64, Self::Error> {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::to_number(self, value)
  }

  fn to_string(
    &mut self,
    _host: &mut Host,
    value: Self::JsValue,
  ) -> Result<Self::JsValue, Self::Error> {
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
    let name_value =
      match <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::with_stack_roots(
        self,
        &[obj],
        |rt| rt.alloc_string(name),
      ) {
        Ok(v) => v,
        Err(err) => return err,
      };
    let message_value =
      match <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::with_stack_roots(
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
          &[
            obj,
            name_value,
            message_value,
            name_key_root,
            message_key_root,
          ],
          |rt| {
            <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::define_data_property(
              rt, obj, name_key, name_value, false,
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
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::WebIdlJsRuntime>::symbol_async_iterator(
      self,
    )
  }

  fn get(
    &mut self,
    _host: &mut Host,
    obj: Self::JsValue,
    key: Self::PropertyKey,
  ) -> Result<Self::JsValue, Self::Error> {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::get(self, obj, key)
  }

  fn own_property_keys(
    &mut self,
    obj: Self::JsValue,
  ) -> Result<Vec<Self::PropertyKey>, Self::Error> {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::own_property_keys(self, obj)
  }

  fn get_own_property(
    &mut self,
    obj: Self::JsValue,
    key: Self::PropertyKey,
  ) -> Result<Option<JsOwnPropertyDescriptor>, Self::Error> {
    let desc = <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::get_own_property(
      self, obj, key,
    )?;
    Ok(desc.map(|d| JsOwnPropertyDescriptor {
      enumerable: d.enumerable,
    }))
  }

  fn property_key_is_symbol(&self, key: Self::PropertyKey) -> bool {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::property_key_is_symbol(
      self, key,
    )
  }

  fn property_key_to_js_string(
    &mut self,
    key: Self::PropertyKey,
  ) -> Result<Self::JsValue, Self::Error> {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::property_key_to_js_string(
      self, key,
    )
  }

  fn is_array(&mut self, value: Self::JsValue) -> Result<bool, Self::Error> {
    let Value::Object(obj) = value else {
      return Ok(false);
    };
    Ok(self.heap().object_is_array(obj)?)
  }

  fn get_method(
    &mut self,
    _host: &mut Host,
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
    let Value::Object(obj) = iterable else {
      return Err(
        <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::WebIdlJsRuntime>::throw_type_error(
          self,
          "GetIterator: expected object",
        ),
      );
    };

    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::with_stack_roots(
      self,
      &[iterable],
      |rt| {
        // Minimal Array fast-path: `vm-js` does not yet expose `%Array.prototype%[@@iterator]` for
        // heap-only runtimes, but arrays should still be accepted as iterable inputs for
        // `sequence<T>`.
        if rt.heap().object_is_array(obj)? {
          let length_key =
            <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::property_key_from_str(
              rt, "length",
            )?;
          let len_value = <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::get(
            rt, iterable, length_key,
          )?;
          let Value::Number(len) = len_value else {
            return Err(<webidl_js_runtime::VmJsRuntime as webidl_js_runtime::WebIdlJsRuntime>::throw_type_error(
              rt,
              "GetIterator: array length is not a number",
            ));
          };
          if !len.is_finite() || len < 0.0 || len.fract() != 0.0 || len > (u32::MAX as f64) {
            return Err(<webidl_js_runtime::VmJsRuntime as webidl_js_runtime::WebIdlJsRuntime>::throw_type_error(
              rt,
              "GetIterator: array length is not a uint32 number",
            ));
          }
          return Ok(IteratorRecord {
            iterator: iterable,
            next_method: Value::Undefined,
            done: false,
            kind: IteratorRecordKind::Array {
              array: iterable,
              next_index: 0,
              length: len as u32,
            },
          });
        }

        let iterator_key =
          <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::WebIdlJsRuntime>::symbol_iterator(
            rt,
          )?;
        let Some(method) =
          <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::get_method(
            rt,
            iterable,
            iterator_key,
          )?
        else {
          return Err(<webidl_js_runtime::VmJsRuntime as webidl_js_runtime::WebIdlJsRuntime>::throw_type_error(
          rt,
          "GetIterator: value is not iterable",
        ));
        };
        rt.get_iterator_from_method(host, iterable, method)
      },
    )
  }

  fn get_iterator_from_method(
    &mut self,
    host: &mut Host,
    iterable: Self::JsValue,
    method: Self::JsValue,
  ) -> Result<IteratorRecord<Self::JsValue>, Self::Error> {
    let record = self.with_host_context(host, |rt| {
      <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::get_iterator_from_method(
        rt, iterable, method,
      )
    })?;
    Ok(IteratorRecord {
      iterator: record.iterator,
      next_method: record.next_method,
      done: record.done,
      kind: IteratorRecordKind::Protocol,
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
      IteratorRecordKind::VmJs(_) => Err(VmError::InvariantViolation(
        "IteratorStepValue: unexpected vm-js iterator record in legacy bindings runtime",
      )),
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
        <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::with_stack_roots(
          self,
          &[*array],
          |rt| {
            let key =
              <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::property_key_from_u32(
                rt, idx,
              )?;
            let value = <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::get(
              rt, *array, key,
            )?;
            Ok(Some(value))
          },
        )
      }
      IteratorRecordKind::Protocol => {
        // Bridge through the legacy iterator record type.
        let mut record = webidl_js_runtime::IteratorRecord {
          iterator: iterator_record.iterator,
          next_method: iterator_record.next_method,
          done: iterator_record.done,
        };
        let out = self.with_host_context(host, |rt| {
          <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::iterator_step_value(
            rt,
            &mut record,
          )
        })?;
        iterator_record.done = record.done;
        Ok(out)
      }
    }
  }

  fn call(
    &mut self,
    host: &mut Host,
    callee: Self::JsValue,
    this: Self::JsValue,
    args: &[Self::JsValue],
  ) -> Result<Self::JsValue, Self::Error> {
    self.with_host_context(host, |rt| {
      <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::JsRuntime>::call(rt, callee, this, args)
    })
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
    call: NativeHostFunction<Self, Host>,
    _construct: NativeHostFunction<Self, Host>,
  ) -> Result<Self::JsValue, Self::Error> {
    // `webidl_js_runtime::VmJsRuntime` is a heap-only adapter that cannot execute JS source and does
    // not model `[[Construct]]`. Keep the migration shim spec-shaped by installing the `[[Call]]`
    // handler (typically `Illegal constructor`) and ignoring the constructor initializer.
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::WebIdlBindingsRuntime<Host>>::create_function(
      self, name, length, call,
    )
  }

  fn root_callback_function(
    &mut self,
    value: Self::JsValue,
  ) -> Result<CallbackHandle, Self::Error> {
    <webidl_js_runtime::VmJsRuntime as webidl_js_runtime::WebIdlBindingsRuntime<Host>>::root_callback_function(
      self, value,
    )
  }

  fn root_callback_interface(
    &mut self,
    value: Self::JsValue,
  ) -> Result<CallbackHandle, Self::Error> {
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
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::js::bindings::{
    install_window_bindings, BindingValue, DomExceptionClassVmJs, WebHostBindings,
  };
  use vm_js::{
    Heap, HeapLimits, JsRuntime as VmJsRuntime, MicrotaskQueue, NativeFunctionId, VmOptions,
  };
  use webidl::{InterfaceId, WebIdlHooks, WebIdlLimits};

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
  struct TestHost {
    saw_record_host: Cell<bool>,
  }

  fn add<'a>(
    rt: &mut VmJsWebIdlBindingsCx<'a, TestHost>,
    host: &mut TestHost,
    _this: Value,
    args: &[Value],
  ) -> Result<Value, VmError> {
    let a = args.get(0).copied().unwrap_or(Value::Undefined);
    let b = args.get(1).copied().unwrap_or(Value::Undefined);
    let a = rt.to_number(host, a)?;
    let b = rt.to_number(host, b)?;
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
      cx.define_data_property_str(
        global,
        "add",
        func,
        DataPropertyAttributes::new(true, true, true),
      )?;
    }

    let mut host = TestHost::default();
    let out = runtime.exec_script_with_host(&mut host, "add(1, 2)")?;
    assert!(matches!(out, Value::Number(n) if (n - 3.0).abs() < f64::EPSILON));
    Ok(())
  }

  #[test]
  fn vmjs_webidl_promise_resolve_returns_promise() -> Result<(), VmError> {
    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(HeapLimits::new(16 * 1024 * 1024, 8 * 1024 * 1024));
    let mut runtime = VmJsRuntime::new(vm, heap)?;
    let state = Box::new(VmJsWebIdlBindingsState::<TestHost>::new(
      runtime.realm().global_object(),
      WebIdlLimits::default(),
      Box::new(NoHooks),
    ));

    // Create `p = PromiseResolve(%Promise%, 1)`.
    {
      let (vm, heap, _realm) = webidl_vm_js::split_js_runtime(&mut runtime);
      let mut cx = VmJsWebIdlBindingsCx::new(vm, heap, &state);

      let promise =
        webidl_js_runtime::WebIdlJsRuntime::promise_resolve(&mut cx, Value::Number(1.0))?;
      let Value::Object(promise_obj) = promise else {
        return Err(VmError::TypeError("promise_resolve returned non-object"));
      };
      assert!(cx.cx.scope.heap().is_promise_object(promise_obj));

      // `promise.then` exists and is callable.
      let then_key = webidl_js_runtime::JsRuntime::property_key_from_str(&mut cx, "then")?;
      let then = webidl_js_runtime::JsRuntime::get(&mut cx, promise, then_key)?;
      assert!(webidl_js_runtime::JsRuntime::is_callable(&cx, then));

      let global = cx.global_object()?;
      cx.define_data_property_str(
        global,
        "p",
        promise,
        DataPropertyAttributes::new(true, true, true),
      )?;
    }

    // Smoke-test from JavaScript.
    let mut host = TestHost::default();
    let out = runtime.exec_script_with_host(&mut host, "p instanceof Promise")?;
    assert!(matches!(out, Value::Bool(true)));
    let out = runtime.exec_script_with_host(&mut host, "typeof p.then === 'function'")?;
    assert!(matches!(out, Value::Bool(true)));
    Ok(())
  }

  #[test]
  fn vmjs_bindings_runtime_webidl_conversions_preserve_vm_host() -> Result<(), VmError> {
    fn record_host(
      _vm: &mut Vm,
      _scope: &mut Scope<'_>,
      host: &mut dyn VmHost,
      _hooks: &mut dyn VmHostHooks,
      _callee: GcObject,
      _this: Value,
      _args: &[Value],
    ) -> Result<Value, VmError> {
      let Some(host) = host.as_any_mut().downcast_mut::<TestHost>() else {
        return Err(VmError::TypeError(
          "expected WebIDL conversion to call JS with the real VmHost",
        ));
      };
      host.saw_record_host.set(true);
      Ok(Value::Undefined)
    }
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

      let global = cx.global_object()?;

      // Install recordHost() as a raw vm-js native function so it directly receives the `VmHost`
      // passed through nested JS calls made by WebIDL conversions.
      let record_host_id = cx.cx.vm.register_native_call(record_host)?;
      let record_host_name = cx.cx.scope.alloc_string("recordHost")?;
      cx.cx.scope.push_root(Value::String(record_host_name))?;
      let record_host_fn =
        cx.cx
          .scope
          .alloc_native_function(record_host_id, None, record_host_name, 0)?;

      cx.define_data_property_str(
        global,
        "recordHost",
        Value::Object(record_host_fn),
        DataPropertyAttributes::new(true, true, true),
      )?;

      let add = cx.create_function("add", 2, add)?;
      cx.define_data_property_str(
        global,
        "add",
        add,
        DataPropertyAttributes::new(true, true, true),
      )?;
    }

    let mut host = TestHost::default();
    let out = runtime.exec_script_with_host(
      &mut host,
      "add({ valueOf() { recordHost(); return 1; } }, 2)",
    )?;
    assert!(matches!(out, Value::Number(n) if (n - 3.0).abs() < f64::EPSILON));
    assert!(host.saw_record_host.get());
    Ok(())
  }

  #[test]
  fn vmjs_bindings_runtime_iterates_arrays_even_if_prototype_changed() -> Result<(), VmError> {
    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(HeapLimits::new(16 * 1024 * 1024, 8 * 1024 * 1024));
    let mut runtime = VmJsRuntime::new(vm, heap)?;

    let state = Box::new(VmJsWebIdlBindingsState::<TestHost>::new(
      runtime.realm().global_object(),
      WebIdlLimits::default(),
      Box::new(NoHooks),
    ));

    let mut host = TestHost::default();
    let (vm, heap, _realm) = webidl_vm_js::split_js_runtime(&mut runtime);
    let mut scope = heap.scope();
    let mut hooks = MicrotaskQueue::new();

    let mut rt = VmJsWebIdlBindingsCx::from_native_call(vm, &mut scope, &mut hooks, &state);

    let arr = rt.create_array(2)?;
    let idx0 = rt.property_key("0")?;
    rt.define_data_property_with_attrs(arr, idx0, Value::Number(1.0), true, true, true)?;
    let idx1 = rt.property_key("1")?;
    rt.define_data_property_with_attrs(arr, idx1, Value::Number(2.0), true, true, true)?;

    // `Array.isArray` must remain true even when the object's `[[Prototype]]` is changed.
    let new_proto = rt.create_object()?;
    rt.set_prototype(arr, Some(new_proto))?;
    assert!(
      rt.is_array(arr)?,
      "expected is_array to detect Array exotic objects"
    );

    let mut record = rt.get_iterator(&mut host, arr)?;
    let values = rt.with_stack_roots(&[record.iterator, record.next_method], |rt| {
      let mut values = Vec::new();
      while let Some(v) = rt.iterator_step_value(&mut host, &mut record)? {
        values.push(v);
      }
      Ok(values)
    })?;

    assert_eq!(values, vec![Value::Number(1.0), Value::Number(2.0)]);
    Ok(())
  }

  #[test]
  fn vmjs_bindings_runtime_buffer_source_internal_slot_checks_use_vm_js_objects(
  ) -> Result<(), VmError> {
    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(HeapLimits::new(16 * 1024 * 1024, 8 * 1024 * 1024));
    let mut runtime = VmJsRuntime::new(vm, heap)?;

    let state = Box::new(VmJsWebIdlBindingsState::<TestHost>::new(
      runtime.realm().global_object(),
      WebIdlLimits::default(),
      Box::new(NoHooks),
    ));

    let (vm, heap, _realm) = webidl_vm_js::split_js_runtime(&mut runtime);
    let mut scope = heap.scope();
    let mut hooks = MicrotaskQueue::new();
    let mut rt = VmJsWebIdlBindingsCx::from_native_call(vm, &mut scope, &mut hooks, &state);

    // ArrayBuffer
    let buf = rt.cx.scope.alloc_array_buffer(8)?;
    rt.cx.scope.push_root(Value::Object(buf))?;
    assert!(
      webidl_js_runtime::WebIdlJsRuntime::is_array_buffer(&rt, Value::Object(buf)),
      "expected vm-js ArrayBuffer objects to satisfy WebIDL is_array_buffer"
    );

    // TypedArrayName (Uint8Array)
    let u8 = rt.cx.scope.alloc_uint8_array(buf, 0, 8)?;
    rt.cx.scope.push_root(Value::Object(u8))?;
    assert_eq!(
      webidl_js_runtime::WebIdlJsRuntime::typed_array_name(&rt, Value::Object(u8)),
      Some("Uint8Array")
    );

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

    let arr = webidl::sequence_to_js_array(&mut cx, &limits, &[1u32, 2u32, 3u32])
      .expect("sequence_to_js_array");

    let proto = cx.scope.heap().object_prototype(arr)?;
    assert_eq!(proto, Some(realm.intrinsics().array_prototype()));
    Ok(())
  }

  #[derive(Default)]
  struct NoopBindingsHost;

  impl<R> WebHostBindings<R> for NoopBindingsHost
  where
    R: crate::js::webidl::WebIdlBindingsRuntime<Self>,
  {
    fn call_operation(
      &mut self,
      _rt: &mut R,
      _receiver: Option<R::JsValue>,
      _interface: &'static str,
      _operation: &'static str,
      _overload: usize,
      _args: Vec<BindingValue<R::JsValue>>,
    ) -> Result<BindingValue<R::JsValue>, R::Error> {
      Ok(BindingValue::Undefined)
    }
  }

  #[test]
  fn vmjs_realm_window_bindings_have_spec_shaped_descriptors() -> Result<(), VmError> {
    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(HeapLimits::new(16 * 1024 * 1024, 8 * 1024 * 1024));
    let mut runtime = VmJsRuntime::new(vm, heap)?;

    let state = Box::new(VmJsWebIdlBindingsState::<NoopBindingsHost>::new(
      runtime.realm().global_object(),
      WebIdlLimits::default(),
      Box::new(NoHooks),
    ));

    // Install generated Window bindings onto the real vm-js realm.
    {
      let (vm, heap, _realm) = webidl_vm_js::split_js_runtime(&mut runtime);
      let mut cx = VmJsWebIdlBindingsCx::new(vm, heap, &state);
      let mut host = NoopBindingsHost::default();
      install_window_bindings(&mut cx, &mut host)?;
    }

    // Inspect property descriptors directly via `Scope::ordinary_get_own_property`.
    let (_vm, heap, realm) = webidl_vm_js::split_js_runtime(&mut runtime);
    let global = realm.global_object();
    let intr = realm.intrinsics();

    let mut scope = heap.scope();
    scope.push_root(Value::Object(global))?;

    // globalThis.URLSearchParams
    let ctor_key_s = scope.alloc_string("URLSearchParams")?;
    scope.push_root(Value::String(ctor_key_s))?;
    let ctor_key = PropertyKey::from_string(ctor_key_s);
    let ctor_desc =
      scope
        .ordinary_get_own_property(global, ctor_key)?
        .ok_or(VmError::InvariantViolation(
          "URLSearchParams constructor missing from global object",
        ))?;
    let PropertyKind::Data {
      value: Value::Object(ctor_obj),
      ..
    } = ctor_desc.kind
    else {
      return Err(VmError::TypeError("URLSearchParams is not a data property"));
    };
    scope.push_root(Value::Object(ctor_obj))?;

    // URLSearchParams.prototype descriptor attributes.
    let prototype_key_s = scope.alloc_string("prototype")?;
    scope.push_root(Value::String(prototype_key_s))?;
    let prototype_key = PropertyKey::from_string(prototype_key_s);
    let proto_desc = scope
      .ordinary_get_own_property(ctor_obj, prototype_key)?
      .ok_or(VmError::TypeError(
        "URLSearchParams is missing own .prototype",
      ))?;
    assert!(!proto_desc.enumerable);
    assert!(!proto_desc.configurable);
    let (proto_obj, proto_writable) = match proto_desc.kind {
      PropertyKind::Data {
        value: Value::Object(o),
        writable,
      } => (o, writable),
      _ => {
        return Err(VmError::TypeError(
          "URLSearchParams.prototype is not a data property",
        ))
      }
    };
    assert!(
      !proto_writable,
      "URLSearchParams.prototype should be non-writable"
    );
    scope.push_root(Value::Object(proto_obj))?;
    assert_eq!(
      scope.object_get_prototype(proto_obj)?,
      Some(intr.object_prototype())
    );

    // URLSearchParams.prototype.constructor descriptor attributes.
    let constructor_key_s = scope.alloc_string("constructor")?;
    scope.push_root(Value::String(constructor_key_s))?;
    let constructor_key = PropertyKey::from_string(constructor_key_s);
    let ctor_link_desc = scope
      .ordinary_get_own_property(proto_obj, constructor_key)?
      .ok_or(VmError::TypeError(
        "URLSearchParams.prototype is missing own .constructor",
      ))?;
    assert!(!ctor_link_desc.enumerable);
    assert!(!ctor_link_desc.configurable);
    let (ctor_link_obj, ctor_link_writable) = match ctor_link_desc.kind {
      PropertyKind::Data {
        value: Value::Object(o),
        writable,
      } => (o, writable),
      _ => {
        return Err(VmError::TypeError(
          "prototype.constructor is not a data property",
        ))
      }
    };
    assert!(
      !ctor_link_writable,
      "URLSearchParams.prototype.constructor should be non-writable"
    );
    assert_eq!(ctor_link_obj, ctor_obj);

    // URLSearchParams.prototype.append descriptor attributes.
    let append_key_s = scope.alloc_string("append")?;
    scope.push_root(Value::String(append_key_s))?;
    let append_key = PropertyKey::from_string(append_key_s);
    let append_desc = scope
      .ordinary_get_own_property(proto_obj, append_key)?
      .ok_or(VmError::TypeError(
        "URLSearchParams.prototype is missing own .append",
      ))?;
    assert!(!append_desc.enumerable);
    assert!(append_desc.configurable);
    let (append_fn, append_writable) = match append_desc.kind {
      PropertyKind::Data {
        value: Value::Object(o),
        writable,
      } => (o, writable),
      _ => {
        return Err(VmError::TypeError(
          "URLSearchParams.prototype.append is not a data property",
        ))
      }
    };
    assert!(append_writable);
    scope.push_root(Value::Object(append_fn))?;
    assert_eq!(
      scope.object_get_prototype(append_fn)?,
      Some(intr.function_prototype())
    );

    // append.name === "append" (non-enumerable)
    let name_key_s = scope.alloc_string("name")?;
    scope.push_root(Value::String(name_key_s))?;
    let name_key = PropertyKey::from_string(name_key_s);
    let name_desc = scope
      .ordinary_get_own_property(append_fn, name_key)?
      .ok_or(VmError::TypeError("append is missing own .name"))?;
    assert!(!name_desc.enumerable);
    let PropertyKind::Data {
      value: Value::String(name_s),
      ..
    } = name_desc.kind
    else {
      return Err(VmError::TypeError(
        "append.name is not a string data property",
      ));
    };
    assert_eq!(scope.heap().get_string(name_s)?.to_utf8_lossy(), "append");

    // append.length === 2 (non-enumerable)
    let length_key_s = scope.alloc_string("length")?;
    scope.push_root(Value::String(length_key_s))?;
    let length_key = PropertyKey::from_string(length_key_s);
    let length_desc = scope
      .ordinary_get_own_property(append_fn, length_key)?
      .ok_or(VmError::TypeError("append is missing own .length"))?;
    assert!(!length_desc.enumerable);
    let PropertyKind::Data {
      value: Value::Number(len),
      ..
    } = length_desc.kind
    else {
      return Err(VmError::TypeError(
        "append.length is not a number data property",
      ));
    };
    assert_eq!(len, 2.0);

    Ok(())
  }

  #[derive(Default, Debug)]
  struct SeqHost {
    last: Vec<String>,
  }

  fn take_sequence<'a>(
    rt: &mut VmJsWebIdlBindingsCx<'a, SeqHost>,
    host: &mut SeqHost,
    _this: Value,
    args: &[Value],
  ) -> Result<Value, VmError> {
    let v0 = args.get(0).copied().unwrap_or(Value::Undefined);

    if !rt.is_object(v0) {
      return Err(rt.throw_type_error("expected object for sequence<DOMString>"));
    }

    let out: Vec<String> = rt.with_stack_roots(&[v0], |rt| {
      let mut iterator_record = rt.get_iterator(host, v0)?;
      rt.with_stack_roots(
        &[iterator_record.iterator, iterator_record.next_method],
        |rt| {
          let mut out = Vec::<String>::new();
          while let Some(next) = rt.iterator_step_value(host, &mut iterator_record)? {
            if out.len() >= rt.limits().max_sequence_length {
              return Err(rt.throw_range_error("sequence exceeds maximum length"));
            }
            let s = rt.to_string(host, next)?;
            out.push(rt.js_string_to_rust_string(s)?);
          }
          Ok(out)
        },
      )
    })?;

    host.last = out;
    Ok(rt.js_undefined())
  }

  fn alloc_key(scope: &mut Scope<'_>, name: &str) -> Result<PropertyKey, VmError> {
    let s = scope.alloc_string(name)?;
    scope.push_root(Value::String(s))?;
    Ok(PropertyKey::from_string(s))
  }

  fn iterator_return_this(
    _vm: &mut Vm,
    _scope: &mut Scope<'_>,
    _host: &mut dyn VmHost,
    _hooks: &mut dyn VmHostHooks,
    _callee: GcObject,
    this: Value,
    _args: &[Value],
  ) -> Result<Value, VmError> {
    Ok(this)
  }

  fn iterator_next_call(
    _vm: &mut Vm,
    scope: &mut Scope<'_>,
    _host: &mut dyn VmHost,
    _hooks: &mut dyn VmHostHooks,
    _callee: GcObject,
    this: Value,
    _args: &[Value],
  ) -> Result<Value, VmError> {
    let Value::Object(iter_obj) = this else {
      return Err(VmError::TypeError(
        "iterator.next called with non-object receiver",
      ));
    };

    let items_key = alloc_key(scope, "items")?;
    let index_key = alloc_key(scope, "index")?;

    let items = scope
      .heap()
      .object_get_own_data_property_value(iter_obj, &items_key)?
      .unwrap_or(Value::Undefined);
    let Value::Object(items_obj) = items else {
      return Err(VmError::TypeError("iterator items is not object"));
    };

    let index = scope
      .heap()
      .object_get_own_data_property_value(iter_obj, &index_key)?
      .unwrap_or(Value::Number(0.0));
    let idx = match index {
      Value::Number(n) => n as usize,
      _ => 0,
    };

    // Read items.length (array objects store it as an own property in `vm-js`).
    let length_key = alloc_key(scope, "length")?;
    let len_value = scope
      .heap()
      .object_get_own_data_property_value(items_obj, &length_key)?
      .unwrap_or(Value::Number(0.0));
    let len = match len_value {
      Value::Number(n) => n as usize,
      _ => 0,
    };

    let done = idx >= len;
    let value = if done {
      Value::Undefined
    } else {
      let idx_key = alloc_key(scope, &idx.to_string())?;
      scope
        .heap()
        .object_get_own_data_property_value(items_obj, &idx_key)?
        .unwrap_or(Value::Undefined)
    };

    // index++
    scope.heap_mut().object_set_existing_data_property_value(
      iter_obj,
      &index_key,
      Value::Number((idx + 1) as f64),
    )?;

    // Return result object { value, done }.
    let result_obj = scope.alloc_object()?;
    scope.push_root(Value::Object(result_obj))?;

    let value_key = alloc_key(scope, "value")?;
    let done_key = alloc_key(scope, "done")?;
    scope.define_property(
      result_obj,
      value_key,
      PropertyDescriptor {
        enumerable: true,
        configurable: true,
        kind: PropertyKind::Data {
          value,
          writable: true,
        },
      },
    )?;
    scope.define_property(
      result_obj,
      done_key,
      PropertyDescriptor {
        enumerable: true,
        configurable: true,
        kind: PropertyKind::Data {
          value: Value::Bool(done),
          writable: true,
        },
      },
    )?;

    Ok(Value::Object(result_obj))
  }

  fn make_custom_iterator(
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    object_proto: GcObject,
    sym_iterator: vm_js::GcSymbol,
  ) -> Result<Value, VmError> {
    // items = ["a", 2, true]
    let a = Value::String(scope.alloc_string("a")?);
    scope.push_root(a)?;
    let items = vm.call_without_host(
      scope,
      Value::Object(vm.intrinsics().unwrap().array_constructor()),
      Value::Undefined,
      &[a, Value::Number(2.0), Value::Bool(true)],
    )?;
    let Value::Object(items_obj) = items else {
      return Err(VmError::InvariantViolation(
        "Array constructor returned non-object",
      ));
    };
    scope.push_root(items)?;

    // iterator = { items, index: 0, next, [Symbol.iterator]: () => this }
    let iterator_obj = scope.alloc_object()?;
    scope.push_root(Value::Object(iterator_obj))?;

    // Ensure the iterator's prototype isn't accidentally treated as an Array by our runtime's
    // conversion fast-path.
    scope
      .heap_mut()
      .object_set_prototype(iterator_obj, Some(object_proto))?;

    let items_key = alloc_key(scope, "items")?;
    scope.define_property(
      iterator_obj,
      items_key,
      PropertyDescriptor {
        enumerable: true,
        configurable: true,
        kind: PropertyKind::Data {
          value: Value::Object(items_obj),
          writable: true,
        },
      },
    )?;
    let index_key = alloc_key(scope, "index")?;
    scope.define_property(
      iterator_obj,
      index_key,
      PropertyDescriptor {
        enumerable: true,
        configurable: true,
        kind: PropertyKind::Data {
          value: Value::Number(0.0),
          writable: true,
        },
      },
    )?;

    // next()
    let next_id: NativeFunctionId = vm.register_native_call(iterator_next_call)?;
    let next_name = scope.alloc_string("next")?;
    scope.push_root(Value::String(next_name))?;
    let next_fn = scope.alloc_native_function(next_id, None, next_name, 0)?;
    scope.push_root(Value::Object(next_fn))?;
    let next_key = alloc_key(scope, "next")?;
    scope.define_property(
      iterator_obj,
      next_key,
      PropertyDescriptor {
        enumerable: true,
        configurable: true,
        kind: PropertyKind::Data {
          value: Value::Object(next_fn),
          writable: true,
        },
      },
    )?;

    // [Symbol.iterator]()
    let iter_id: NativeFunctionId = vm.register_native_call(iterator_return_this)?;
    let iter_name = scope.alloc_string("iterator")?;
    scope.push_root(Value::String(iter_name))?;
    let iter_fn = scope.alloc_native_function(iter_id, None, iter_name, 0)?;
    scope.push_root(Value::Object(iter_fn))?;
    let key = PropertyKey::from_symbol(sym_iterator);
    scope.define_property(
      iterator_obj,
      key,
      PropertyDescriptor {
        enumerable: true,
        configurable: true,
        kind: PropertyKind::Data {
          value: Value::Object(iter_fn),
          writable: true,
        },
      },
    )?;

    Ok(Value::Object(iterator_obj))
  }

  fn assert_range_error(
    scope: &mut Scope<'_>,
    realm: &vm_js::Realm,
    err: VmError,
  ) -> Result<(), VmError> {
    let Some(thrown) = err.thrown_value() else {
      return Err(VmError::TypeError("expected thrown JS value"));
    };
    let Value::Object(obj) = thrown else {
      return Err(VmError::TypeError("expected thrown object"));
    };
    let proto = scope.heap().object_prototype(obj)?;
    assert_eq!(proto, Some(realm.intrinsics().range_error_prototype()));
    Ok(())
  }

  #[test]
  fn vmjs_sequence_domstring_via_iterator_protocol_and_limits() -> Result<(), VmError> {
    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(HeapLimits::new(16 * 1024 * 1024, 8 * 1024 * 1024));
    let mut runtime = VmJsRuntime::new(vm, heap)?;

    let mut limits = WebIdlLimits::default();
    limits.max_sequence_length = 8;

    let mut state = Box::new(VmJsWebIdlBindingsState::<SeqHost>::new(
      runtime.realm().global_object(),
      limits,
      Box::new(NoHooks),
    ));

    // Install `takeSequence` on the global object.
    {
      let (vm, heap, _realm) = webidl_vm_js::split_js_runtime(&mut runtime);
      let mut cx = VmJsWebIdlBindingsCx::new(vm, heap, &state);
      let func = cx.create_function("takeSequence", 1, take_sequence)?;
      let global = cx.global_object()?;
      cx.define_data_property_str(
        global,
        "takeSequence",
        func,
        DataPropertyAttributes::new(true, true, true),
      )?;
    }

    let mut host = SeqHost::default();
    let mut hooks = MicrotaskQueue::new();

    // Drive calls directly through the VM.
    let (vm, heap, realm) = webidl_vm_js::split_js_runtime(&mut runtime);
    let mut scope = heap.scope();

    let intr = realm.intrinsics();
    let global = realm.global_object();
    scope.push_root(Value::Object(global))?;

    let take_key = alloc_key(&mut scope, "takeSequence")?;
    let take_fn = vm.get(&mut scope, global, take_key)?;

    // --- array input ----------------------------------------------------------
    let s_x = scope.alloc_string("x")?;
    scope.push_root(Value::String(s_x))?;
    let arr = vm.call_with_host_and_hooks(
      &mut host,
      &mut scope,
      &mut hooks,
      Value::Object(intr.array_constructor()),
      Value::Undefined,
      &[Value::String(s_x), Value::Number(2.0), Value::Bool(true)],
    )?;
    scope.push_root(arr)?;
    vm.call_with_host_and_hooks(
      &mut host,
      &mut scope,
      &mut hooks,
      take_fn,
      Value::Undefined,
      &[arr],
    )?;
    assert_eq!(host.last, vec!["x", "2", "true"]);

    // --- custom iterator input ------------------------------------------------
    let custom_iter = make_custom_iterator(
      vm,
      &mut scope,
      intr.object_prototype(),
      intr.well_known_symbols().iterator,
    )?;
    scope.push_root(custom_iter)?;
    vm.call_with_host_and_hooks(
      &mut host,
      &mut scope,
      &mut hooks,
      take_fn,
      Value::Undefined,
      &[custom_iter],
    )?;
    assert_eq!(host.last, vec!["a", "2", "true"]);

    // --- limit enforcement ----------------------------------------------------
    state.limits.max_sequence_length = 2;
    let err = vm
      .call_with_host_and_hooks(
        &mut host,
        &mut scope,
        &mut hooks,
        take_fn,
        Value::Undefined,
        &[arr],
      )
      .unwrap_err();
    assert_range_error(&mut scope, realm, err)?;

    Ok(())
  }
}
