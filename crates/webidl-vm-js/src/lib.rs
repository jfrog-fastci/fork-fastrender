//! `vm-js` adapter for the `webidl` conversion/runtime traits.
//!
//! WebIDL conversions are host-driven Rust code that may keep VM handles (strings/objects/symbols)
//! in local variables across allocations. `vm-js` GC handles are *not* automatically rooted, so this
//! adapter conservatively roots values it produces/consumes for the lifetime of the conversion
//! context using a long-lived [`Scope`].
//!
//! FastRender vendors `ecma-rs` under `vendor/ecma-rs/`, which also contains an upstream crate named
//! `webidl-vm-js`. FastRender keeps a workspace-local copy of that crate here so it can be used as a
//! normal workspace member (and carry embedder-specific adjustments) without depending on the
//! vendored `ecma-rs` workspace directly. See `crates/webidl-vm-js/README.md` for sync notes.

use std::ptr::NonNull;
use vm_js::{
  ExecutionContext, GcObject, GcString, GcSymbol, Heap, JsBigInt, JsRuntime as VmJsRuntime,
  PropertyKey as VmPropertyKey, Realm, RealmId, RootId, Scope, Value, Vm, VmError, VmHost, VmHostHooks,
};
use webidl::{
  InterfaceId, IteratorResult, JsOwnPropertyDescriptor, JsPropertyKind, JsRuntime, PropertyKey,
  WebIdlHooks, WebIdlJsRuntime, WebIdlLimits, WellKnownSymbol,
};

pub mod bindings_runtime;

/// Borrow-splits a `vm-js` [`VmJsRuntime`] into its `(vm, heap, realm)` components.
///
/// `vm-js::JsRuntime` stores `vm`, `heap`, and `realm` as disjoint fields, but only exposes `&Realm`
/// via `JsRuntime::realm()`. Embeddings that need `&mut Vm` + `&mut Heap` while also referencing the
/// realm (for its global object or intrinsics) must temporarily borrow-split using a raw pointer.
///
/// # Safety
///
/// This helper is safe to call because:
/// - `vm`, `heap`, and `realm` are stored as distinct fields in `vm-js::JsRuntime`.
/// - `realm_ptr` is only used while `rt` is immutably borrowed, and
/// - the returned references are tied to the lifetime of `&mut VmJsRuntime`.
///
/// Callers must still follow normal Rust borrowing rules: do not move the runtime while the
/// returned references are live.
pub fn split_js_runtime(rt: &mut VmJsRuntime) -> (&mut Vm, &mut Heap, &Realm) {
  // SAFETY: `vm-js::JsRuntime` stores `vm`, `heap`, and `realm` as disjoint fields.
  let realm_ptr = rt.realm() as *const Realm;
  let vm = &mut rt.vm;
  let heap = &mut rt.heap;
  let realm = unsafe { &*realm_ptr };
  (vm, heap, realm)
}

/// TypeError message used when vm-js WebIDL bindings cannot find an embedder host dispatch object.
pub const WEBIDL_BINDINGS_HOST_NOT_AVAILABLE: &str =
  "WebIDL bindings host not available: VmHostHooks::as_any_mut did not expose WebIdlBindingsHostSlot";

/// Host-facing dispatch API used by vm-js realm WebIDL bindings.
///
/// `vm-js` native call handlers (`vm_js::NativeCall`) receive both:
/// - `host: &mut dyn vm_js::VmHost` (embedder-provided context), and
/// - `hooks: &mut dyn vm_js::VmHostHooks` (host hooks for Promise jobs, job callbacks, etc).
///
/// Many real call paths (including FastRender's script evaluator) use `Vm::call_with_host`, which
/// supplies a dummy `VmHost` (`()`). This means native handlers must not rely on downcasting
/// `VmHost` for access to embedder state.
///
/// The canonical mechanism for vm-js WebIDL bindings to reach embedder state is:
/// 1) the embedding stores a pointer to an implementation of this trait inside a
///    [`WebIdlBindingsHostSlot`], and
/// 2) the embedding's `VmHostHooks` implementation exposes that slot via
///    `VmHostHooks::as_any_mut()`.
///
/// Generated WebIDL `NativeCall` wrappers should then use [`host_from_hooks`] to retrieve the host.
pub trait WebIdlBindingsHost: 'static {
  /// Dispatch a WebIDL operation/constructor into the embedding.
  ///
  /// - `receiver` is `None` for static operations and constructors.
  /// - `interface`/`operation`/`overload` identify which overload was selected by the generated
  ///   binding wrapper.
  ///
  /// The embedding may use `vm`/`scope` to allocate return values or perform additional JS-visible
  /// work.
  fn call_operation(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    receiver: Option<Value>,
    interface: &'static str,
    operation: &'static str,
    overload: usize,
    args: &[Value],
  ) -> Result<Value, VmError>;

  /// Dispatch a WebIDL constructor into the embedding.
  ///
  /// `new_target` is the JavaScript `new.target` value from the construct call.
  fn call_constructor(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    interface: &'static str,
    overload: usize,
    args: &[Value],
    new_target: Value,
  ) -> Result<Value, VmError>;
}

/// Sized container used for exposing a `dyn WebIdlBindingsHost` through `VmHostHooks::as_any_mut`.
///
/// Rust's `Any::downcast_mut` requires a **sized** concrete type. Since `dyn WebIdlBindingsHost` is
/// unsized, embeddings must store a pointer to it in this slot, then return the slot from
/// `VmHostHooks::as_any_mut()`.
#[derive(Debug, Default)]
pub struct WebIdlBindingsHostSlot {
  host: Option<NonNull<dyn WebIdlBindingsHost>>,
}

impl WebIdlBindingsHostSlot {
  /// Create a slot pointing at `host`.
  #[inline]
  pub fn new(host: &mut dyn WebIdlBindingsHost) -> Self {
    Self {
      host: Some(NonNull::from(host)),
    }
  }

  /// Replace the host pointer stored in this slot.
  #[inline]
  pub fn set(&mut self, host: &mut dyn WebIdlBindingsHost) {
    self.host = Some(NonNull::from(host));
  }

  /// Clears the slot (subsequent [`host_from_hooks`] calls will throw a TypeError).
  #[inline]
  pub fn clear(&mut self) {
    self.host = None;
  }

  fn get_mut(&mut self) -> Option<&mut dyn WebIdlBindingsHost> {
    let mut ptr = self.host?;
    // SAFETY: The embedding is responsible for ensuring the stored pointer is valid for the
    // duration of any native calls that may access it via `host_from_hooks`.
    Some(unsafe { ptr.as_mut() })
  }
}

/// Retrieves the embedder WebIDL bindings host from `hooks`.
///
/// This is the canonical plumbing used by generated vm-js WebIDL `NativeCall` wrappers.
pub fn host_from_hooks<'a>(
  hooks: &'a mut dyn VmHostHooks,
) -> Result<&'a mut dyn WebIdlBindingsHost, VmError> {
  let Some(any) = hooks.as_any_mut() else {
    return Err(VmError::TypeError(WEBIDL_BINDINGS_HOST_NOT_AVAILABLE));
  };
  let Some(slot) = any.downcast_mut::<WebIdlBindingsHostSlot>() else {
    return Err(VmError::TypeError(WEBIDL_BINDINGS_HOST_NOT_AVAILABLE));
  };
  slot
    .get_mut()
    .ok_or(VmError::TypeError(WEBIDL_BINDINGS_HOST_NOT_AVAILABLE))
}

/// Kind of WebIDL callback represented by [`CallbackHandle`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallbackKind {
  /// `callback Foo = ...` (must be callable when present).
  Function,
  /// `callback interface Foo { ... }` (callable or has callable `handleEvent`).
  Interface,
}

/// A host-owned, GC-safe handle to a JavaScript callback.
///
/// This is a **persistent root** in the `vm-js` heap so it can be stored and invoked later without
/// risking use-after-GC.
#[derive(Debug)]
pub struct CallbackHandle {
  kind: CallbackKind,
  root: RootId,
  /// Optional realm to run the callback in.
  ///
  /// `vm-js` currently supports a single active realm, but the host hooks and job queue APIs are
  /// spec-shaped around realm tokens, so keep this metadata for future multi-realm support.
  realm: Option<RealmId>,
}

impl CallbackHandle {
  /// Creates a new handle by registering `value` as a persistent root.
  pub fn new(heap: &mut Heap, kind: CallbackKind, value: Value, realm: Option<RealmId>) -> Result<Self, VmError> {
    let root = heap.add_root(value)?;
    Ok(Self { kind, root, realm })
  }

  /// Convert a JS value to a callback function handle.
  ///
  /// If `allow_null_or_undefined` is true, `null`/`undefined` are accepted as "no callback" and this
  /// returns `Ok(None)`.
  pub fn from_callback_function(
    vm: &Vm,
    heap: &mut Heap,
    value: Value,
    allow_null_or_undefined: bool,
  ) -> Result<Option<Self>, VmError> {
    if allow_null_or_undefined && matches!(value, Value::Undefined | Value::Null) {
      return Ok(None);
    }
    if !heap.is_callable(value)? {
      return Err(VmError::TypeError("Value is not a callable callback function"));
    }
    Ok(Some(Self::new(
      heap,
      CallbackKind::Function,
      value,
      vm.current_realm(),
    )?))
  }

  /// Convert a JS value to a callback interface handle.
  ///
  /// If `allow_null_or_undefined` is true, `null`/`undefined` are accepted as "no callback" and this
  /// returns `Ok(None)`.
  pub fn from_callback_interface(
    vm: &mut Vm,
    heap: &mut Heap,
    value: Value,
    allow_null_or_undefined: bool,
  ) -> Result<Option<Self>, VmError> {
    if allow_null_or_undefined && matches!(value, Value::Undefined | Value::Null) {
      return Ok(None);
    }

    if heap.is_callable(value)? {
      return Ok(Some(Self::new(
        heap,
        CallbackKind::Interface,
        value,
        vm.current_realm(),
      )?));
    }

    let Value::Object(obj) = value else {
      return Err(VmError::TypeError("Value is not a callback interface object"));
    };

    // Ensure `value` stays alive across key allocation and any user code invoked by accessors.
    let mut scope = heap.scope();
    scope.push_root(value)?;

    let key_str = scope.alloc_string("handleEvent")?;
    scope.push_root(Value::String(key_str))?;
    let key = VmPropertyKey::from_string(key_str);

    let Some(_method) = vm.get_method_from_object(&mut scope, obj, key)? else {
      return Err(VmError::TypeError(
        "Callback interface object is missing a callable handleEvent method",
      ));
    };

    drop(scope);

    Ok(Some(Self::new(
      heap,
      CallbackKind::Interface,
      value,
      vm.current_realm(),
    )?))
  }

  pub fn kind(&self) -> CallbackKind {
    self.kind
  }

  pub fn root_id(&self) -> RootId {
    self.root
  }

  pub fn realm(&self) -> Option<RealmId> {
    self.realm
  }

  /// Returns the current rooted callback value.
  pub fn value(&self, heap: &Heap) -> Result<Value, VmError> {
    heap
      .get_root(self.root)
      .ok_or_else(|| VmError::invalid_handle())
  }

