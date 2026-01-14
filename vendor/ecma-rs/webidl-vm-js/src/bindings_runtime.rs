//! Runtime helpers for installing and invoking generated WebIDL bindings in `vm-js`.
//!
//! Generated bindings need a small amount of glue around the low-level `vm-js` API:
//! - allocating native functions,
//! - defining properties with correct attribute flags,
//! - setting prototype chains safely (rooting across allocations),
//! - interning common property-key strings to avoid repeated UTF-16 allocations,
//! - converting host-facing `BindingValue` containers back to JS values, and
//! - throwing spec-shaped `TypeError`/`RangeError` objects.
//!
//! This module intentionally does **not** implement DOM/Web API behaviour. It only defines the
//! binding-layer runtime surface.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::ops::{Deref, DerefMut};

use vm_js::{
  GcObject, GcString, Heap, NativeConstruct, NativeConstructId, NativeFunctionId,
  PropertyDescriptor, PropertyKey, PropertyKind, Scope, Value, Vm, VmError, VmHost,
  VmHostHooks,
};

use webidl::WebIdlLimits;

use crate::{VmJsHostHooksPayload, WebIdlBindingsHost};

/// A minimally-typed value container used by generated binding shims when crossing into the host.
///
/// This is intentionally **not** a full JS value model.
/// - Objects/functions/symbols are passed through as opaque `vm_js::Value` handles.
/// - Strings can be passed either as a GC handle ([`BindingValue::String`]) or as a Rust-owned
///   UTF-8 string ([`BindingValue::RustString`]).
#[derive(Debug, Clone, PartialEq)]
pub enum BindingValue {
  Undefined,
  Null,
  Bool(bool),
  Number(f64),
  /// A GC-managed JS string handle.
  String(GcString),
  /// A Rust-owned UTF-8 string (used by conversions like `DOMString`).
  RustString(String),
  /// Opaque JS value handle (usually an object, but may be any `Value`).
  Object(Value),
  Sequence(Vec<BindingValue>),
  Dictionary(BTreeMap<String, BindingValue>),
}

impl BindingValue {
  /// Minimal JS -> host conversion used by early scaffolding/tests.
  ///
  /// Generated bindings are expected to perform full WebIDL conversions themselves; this helper
  /// is only intended for trivial pass-through cases.
  #[inline]
  pub fn from_js(value: Value) -> Self {
    match value {
      Value::Undefined => Self::Undefined,
      Value::Null => Self::Null,
      Value::Bool(b) => Self::Bool(b),
      Value::Number(n) => Self::Number(n),
      Value::String(s) => Self::String(s),
      other => Self::Object(other),
    }
  }
}

/// Convert an ECMAScript `Number` to `i32` using the `ToInt32` algorithm.
///
/// This is used by generated bindings when converting WebIDL integer types from `f64`.
#[inline]
pub fn to_int32_f64(n: f64) -> i32 {
  let attrs = webidl::IntegerConversionAttrs::default();
  match webidl::convert_to_int(n, 32, true, attrs) {
    Ok(v) => v as i32,
    Err(_) => {
      debug_assert!(false, "default ToInt32 conversion should never error");
      0
    }
  }
}

/// Convert an ECMAScript `Number` to `u32` using the `ToUint32` algorithm.
///
/// This is used by generated bindings when converting WebIDL integer types from `f64`.
#[inline]
pub fn to_uint32_f64(n: f64) -> u32 {
  let attrs = webidl::IntegerConversionAttrs::default();
  match webidl::convert_to_int(n, 32, false, attrs) {
    Ok(v) => v as u32,
    Err(_) => {
      debug_assert!(false, "default ToUint32 conversion should never error");
      0
    }
  }
}

/// Attributes for a data property definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataPropertyAttributes {
  pub writable: bool,
  pub enumerable: bool,
  pub configurable: bool,
}

impl DataPropertyAttributes {
  #[inline]
  pub const fn new(writable: bool, enumerable: bool, configurable: bool) -> Self {
    Self {
      writable,
      enumerable,
      configurable,
    }
  }