  /// Unregister the underlying persistent root.
  ///
  /// This consumes the handle to prevent double-unroot bugs.
  pub fn unroot(self, heap: &mut Heap) {
    heap.remove_root(self.root);
  }

  /// Invoke this callback with an explicit `this` value.
  ///
  /// For callback interface objects, `this` is ignored and `handleEvent` is called with the
  /// callback object as the receiver.
  pub fn invoke_with_this(
    &self,
    vm: &mut Vm,
    heap: &mut Heap,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    this: Value,
    args: &[Value],
  ) -> Result<Value, VmError> {
    let callback = self.value(heap)?;

    let mut scope = heap.scope();
    // Root `callback`/`this`/`args` across any allocations we do while preparing the call (e.g.
    // allocating the `"handleEvent"` property key).
    let roots_len = args.len().checked_add(2).ok_or(VmError::OutOfMemory)?;
    let mut roots = Vec::<Value>::new();
    roots
      .try_reserve_exact(roots_len)
      .map_err(|_| VmError::OutOfMemory)?;
    roots.push(callback);
    roots.push(this);
    roots.extend_from_slice(args);
    scope.push_roots(&roots)?;

    let mut call = |vm: &mut Vm, scope: &mut Scope<'_>| -> Result<Value, VmError> {
      match self.kind {
        CallbackKind::Function => vm.call_with_host_and_hooks(host, scope, hooks, callback, this, args),
        CallbackKind::Interface => {
          if scope.heap().is_callable(callback)? {
            return vm.call_with_host_and_hooks(host, scope, hooks, callback, this, args);
          }

          let Value::Object(obj) = callback else {
            return Err(VmError::TypeError("Callback interface value is not an object"));
          };

          // `handleEvent`
          let key_str = scope.alloc_string("handleEvent")?;
          scope.push_root(Value::String(key_str))?;
          let key = VmPropertyKey::from_string(key_str);
          // Implement `GetMethod` + `[[Get]]` directly so accessor getters are invoked via
          // `call_with_host_and_hooks` (preserving embedder host context / host hooks).
          let method = match scope.heap().get_property(obj, &key)? {
            None => Value::Undefined,
            Some(desc) => match desc.kind {
              vm_js::PropertyKind::Data { value, .. } => value,
              vm_js::PropertyKind::Accessor { get, .. } => {
                if matches!(get, Value::Undefined) {
                  Value::Undefined
                } else {
                  if !scope.heap().is_callable(get)? {
                    return Err(VmError::TypeError("accessor getter is not callable"));
                  }
                  vm.call_with_host_and_hooks(host, scope, hooks, get, callback, &[])?
                }
              }
            },
          };
          if matches!(method, Value::Undefined | Value::Null) {
            return Err(VmError::TypeError(
              "Callback interface object is missing a callable handleEvent method",
            ));
          }
          if !scope.heap().is_callable(method)? {
            return Err(VmError::TypeError("GetMethod: target is not callable"));
          }
          scope.push_root(method)?;

          vm.call_with_host_and_hooks(host, scope, hooks, method, callback, args)
        }
      }
    };

    if let Some(realm) = self.realm {
      let mut vm = vm.execution_context_guard(ExecutionContext {
        realm,
        script_or_module: None,
      });
      call(&mut vm, &mut scope)
    } else {
      call(vm, &mut scope)
    }
  }

  /// Convenience wrapper around [`CallbackHandle::invoke_with_this`] that uses `undefined` as the
  /// receiver for callable callbacks.
  pub fn invoke(
    &self,
    vm: &mut Vm,
    heap: &mut Heap,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    args: &[Value],
  ) -> Result<Value, VmError> {
    self.invoke_with_this(vm, heap, host, hooks, Value::Undefined, args)
  }
}
/// `webidl` conversion context backed by `vm-js`.
pub struct VmJsWebIdlCx<'a> {
  pub vm: &'a mut Vm,
  pub scope: Scope<'a>,
  pub limits: WebIdlLimits,
  pub hooks: &'a dyn WebIdlHooks<Value>,
  host: Option<&'a mut dyn VmHost>,
  host_hooks: Option<&'a mut dyn VmHostHooks>,
  well_known_iterator: Option<GcSymbol>,
  well_known_async_iterator: Option<GcSymbol>,
}

impl<'a> VmJsWebIdlCx<'a> {
  pub fn from_scope(
    vm: &'a mut Vm,
    scope: Scope<'a>,
    limits: WebIdlLimits,
    hooks: &'a dyn WebIdlHooks<Value>,
  ) -> Self {
    Self {
      vm,
      scope,
      limits,
      hooks,
      host: None,
      host_hooks: None,
      well_known_iterator: None,
      well_known_async_iterator: None,
    }
  }

  pub fn new(
    vm: &'a mut Vm,
    heap: &'a mut Heap,
    limits: WebIdlLimits,
    hooks: &'a dyn WebIdlHooks<Value>,
  ) -> Self {
    Self::from_scope(vm, heap.scope(), limits, hooks)
  }

  pub fn new_in_scope(
    vm: &'a mut Vm,
    scope: &'a mut Scope<'_>,
    limits: WebIdlLimits,
    hooks: &'a dyn WebIdlHooks<Value>,
  ) -> Self {
    Self {
      vm,
      scope: scope.reborrow(),
      limits,
      hooks,
      host: None,
      host_hooks: None,
      well_known_iterator: None,
      well_known_async_iterator: None,
    }
  }

  /// Creates a conversion context suitable for use inside a `vm-js` native call/construct handler.
  ///
  /// This captures the active embedder host context and host hooks so WebIDL conversions that need
  /// to call back into JS (iterator protocol, callbacks, etc.) can route through the embedder's
  /// active `VmHostHooks` instead of `Vm::call_without_host`.
  pub fn from_native_call(
    vm: &'a mut Vm,
    scope: &'a mut Scope<'_>,
    host: &'a mut dyn VmHost,
    host_hooks: &'a mut dyn VmHostHooks,
    limits: WebIdlLimits,
    hooks: &'a dyn WebIdlHooks<Value>,
  ) -> Self {
    Self {
      vm,
      scope: scope.reborrow(),
      limits,
      hooks,
      host: Some(host),
      host_hooks: Some(host_hooks),
      well_known_iterator: None,
      well_known_async_iterator: None,
    }
  }

  /// Convenience helper: `hooks.is_platform_object`.
  #[inline]
  pub fn is_platform_object(&self, value: Value) -> bool {
    self.hooks.is_platform_object(value)
  }

  /// Convenience helper: `hooks.implements_interface`.
  #[inline]
  pub fn implements_interface(&self, value: Value, interface: InterfaceId) -> bool {
    self.hooks.implements_interface(value, interface)
  }

  fn root(&mut self, value: Value) -> Result<(), VmError> {
    self.scope.push_root(value)?;
    Ok(())
  }

  fn call_js(&mut self, callee: Value, this: Value, args: &[Value]) -> Result<Value, VmError> {
    match (self.host.as_deref_mut(), self.host_hooks.as_deref_mut()) {
      (Some(host), Some(hooks)) => self
        .vm
        .call_with_host_and_hooks(host, &mut self.scope, hooks, callee, this, args),
      (Some(host), None) => self.vm.call(host, &mut self.scope, callee, this, args),
      (None, Some(hooks)) => self.vm.call_with_host(&mut self.scope, hooks, callee, this, args),
      (None, None) => {
        let mut dummy_host = ();
        self.vm.call(&mut dummy_host, &mut self.scope, callee, this, args)
      }
    }
  }

  fn to_vm_property_key(key: PropertyKey<GcString, GcSymbol>) -> VmPropertyKey {
    match key {
      PropertyKey::String(s) => VmPropertyKey::from_string(s),
      PropertyKey::Symbol(s) => VmPropertyKey::from_symbol(s),
    }
  }

  fn from_vm_property_key(key: VmPropertyKey) -> PropertyKey<GcString, GcSymbol> {
    match key {
      VmPropertyKey::String(s) => PropertyKey::String(s),
      VmPropertyKey::Symbol(s) => PropertyKey::Symbol(s),
    }
  }

  fn get_well_known_symbol_cached(&mut self, sym: WellKnownSymbol) -> Result<GcSymbol, VmError> {
    if let Some(intrinsics) = self.vm.intrinsics() {
      let syms = intrinsics.well_known_symbols();
      return Ok(match sym {
        WellKnownSymbol::Iterator => syms.iterator,
        WellKnownSymbol::AsyncIterator => syms.async_iterator,
      });
    }

    match sym {
      WellKnownSymbol::Iterator => {
        if let Some(sym) = self.well_known_iterator {
          return Ok(sym);
        }
        let key = self.scope.alloc_string("Symbol.iterator")?;
        let sym = self.scope.heap_mut().symbol_for(key)?;
        self.well_known_iterator = Some(sym);
        Ok(sym)
      }
      WellKnownSymbol::AsyncIterator => {
        if let Some(sym) = self.well_known_async_iterator {
          return Ok(sym);
        }
        let key = self.scope.alloc_string("Symbol.asyncIterator")?;
        let sym = self.scope.heap_mut().symbol_for(key)?;
        self.well_known_async_iterator = Some(sym);
        Ok(sym)
      }
    }
  }

  fn to_primitive_hint_str(hint: ToPrimitiveHint) -> &'static str {
    match hint {
      ToPrimitiveHint::Default => "default",
      ToPrimitiveHint::String => "string",
      ToPrimitiveHint::Number => "number",
    }
  }

  /// ECMAScript `ToPrimitive(input, preferredType)`.
  ///
  /// This can invoke user code (`@@toPrimitive`, `valueOf`, `toString`) and therefore must call into
  /// JS via `call_js` (so host hooks overrides / embedder host context are preserved when
  /// available).
  fn to_primitive(&mut self, value: Value, preferred_type: ToPrimitiveHint) -> Result<Value, VmError> {
    let Value::Object(obj) = value else {
      return Ok(value);
    };

    let intr = self
      .vm
      .intrinsics()
      .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
    let to_prim_key = PropertyKey::Symbol(intr.well_known_symbols().to_primitive);

    // 1. Let exoticToPrim be ? GetMethod(input, @@toPrimitive).
    let exotic = self.get_method(obj, to_prim_key)?;
    if let Some(exotic) = exotic {
      // 2.a. Let hint be "default"/"string"/"number".
      let hint_s = self.scope.alloc_string(Self::to_primitive_hint_str(preferred_type))?;
      // 2.b. Let result be ? Call(exoticToPrim, input, « hint »).
      let result = self.call_js(exotic, Value::Object(obj), &[Value::String(hint_s)])?;
      // 2.c. If result is not an Object, return result.
      if !matches!(result, Value::Object(_)) {
        self.root(result)?;
        return Ok(result);
      }
      // 2.d. Throw a TypeError exception.
      return Err(VmError::TypeError("Cannot convert object to primitive value"));
    }

    // OrdinaryToPrimitive (spec-shaped ordering).
    let preferred_type = match preferred_type {
      ToPrimitiveHint::Default => ToPrimitiveHint::Number,
      other => other,
    };
    let method_names = match preferred_type {
      ToPrimitiveHint::String => ["toString", "valueOf"],
      ToPrimitiveHint::Number | ToPrimitiveHint::Default => ["valueOf", "toString"],
    };

    for name in method_names {
      let key_s = self.scope.alloc_string(name)?;
      let key = PropertyKey::String(key_s);
      let method = self.get(obj, key)?;
      if matches!(method, Value::Undefined | Value::Null) {
        continue;
      }
      if !self.scope.heap().is_callable(method)? {
        continue;
      }
      let result = self.call_js(method, Value::Object(obj), &[])?;
      if !matches!(result, Value::Object(_)) {
        self.root(result)?;
        return Ok(result);
      }
    }

    Err(VmError::TypeError("Cannot convert object to primitive value"))
  }
}

#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
enum ToPrimitiveHint {
  Default,
  String,
  Number,
}

impl JsRuntime for VmJsWebIdlCx<'_> {
  type Value = Value;
  type String = GcString;
  type Object = GcObject;
  type Symbol = GcSymbol;
  type Error = VmError;

  fn limits(&self) -> WebIdlLimits {
    self.limits
  }

  fn hooks(&self) -> &dyn WebIdlHooks<Self::Value> {
    self.hooks
  }

  fn value_undefined(&self) -> Self::Value {
    Value::Undefined
  }

  fn value_null(&self) -> Self::Value {
    Value::Null
  }

  fn value_bool(&self, value: bool) -> Self::Value {
    Value::Bool(value)
  }

  fn value_number(&self, value: f64) -> Self::Value {
    Value::Number(value)
  }

  fn value_string(&self, value: Self::String) -> Self::Value {
    Value::String(value)
  }

  fn value_object(&self, value: Self::Object) -> Self::Value {
    Value::Object(value)
  }

  fn is_undefined(&self, value: Self::Value) -> bool {
    matches!(value, Value::Undefined)
  }

  fn is_null(&self, value: Self::Value) -> bool {
    matches!(value, Value::Null)
  }

  fn is_boolean(&self, value: Self::Value) -> bool {
    matches!(value, Value::Bool(_))
  }

  fn is_number(&self, value: Self::Value) -> bool {
    matches!(value, Value::Number(_))
  }

  fn is_string(&self, value: Self::Value) -> bool {
    matches!(value, Value::String(_))
  }

  fn is_symbol(&self, value: Self::Value) -> bool {
    matches!(value, Value::Symbol(_))
  }

  fn is_object(&self, value: Self::Value) -> bool {
    matches!(value, Value::Object(_))
  }

  fn is_string_object(&self, _value: Self::Value) -> bool {
    // `vm-js` does not yet model boxed String objects.
    false
  }

  fn as_string(&self, value: Self::Value) -> Option<Self::String> {
    match value {
      Value::String(s) => Some(s),
      _ => None,
    }
  }

  fn as_object(&self, value: Self::Value) -> Option<Self::Object> {
    match value {
      Value::Object(o) => Some(o),
      _ => None,
    }
  }

  fn as_symbol(&self, value: Self::Value) -> Option<Self::Symbol> {
    match value {
      Value::Symbol(s) => Some(s),
      _ => None,
    }
  }

  fn to_boolean(&mut self, value: Self::Value) -> Result<bool, Self::Error> {
    self.scope.heap().to_boolean(value)
  }

  fn to_string(&mut self, value: Self::Value) -> Result<Self::String, Self::Error> {
    let s = match value {
      Value::Object(_) => {
        let prim = self.to_primitive(value, ToPrimitiveHint::String)?;
        self.scope.heap_mut().to_string(prim)?
      }
      other => self.scope.heap_mut().to_string(other)?,
    };
    self.root(Value::String(s))?;
    Ok(s)
  }

  fn to_number(&mut self, value: Self::Value) -> Result<f64, Self::Error> {
    match value {
      Value::Object(_) => {
        let prim = self.to_primitive(value, ToPrimitiveHint::Number)?;
        self.scope.heap_mut().to_number(prim)
      }
      other => self.scope.heap_mut().to_number(other),
    }
  }

  fn type_error(&mut self, message: &'static str) -> Self::Error {
    VmError::TypeError(message)
  }

  fn get(
    &mut self,
    object: Self::Object,
    key: PropertyKey<Self::String, Self::Symbol>,
  ) -> Result<Self::Value, Self::Error> {
    self.root(Value::Object(object))?;
    match key {
      PropertyKey::String(s) => self.root(Value::String(s))?,
      PropertyKey::Symbol(s) => self.root(Value::Symbol(s))?,
    };

    let key = Self::to_vm_property_key(key);
    // Implement ECMAScript `[[Get]]` directly so accessor getters are invoked via `call_js`.
    //
    // This matters because `Vm::get` → `Scope::ordinary_get` always calls accessor getters with a
    // dummy `VmHost` context (`()`), which breaks host-context propagation when conversions run
    // inside a `NativeCall`/`NativeConstruct` handler. `call_js` forwards the embedder host context
    // when available and respects any active `Vm::with_host_hooks_override`.
    let value = match self.scope.heap().get_property(object, &key)? {
      None => Value::Undefined,
      Some(desc) => match desc.kind {
        vm_js::PropertyKind::Data { value, .. } => value,
        vm_js::PropertyKind::Accessor { get, .. } => {
          if matches!(get, Value::Undefined) {
            Value::Undefined
          } else {
            if !self.scope.heap().is_callable(get)? {
              return Err(VmError::TypeError("accessor getter is not callable"));
            }
            self.call_js(get, Value::Object(object), &[])?
          }
        }
      },
    };
    self.root(value)?;
    Ok(value)
  }

  fn get_method(
    &mut self,
    object: Self::Object,
    key: PropertyKey<Self::String, Self::Symbol>,
  ) -> Result<Option<Self::Value>, Self::Error> {
    // ECMAScript `GetMethod(O, P)` where `O` is already known to be an object.
    //
    // Like `get`, we avoid `Vm::get_method` so any accessor getters run via `call_js` (preserving
    // embedder host context / host hooks overrides).
    let value = self.get(object, key)?;
    if matches!(value, Value::Undefined | Value::Null) {
      return Ok(None);
    }
    if !self.scope.heap().is_callable(value)? {
      return Err(VmError::TypeError("GetMethod: target is not callable"));
    }
    Ok(Some(value))
  }

  fn own_property_keys(
    &mut self,
    object: Self::Object,
  ) -> Result<Vec<PropertyKey<Self::String, Self::Symbol>>, Self::Error> {
    self.root(Value::Object(object))?;
    let keys = self.scope.heap().ordinary_own_property_keys(object)?;
    Ok(keys.into_iter().map(Self::from_vm_property_key).collect())
  }

  fn alloc_string_from_code_units(&mut self, units: &[u16]) -> Result<Self::String, Self::Error> {
    let s = self.scope.alloc_string_from_code_units(units)?;
    self.root(Value::String(s))?;
    Ok(s)
  }

  fn alloc_object(&mut self) -> Result<Self::Object, Self::Error> {
    let obj = self.scope.alloc_object()?;
    self.root(Value::Object(obj))?;

    // When a realm is initialized, prefer `%Object.prototype%` so the result behaves like a normal
    // JavaScript object (e.g. has standard methods).
    if let Some(intrinsics) = self.vm.intrinsics() {
      let proto = intrinsics.object_prototype();
      self.root(Value::Object(proto))?;
      self.scope.object_set_prototype(obj, Some(proto))?;
    }
    Ok(obj)
  }

  fn alloc_array(&mut self, len: usize) -> Result<Self::Object, Self::Error> {
    let obj = self.scope.alloc_array(len)?;
    self.root(Value::Object(obj))?;

    // When a realm is initialized, prefer `%Array.prototype%` so the result behaves like a normal
    // JavaScript array (e.g. is iterable, has standard methods).
    if let Some(intrinsics) = self.vm.intrinsics() {
      let proto = intrinsics.array_prototype();
      self.root(Value::Object(proto))?;
      self.scope.object_set_prototype(obj, Some(proto))?;
    }

    Ok(obj)
  }

  fn create_data_property_or_throw(
    &mut self,
    object: Self::Object,
    key: PropertyKey<Self::String, Self::Symbol>,
    value: Self::Value,
  ) -> Result<(), Self::Error> {
    self.root(Value::Object(object))?;
    match key {
      PropertyKey::String(s) => self.root(Value::String(s))?,
      PropertyKey::Symbol(s) => self.root(Value::Symbol(s))?,
    };
    self.root(value)?;

    let key = Self::to_vm_property_key(key);
    self.scope.create_data_property_or_throw(object, key, value)?;
    Ok(())
  }

  fn well_known_symbol(&mut self, sym: WellKnownSymbol) -> Result<Self::Symbol, Self::Error> {
    self.get_well_known_symbol_cached(sym)
  }

  fn get_iterator(&mut self, value: Self::Value) -> Result<Self::Object, Self::Error> {
    let Value::Object(obj) = value else {
      return Err(self.type_error("GetIterator(value): value is not an object"));
    };

    let sym = self.well_known_symbol(WellKnownSymbol::Iterator)?;
    let Some(method) = self.get_method(obj, PropertyKey::Symbol(sym))? else {
      return Err(self.type_error("GetIterator(value): @@iterator is undefined/null"));
    };
    self.get_iterator_from_method(obj, method)
  }

  fn get_iterator_from_method(
    &mut self,
    object: Self::Object,
    method: Self::Value,
  ) -> Result<Self::Object, Self::Error> {
    self.root(Value::Object(object))?;
    self.root(method)?;

    let iterator = self.call_js(method, Value::Object(object), &[])?;
    let Value::Object(iterator) = iterator else {
      return Err(self.type_error("Iterator method did not return an object"));
    };
    self.root(Value::Object(iterator))?;
    Ok(iterator)
  }

  fn iterator_next(
    &mut self,
    iterator: Self::Object,
  ) -> Result<IteratorResult<Self::Value>, Self::Error> {
    self.root(Value::Object(iterator))?;

    let next_key = PropertyKey::String(self.scope.alloc_string("next")?);
    let Some(next_method) = self.get_method(iterator, next_key)? else {
      return Err(self.type_error("IteratorNext(iterator): next is undefined/null"));
    };

    let result = self.call_js(next_method, Value::Object(iterator), &[])?;
    let Value::Object(result_obj) = result else {
      return Err(self.type_error("IteratorNext(iterator): next() did not return an object"));
    };
    self.root(Value::Object(result_obj))?;

    let done_key = PropertyKey::String(self.scope.alloc_string("done")?);
    let done_value = self.get(result_obj, done_key)?;
    let done = self.to_boolean(done_value)?;

    let value_key = PropertyKey::String(self.scope.alloc_string("value")?);
    let value = self.get(result_obj, value_key)?;
    self.root(value)?;

    Ok(IteratorResult { value, done })
  }
}