  /// Typical attributes for WebIDL operations: writable, **non-enumerable**, configurable.
  pub const METHOD: Self = Self::new(true, false, true);

  /// Typical attributes for WebIDL `const` values: non-writable, enumerable, non-configurable.
  pub const CONST: Self = Self::new(false, true, false);

  /// Typical attributes for interface constructors installed on the global object.
  ///
  /// Most Web IDL interface objects are exposed as:
  /// - writable
  /// - non-enumerable
  /// - configurable
  /// data properties on the realm global.
  pub const CONSTRUCTOR: Self = Self::new(true, false, true);

  /// Typical attributes for `Interface.prototype` (constructor → prototype link).
  ///
  /// Browsers commonly define `prototype` as:
  /// - non-writable
  /// - non-enumerable
  /// - non-configurable
  /// even for interface objects (unlike ordinary user-defined JS functions).
  pub const CONSTRUCTOR_PROTOTYPE: Self = Self::new(false, false, false);

  /// Typical attributes for `Interface.prototype.constructor` (prototype → constructor link).
  ///
  /// Browsers commonly define `constructor` as:
  /// - non-writable
  /// - non-enumerable
  /// - non-configurable
  /// for Web IDL prototype objects.
  pub const PROTOTYPE_CONSTRUCTOR: Self = Self::new(false, false, false);

  /// Typical attributes for constructor ↔ prototype links (`ctor.prototype` and `proto.constructor`):
  /// non-writable, non-enumerable, non-configurable.
  ///
  /// Alias for [`DataPropertyAttributes::CONSTRUCTOR_PROTOTYPE`] /
  /// [`DataPropertyAttributes::PROTOTYPE_CONSTRUCTOR`].
  pub const CONSTRUCTOR_LINK: Self = Self::CONSTRUCTOR_PROTOTYPE;
}

/// Attributes for an accessor property definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccessorPropertyAttributes {
  pub enumerable: bool,
  pub configurable: bool,
}

impl AccessorPropertyAttributes {
  #[inline]
  pub const fn new(enumerable: bool, configurable: bool) -> Self {
    Self {
      enumerable,
      configurable,
    }
  }

  /// Typical attributes for WebIDL interface attributes: **non-enumerable**, configurable.
  ///
  /// Note: This matches the behavior asserted by WPT and typical browser descriptor flags for
  /// interface prototype attributes (e.g. `TreeWalker.prototype.root`).
  pub const ATTRIBUTE: Self = Self::new(false, true);
}

/// Host-defined behaviour implementation for WebIDL bindings (vm-js flavour).
///
/// Generated bindings are responsible for:
/// - overload resolution,
/// - argument conversion,
/// - return value conversion (to [`BindingValue`]).
///
/// The host is responsible for implementing the actual DOM/Web API behaviour.
///
/// This trait is intentionally object-safe so native call handlers can downcast the `&mut dyn
/// VmHost` they receive into a [`BindingsHost`] wrapper and call into an underlying `dyn
/// WebHostBindingsVm`.
pub trait WebHostBindingsVm {
  fn call_operation(
    &mut self,
    receiver: Option<Value>,
    interface: &'static str,
    member: &'static str,
    overload: usize,
    args: Vec<BindingValue>,
  ) -> Result<BindingValue, VmError>;

  fn call_constructor(
    &mut self,
    interface: &'static str,
    overload: usize,
    args: Vec<BindingValue>,
  ) -> Result<BindingValue, VmError>;
}

/// A [`Scope`] wrapper for generated WebIDL bindings that enforces [`WebIdlLimits`].
///
/// The generator frequently calls `rt.scope.to_string(...)` directly. By providing an inherent
/// `to_string` method with the same signature as [`Scope::to_string`], we can enforce resource
/// limits without regenerating bindings.
pub struct WebIdlBindingsScope<'a> {
  inner: Scope<'a>,
  limits: WebIdlLimits,
}

impl<'a> WebIdlBindingsScope<'a> {
  #[inline]
  pub fn new(scope: Scope<'a>, limits: WebIdlLimits) -> Self {
    Self {
      inner: scope,
      limits,
    }
  }

  #[inline]
  pub fn set_limits(&mut self, limits: WebIdlLimits) {
    self.limits = limits;
  }

  /// Like [`Scope::to_string`], but enforces [`WebIdlLimits::max_string_code_units`].
  pub fn to_string(
    &mut self,
    vm: &mut Vm,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    value: Value,
  ) -> Result<GcString, VmError> {
    let s = self.inner.to_string(vm, host, hooks, value)?;

    let len = self.inner.heap().get_string(s)?.as_code_units().len();
    if len > self.limits.max_string_code_units {
      let Some(intr) = vm.intrinsics() else {
        return Err(VmError::Unimplemented(
          "throw_range_error requires initialized realm intrinsics",
        ));
      };
      return match vm_js::new_error(
        &mut self.inner,
        intr.range_error_prototype(),
        "RangeError",
        "string exceeds maximum length",
      ) {
        Ok(value) => Err(VmError::Throw(value)),
        Err(err) => Err(err),
      };
    }

    Ok(s)
  }
}

impl<'a> Deref for WebIdlBindingsScope<'a> {
  type Target = Scope<'a>;

  #[inline]
  fn deref(&self) -> &Self::Target {
    &self.inner
  }
}

impl<'a> DerefMut for WebIdlBindingsScope<'a> {
  #[inline]
  fn deref_mut(&mut self) -> &mut Self::Target {
    &mut self.inner
  }
}

/// A `vm-js` host-context wrapper that exposes a `dyn WebHostBindingsVm` implementation.
///
/// `vm-js` native call handlers receive `&mut dyn VmHost`. Downcasting `VmHost` directly to a
/// concrete host type is not possible from generated code, because generated code does not know
/// the concrete host type.
///
/// Instead, embeddings can pass a `BindingsHost` as the `VmHost` context. Generated call handlers
/// can downcast to `BindingsHost` and then call into `dyn WebHostBindingsVm`.
pub struct BindingsHost {
  inner: *mut dyn WebHostBindingsVm,
}

impl BindingsHost {
  pub fn new(inner: &mut dyn WebHostBindingsVm) -> Self {
    // `dyn Trait` defaults to `dyn Trait + 'static` in most positions, including raw pointers.
    // `inner` is *not* `'static`, but we only use the pointer during the VM call boundary where the
    // embedding guarantees it remains valid. Erase the lifetime so `BindingsHost` can be `Any` and
    // downcasted via `VmHost::as_any[_mut]`.
    let inner: *mut (dyn WebHostBindingsVm + 'static) = unsafe {
      std::mem::transmute::<*mut (dyn WebHostBindingsVm + '_), *mut (dyn WebHostBindingsVm + 'static)>(
        inner as *mut (dyn WebHostBindingsVm + '_),
      )
    };
    Self { inner }
  }

  /// Returns the underlying bindings host.
  ///
  /// # Safety
  ///
  /// This is safe as long as the `BindingsHost` was constructed from a valid mutable reference and
  /// the reference does not escape the call boundary.
  #[inline]
  pub fn bindings_mut(&mut self) -> &mut (dyn WebHostBindingsVm + 'static) {
    // SAFETY: `inner` is created from `&mut dyn WebHostBindingsVm` in `new` and is only used while
    // the embedding guarantees the pointee outlives the call.
    unsafe { &mut *self.inner }
  }
}

impl WebIdlBindingsHost for BindingsHost {
  fn call_operation(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    receiver: Option<Value>,
    interface: &'static str,
    operation: &'static str,
    overload: usize,
    args: &[Value],
  ) -> Result<Value, VmError> {
    let args = args
      .iter()
      .copied()
      .map(BindingValue::from_js)
      .collect::<Vec<_>>();
    let result = self
      .bindings_mut()
      .call_operation(receiver, interface, operation, overload, args)?;
    binding_value_to_js(vm, scope, result)
  }