impl WebIdlJsRuntime for VmJsWebIdlCx<'_> {
  fn is_callable(&self, value: Self::Value) -> bool {
    self.scope.heap().is_callable(value).unwrap_or(false)
  }

  fn is_bigint(&self, value: Self::Value) -> bool {
    matches!(value, Value::BigInt(_))
  }

  fn to_bigint(&mut self, value: Self::Value) -> Result<Self::Value, Self::Error> {
    // Minimal ECMAScript `ToBigInt`: support BigInt, boolean, and integral finite numbers.
    let out = match value {
      Value::BigInt(_) => value,
      Value::Bool(b) => Value::BigInt(if b { JsBigInt::from_u128(1) } else { JsBigInt::zero() }),
      Value::Number(n) => {
        if !n.is_finite() {
          return Err(self.throw_range_error("Cannot convert non-finite number to a BigInt"));
        }
        if n.fract() != 0.0 {
          return Err(self.throw_range_error("Cannot convert non-integer number to a BigInt"));
        }

        let abs = n.abs();
        if abs > (u128::MAX as f64) {
          return Err(self.throw_range_error("BigInt value is out of range"));
        }
        let mag = abs as u128;
        let mut bi = JsBigInt::from_u128(mag);
        if n.is_sign_negative() {
          bi = bi.negate();
        }
        Value::BigInt(bi)
      }
      Value::String(s) => {
        // Parse a base-10/0x/0o/0b BigInt string. We accept leading/trailing whitespace.
        let raw = self.scope.heap().get_string(s)?.to_utf8_lossy();
        let trimmed = raw.trim();
        let Some(first) = trimmed.chars().next() else {
          return Err(self.throw_type_error("Cannot convert empty string to a BigInt"));
        };

        let (negative, digits) = match first {
          '+' => (false, &trimmed[1..]),
          '-' => (true, &trimmed[1..]),
          _ => (false, trimmed),
        };

        let (radix, digits) = if let Some(rest) = digits.strip_prefix("0x").or_else(|| digits.strip_prefix("0X")) {
          (16u32, rest)
        } else if let Some(rest) = digits.strip_prefix("0o").or_else(|| digits.strip_prefix("0O")) {
          (8u32, rest)
        } else if let Some(rest) = digits.strip_prefix("0b").or_else(|| digits.strip_prefix("0B")) {
          (2u32, rest)
        } else {
          (10u32, digits)
        };

        if digits.is_empty() {
          return Err(self.throw_type_error("Cannot convert string to a BigInt"));
        }

        let mag = u128::from_str_radix(digits, radix)
          .map_err(|_| self.throw_type_error("Cannot convert string to a BigInt"))?;
        let mut bi = JsBigInt::from_u128(mag);
        if negative {
          bi = bi.negate();
        }
        Value::BigInt(bi)
      }
      _ => return Err(self.throw_type_error("Cannot convert value to a BigInt")),
    };

    self.root(out)?;
    Ok(out)
  }

  fn to_numeric(&mut self, value: Self::Value) -> Result<Self::Value, Self::Error> {
    let out = match value {
      Value::BigInt(_) => value,
      other => match self.scope.heap_mut().to_number(other) {
        Ok(n) => Value::Number(n),
        Err(VmError::TypeError(msg)) => return Err(self.throw_type_error(msg)),
        Err(err) => return Err(err),
      },
    };

    self.root(out)?;
    Ok(out)
  }

  fn get_own_property(
    &mut self,
    object: Self::Object,
    key: PropertyKey<Self::String, Self::Symbol>,
  ) -> Result<Option<JsOwnPropertyDescriptor<Self::Value>>, Self::Error> {
    self.root(Value::Object(object))?;
    match key {
      PropertyKey::String(s) => self.root(Value::String(s))?,
      PropertyKey::Symbol(s) => self.root(Value::Symbol(s))?,
    };

    let key = Self::to_vm_property_key(key);
    let Some(desc) = self
      .scope
      .heap()
      .object_get_own_property(object, &key)?
    else {
      return Ok(None);
    };

    let enumerable = desc.enumerable;
    let kind = match desc.kind {
      vm_js::PropertyKind::Data { value, .. } => {
        self.root(value)?;
        JsPropertyKind::Data { value }
      }
      vm_js::PropertyKind::Accessor { get, set } => {
        self.root(get)?;
        self.root(set)?;
        JsPropertyKind::Accessor { get, set }
      }
    };

    Ok(Some(JsOwnPropertyDescriptor { enumerable, kind }))
  }

  fn throw_type_error(&mut self, message: &str) -> Self::Error {
    let Some(intr) = self.vm.intrinsics() else {
      return VmError::Unimplemented("intrinsics not initialized");
    };

    match vm_js::new_error(
      &mut self.scope,
      intr.type_error_prototype(),
      "TypeError",
      message,
    ) {
      Ok(value) => match self.root(value) {
        Ok(()) => VmError::Throw(value),
        Err(err) => err,
      },
      Err(err) => err,
    }
  }

  fn throw_range_error(&mut self, message: &str) -> Self::Error {
    let Some(intr) = self.vm.intrinsics() else {
      return VmError::Unimplemented("intrinsics not initialized");
    };

    match vm_js::new_error(
      &mut self.scope,
      intr.range_error_prototype(),
      "RangeError",
      message,
    ) {
      Ok(value) => match self.root(value) {
        Ok(()) => VmError::Throw(value),
        Err(err) => err,
      },
      Err(err) => err,
    }
  }

  fn is_array_buffer(&self, _value: Self::Value) -> bool {
    false
  }

  fn is_shared_array_buffer(&self, _value: Self::Value) -> bool {
    false
  }

  fn is_data_view(&self, _value: Self::Value) -> bool {
    false
  }

  fn typed_array_name(&self, _value: Self::Value) -> Option<&'static str> {
    None
  }
}

/// Convert an ECMAScript value to a WebIDL callback function value.
///
/// Spec: <https://webidl.spec.whatwg.org/#es-callback-function>
///
/// # GC / lifetime safety
///
/// The returned [`Value`] is **not** automatically rooted. If the embedding intends to store it
/// beyond the current VM call boundary, it must keep it alive (e.g. via [`Heap::add_root`]) and
/// store the returned [`vm_js::RootId`].
pub fn to_callback_function(
  heap: &Heap,
  value: Value,
  legacy_treat_non_object_as_null: bool,
) -> Result<Value, VmError> {
  if legacy_treat_non_object_as_null && !matches!(value, Value::Object(_)) {
    return Ok(Value::Null);
  }
  if heap.is_callable(value)? {
    return Ok(value);
  }
  Err(VmError::TypeError(
    "Value is not a callable callback function",
  ))
}

/// Convert an ECMAScript value to a WebIDL callback interface value.
///
/// Spec: <https://webidl.spec.whatwg.org/#es-callback-interface>
///
/// MVP behaviour: validate that the value is an object and return it.
///
/// # GC / lifetime safety
///
/// Like [`to_callback_function`], the returned value is not automatically rooted.
pub fn to_callback_interface(_heap: &Heap, value: Value) -> Result<Value, VmError> {
  if matches!(value, Value::Object(_)) {
    return Ok(value);
  }
  Err(VmError::TypeError(
    "Value is not a callback interface object",
  ))
}

/// Nullable wrapper for [`to_callback_function`].
#[inline]
pub fn to_nullable_callback_function(
  heap: &Heap,
  value: Value,
  legacy_treat_non_object_as_null: bool,
) -> Result<Value, VmError> {
  if matches!(value, Value::Undefined | Value::Null) {
    return Ok(Value::Null);
  }
  to_callback_function(heap, value, legacy_treat_non_object_as_null)
}

/// Nullable wrapper for [`to_callback_interface`].
#[inline]
pub fn to_nullable_callback_interface(heap: &Heap, value: Value) -> Result<Value, VmError> {
  if matches!(value, Value::Undefined | Value::Null) {
    return Ok(Value::Null);
  }
  to_callback_interface(heap, value)
}

/// Invoke a callback function value, ensuring Promise jobs enqueue via `hooks`.
///
/// This uses [`Vm::call_with_host`], which:
/// - passes a dummy [`VmHost`] context (`()`), and
/// - installs `hooks` as the active host hooks override so Promise jobs are routed via
///   [`VmHostHooks::host_enqueue_promise_job`].
///
/// Embeddings that need native call handlers to access host state should ensure their bindings use
/// the `VmHostHooks`-based dispatch plumbing (see [`WebIdlBindingsHostSlot`]) rather than relying on
/// downcasting the `VmHost` context.
pub fn invoke_callback_function(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  hooks: &mut dyn VmHostHooks,
  callback: Value,
  this_arg: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  if !scope.heap().is_callable(callback)? {
    return Err(VmError::TypeError(
      "Callback function value is not callable",
    ));
  }
  vm.call_with_host(scope, hooks, callback, this_arg, args)
}

/// Invoke a callback interface value.
///
/// Web IDL callback interfaces are "dual" objects: they may be either callable (functions) or
/// objects with callable members (e.g. `handleEvent` for `EventListener`).
///
/// - If `callback` is callable, it is invoked with `this = this_for_callable`.
/// - Otherwise, `callback.handleEvent(...args)` is invoked with `this = callback`.
///
/// Promise jobs enqueued by the callback are routed via `hooks`.
pub fn invoke_callback_interface(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  hooks: &mut dyn VmHostHooks,
  callback: Value,
  this_for_callable: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  if scope.heap().is_callable(callback)? {
    return vm.call_with_host(scope, hooks, callback, this_for_callable, args);
  }
  let Value::Object(obj) = callback else {
    return Err(VmError::TypeError(
      "Callback interface value is not callable or an object",
    ));
  };

  // Root the receiver while allocating the `handleEvent` property key and while invoking accessors.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(obj))?;

  let key_s = scope.alloc_string("handleEvent")?;
  scope.push_root(Value::String(key_s))?;
  let key = VmPropertyKey::from_string(key_s);

  // Implement `GetMethod` in a way that invokes accessor getters via `Vm::call_with_host` so host
  // hooks overrides are respected.
  let method = match scope.heap().get_property(obj, &key)? {
    None => Value::Undefined,
    Some(desc) => match desc.kind {
      vm_js::PropertyKind::Data { value, .. } => value,
      vm_js::PropertyKind::Accessor { get, .. } => {
        if matches!(get, Value::Undefined) {
          Value::Undefined
        } else {
          if !scope.heap().is_callable(get)? {
            return Err(VmError::TypeError("accessor getter is not callable"));
          }
          vm.call_with_host(&mut scope, hooks, get, Value::Object(obj), &[])?
        }
      }
    },
  };

  if matches!(method, Value::Undefined | Value::Null) {
    return Err(VmError::TypeError(
      "Callback interface object is missing a callable handleEvent method",
    ));
  }
  if !scope.heap().is_callable(method)? {
    return Err(VmError::TypeError(
      "Callback interface object is missing a callable handleEvent method",
    ));
  }
  vm.call_with_host(&mut scope, hooks, method, Value::Object(obj), args)
}

#[cfg(test)]
mod tests {
  use super::VmJsWebIdlCx;
  use vm_js::{
    GcObject, Heap, HeapLimits, Job, NativeFunctionId, PropertyDescriptor,
    PropertyKey as VmPropertyKey, PropertyKind, Realm, RealmId, Scope, Value, Vm, VmError, VmHost,
    VmHostHooks, VmOptions,
  };
  use webidl::{
    conversions, index_to_property_key, record_to_js_object, sequence_to_js_array, DomString,
    IdlRecord, InterfaceId, JsRuntime, PropertyKey as WebIdlPropertyKey, ToJsValue, WebIdlHooks,
    WebIdlJsRuntime, WebIdlLimits,
  };