  fn call_constructor(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    interface: &'static str,
    overload: usize,
    args: &[Value],
    _new_target: Value,
  ) -> Result<Value, VmError> {
    let args = args
      .iter()
      .copied()
      .map(BindingValue::from_js)
      .collect::<Vec<_>>();
    let result = self
      .bindings_mut()
      .call_constructor(interface, overload, args)?;
    binding_value_to_js(vm, scope, result)
  }
}

/// Binding runtime context used by generated bindings.
///
/// This owns a [`Scope`] and uses stack roots to keep values live for the lifetime of the context,
/// allowing generated code to keep `Gc*` handles in locals across allocations without manual rooting.
pub struct BindingsRuntime<'a> {
  pub vm: &'a mut Vm,
  pub scope: WebIdlBindingsScope<'a>,
  limits: WebIdlLimits,
  interned: HashMap<&'static str, GcString>,
}

impl<'a> BindingsRuntime<'a> {
  /// Create a new bindings runtime context backed by `heap.scope()`.
  pub fn new(vm: &'a mut Vm, heap: &'a mut Heap) -> Self {
    Self::from_scope(vm, heap.scope())
  }

  /// Create a bindings runtime context from an existing [`Scope`], e.g. `scope.reborrow()`.
  pub fn from_scope(vm: &'a mut Vm, scope: Scope<'a>) -> Self {
    let mut limits = WebIdlLimits::default();
    if let Some(hooks_ptr) = vm.active_host_hooks_ptr() {
      // SAFETY: `hooks_ptr` comes from `Vm::active_host_hooks_ptr` and is only valid within the
      // dynamic extent of a VM entry point that installed a host hooks override (for example
      // `Vm::call_with_host*`). `BindingsRuntime::from_scope` is typically invoked from native call
      // handlers, so we are within that extent when `hooks_ptr` is present.
      let hooks: &mut dyn VmHostHooks = unsafe { &mut *hooks_ptr };
      if let Some(any) = hooks.as_any_mut() {
        if let Some(payload) = any.downcast_mut::<VmJsHostHooksPayload>() {
          limits = payload.webidl_limits();
        }
      }
    }

    Self {
      vm,
      scope: WebIdlBindingsScope::new(scope, limits),
      limits,
      interned: HashMap::new(),
    }
  }

  /// WebIDL conversion limits configured for this bindings runtime.
  ///
  /// Generated bindings should treat these limits as the source of truth when materializing
  /// potentially unbounded values such as `sequence<T>` and `record<K, V>`.
  #[inline]
  pub fn limits(&self) -> WebIdlLimits {
    self.limits
  }

  /// Override the conversion limits used by this bindings runtime.
  #[inline]
  pub fn set_limits(&mut self, limits: WebIdlLimits) {
    self.limits = limits;
    self.scope.set_limits(limits);
  }

  #[inline]
  fn root(&mut self, value: Value) -> Result<Value, VmError> {
    self.scope.push_root(value)
  }

  #[inline]
  fn root_key(&mut self, key: PropertyKey) -> Result<(), VmError> {
    match key {
      PropertyKey::String(s) => {
        let _ = self.root(Value::String(s))?;
      }
      PropertyKey::Symbol(s) => {
        let _ = self.root(Value::Symbol(s))?;
      }
    }
    Ok(())
  }

  /// Intern a `&'static str` as a GC string handle, rooting it for the lifetime of this context.
  pub fn intern_string(&mut self, s: &'static str) -> Result<GcString, VmError> {
    if let Some(handle) = self.interned.get(s).copied() {
      return Ok(handle);
    }
    let handle = self.scope.alloc_string(s)?;
    let _ = self.root(Value::String(handle))?;
    self.interned.insert(s, handle);
    Ok(handle)
  }

  /// Allocate an uninterned JS string from UTF-8, rooting it for the lifetime of this context.
  pub fn alloc_string(&mut self, s: &str) -> Result<GcString, VmError> {
    let handle = self.scope.alloc_string(s)?;
    let _ = self.root(Value::String(handle))?;
    Ok(handle)
  }

  /// Allocate a string property key, interning/keeping the underlying string live.
  pub fn property_key(&mut self, s: &'static str) -> Result<PropertyKey, VmError> {
    Ok(PropertyKey::from_string(self.intern_string(s)?))
  }

  /// Sets `obj.[[Prototype]] = proto` with cycle checks.
  pub fn set_prototype(&mut self, obj: GcObject, proto: Option<GcObject>) -> Result<(), VmError> {
    // Root inputs in case growing the root stack triggers a GC.
    let _ = self.root(Value::Object(obj))?;
    if let Some(proto) = proto {
      let _ = self.root(Value::Object(proto))?;
      self.scope.object_set_prototype(obj, Some(proto))?;
    } else {
      self.scope.object_set_prototype(obj, None)?;
    }
    Ok(())
  }

  /// Allocates an ordinary object and sets its prototype to `%Object.prototype%` when available.
  pub fn alloc_object(&mut self) -> Result<GcObject, VmError> {
    let obj = self.scope.alloc_object()?;
    let _ = self.root(Value::Object(obj))?;
    if let Some(intr) = self.vm.intrinsics() {
      let proto = intr.object_prototype();
      let _ = self.root(Value::Object(proto))?;
      self.scope.object_set_prototype(obj, Some(proto))?;
    }
    Ok(obj)
  }

  /// Allocates an ordinary object with an explicit prototype.
  ///
  /// This is primarily used by WebIDL constructors, which must create wrapper objects whose
  /// `[[Prototype]]` is the interface prototype object (e.g. `URLSearchParams.prototype`).
  pub fn alloc_object_with_prototype(
    &mut self,
    proto: Option<GcObject>,
  ) -> Result<GcObject, VmError> {
    // Root the prototype across allocation (GC can run while allocating the new object).
    if let Some(proto) = proto {
      let _ = self.root(Value::Object(proto))?;
      let obj = self.scope.alloc_object_with_prototype(Some(proto))?;
      let _ = self.root(Value::Object(obj))?;
      Ok(obj)
    } else {
      let obj = self.scope.alloc_object_with_prototype(None)?;
      let _ = self.root(Value::Object(obj))?;
      Ok(obj)
    }
  }

  /// Reads a `GcObject` from a native slot on `callee`, rooting it for the lifetime of this runtime.
  ///
  /// Generated WebIDL constructors store their interface prototype object in a native slot so
  /// `NativeConstruct` handlers can allocate wrapper objects without property lookups.
  pub fn require_native_object_slot(
    &mut self,
    callee: GcObject,
    slot_index: usize,
    what: &'static str,
  ) -> Result<GcObject, VmError> {
    // Root callee across slot access; the heap accessor shouldn't allocate, but keep patterns
    // consistent because the returned slot value is a GC handle.
    let _ = self.root(Value::Object(callee))?;

    let slots = self.scope.heap().get_function_native_slots(callee)?;
    let v = slots.get(slot_index).copied().unwrap_or(Value::Undefined);
    match v {
      Value::Object(obj) => {
        let _ = self.root(Value::Object(obj))?;
        Ok(obj)
      }
      _ => Err(VmError::InvariantViolation(what)),
    }
  }

  /// Allocates an array exotic object and sets its prototype to `%Array.prototype%` when available.
  pub fn alloc_array(&mut self, len: usize) -> Result<GcObject, VmError> {
    if len > self.limits.max_sequence_length {
      return Err(self.throw_range_error("sequence exceeds maximum length"));
    }
    let obj = self.scope.alloc_array(len)?;
    let _ = self.root(Value::Object(obj))?;
    if let Some(intr) = self.vm.intrinsics() {
      let proto = intr.array_prototype();
      let _ = self.root(Value::Object(proto))?;
      self.scope.object_set_prototype(obj, Some(proto))?;
    }
    Ok(obj)
  }

  /// Register `call` (and optional `construct`) and allocate a JS function object.
  pub fn alloc_native_function(
    &mut self,
    call: vm_js::NativeCall,
    construct: Option<NativeConstruct>,
    name: &'static str,
    length: u32,
  ) -> Result<GcObject, VmError> {
    self.alloc_native_function_impl(call, construct, name, length, &[])
  }