  struct NoHooks;

  impl WebIdlHooks<Value> for NoHooks {
    fn is_platform_object(&self, _value: Value) -> bool {
      false
    }

    fn implements_interface(&self, _value: Value, _interface: InterfaceId) -> bool {
      false
    }
  }

  fn prop_key_str(
    cx: &mut VmJsWebIdlCx<'_>,
    s: &str,
  ) -> Result<WebIdlPropertyKey<vm_js::GcString, vm_js::GcSymbol>, VmError> {
    let units: Vec<u16> = s.encode_utf16().collect();
    let handle = cx.alloc_string_from_code_units(&units)?;
    Ok(WebIdlPropertyKey::String(handle))
  }

  #[test]
  fn domstring_smoke_roundtrips_code_units() -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

    let units: Vec<u16> = vec![0x0041, 0xD83D, 0xDE00, 0x0000, 0xFFFF];
    let s = {
      let mut scope = heap.scope();
      scope
        .alloc_string_from_code_units(&units)
        .expect("alloc string")
    };
    let _root = heap.add_root(Value::String(s)).expect("add_root");

    let hooks = NoHooks;
    let limits = WebIdlLimits::default();
    let mut cx = VmJsWebIdlCx::new(&mut vm, &mut heap, limits, &hooks);

    let out = conversions::dom_string(&mut cx, Value::String(s)).expect("DOMString conversion");
    drop(cx);