  /// Like [`BindingsRuntime::alloc_native_function`], but captures `slots` as native slots.
  pub fn alloc_native_function_with_slots(
    &mut self,
    call: vm_js::NativeCall,
    construct: Option<NativeConstruct>,
    name: &'static str,
    length: u32,
    slots: &[Value],
  ) -> Result<GcObject, VmError> {
    self.alloc_native_function_impl(call, construct, name, length, slots)
  }

  fn alloc_native_function_impl(
    &mut self,
    call: vm_js::NativeCall,
    construct: Option<NativeConstruct>,
    name: &'static str,
    length: u32,
    slots: &[Value],
  ) -> Result<GcObject, VmError> {
    // Root slots across any intermediate allocations (like interning the name string).
    if !slots.is_empty() {
      self.scope.push_roots(slots)?;
    }

    let call_id: NativeFunctionId = self.vm.register_native_call(call)?;
    let construct_id: Option<NativeConstructId> = match construct {
      Some(f) => Some(self.vm.register_native_construct(f)?),
      None => None,
    };

    let name_s = self.intern_string(name)?;

    let func = if slots.is_empty() {
      self
        .scope
        .alloc_native_function(call_id, construct_id, name_s, length)?
    } else {
      self
        .scope
        .alloc_native_function_with_slots(call_id, construct_id, name_s, length, slots)?
    };
    let _ = self.root(Value::Object(func))?;

    // Prefer realm intrinsics prototypes when available.
    if let Some(intr) = self.vm.intrinsics() {
      // Function objects should inherit from `%Function.prototype%`.
      self
        .scope
        .heap_mut()
        .object_set_prototype(func, Some(intr.function_prototype()))?;

      // Constructor functions have a `.prototype` object that should inherit from `%Object.prototype%`.
      if construct_id.is_some() {
        let proto_key = self.property_key("prototype")?;
        // Root key in case `get` performs allocations (it shouldn't, but keep patterns consistent).
        self.root_key(proto_key)?;
        let proto_value = self.vm.get(&mut self.scope, func, proto_key)?;
        if let Value::Object(proto_obj) = proto_value {
          self
            .scope
            .heap_mut()
            .object_set_prototype(proto_obj, Some(intr.object_prototype()))?;
        }
      }
    }

    Ok(func)
  }

  pub fn define_data_property(
    &mut self,
    obj: GcObject,
    key: PropertyKey,
    value: Value,
    attrs: DataPropertyAttributes,
  ) -> Result<(), VmError> {
    // Root inputs in case we allocate while pushing roots.
    let _ = self.root(Value::Object(obj))?;
    self.root_key(key)?;
    let _ = self.root(value)?;

    self.scope.define_property(
      obj,
      key,
      PropertyDescriptor {
        enumerable: attrs.enumerable,
        configurable: attrs.configurable,
        kind: PropertyKind::Data {
          value,
          writable: attrs.writable,
        },
      },
    )
  }

  pub fn define_data_property_str(
    &mut self,
    obj: GcObject,
    name: &'static str,
    value: Value,
    attrs: DataPropertyAttributes,
  ) -> Result<(), VmError> {
    // Root `obj`/`value` across interning the key string.
    let _ = self.root(Value::Object(obj))?;
    let _ = self.root(value)?;
    let key = self.property_key(name)?;
    self.define_data_property(obj, key, value, attrs)
  }

  pub fn define_accessor_property(
    &mut self,
    obj: GcObject,
    key: PropertyKey,
    get: Value,
    set: Value,
    attrs: AccessorPropertyAttributes,
  ) -> Result<(), VmError> {
    // Root inputs in case we allocate while pushing roots.
    let _ = self.root(Value::Object(obj))?;
    self.root_key(key)?;
    let _ = self.root(get)?;
    let _ = self.root(set)?;

    self.scope.define_property(
      obj,
      key,
      PropertyDescriptor {
        enumerable: attrs.enumerable,
        configurable: attrs.configurable,
        kind: PropertyKind::Accessor { get, set },
      },
    )
  }

  pub fn define_accessor_property_str(
    &mut self,
    obj: GcObject,
    name: &'static str,
    get: Value,
    set: Value,
    attrs: AccessorPropertyAttributes,
  ) -> Result<(), VmError> {
    // Root `obj`/`get`/`set` across interning the key string.
    let _ = self.root(Value::Object(obj))?;
    let _ = self.root(get)?;
    let _ = self.root(set)?;
    let key = self.property_key(name)?;
    self.define_accessor_property(obj, key, get, set, attrs)
  }

  /// Convert a host-returned [`BindingValue`] into a `vm-js` [`Value`].
  pub fn binding_value_to_js(&mut self, value: BindingValue) -> Result<Value, VmError> {
    binding_value_to_js_with_limits(&mut *self.vm, &mut self.scope, self.limits, value)
  }

  /// Derive the prototype used for a WebIDL constructor-created wrapper object.
  ///
  /// Generated constructors cache their interface prototype object in a native slot. When invoked
  /// with `new`, JavaScript subclassing semantics require:
  /// - defaulting to that cached prototype, and
  /// - overriding it with `new_target.prototype` when `new_target` is an object and the property is
  ///   itself an object.
  ///
  /// This follows the spirit of `GetPrototypeFromConstructor` / `OrdinaryCreateFromConstructor`.
  pub fn derive_prototype_from_new_target(
    &mut self,
    host: &mut dyn VmHost,
    hooks: &mut dyn vm_js::VmHostHooks,
    default_proto: GcObject,
    new_target: Value,
  ) -> Result<GcObject, VmError> {
    // Root inputs across property lookups (which can invoke user code and allocate).
    let _ = self.root(Value::Object(default_proto))?;
    let _ = self.root(new_target)?;

    let mut wrapper_proto = default_proto;
    if let Value::Object(new_target_obj) = new_target {
      let _ = self.root(Value::Object(new_target_obj))?;

      let proto_key = self.property_key("prototype")?;
      let candidate = self.scope.ordinary_get_with_host_and_hooks(
        &mut *self.vm,
        host,
        hooks,
        new_target_obj,
        proto_key,
        Value::Object(new_target_obj),
      )?;
      if let Value::Object(candidate_obj) = candidate {
        let _ = self.root(Value::Object(candidate_obj))?;
        wrapper_proto = candidate_obj;
      }
    }

    Ok(wrapper_proto)
  }

  /// Create and throw a realm-aware `TypeError` object with the given message.
  pub fn throw_type_error(&mut self, message: &str) -> VmError {
    let Some(intr) = self.vm.intrinsics() else {
      return VmError::Unimplemented("throw_type_error requires initialized realm intrinsics");
    };
    match vm_js::new_error(
      &mut self.scope,
      intr.type_error_prototype(),
      "TypeError",
      message,
    ) {
      Ok(value) => VmError::Throw(value),
      Err(err) => err,
    }
  }

  /// Create and throw a realm-aware `RangeError` object with the given message.
  pub fn throw_range_error(&mut self, message: &str) -> VmError {
    let Some(intr) = self.vm.intrinsics() else {
      return VmError::Unimplemented("throw_range_error requires initialized realm intrinsics");
    };
    match vm_js::new_error(
      &mut self.scope,
      intr.range_error_prototype(),
      "RangeError",
      message,
    ) {
      Ok(value) => VmError::Throw(value),
      Err(err) => err,
    }
  }

  /// Convenience for downcasting a `&mut dyn VmHost` into a `&mut BindingsHost`.
  pub fn require_bindings_host<'h>(
    &mut self,
    host: &'h mut dyn VmHost,
  ) -> Result<&'h mut BindingsHost, VmError> {
    host
      .as_any_mut()
      .downcast_mut::<BindingsHost>()
      .ok_or_else(|| self.throw_type_error("vm-js host context is not a BindingsHost"))
  }
}

/// Convert a host-returned [`BindingValue`] into a `vm-js` [`Value`].
///
/// This is a standalone helper so generated/native code that already has `&mut Vm`/`&mut Scope`
/// does not need to construct a full [`BindingsRuntime`] just for return-value conversion.
///
/// Note: this helper enforces [`WebIdlLimits::default()`]. Embeddings that need custom limits should
/// convert via [`BindingsRuntime::binding_value_to_js`] after configuring
/// [`BindingsRuntime::set_limits`].
pub fn binding_value_to_js(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  value: BindingValue,
) -> Result<Value, VmError> {
  // By default, `BindingsRuntime` uses `WebIdlLimits::default()`, so using the default limits here
  // ensures this convenience helper does not perform unbounded allocations.
  binding_value_to_js_with_limits(vm, scope, WebIdlLimits::default(), value)
}