    let out_units = heap.get_string(out).expect("get string").as_code_units();
    assert_eq!(out_units, units.as_slice());
    Ok(())
  }

  #[test]
  fn to_number_string_parsing_matches_ecma262_roughly() -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let hooks = NoHooks;
    let limits = WebIdlLimits::default();

    let mut cx = VmJsWebIdlCx::new(&mut vm, &mut heap, limits, &hooks);

    assert!(cx.to_number(Value::Undefined)?.is_nan());
    assert_eq!(cx.to_number(Value::Null)?, 0.0);
    assert_eq!(cx.to_number(Value::Bool(true))?, 1.0);
    assert_eq!(cx.to_number(Value::Bool(false))?, 0.0);

    let s = cx.scope.alloc_string("  123  ")?;
    assert_eq!(cx.to_number(Value::String(s))?, 123.0);

    let s = cx.scope.alloc_string("")?;
    assert_eq!(cx.to_number(Value::String(s))?, 0.0);

    let s = cx.scope.alloc_string("Infinity")?;
    assert!(cx.to_number(Value::String(s))?.is_infinite());

    let s = cx.scope.alloc_string("0x10")?;
    assert_eq!(cx.to_number(Value::String(s))?, 16.0);

    let s = cx.scope.alloc_string("0b10")?;
    assert_eq!(cx.to_number(Value::String(s))?, 2.0);

    let s = cx.scope.alloc_string("0o10")?;
    assert_eq!(cx.to_number(Value::String(s))?, 8.0);

    let s = cx.scope.alloc_string("010")?;
    assert_eq!(cx.to_number(Value::String(s))?, 10.0);

    let s = cx.scope.alloc_string("-0x10")?;
    assert!(cx.to_number(Value::String(s))?.is_nan());

    Ok(())
  }

  #[test]
  fn well_known_symbol_uses_realm_intrinsics_when_available() -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;

    let hooks = NoHooks;
    let limits = WebIdlLimits::default();

    let mut cx = VmJsWebIdlCx::new(&mut vm, &mut heap, limits, &hooks);
    let iter_sym = cx.well_known_symbol(webidl::WellKnownSymbol::Iterator)?;
    assert_eq!(iter_sym, realm.well_known_symbols().iterator);

    // `alloc_array` should use `%Array.prototype%` when intrinsics are installed.
    let arr = cx.alloc_array(0)?;
    assert_eq!(
      cx.scope.heap().object_prototype(arr)?,
      Some(realm.intrinsics().array_prototype())
    );

    drop(cx);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn record_to_js_defines_own_enumerable_data_properties() -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let hooks = NoHooks;
    let limits = WebIdlLimits::default();
    let mut cx = VmJsWebIdlCx::new(&mut vm, &mut heap, limits, &hooks);

    let record = IdlRecord(vec![
      (DomString::from_str("a"), 1u32),
      (DomString::from_str("b"), 2u32),
    ]);

    let v = record.to_js(&mut cx, &limits).expect("record to_js");
    let Value::Object(obj) = v else {
      return Err(VmError::TypeError("expected object from record.to_js"));
    };

    // Use fresh key strings for lookup; vm-js compares string keys by UTF-16 code units.
    let key_a = cx.scope.alloc_string("a")?;
    let key_b = cx.scope.alloc_string("b")?;

    let desc_a = cx
      .scope
      .heap()
      .object_get_own_property(obj, &VmPropertyKey::from_string(key_a))?
      .expect("property a exists");
    assert!(desc_a.enumerable);
    assert!(desc_a.configurable);
    let PropertyKind::Data { value, writable } = desc_a.kind else {
      return Err(VmError::TypeError("expected data descriptor for a"));
    };
    assert!(writable);
    assert_eq!(value, Value::Number(1.0));

    let desc_b = cx
      .scope
      .heap()
      .object_get_own_property(obj, &VmPropertyKey::from_string(key_b))?
      .expect("property b exists");
    assert!(desc_b.enumerable);
    assert!(desc_b.configurable);
    let PropertyKind::Data { value, writable } = desc_b.kind else {
      return Err(VmError::TypeError("expected data descriptor for b"));
    };
    assert!(writable);
    assert_eq!(value, Value::Number(2.0));

    Ok(())
  }

  #[test]
  fn sequence_to_js_array_sets_length_indices_and_own_property_keys() -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());
    // Stress rooting: force a GC before each allocation.
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 0));
    let hooks = NoHooks;
    let limits = WebIdlLimits::default();
    let mut cx = VmJsWebIdlCx::new(&mut vm, &mut heap, limits, &hooks);

    let array =
      sequence_to_js_array(&mut cx, &limits, &[1u32, 2u32]).expect("sequence_to_js_array");

    // length === 2
    let length_key = prop_key_str(&mut cx, "length")?;
    let length = cx.get(array, length_key)?;
    assert_eq!(length, Value::Number(2.0));

    // array[0] === 1, array[1] === 2
    for (i, expected) in [1.0, 2.0].into_iter().enumerate() {
      let key = index_to_property_key(&mut cx, i).expect("index key");
      let v = cx.get(array, key)?;
      assert_eq!(v, Value::Number(expected));
    }

    let keys = cx.own_property_keys(array)?;
    let keys = keys
      .into_iter()
      .map(|k| match k {
        WebIdlPropertyKey::String(s) => cx.scope.heap().get_string(s).unwrap().to_utf8_lossy(),
        WebIdlPropertyKey::Symbol(sym) => {
          format!("sym:{}", cx.scope.heap().get_symbol_id(sym).unwrap())
        }
      })
      .collect::<Vec<_>>();
    assert_eq!(keys, vec!["0".to_string(), "1".to_string(), "length".to_string()]);
    Ok(())
  }

  #[test]
  fn record_to_js_object_sets_properties_and_own_property_keys() -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());
    // Stress rooting: force a GC before each allocation.
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 0));
    let hooks = NoHooks;
    let limits = WebIdlLimits::default();
    let mut cx = VmJsWebIdlCx::new(&mut vm, &mut heap, limits, &hooks);

    let entries = [("b", 2u32), ("a", 1u32)];
    let obj = record_to_js_object(&mut cx, &limits, &entries).expect("record_to_js_object");

    for (k, v_expected) in entries.iter() {
      let key = prop_key_str(&mut cx, k)?;
      let v = cx.get(obj, key)?;
      assert_eq!(v, Value::Number(*v_expected as f64));
    }

    let keys = cx.own_property_keys(obj)?;
    let keys = keys
      .into_iter()
      .map(|k| match k {
        WebIdlPropertyKey::String(s) => cx.scope.heap().get_string(s).unwrap().to_utf8_lossy(),
        WebIdlPropertyKey::Symbol(sym) => {
          format!("sym:{}", cx.scope.heap().get_symbol_id(sym).unwrap())
        }
      })
      .collect::<Vec<_>>();
    assert_eq!(keys, vec!["b".to_string(), "a".to_string()]);
    Ok(())
  }

  fn get_method_getter(this: Value, scope: &mut Scope<'_>) -> Result<Value, VmError> {
    let Value::Object(obj) = this else {
      return Err(VmError::TypeError("getter this is not object"));
    };

    // calls++
    let calls_key = VmPropertyKey::from_string(scope.alloc_string("calls")?);
    let calls = scope
      .heap()
      .object_get_own_data_property_value(obj, &calls_key)?
      .unwrap_or(Value::Number(0.0));
    let calls_n = match calls {
      Value::Number(n) => n,
      _ => 0.0,
    };
    scope.heap_mut().object_set_existing_data_property_value(
      obj,
      &calls_key,
      Value::Number(calls_n + 1.0),
    )?;

    // return this.fn
    let fn_key = VmPropertyKey::from_string(scope.alloc_string("fn")?);
    Ok(
      scope
        .heap()
        .object_get_own_data_property_value(obj, &fn_key)?
        .unwrap_or(Value::Undefined),
    )
  }

  fn getter_call_handler(
    _vm: &mut Vm,
    scope: &mut Scope<'_>,
    _host: &mut dyn VmHost,
    _hooks: &mut dyn VmHostHooks,
    _callee: GcObject,
    this: Value,
    _args: &[Value],
  ) -> Result<Value, VmError> {
    get_method_getter(this, scope)
  }

  fn noop_call_handler(
    _vm: &mut Vm,
    _scope: &mut Scope<'_>,
    _host: &mut dyn VmHost,
    _hooks: &mut dyn VmHostHooks,
    _callee: GcObject,
    _this: Value,
    _args: &[Value],
  ) -> Result<Value, VmError> {
    Ok(Value::Undefined)
  }

  #[test]
  fn get_method_invokes_accessor_getter() -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

    // Create a method function and a getter function.
    let method;
      let getter;
    {
      let noop_id: NativeFunctionId = vm.register_native_call(noop_call_handler)?;
      let getter_id: NativeFunctionId = vm.register_native_call(getter_call_handler)?;

      let mut scope = heap.scope();
      let method_name = scope.alloc_string("method")?;
      let getter_name = scope.alloc_string("getter")?;
      method = scope.alloc_native_function(noop_id, None, method_name, 0)?;
      getter = scope.alloc_native_function(getter_id, None, getter_name, 0)?;
    }

    let obj;
    {
      let mut scope = heap.scope();
      obj = scope.alloc_object()?;

      // calls = 0
      let calls_key = VmPropertyKey::from_string(scope.alloc_string("calls")?);
      scope.define_property(
        obj,
        calls_key,
        PropertyDescriptor {
          enumerable: true,
          configurable: true,
          kind: PropertyKind::Data {
            value: Value::Number(0.0),
            writable: true,
          },
        },
      )?;

      // fn = method
      let fn_key = VmPropertyKey::from_string(scope.alloc_string("fn")?);
      scope.define_property(
        obj,
        fn_key,
        PropertyDescriptor {
          enumerable: true,
          configurable: true,
          kind: PropertyKind::Data {
            value: Value::Object(method),
            writable: true,
          },
        },
      )?;

      // get m() { ... }
      let m_key = VmPropertyKey::from_string(scope.alloc_string("m")?);
      scope.define_property(
        obj,
        m_key,
        PropertyDescriptor {
          enumerable: true,
          configurable: true,
          kind: PropertyKind::Accessor {
            get: Value::Object(getter),
            set: Value::Undefined,
          },
        },
      )?;
    }

    let hooks = NoHooks;
    let limits = WebIdlLimits::default();
    let mut cx = VmJsWebIdlCx::new(&mut vm, &mut heap, limits, &hooks);
    cx.scope.push_root(Value::Object(obj))?;

    let key = WebIdlPropertyKey::String(cx.scope.alloc_string("m")?);
    let got = cx.get_method(obj, key)?;
    assert_eq!(got, Some(Value::Object(method)));

    // Ensure getter ran exactly once.
    let calls_key = VmPropertyKey::from_string(cx.scope.alloc_string("calls")?);
    let calls = cx
      .scope
      .heap()
      .object_get_own_data_property_value(obj, &calls_key)?
      .unwrap_or(Value::Undefined);
    assert_eq!(calls, Value::Number(1.0));
    Ok(())
  }

  fn iterator_method_call(
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    _callee: GcObject,
    this: Value,
    _args: &[Value],
  ) -> Result<Value, VmError> {
    let Value::Object(iterable) = this else {
      return Err(VmError::TypeError("iterator method this is not object"));
    };

    // Read iterable.items.
    let items_key = VmPropertyKey::from_string(scope.alloc_string("items")?);
    let items = scope
      .heap()
      .object_get_own_data_property_value(iterable, &items_key)?
      .unwrap_or(Value::Undefined);

    // Create iterator object: { items, index: 0, next: <native fn> }.
    let iter_obj = scope.alloc_object()?;
    scope.push_root(Value::Object(iter_obj))?;

    // items
    scope.define_property(
      iter_obj,
      items_key,
      PropertyDescriptor {
        enumerable: true,
        configurable: true,
        kind: PropertyKind::Data {
          value: items,
          writable: true,
        },
      },
    )?;

    // index
    let index_key = VmPropertyKey::from_string(scope.alloc_string("index")?);
    scope.define_property(
      iter_obj,
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

    // next
    let next_key = VmPropertyKey::from_string(scope.alloc_string("next")?);
    let next_name = scope.alloc_string("next")?;
    let next_id = vm.register_native_call(iterator_next_call)?;
    let next_fn = scope.alloc_native_function(next_id, None, next_name, 0)?;
    scope.define_property(
      iter_obj,
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

    Ok(Value::Object(iter_obj))
  }

  fn iterator_method_call_handler(
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    _host: &mut dyn VmHost,
    _hooks: &mut dyn VmHostHooks,
    callee: GcObject,
    this: Value,
    args: &[Value],
  ) -> Result<Value, VmError> {
    iterator_method_call(vm, scope, callee, this, args)
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
      return Err(VmError::TypeError("iterator this is not object"));
    };

    let items_key = VmPropertyKey::from_string(scope.alloc_string("items")?);
    let index_key = VmPropertyKey::from_string(scope.alloc_string("index")?);

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

    // Read items.length.
    let length_key = VmPropertyKey::from_string(scope.alloc_string("length")?);
    let len = scope
      .heap()
      .object_get_own_data_property_value(items_obj, &length_key)?
      .unwrap_or(Value::Number(0.0));
    let len = match len {
      Value::Number(n) => n as usize,
      _ => 0,
    };

    let done = idx >= len;
    let value = if done {
      Value::Undefined
    } else {
      let idx_key = VmPropertyKey::from_string(scope.alloc_string(&idx.to_string())?);
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

    let value_key = VmPropertyKey::from_string(scope.alloc_string("value")?);
    let done_key = VmPropertyKey::from_string(scope.alloc_string("done")?);

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

  #[test]
  fn iterator_protocol_smoke() -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let hooks = NoHooks;
    let limits = WebIdlLimits::default();

    let mut cx = VmJsWebIdlCx::new(&mut vm, &mut heap, limits, &hooks);

    // Build an array-like object `items` via WebIDL's IDL->JS helper (exercises alloc_array +
    // CreateDataPropertyOrThrow).
    let items =
      sequence_to_js_array(&mut cx, &limits, &["a", "b", "c"]).expect("sequence_to_js_array");

    // iterable = { items, [Symbol.iterator]: iterator_method }
    let iterable;
    {
      iterable = cx.scope.alloc_object()?;
      cx.scope.push_root(Value::Object(iterable))?;

      let items_key = VmPropertyKey::from_string(cx.scope.alloc_string("items")?);
      cx.scope.define_property(
        iterable,
        items_key,
        PropertyDescriptor {
          enumerable: true,
          configurable: true,
          kind: PropertyKind::Data {
            value: Value::Object(items),
            writable: true,
          },
        },
      )?;

      let iter_name = cx.scope.alloc_string("iterator")?;
      let iter_id = cx.vm.register_native_call(iterator_method_call_handler)?;
      let iter_fn = cx.scope.alloc_native_function(iter_id, None, iter_name, 0)?;

      let sym = cx.well_known_symbol(webidl::WellKnownSymbol::Iterator)?;
      let key = VmPropertyKey::from_symbol(sym);
      cx.scope.define_property(
        iterable,
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
    }

    let iterator = cx.get_iterator(Value::Object(iterable))?;
    let mut out = Vec::<String>::new();
    loop {
      let step = cx.iterator_next(iterator)?;
      if step.done {
        break;
      }
      let Value::String(s) = step.value else {
        return Err(VmError::TypeError("expected string iterator value"));
      };
      out.push(cx.scope.heap().get_string(s)?.to_utf8_lossy());
    }

    assert_eq!(out, vec!["a", "b", "c"]);
    Ok(())
  }

  #[derive(Default)]
  struct TestHostHooks {
    iterator_calls: usize,
    getter_calls: usize,
    to_primitive_calls: usize,
    value_of_calls: usize,
    to_string_calls: usize,
  }

  #[derive(Default)]
  struct TestHostCtx {
    host_calls: usize,
  }

  impl VmHostHooks for TestHostHooks {
    fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {
      panic!("unexpected promise job enqueued during iterator conversion");
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
      Some(self)
    }
  }

  fn iterator_method_observes_host_hooks(
    _vm: &mut Vm,
    scope: &mut Scope<'_>,
    _host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    _callee: GcObject,
    _this: Value,
    _args: &[Value],
  ) -> Result<Value, VmError> {
    if let Some(any) = hooks.as_any_mut() {
      if let Some(hooks) = any.downcast_mut::<TestHostHooks>() {
        hooks.iterator_calls += 1;
      }
    }

    // Return any object to satisfy `GetIteratorFromMethod`.
    let iter_obj = scope.alloc_object()?;
    scope.push_root(Value::Object(iter_obj))?;
    Ok(Value::Object(iter_obj))
  }

  fn iterator_method_observes_host_ctx_and_hooks(
    _vm: &mut Vm,
    scope: &mut Scope<'_>,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    _callee: GcObject,
    _this: Value,
    _args: &[Value],
  ) -> Result<Value, VmError> {
    if let Some(any) = host.as_any_mut().downcast_mut::<TestHostCtx>() {
      any.host_calls += 1;
    }
    if let Some(any) = hooks.as_any_mut() {
      if let Some(hooks) = any.downcast_mut::<TestHostHooks>() {
        hooks.iterator_calls += 1;
      }
    }

    let iter_obj = scope.alloc_object()?;
    scope.push_root(Value::Object(iter_obj))?;
    Ok(Value::Object(iter_obj))
  }

  fn iterator_getter_observes_host_hooks(
    _vm: &mut Vm,
    scope: &mut Scope<'_>,
    _host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    _callee: GcObject,
    this: Value,
    _args: &[Value],
  ) -> Result<Value, VmError> {
    if let Some(any) = hooks.as_any_mut() {
      if let Some(hooks) = any.downcast_mut::<TestHostHooks>() {
        hooks.getter_calls += 1;
      }
    }

    let Value::Object(obj) = this else {
      return Err(VmError::TypeError("iterator getter this is not object"));
    };

    // Return this.fn.
    let fn_key = VmPropertyKey::from_string(scope.alloc_string("fn")?);
    Ok(
      scope
        .heap()
        .object_get_own_data_property_value(obj, &fn_key)?
        .unwrap_or(Value::Undefined),
    )
  }

  fn value_of_observes_host_ctx_and_hooks(
    _vm: &mut Vm,
    _scope: &mut Scope<'_>,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    _callee: GcObject,
    _this: Value,
    _args: &[Value],
  ) -> Result<Value, VmError> {
    if let Some(any) = host.as_any_mut().downcast_mut::<TestHostCtx>() {
      any.host_calls += 1;
    }
    if let Some(any) = hooks.as_any_mut() {
      if let Some(hooks) = any.downcast_mut::<TestHostHooks>() {
        hooks.value_of_calls += 1;
      }
    }
    Ok(Value::Number(42.0))
  }

  fn to_primitive_observes_host_hooks(
    _vm: &mut Vm,
    scope: &mut Scope<'_>,
    _host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    _callee: GcObject,
    _this: Value,
    args: &[Value],
  ) -> Result<Value, VmError> {
    if let Some(any) = hooks.as_any_mut() {
      if let Some(hooks) = any.downcast_mut::<TestHostHooks>() {
        hooks.to_primitive_calls += 1;
      }
    }
 
    let hint = args.get(0).copied().unwrap_or(Value::Undefined);
    let Value::String(hint_s) = hint else {
      return Err(VmError::TypeError("@@toPrimitive hint is not a string"));
    };
    let hint = scope.heap().get_string(hint_s)?.to_utf8_lossy();
    Ok(if hint == "string" {
      Value::String(scope.alloc_string("hello")?)
    } else {
      Value::Number(42.0)
    })
  }

  fn to_primitive_observes_host_ctx_and_hooks(
    _vm: &mut Vm,
    scope: &mut Scope<'_>,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    _callee: GcObject,
    _this: Value,
    args: &[Value],
  ) -> Result<Value, VmError> {
    if let Some(any) = host.as_any_mut().downcast_mut::<TestHostCtx>() {
      any.host_calls += 1;
    }
    if let Some(any) = hooks.as_any_mut() {
      if let Some(hooks) = any.downcast_mut::<TestHostHooks>() {
        hooks.to_primitive_calls += 1;
      }
    }
 
    let hint = args.get(0).copied().unwrap_or(Value::Undefined);
    let Value::String(hint_s) = hint else {
      return Err(VmError::TypeError("@@toPrimitive hint is not a string"));
    };
    let hint = scope.heap().get_string(hint_s)?.to_utf8_lossy();
    Ok(if hint == "string" {
      Value::String(scope.alloc_string("hello")?)
    } else {
      Value::Number(42.0)
    })
  }

  fn to_string_observes_host_hooks(
    _vm: &mut Vm,
    scope: &mut Scope<'_>,
    _host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    _callee: GcObject,
    _this: Value,
    _args: &[Value],
  ) -> Result<Value, VmError> {
    if let Some(any) = hooks.as_any_mut() {
      if let Some(hooks) = any.downcast_mut::<TestHostHooks>() {
        hooks.to_string_calls += 1;
      }
    }
    let s = scope.alloc_string("hello")?;
    Ok(Value::String(s))
  }

  #[test]
  fn iterator_method_call_propagates_embedder_host_hooks_override() -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

    let hooks = NoHooks;
    let limits = WebIdlLimits::default();
    let mut host_hooks = TestHostHooks::default();

    vm.with_host_hooks_override(&mut host_hooks, |vm| -> Result<(), VmError> {
      let mut cx = VmJsWebIdlCx::new(vm, &mut heap, limits, &hooks);

      // iterable = { [Symbol.iterator]: <native fn> }
      let iterable = cx.scope.alloc_object()?;
      cx.scope.push_root(Value::Object(iterable))?;

      let iter_name = cx.scope.alloc_string("iterator")?;
      let iter_id = cx.vm.register_native_call(iterator_method_observes_host_hooks)?;
      let iter_fn = cx.scope.alloc_native_function(iter_id, None, iter_name, 0)?;
      cx.scope.push_root(Value::Object(iter_fn))?;

      let sym = cx.well_known_symbol(webidl::WellKnownSymbol::Iterator)?;
      let key = VmPropertyKey::from_symbol(sym);
      cx.scope.define_property(
        iterable,
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

      let iterator = cx.get_iterator(Value::Object(iterable))?;
      cx.scope.push_root(Value::Object(iterator))?;
      Ok(())
    })?;

    assert_eq!(host_hooks.iterator_calls, 1);
    assert_eq!(host_hooks.getter_calls, 0);
    Ok(())
  }

  #[test]
  fn iterator_method_call_propagates_host_context_from_native_call() -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

    let hooks = NoHooks;
    let limits = WebIdlLimits::default();
    let mut host_ctx = TestHostCtx::default();
    let mut host_hooks = TestHostHooks::default();

    {
      let mut outer = heap.scope();
      let mut cx = VmJsWebIdlCx::from_native_call(
        &mut vm,
        &mut outer,
        &mut host_ctx,
        &mut host_hooks,
        limits,
        &hooks,
      );

      // iterable = { [Symbol.iterator]: <native fn> }
      let iterable = cx.scope.alloc_object()?;
      cx.scope.push_root(Value::Object(iterable))?;

      let iter_name = cx.scope.alloc_string("iterator")?;
      let iter_id = cx
        .vm
        .register_native_call(iterator_method_observes_host_ctx_and_hooks)?;
      let iter_fn = cx.scope.alloc_native_function(iter_id, None, iter_name, 0)?;
      cx.scope.push_root(Value::Object(iter_fn))?;

      let sym = cx.well_known_symbol(webidl::WellKnownSymbol::Iterator)?;
      let key = VmPropertyKey::from_symbol(sym);
      cx.scope.define_property(
        iterable,
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

      let iterator = cx.get_iterator(Value::Object(iterable))?;
      cx.scope.push_root(Value::Object(iterator))?;
    }

    assert_eq!(host_ctx.host_calls, 1);
    assert_eq!(host_hooks.iterator_calls, 1);
    Ok(())
  }

  #[test]
  fn to_number_calls_value_of_with_host_context_and_hooks() -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let webidl_hooks = NoHooks;
    let limits = WebIdlLimits::default();

    let result = (|| -> Result<(), VmError> {
      let value_of_id = vm.register_native_call(value_of_observes_host_ctx_and_hooks)?;

      let mut scope = heap.scope();
      let obj = scope.alloc_object()?;
      scope.push_root(Value::Object(obj))?;

      let value_of_name = scope.alloc_string("valueOf")?;
      let value_of_fn = scope.alloc_native_function(value_of_id, None, value_of_name, 0)?;
      scope.push_root(Value::Object(value_of_fn))?;

      let value_of_key = VmPropertyKey::from_string(scope.alloc_string("valueOf")?);
      scope.define_property(
        obj,
        value_of_key,
        PropertyDescriptor {
          enumerable: true,
          configurable: true,
          kind: PropertyKind::Data {
            value: Value::Object(value_of_fn),
            writable: true,
          },
        },
      )?;

      let mut host_ctx = TestHostCtx::default();
      let mut host_hooks = TestHostHooks::default();
      let mut cx = VmJsWebIdlCx::from_native_call(
        &mut vm,
        &mut scope,
        &mut host_ctx,
        &mut host_hooks,
        limits,
        &webidl_hooks,
      );
      cx.scope.push_root(Value::Object(obj))?;

      let n = cx.to_number(Value::Object(obj))?;
      assert_eq!(n, 42.0);
      drop(cx);
      assert_eq!(host_ctx.host_calls, 1);
      assert_eq!(host_hooks.value_of_calls, 1);
      Ok(())
    })();

    realm.teardown(&mut heap);
    result
  }

  #[test]
  fn to_string_calls_to_string_with_host_hooks() -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let webidl_hooks = NoHooks;
    let limits = WebIdlLimits::default();

    let result = (|| -> Result<(), VmError> {
      let to_string_id = vm.register_native_call(to_string_observes_host_hooks)?;

      let mut scope = heap.scope();
      let obj = scope.alloc_object()?;
      scope.push_root(Value::Object(obj))?;

      let to_string_name = scope.alloc_string("toString")?;
      let to_string_fn = scope.alloc_native_function(to_string_id, None, to_string_name, 0)?;
      scope.push_root(Value::Object(to_string_fn))?;

      let to_string_key = VmPropertyKey::from_string(scope.alloc_string("toString")?);
      scope.define_property(
        obj,
        to_string_key,
        PropertyDescriptor {
          enumerable: true,
          configurable: true,
          kind: PropertyKind::Data {
            value: Value::Object(to_string_fn),
            writable: true,
          },
        },
      )?;

      let mut host_ctx = ();
      let mut host_hooks = TestHostHooks::default();
      let mut cx = VmJsWebIdlCx::from_native_call(
        &mut vm,
        &mut scope,
        &mut host_ctx,
        &mut host_hooks,
        limits,
        &webidl_hooks,
      );
      cx.scope.push_root(Value::Object(obj))?;

      let s = cx.to_string(Value::Object(obj))?;
      assert_eq!(cx.scope.heap().get_string(s)?.to_utf8_lossy(), "hello");
      drop(cx);
      assert_eq!(host_hooks.to_string_calls, 1);
      Ok(())
    })();

    realm.teardown(&mut heap);
    result
  }

  #[test]
  fn to_number_calls_value_of_propagates_embedder_host_hooks_override() -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let webidl_hooks = NoHooks;
    let limits = WebIdlLimits::default();
    let mut host_hooks = TestHostHooks::default();

    let result = vm.with_host_hooks_override(&mut host_hooks, |vm| -> Result<(), VmError> {
      let value_of_id = vm.register_native_call(value_of_observes_host_ctx_and_hooks)?;

      let mut scope = heap.scope();
      let obj = scope.alloc_object()?;
      scope.push_root(Value::Object(obj))?;

      let value_of_name = scope.alloc_string("valueOf")?;
      let value_of_fn = scope.alloc_native_function(value_of_id, None, value_of_name, 0)?;
      scope.push_root(Value::Object(value_of_fn))?;

      let value_of_key = VmPropertyKey::from_string(scope.alloc_string("valueOf")?);
      scope.define_property(
        obj,
        value_of_key,
        PropertyDescriptor {
          enumerable: true,
          configurable: true,
          kind: PropertyKind::Data {
            value: Value::Object(value_of_fn),
            writable: true,
          },
        },
      )?;

      let mut cx = VmJsWebIdlCx::new_in_scope(vm, &mut scope, limits, &webidl_hooks);
      cx.scope.push_root(Value::Object(obj))?;
      let n = cx.to_number(Value::Object(obj))?;
      assert_eq!(n, 42.0);
      Ok(())
    });

    realm.teardown(&mut heap);
    result?;
    assert_eq!(host_hooks.value_of_calls, 1);
    Ok(())
  }

  #[test]
  fn to_string_calls_to_string_propagates_embedder_host_hooks_override() -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let webidl_hooks = NoHooks;
    let limits = WebIdlLimits::default();
    let mut host_hooks = TestHostHooks::default();

    let result = vm.with_host_hooks_override(&mut host_hooks, |vm| -> Result<(), VmError> {
      let to_string_id = vm.register_native_call(to_string_observes_host_hooks)?;

      let mut scope = heap.scope();
      let obj = scope.alloc_object()?;
      scope.push_root(Value::Object(obj))?;

      let to_string_name = scope.alloc_string("toString")?;
      let to_string_fn = scope.alloc_native_function(to_string_id, None, to_string_name, 0)?;
      scope.push_root(Value::Object(to_string_fn))?;

      let to_string_key = VmPropertyKey::from_string(scope.alloc_string("toString")?);
      scope.define_property(
        obj,
        to_string_key,
        PropertyDescriptor {
          enumerable: true,
          configurable: true,
          kind: PropertyKind::Data {
            value: Value::Object(to_string_fn),
            writable: true,
          },
        },
      )?;

      let mut cx = VmJsWebIdlCx::new_in_scope(vm, &mut scope, limits, &webidl_hooks);
      cx.scope.push_root(Value::Object(obj))?;
      let s = cx.to_string(Value::Object(obj))?;
      assert_eq!(cx.scope.heap().get_string(s)?.to_utf8_lossy(), "hello");
      Ok(())
    });

    realm.teardown(&mut heap);
    result?;
    assert_eq!(host_hooks.to_string_calls, 1);
    Ok(())
  }

  #[test]
  fn to_number_calls_to_primitive_propagates_embedder_host_hooks_override() -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let webidl_hooks = NoHooks;
    let limits = WebIdlLimits::default();
    let mut host_hooks = TestHostHooks::default();
 
    let result = vm.with_host_hooks_override(&mut host_hooks, |vm| -> Result<(), VmError> {
      let to_prim_id = vm.register_native_call(to_primitive_observes_host_hooks)?;
 
      let mut scope = heap.scope();
      let obj = scope.alloc_object()?;
      scope.push_root(Value::Object(obj))?;
 
      let to_prim_name = scope.alloc_string("[Symbol.toPrimitive]")?;
      let to_prim_fn = scope.alloc_native_function(to_prim_id, None, to_prim_name, 1)?;
      scope.push_root(Value::Object(to_prim_fn))?;
 
      let sym = vm
        .intrinsics()
        .ok_or(VmError::Unimplemented("intrinsics not initialized"))?
        .well_known_symbols()
        .to_primitive;
      let key = VmPropertyKey::from_symbol(sym);
      scope.define_property(
        obj,
        key,
        PropertyDescriptor {
          enumerable: true,
          configurable: true,
          kind: PropertyKind::Data {
            value: Value::Object(to_prim_fn),
            writable: true,
          },
        },
      )?;
 
      let mut cx = VmJsWebIdlCx::new_in_scope(vm, &mut scope, limits, &webidl_hooks);
      cx.scope.push_root(Value::Object(obj))?;
      assert_eq!(cx.to_number(Value::Object(obj))?, 42.0);
      Ok(())
    });
 
    realm.teardown(&mut heap);
    result?;
    assert_eq!(host_hooks.to_primitive_calls, 1);
    Ok(())
  }

  #[test]
  fn to_number_calls_to_primitive_propagates_host_context_from_native_call() -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let webidl_hooks = NoHooks;
    let limits = WebIdlLimits::default();
 
    let result = (|| -> Result<(TestHostCtx, TestHostHooks), VmError> {
      let to_prim_id = vm.register_native_call(to_primitive_observes_host_ctx_and_hooks)?;
 
      let mut scope = heap.scope();
      let obj = scope.alloc_object()?;
      scope.push_root(Value::Object(obj))?;
 
      let to_prim_name = scope.alloc_string("[Symbol.toPrimitive]")?;
      let to_prim_fn = scope.alloc_native_function(to_prim_id, None, to_prim_name, 1)?;
      scope.push_root(Value::Object(to_prim_fn))?;
 
      let sym = vm
        .intrinsics()
        .ok_or(VmError::Unimplemented("intrinsics not initialized"))?
        .well_known_symbols()
        .to_primitive;
      let key = VmPropertyKey::from_symbol(sym);
      scope.define_property(
        obj,
        key,
        PropertyDescriptor {
          enumerable: true,
          configurable: true,
          kind: PropertyKind::Data {
            value: Value::Object(to_prim_fn),
            writable: true,
          },
        },
      )?;
 
      let mut host_ctx = TestHostCtx::default();
      let mut host_hooks = TestHostHooks::default();
      let mut cx = VmJsWebIdlCx::from_native_call(
        &mut vm,
        &mut scope,
        &mut host_ctx,
        &mut host_hooks,
        limits,
        &webidl_hooks,
      );
      cx.scope.push_root(Value::Object(obj))?;
      assert_eq!(cx.to_number(Value::Object(obj))?, 42.0);
      drop(cx);
      Ok((host_ctx, host_hooks))
    })();
 
    realm.teardown(&mut heap);
    let (host_ctx, host_hooks) = result?;
    assert_eq!(host_ctx.host_calls, 1);
    assert_eq!(host_hooks.to_primitive_calls, 1);
    Ok(())
  }

  #[test]
  fn iterator_get_method_propagates_embedder_host_hooks_override() -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

    let hooks = NoHooks;
    let limits = WebIdlLimits::default();
    let mut host_hooks = TestHostHooks::default();

    vm.with_host_hooks_override(&mut host_hooks, |vm| -> Result<(), VmError> {
      let mut cx = VmJsWebIdlCx::new(vm, &mut heap, limits, &hooks);

      // iterable = { fn: <native iter fn>, get [Symbol.iterator]() { ... } }
      let iterable = cx.scope.alloc_object()?;
      cx.scope.push_root(Value::Object(iterable))?;

      let iter_name = cx.scope.alloc_string("iterator")?;
      let iter_id = cx.vm.register_native_call(iterator_method_observes_host_hooks)?;
      let iter_fn = cx.scope.alloc_native_function(iter_id, None, iter_name, 0)?;
      cx.scope.push_root(Value::Object(iter_fn))?;

      // fn = iter_fn
      let fn_key = VmPropertyKey::from_string(cx.scope.alloc_string("fn")?);
      cx.scope.define_property(
        iterable,
        fn_key,
        PropertyDescriptor {
          enumerable: true,
          configurable: true,
          kind: PropertyKind::Data {
            value: Value::Object(iter_fn),
            writable: true,
          },
        },
      )?;

      // get [Symbol.iterator]() { return this.fn }
      let getter_name = cx.scope.alloc_string("getIterator")?;
      let getter_id: NativeFunctionId = cx.vm.register_native_call(iterator_getter_observes_host_hooks)?;
      let getter_fn = cx
        .scope
        .alloc_native_function(getter_id, None, getter_name, 0)?;
      cx.scope.push_root(Value::Object(getter_fn))?;

      let sym = cx.well_known_symbol(webidl::WellKnownSymbol::Iterator)?;
      let key = VmPropertyKey::from_symbol(sym);
      cx.scope.define_property(
        iterable,
        key,
        PropertyDescriptor {
          enumerable: true,
          configurable: true,
          kind: PropertyKind::Accessor {
            get: Value::Object(getter_fn),
            set: Value::Undefined,
          },
        },
      )?;

      let iterator = cx.get_iterator(Value::Object(iterable))?;
      cx.scope.push_root(Value::Object(iterator))?;
      Ok(())
    })?;

    assert_eq!(host_hooks.getter_calls, 1);
    assert_eq!(host_hooks.iterator_calls, 1);
    Ok(())
  }

  #[test]
  fn from_scope_roots_values_across_allocations() -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());
    // Stress rooting: force a GC before each allocation.
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 0));
    let hooks = NoHooks;
    let limits = WebIdlLimits::default();

    let mut outer = heap.scope();
    {
      let mut cx = VmJsWebIdlCx::from_scope(&mut vm, outer.reborrow(), limits, &hooks);

      let units: Vec<u16> = "hello".encode_utf16().collect();
      let s = cx.alloc_string_from_code_units(&units)?;

      // Perform many allocations that will trigger GC; `s` must remain valid via rooting.
      for i in 0..128usize {
        let _ = cx.scope.alloc_string(&format!("tmp{i}"))?;
        let _ = cx.scope.alloc_object()?;
      }

      assert_eq!(cx.scope.heap().get_string(s)?.to_utf8_lossy(), "hello");
    }

    Ok(())
  }

  fn assert_error_object(
    scope: &mut Scope<'_>,
    prototype: GcObject,
    value: Value,
    expected_name: &str,
    expected_message: &str,
  ) -> Result<(), VmError> {
    let Value::Object(obj) = value else {
      return Err(VmError::TypeError("expected thrown object"));
    };
    scope.push_root(Value::Object(obj))?;

    assert_eq!(scope.object_get_prototype(obj)?, Some(prototype));

    // name
    let name_key_s = scope.alloc_string("name")?;
    scope.push_root(Value::String(name_key_s))?;
    let name_key = VmPropertyKey::from_string(name_key_s);
    let name_desc = scope
      .heap()
      .object_get_own_property(obj, &name_key)?
      .ok_or(VmError::TypeError("missing name property"))?;
    assert!(!name_desc.enumerable);
    let PropertyKind::Data { value: name_v, .. } = name_desc.kind else {
      return Err(VmError::TypeError("name is not a data property"));
    };
    let Value::String(name_s) = name_v else {
      return Err(VmError::TypeError("name is not a string"));
    };
    assert_eq!(scope.heap().get_string(name_s)?.to_utf8_lossy(), expected_name);

    // message
    let msg_key_s = scope.alloc_string("message")?;
    scope.push_root(Value::String(msg_key_s))?;
    let msg_key = VmPropertyKey::from_string(msg_key_s);
    let msg_desc = scope
      .heap()
      .object_get_own_property(obj, &msg_key)?
      .ok_or(VmError::TypeError("missing message property"))?;
    assert!(!msg_desc.enumerable);
    let PropertyKind::Data { value: msg_v, .. } = msg_desc.kind else {
      return Err(VmError::TypeError("message is not a data property"));
    };
    let Value::String(msg_s) = msg_v else {
      return Err(VmError::TypeError("message is not a string"));
    };
    assert_eq!(
      scope.heap().get_string(msg_s)?.to_utf8_lossy(),
      expected_message
    );

    Ok(())
  }

  #[test]
  fn throw_type_error_creates_realm_error_object() -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let intr = *realm.intrinsics();

    let hooks = NoHooks;
    let limits = WebIdlLimits::default();
    let mut cx = VmJsWebIdlCx::new(&mut vm, &mut heap, limits, &hooks);

    let err = cx.throw_type_error("boom");
    let VmError::Throw(value) = err else {
      return Err(VmError::TypeError("expected VmError::Throw"));
    };

    {
      let mut inspect = cx.scope.reborrow();
      assert_error_object(
        &mut inspect,
        intr.type_error_prototype(),
        value,
        "TypeError",
        "boom",
      )?;
    }

    drop(cx);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn throw_range_error_creates_realm_error_object() -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let intr = *realm.intrinsics();

    let hooks = NoHooks;
    let limits = WebIdlLimits::default();
    let mut cx = VmJsWebIdlCx::new(&mut vm, &mut heap, limits, &hooks);

    let err = cx.throw_range_error("nope");
    let VmError::Throw(value) = err else {
      return Err(VmError::TypeError("expected VmError::Throw"));
    };

    {
      let mut inspect = cx.scope.reborrow();
      assert_error_object(
        &mut inspect,
        intr.range_error_prototype(),
        value,
        "RangeError",
        "nope",
      )?;
    }

    drop(cx);
    realm.teardown(&mut heap);
    Ok(())
  }

  #[test]
  fn get_own_property_reports_data_and_accessor_descriptors() -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let hooks = NoHooks;
    let limits = WebIdlLimits::default();
    let mut cx = VmJsWebIdlCx::new(&mut vm, &mut heap, limits, &hooks);

    let getter_name = cx.scope.alloc_string("g")?;
    cx.scope.push_root(Value::String(getter_name))?;
    let noop_id: NativeFunctionId = cx.vm.register_native_call(noop_call_handler)?;
    let getter = cx
      .scope
      .alloc_native_function(noop_id, None, getter_name, 0)?;
    cx.scope.push_root(Value::Object(getter))?;

    let obj = cx.scope.alloc_object()?;
    cx.scope.push_root(Value::Object(obj))?;

    // data property: enumerable
    let data_key = VmPropertyKey::from_string(cx.scope.alloc_string("data")?);
    cx.scope.define_property(
      obj,
      data_key,
      PropertyDescriptor {
        enumerable: true,
        configurable: true,
        kind: PropertyKind::Data {
          value: Value::Number(1.0),
          writable: true,
        },
      },
    )?;

    // accessor property: non-enumerable
    let acc_key = VmPropertyKey::from_string(cx.scope.alloc_string("acc")?);
    cx.scope.define_property(
      obj,
      acc_key,
      PropertyDescriptor {
        enumerable: false,
        configurable: true,
        kind: PropertyKind::Accessor {
          get: Value::Object(getter),
          set: Value::Undefined,
        },
      },
    )?;

    let data_key_s = cx.scope.alloc_string("data")?;
    let data_desc = cx
      .get_own_property(obj, WebIdlPropertyKey::String(data_key_s))?
      .expect("data property exists");
    assert!(data_desc.enumerable);
    let webidl::JsPropertyKind::Data { value } = data_desc.kind else {
      return Err(VmError::TypeError("expected data descriptor"));
    };
    assert_eq!(value, Value::Number(1.0));

    let acc_key_s = cx.scope.alloc_string("acc")?;
    let acc_desc = cx
      .get_own_property(obj, WebIdlPropertyKey::String(acc_key_s))?
      .expect("acc property exists");
    assert!(!acc_desc.enumerable);
    let webidl::JsPropertyKind::Accessor { get, set } = acc_desc.kind else {
      return Err(VmError::TypeError("expected accessor descriptor"));
    };
    assert_eq!(get, Value::Object(getter));
    assert_eq!(set, Value::Undefined);

    Ok(())
  }
}