fn binding_value_to_js_with_limits(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  limits: WebIdlLimits,
  value: BindingValue,
) -> Result<Value, VmError> {
  fn throw_range_error(vm: &Vm, scope: &mut Scope<'_>, message: &str) -> VmError {
    let Some(intr) = vm.intrinsics() else {
      return VmError::Unimplemented("throw_range_error requires initialized realm intrinsics");
    };
    match vm_js::new_error(scope, intr.range_error_prototype(), "RangeError", message) {
      Ok(value) => VmError::Throw(value),
      Err(err) => err,
    }
  }

  #[inline]
  fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
  }

  match value {
    BindingValue::Undefined => Ok(Value::Undefined),
    BindingValue::Null => Ok(Value::Null),
    BindingValue::Bool(b) => Ok(Value::Bool(b)),
    BindingValue::Number(n) => Ok(Value::Number(n)),
    BindingValue::String(s) => Ok(Value::String(s)),
    BindingValue::RustString(s) => {
      if utf16_len(&s) > limits.max_string_code_units {
        return Err(throw_range_error(vm, scope, "string exceeds maximum length"));
      }
      Ok(Value::String(scope.alloc_string(&s)?))
    }
    BindingValue::Object(v) => Ok(v),
    BindingValue::Sequence(items) => {
      if items.len() > limits.max_sequence_length {
        return Err(throw_range_error(vm, scope, "sequence exceeds maximum length"));
      }

      let arr = scope.alloc_array(items.len())?;
      scope.push_root(Value::Object(arr))?;

      if let Some(intr) = vm.intrinsics() {
        let proto = intr.array_prototype();
        scope.push_root(Value::Object(proto))?;
        scope.object_set_prototype(arr, Some(proto))?;
      }

      for (idx, item) in items.into_iter().enumerate() {
        let mut child = scope.reborrow();
        child.push_root(Value::Object(arr))?;

        // Root the key string across the recursive conversion (which may allocate/GC).
        let key_str = idx.to_string();
        if utf16_len(&key_str) > limits.max_string_code_units {
          return Err(throw_range_error(vm, &mut child, "string exceeds maximum length"));
        }
        let key_s = child.alloc_string(&key_str)?;
        child.push_root(Value::String(key_s))?;
        let key = PropertyKey::from_string(key_s);

        let v = binding_value_to_js_with_limits(vm, &mut child, limits, item)?;
        child.push_root(v)?;
        child.create_data_property_or_throw(arr, key, v)?;
      }

      Ok(Value::Object(arr))
    }
    BindingValue::Dictionary(map) => {
      if map.len() > limits.max_record_entries {
        return Err(throw_range_error(
          vm,
          scope,
          "record exceeds maximum entry count",
        ));
      }

      let obj = scope.alloc_object()?;
      scope.push_root(Value::Object(obj))?;

      if let Some(intr) = vm.intrinsics() {
        let proto = intr.object_prototype();
        scope.push_root(Value::Object(proto))?;
        scope.object_set_prototype(obj, Some(proto))?;
      }

      for (k, item) in map {
        if utf16_len(&k) > limits.max_string_code_units {
          return Err(throw_range_error(vm, scope, "string exceeds maximum length"));
        }

        let mut child = scope.reborrow();
        child.push_root(Value::Object(obj))?;

        let key_s = child.alloc_string(&k)?;
        child.push_root(Value::String(key_s))?;
        let key = PropertyKey::from_string(key_s);

        let v = binding_value_to_js_with_limits(vm, &mut child, limits, item)?;
        child.push_root(v)?;
        child.create_data_property_or_throw(obj, key, v)?;
      }

      Ok(Value::Object(obj))
    }
  }
}
