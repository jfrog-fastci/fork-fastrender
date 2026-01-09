use crate::runtime::{
  InterfaceId, IteratorRecord, JsOwnPropertyDescriptor, JsPropertyKind, JsRuntime, WebIdlHooks,
  WebIdlJsRuntime, WebIdlLimits,
};
use std::collections::HashMap;
use std::rc::Rc;
use vm_js::{
  GcObject, GcString, GcSymbol, Heap, HeapLimits, JsBigInt, PropertyDescriptor, PropertyKey,
  PropertyKind, Value, VmError, WeakGcObject,
};

type HostFn = Rc<dyn Fn(&mut VmJsRuntime, Value, &[Value]) -> Result<Value, VmError>>;

#[derive(Clone)]
enum HostObjectKind {
  PlatformObject {
    primary_interface: &'static str,
    implements: &'static [&'static str],
    opaque: u64,
  },
  Function(HostFn),
  Error {
    name: &'static str,
  },
  // Built-in internal-slot stubs (not yet provided by `vm-js`).
  #[allow(dead_code)]
  ArrayBuffer {
    shared: bool,
  },
  #[allow(dead_code)]
  DataView,
  #[allow(dead_code)]
  TypedArray {
    name: &'static str,
  },
}

/// A concrete [`WebIdlJsRuntime`] implementation backed by `ecma-rs`'s `vm-js` value types.
///
/// `vm-js` is currently a GC/value/interrupt scaffolding crate. For Web IDL conversions and
/// overload resolution we only need a small subset of ECMAScript abstract operations and an
/// object/property model that host bindings can use. This runtime provides that adapter layer.
pub struct VmJsRuntime {
  heap: Heap,
  /// Host-side metadata for objects that need special behaviour (callability, platform object
  /// branding, internal-slot stubs, etc). Keyed by `WeakGcObject` so it does not keep wrappers alive.
  host_objects: HashMap<WeakGcObject, HostObjectKind>,
  webidl_limits: WebIdlLimits,
  well_known_iterator: Option<GcSymbol>,
  well_known_async_iterator: Option<GcSymbol>,

  // Internal-slot emulation for `ToObject` wrapper objects.
  //
  // These are stored as hidden, non-enumerable symbol-keyed data properties so the heap GC traces
  // the stored values without requiring host-side tracing hooks.
  string_data_symbol: Option<GcSymbol>,
  boolean_data_symbol: Option<GcSymbol>,
  number_data_symbol: Option<GcSymbol>,
  bigint_data_symbol: Option<GcSymbol>,
  symbol_data_symbol: Option<GcSymbol>,

  last_swept_gc_runs: u64,

  // A tiny, explicit intern table for values where host code relies on stable string identity.
  //
  // `vm-js` strings are not automatically interned, and `Value` equality compares string handles
  // (not contents). Some host shims use strings as stand-ins for stable JS identity (until real
  // platform objects exist), so keep those specific strings alive and re-use the same handle.
  interned_window: Option<GcString>,
  interned_document: Option<GcString>,
}

impl VmJsRuntime {
  fn value_is_valid_or_primitive(&self, value: Value) -> bool {
    match value {
      Value::Undefined | Value::Null | Value::Bool(_) | Value::Number(_) | Value::BigInt(_) => true,
      Value::String(s) => self.heap.is_valid_string(s),
      Value::Symbol(s) => self.heap.is_valid_symbol(s),
      Value::Object(o) => self.heap.is_valid_object(o),
    }
  }

  /// Creates a new runtime with conservative heap limits.
  pub fn new() -> Self {
    // Defaults should be conservative; real embeddings should plumb in renderer budgets via
    // `VmJsRuntime::with_limits`.
    Self::with_limits(HeapLimits::new(16 * 1024 * 1024, 8 * 1024 * 1024))
  }

  pub fn with_limits(limits: HeapLimits) -> Self {
    Self {
      heap: Heap::new(limits),
      host_objects: HashMap::new(),
      webidl_limits: WebIdlLimits::default(),
      well_known_iterator: None,
      well_known_async_iterator: None,
      string_data_symbol: None,
      boolean_data_symbol: None,
      number_data_symbol: None,
      bigint_data_symbol: None,
      symbol_data_symbol: None,
      last_swept_gc_runs: 0,
      interned_window: None,
      interned_document: None,
    }
  }

  pub fn heap(&self) -> &Heap {
    &self.heap
  }

  pub fn heap_mut(&mut self) -> &mut Heap {
    &mut self.heap
  }

  pub fn set_webidl_limits(&mut self, limits: WebIdlLimits) {
    self.webidl_limits = limits;
  }

  fn sweep_dead_host_objects_if_needed(&mut self) {
    let gc_runs = self.heap.gc_runs();
    if gc_runs == self.last_swept_gc_runs {
      return;
    }
    self.last_swept_gc_runs = gc_runs;

    let heap = &self.heap;
    self
      .host_objects
      .retain(|weak, _| weak.upgrade(heap).is_some());
  }

  fn with_stack_roots<R>(
    &mut self,
    roots: impl IntoIterator<Item = Value>,
    f: impl FnOnce(&mut Self) -> Result<R, VmError>,
  ) -> Result<R, VmError> {
    // We can't hold a `Scope` guard across `f(self)` because `f` needs mutable access to the heap
    // (and may create its own scopes).
    //
    // Instead, explicitly push stack roots and truncate them after `f` returns.
    let base_len = self.heap.stack_root_len();
    for v in roots {
      // `vm-js` only debug-asserts root validity when pushing stack roots. Ensure we return an
      // error in release builds rather than silently enqueuing stale handles.
      if !self.value_is_valid_or_primitive(v) {
        self.heap.truncate_stack_roots(base_len);
        return Err(VmError::InvalidHandle);
      }
      if let Err(err) = self.heap.push_stack_root(v) {
        self.heap.truncate_stack_roots(base_len);
        return Err(err);
      }
    }

    let result = f(self);
    self.heap.truncate_stack_roots(base_len);
    result
  }

  fn alloc_string_handle(&mut self, s: &str) -> Result<GcString, VmError> {
    let mut scope = self.heap.scope();
    scope.alloc_string(s)
  }

  fn intern_window_string(&mut self) -> Result<GcString, VmError> {
    if let Some(handle) = self.interned_window {
      return Ok(handle);
    }
    let handle = self.alloc_string_handle("window")?;
    // Keep this handle alive for the lifetime of the runtime.
    let _ = self.heap.add_root(Value::String(handle))?;
    self.interned_window = Some(handle);
    Ok(handle)
  }

  fn intern_document_string(&mut self) -> Result<GcString, VmError> {
    if let Some(handle) = self.interned_document {
      return Ok(handle);
    }
    let handle = self.alloc_string_handle("document")?;
    // Keep this handle alive for the lifetime of the runtime.
    let _ = self.heap.add_root(Value::String(handle))?;
    self.interned_document = Some(handle);
    Ok(handle)
  }

  /// Creates a string [`PropertyKey`] from a Rust `&str`.
  ///
  /// This is a convenience for embeddings that want to access properties by name without having to
  /// allocate a JS string value first.
  pub fn prop_key_str(&mut self, s: &str) -> Result<PropertyKey, VmError> {
    self.property_key_from_str(s)
  }

  fn internal_symbol(
    slot: &mut Option<GcSymbol>,
    heap: &mut Heap,
    key: GcString,
  ) -> Result<GcSymbol, VmError> {
    if let Some(sym) = *slot {
      return Ok(sym);
    }
    let sym = heap.symbol_for(key)?;
    *slot = Some(sym);
    Ok(sym)
  }

  fn string_data_symbol(&mut self) -> Result<GcSymbol, VmError> {
    let key = self.alloc_string_handle("VmJsRuntime.[[StringData]]")?;
    Self::internal_symbol(&mut self.string_data_symbol, &mut self.heap, key)
  }

  fn boolean_data_symbol(&mut self) -> Result<GcSymbol, VmError> {
    let key = self.alloc_string_handle("VmJsRuntime.[[BooleanData]]")?;
    Self::internal_symbol(&mut self.boolean_data_symbol, &mut self.heap, key)
  }

  fn number_data_symbol(&mut self) -> Result<GcSymbol, VmError> {
    let key = self.alloc_string_handle("VmJsRuntime.[[NumberData]]")?;
    Self::internal_symbol(&mut self.number_data_symbol, &mut self.heap, key)
  }

  fn bigint_data_symbol(&mut self) -> Result<GcSymbol, VmError> {
    let key = self.alloc_string_handle("VmJsRuntime.[[BigIntData]]")?;
    Self::internal_symbol(&mut self.bigint_data_symbol, &mut self.heap, key)
  }

  fn symbol_data_symbol(&mut self) -> Result<GcSymbol, VmError> {
    let key = self.alloc_string_handle("VmJsRuntime.[[SymbolData]]")?;
    Self::internal_symbol(&mut self.symbol_data_symbol, &mut self.heap, key)
  }

  fn is_internal_key(&self, key: &PropertyKey) -> bool {
    let PropertyKey::Symbol(sym) = key else {
      return false;
    };
    self.string_data_symbol == Some(*sym)
      || self.boolean_data_symbol == Some(*sym)
      || self.number_data_symbol == Some(*sym)
      || self.bigint_data_symbol == Some(*sym)
      || self.symbol_data_symbol == Some(*sym)
  }

  /// Create a property key for the given string.
  ///
  /// This interns and roots the underlying string so the returned key remains valid even if the
  /// heap later runs GC.
  pub fn prop_key(&mut self, s: &str) -> Result<PropertyKey, VmError> {
    self.prop_key_str(s)
  }

  pub fn alloc_string_value(&mut self, s: &str) -> Result<Value, VmError> {
    let handle = match s {
      "window" => self.intern_window_string()?,
      "document" => self.intern_document_string()?,
      _ => self.alloc_string_handle(s)?,
    };
    Ok(Value::String(handle))
  }

  pub fn alloc_object_value(&mut self) -> Result<Value, VmError> {
    let obj = {
      let mut scope = self.heap.scope();
      scope.alloc_object()?
    };
    Ok(Value::Object(obj))
  }

  pub fn alloc_platform_object_value(
    &mut self,
    primary_interface: &'static str,
    implements: &'static [&'static str],
    opaque: u64,
  ) -> Result<Value, VmError> {
    let obj = {
      let mut scope = self.heap.scope();
      scope.alloc_object()?
    };
    self.host_objects.insert(
      WeakGcObject::from(obj),
      HostObjectKind::PlatformObject {
        primary_interface,
        implements,
        opaque,
      },
    );
    Ok(Value::Object(obj))
  }

  pub fn platform_object_opaque(&self, v: Value) -> Option<u64> {
    let Value::Object(obj) = v else {
      return None;
    };
    if !self.heap.is_valid_object(obj) {
      return None;
    }
    match self.host_objects.get(&WeakGcObject::from(obj))? {
      HostObjectKind::PlatformObject { opaque, .. } => Some(*opaque),
      _ => None,
    }
  }

  pub fn platform_object_primary_interface(&self, v: Value) -> Option<&'static str> {
    let Value::Object(obj) = v else {
      return None;
    };
    if !self.heap.is_valid_object(obj) {
      return None;
    }
    match self.host_objects.get(&WeakGcObject::from(obj))? {
      HostObjectKind::PlatformObject {
        primary_interface, ..
      } => Some(*primary_interface),
      _ => None,
    }
  }

  pub fn implements_interface(&self, v: Value, interface: &str) -> bool {
    <Self as WebIdlJsRuntime>::implements_interface(self, v, interface)
  }

  pub fn set_prototype(&mut self, obj: Value, proto: Option<Value>) -> Result<(), VmError> {
    let Value::Object(obj) = obj else {
      return Err(self.throw_type_error("set_prototype: receiver is not an object"));
    };
    let proto_obj = match proto {
      None => None,
      Some(Value::Object(p)) => Some(p),
      Some(_) => return Err(self.throw_type_error("set_prototype: prototype is not an object")),
    };
    self.heap.object_set_prototype(obj, proto_obj)
  }

  pub fn define_accessor_property(
    &mut self,
    obj: Value,
    key: PropertyKey,
    get: Value,
    set: Value,
    enumerable: bool,
  ) -> Result<(), VmError> {
    let Value::Object(obj) = obj else {
      return Err(self.throw_type_error("define_accessor_property: receiver is not an object"));
    };

    let desc = PropertyDescriptor {
      enumerable,
      configurable: true,
      kind: PropertyKind::Accessor { get, set },
    };
    let mut scope = self.heap.scope();
    scope.define_property(obj, key, desc)
  }

  fn define_hidden_slot(
    &mut self,
    obj: GcObject,
    sym: GcSymbol,
    value: Value,
  ) -> Result<(), VmError> {
    let desc = PropertyDescriptor {
      enumerable: false,
      configurable: false,
      kind: PropertyKind::Data {
        value,
        writable: false,
      },
    };
    let mut scope = self.heap.scope();
    scope.define_property(obj, PropertyKey::Symbol(sym), desc)
  }

  pub fn alloc_string_object_value(&mut self, s: &str) -> Result<Value, VmError> {
    let string_data = self.alloc_string_handle(s)?;
    self.alloc_string_object_from_handle(string_data)
  }

  fn alloc_string_object_from_handle(&mut self, string_data: GcString) -> Result<Value, VmError> {
    let sym = self.string_data_symbol()?;
    self.with_stack_roots([Value::String(string_data)], |rt| {
      let obj = {
        let mut scope = rt.heap.scope();
        scope.alloc_object()?
      };
      rt.define_hidden_slot(obj, sym, Value::String(string_data))?;
      Ok(Value::Object(obj))
    })
  }

  fn alloc_boolean_object_value(&mut self, boolean_data: bool) -> Result<Value, VmError> {
    let sym = self.boolean_data_symbol()?;
    let obj = {
      let mut scope = self.heap.scope();
      scope.alloc_object()?
    };
    self.define_hidden_slot(obj, sym, Value::Bool(boolean_data))?;
    Ok(Value::Object(obj))
  }

  fn alloc_number_object_value(&mut self, number_data: f64) -> Result<Value, VmError> {
    let sym = self.number_data_symbol()?;
    let obj = {
      let mut scope = self.heap.scope();
      scope.alloc_object()?
    };
    self.define_hidden_slot(obj, sym, Value::Number(number_data))?;
    Ok(Value::Object(obj))
  }

  fn alloc_bigint_object_value(&mut self, bigint_data: JsBigInt) -> Result<Value, VmError> {
    let sym = self.bigint_data_symbol()?;
    let obj = {
      let mut scope = self.heap.scope();
      scope.alloc_object()?
    };
    self.define_hidden_slot(obj, sym, Value::BigInt(bigint_data))?;
    Ok(Value::Object(obj))
  }

  fn alloc_symbol_object_value(&mut self, symbol_data: GcSymbol) -> Result<Value, VmError> {
    let sym = self.symbol_data_symbol()?;
    self.with_stack_roots([Value::Symbol(symbol_data)], |rt| {
      let obj = {
        let mut scope = rt.heap.scope();
        scope.alloc_object()?
      };
      rt.define_hidden_slot(obj, sym, Value::Symbol(symbol_data))?;
      Ok(Value::Object(obj))
    })
  }

  pub fn alloc_function_value<F>(&mut self, f: F) -> Result<Value, VmError>
  where
    F: Fn(&mut VmJsRuntime, Value, &[Value]) -> Result<Value, VmError> + 'static,
  {
    let obj = {
      let mut scope = self.heap.scope();
      scope.alloc_object()?
    };
    self.host_objects.insert(
      WeakGcObject::from(obj),
      HostObjectKind::Function(Rc::new(f)),
    );
    Ok(Value::Object(obj))
  }

  fn call_internal(
    &mut self,
    callee: Value,
    this: Value,
    args: &[Value],
  ) -> Result<Value, VmError> {
    self.sweep_dead_host_objects_if_needed();
    let Value::Object(func) = callee else {
      return Err(self.throw_type_error("value is not callable"));
    };
    if !self.heap.is_valid_object(func) {
      return Err(VmError::InvalidHandle);
    }
    let Some(HostObjectKind::Function(f)) = self.host_objects.get(&WeakGcObject::from(func)) else {
      return Err(self.throw_type_error("value is not callable"));
    };

    let f = f.clone();
    self.with_stack_roots(
      std::iter::once(callee)
        .chain(std::iter::once(this))
        .chain(args.iter().copied()),
      |rt| f(rt, this, args),
    )
  }

  /// Call a JS function value.
  ///
  /// This currently only supports host-defined function objects created via
  /// [`VmJsRuntime::alloc_function_value`]. It is sufficient for early host integration plumbing
  /// while the full `vm-js` interpreter is still under development.
  pub fn call_function(
    &mut self,
    callee: Value,
    this: Value,
    args: &[Value],
  ) -> Result<Value, VmError> {
    <Self as JsRuntime>::call(self, callee, this, args)
  }

  fn to_number_from_string(&self, s: GcString) -> Result<f64, VmError> {
    let js = self.heap.get_string(s)?;
    let text = js.to_utf8_lossy();
    let trimmed = text.trim();
    if trimmed.is_empty() {
      return Ok(0.0);
    }
    if trimmed.eq("Infinity") || trimmed.eq("+Infinity") {
      return Ok(f64::INFINITY);
    }
    if trimmed.eq("-Infinity") {
      return Ok(f64::NEG_INFINITY);
    }
    // Hex/binary/octal integer literals (ToNumber per ECMA-262).
    //
    // Note: StringToNumber does *not* accept a leading sign for these forms:
    // `Number("-0x10")` is `NaN` (use `parseInt` for signed radix parsing).
    let (has_sign, rest) = if let Some(rest) = trimmed.strip_prefix('-') {
      (true, rest)
    } else if let Some(rest) = trimmed.strip_prefix('+') {
      (true, rest)
    } else {
      (false, trimmed)
    };
    if has_sign {
      if rest.starts_with("0x")
        || rest.starts_with("0X")
        || rest.starts_with("0b")
        || rest.starts_with("0B")
        || rest.starts_with("0o")
        || rest.starts_with("0O")
      {
        return Ok(f64::NAN);
      }
    }

    if let Some(rest) = trimmed.strip_prefix("0x").or_else(|| trimmed.strip_prefix("0X")) {
      if rest.is_empty() {
        return Ok(f64::NAN);
      }
      if let Ok(v) = u64::from_str_radix(rest, 16) {
        return Ok(v as f64);
      }
      return Ok(f64::NAN);
    }
    if let Some(rest) = trimmed.strip_prefix("0b").or_else(|| trimmed.strip_prefix("0B")) {
      if rest.is_empty() {
        return Ok(f64::NAN);
      }
      if let Ok(v) = u64::from_str_radix(rest, 2) {
        return Ok(v as f64);
      }
      return Ok(f64::NAN);
    }
    if let Some(rest) = trimmed.strip_prefix("0o").or_else(|| trimmed.strip_prefix("0O")) {
      if rest.is_empty() {
        return Ok(f64::NAN);
      }
      if let Ok(v) = u64::from_str_radix(rest, 8) {
        return Ok(v as f64);
      }
      return Ok(f64::NAN);
    }

    match trimmed.parse::<f64>() {
      Ok(v) => Ok(v),
      Err(_) => Ok(f64::NAN),
    }
  }

  fn to_string_from_number(&mut self, n: f64) -> Result<GcString, VmError> {
    if n.is_nan() {
      return self.alloc_string_handle("NaN");
    }
    if n == 0.0 {
      // Covers +0 and -0.
      return self.alloc_string_handle("0");
    }
    if n == f64::INFINITY {
      return self.alloc_string_handle("Infinity");
    }
    if n == f64::NEG_INFINITY {
      return self.alloc_string_handle("-Infinity");
    }
    self.alloc_string_handle(&n.to_string())
  }

  fn create_error_object(&mut self, name: &'static str, message: &str) -> Value {
    let obj = match {
      let mut scope = self.heap.scope();
      scope.alloc_object()
    } {
      Ok(obj) => obj,
      Err(_) => return Value::Undefined,
    };

    let message_handle = match self
      .alloc_string_handle(message)
      .or_else(|_| self.alloc_string_handle("error"))
    {
      Ok(s) => s,
      Err(_) => return Value::Undefined,
    };

    self
      .host_objects
      .insert(WeakGcObject::from(obj), HostObjectKind::Error { name });

    let _ = self.with_stack_roots([Value::Object(obj), Value::String(message_handle)], |rt| {
      let name_key = rt.property_key_from_str("name")?;
      let name_value = rt.alloc_string("TypeError")?; // overwritten below when name != TypeError
      let name_value = match name {
        "TypeError" => name_value,
        other => rt.alloc_string(other)?,
      };
      rt.define_data_property(Value::Object(obj), name_key, name_value, false)?;

      let message_key = rt.property_key_from_str("message")?;
      rt.define_data_property(
        Value::Object(obj),
        message_key,
        Value::String(message_handle),
        false,
      )?;
      Ok(())
    });

    Value::Object(obj)
  }

  fn string_object_data(&self, obj: GcObject) -> Result<Option<GcString>, VmError> {
    let Some(sym) = self.string_data_symbol else {
      return Ok(None);
    };
    match self
      .heap
      .object_get_own_data_property_value(obj, &PropertyKey::Symbol(sym))
    {
      Ok(Some(Value::String(s))) => Ok(Some(s)),
      Ok(_) => Ok(None),
      Err(_) => Ok(None),
    }
  }

  fn boolean_object_data(&self, obj: GcObject) -> Result<Option<bool>, VmError> {
    let Some(sym) = self.boolean_data_symbol else {
      return Ok(None);
    };
    match self
      .heap
      .object_get_own_data_property_value(obj, &PropertyKey::Symbol(sym))
    {
      Ok(Some(Value::Bool(b))) => Ok(Some(b)),
      Ok(_) => Ok(None),
      Err(_) => Ok(None),
    }
  }

  fn number_object_data(&self, obj: GcObject) -> Result<Option<f64>, VmError> {
    let Some(sym) = self.number_data_symbol else {
      return Ok(None);
    };
    match self
      .heap
      .object_get_own_data_property_value(obj, &PropertyKey::Symbol(sym))
    {
      Ok(Some(Value::Number(n))) => Ok(Some(n)),
      Ok(_) => Ok(None),
      Err(_) => Ok(None),
    }
  }

  fn bigint_object_data(&self, obj: GcObject) -> Result<Option<JsBigInt>, VmError> {
    let Some(sym) = self.bigint_data_symbol else {
      return Ok(None);
    };
    match self
      .heap
      .object_get_own_data_property_value(obj, &PropertyKey::Symbol(sym))
    {
      Ok(Some(Value::BigInt(b))) => Ok(Some(b)),
      Ok(_) => Ok(None),
      Err(_) => Ok(None),
    }
  }

  fn symbol_object_data(&self, obj: GcObject) -> Result<Option<GcSymbol>, VmError> {
    let Some(sym) = self.symbol_data_symbol else {
      return Ok(None);
    };
    match self
      .heap
      .object_get_own_data_property_value(obj, &PropertyKey::Symbol(sym))
    {
      Ok(Some(Value::Symbol(s))) => Ok(Some(s)),
      Ok(_) => Ok(None),
      Err(_) => Ok(None),
    }
  }

  fn throw_symbol_to_number(&mut self, symbol_data: GcSymbol) -> VmError {
    let message = match self
      .heap
      .symbol_description(symbol_data)
      .and_then(|s| self.heap.get_string(s).ok())
      .map(|s| s.to_utf8_lossy())
    {
      Some(desc) => format!("Cannot convert a Symbol({desc}) value to a number"),
      None => "Cannot convert a Symbol value to a number".to_string(),
    };
    self.throw_type_error(&message)
  }

  fn throw_symbol_to_string(&mut self, symbol_data: GcSymbol) -> VmError {
    let message = match self
      .heap
      .symbol_description(symbol_data)
      .and_then(|s| self.heap.get_string(s).ok())
      .map(|s| s.to_utf8_lossy())
    {
      Some(desc) => format!("Cannot convert a Symbol({desc}) value to a string"),
      None => "Cannot convert a Symbol value to a string".to_string(),
    };
    self.throw_type_error(&message)
  }
}

impl WebIdlHooks<Value> for VmJsRuntime {
  fn is_platform_object(&self, value: Value) -> bool {
    let Value::Object(obj) = value else {
      return false;
    };
    if !self.heap.is_valid_object(obj) {
      return false;
    }
    matches!(
      self.host_objects.get(&WeakGcObject::from(obj)),
      Some(HostObjectKind::PlatformObject { .. })
    )
  }

  fn implements_interface(&self, value: Value, interface: InterfaceId) -> bool {
    let Value::Object(obj) = value else {
      return false;
    };
    if !self.heap.is_valid_object(obj) {
      return false;
    };
    let Some(HostObjectKind::PlatformObject {
      primary_interface,
      implements,
      ..
    }) = self.host_objects.get(&WeakGcObject::from(obj))
    else {
      return false;
    };
    InterfaceId::from_name(primary_interface) == interface
      || implements
        .iter()
        .any(|name| InterfaceId::from_name(name) == interface)
  }
}

impl JsRuntime for VmJsRuntime {
  type JsValue = Value;
  type PropertyKey = PropertyKey;
  type Error = VmError;

  fn js_undefined(&self) -> Value {
    Value::Undefined
  }

  fn js_null(&self) -> Value {
    Value::Null
  }

  fn js_boolean(&self, value: bool) -> Value {
    Value::Bool(value)
  }

  fn js_number(&self, value: f64) -> Value {
    Value::Number(value)
  }

  fn alloc_string(&mut self, value: &str) -> Result<Value, VmError> {
    self.alloc_string_value(value)
  }

  fn alloc_string_from_code_units(&mut self, units: &[u16]) -> Result<Value, VmError> {
    let handle = {
      let mut scope = self.heap.scope();
      scope.alloc_string_from_code_units(units)?
    };
    Ok(Value::String(handle))
  }

  fn is_undefined(&self, value: Value) -> bool {
    matches!(value, Value::Undefined)
  }

  fn is_null(&self, value: Value) -> bool {
    matches!(value, Value::Null)
  }

  fn with_string_code_units<R>(
    &mut self,
    string: Value,
    f: impl FnOnce(&[u16]) -> R,
  ) -> Result<R, VmError> {
    let handle = match string {
      Value::String(s) => s,
      Value::Object(obj) => self
        .string_object_data(obj)?
        .ok_or_else(|| self.throw_type_error("value is not a string"))?,
      _ => return Err(self.throw_type_error("value is not a string")),
    };
    let js = self.heap.get_string(handle)?;
    Ok(f(js.as_code_units()))
  }

  fn property_key_from_str(&mut self, s: &str) -> Result<PropertyKey, VmError> {
    Ok(PropertyKey::String(self.alloc_string_handle(s)?))
  }

  fn property_key_from_u32(&mut self, index: u32) -> Result<PropertyKey, VmError> {
    Ok(PropertyKey::String(
      self.alloc_string_handle(&index.to_string())?,
    ))
  }

  fn property_key_is_symbol(&self, key: PropertyKey) -> bool {
    matches!(key, PropertyKey::Symbol(_))
  }

  fn property_key_is_string(&self, key: PropertyKey) -> bool {
    matches!(key, PropertyKey::String(_))
  }

  fn property_key_to_js_string(&mut self, key: PropertyKey) -> Result<Value, VmError> {
    match key {
      PropertyKey::String(s) => Ok(Value::String(s)),
      PropertyKey::Symbol(_) => {
        Err(self.throw_type_error("Cannot convert a Symbol property key to a string"))
      }
    }
  }

  fn alloc_object(&mut self) -> Result<Value, VmError> {
    self.alloc_object_value()
  }

  fn alloc_array(&mut self) -> Result<Value, VmError> {
    let obj = {
      let mut scope = self.heap.scope();
      scope.alloc_array(0)?
    };
    Ok(Value::Object(obj))
  }

  fn define_data_property(
    &mut self,
    obj: Value,
    key: PropertyKey,
    value: Value,
    enumerable: bool,
  ) -> Result<(), VmError> {
    let Value::Object(obj) = obj else {
      return Err(self.throw_type_error("define_data_property: receiver is not an object"));
    };

    let desc = PropertyDescriptor {
      enumerable,
      configurable: true,
      kind: PropertyKind::Data {
        value,
        writable: true,
      },
    };
    let mut scope = self.heap.scope();
    scope.define_property(obj, key, desc)
  }

  fn is_object(&self, value: Value) -> bool {
    matches!(value, Value::Object(_))
  }

  fn is_callable(&self, value: Value) -> bool {
    let Value::Object(obj) = value else {
      return false;
    };
    if !self.heap.is_valid_object(obj) {
      return false;
    }
    matches!(
      self.host_objects.get(&WeakGcObject::from(obj)),
      Some(HostObjectKind::Function(_))
    )
  }

  fn is_boolean(&self, value: Value) -> bool {
    matches!(value, Value::Bool(_))
  }

  fn is_number(&self, value: Value) -> bool {
    matches!(value, Value::Number(_))
  }

  fn is_bigint(&self, value: Value) -> bool {
    matches!(value, Value::BigInt(_))
  }

  fn is_string(&self, value: Value) -> bool {
    matches!(value, Value::String(_))
  }

  fn is_symbol(&self, value: Value) -> bool {
    matches!(value, Value::Symbol(_))
  }

  fn to_object(&mut self, value: Value) -> Result<Value, VmError> {
    match value {
      Value::Undefined | Value::Null => {
        Err(self.throw_type_error("ToObject: cannot convert null or undefined to object"))
      }
      Value::Object(_) => Ok(value),
      Value::String(string_data) => Ok(self.alloc_string_object_from_handle(string_data)?),
      Value::Bool(boolean_data) => Ok(self.alloc_boolean_object_value(boolean_data)?),
      Value::Number(number_data) => Ok(self.alloc_number_object_value(number_data)?),
      Value::BigInt(bigint_data) => Ok(self.alloc_bigint_object_value(bigint_data)?),
      Value::Symbol(symbol_data) => Ok(self.alloc_symbol_object_value(symbol_data)?),
    }
  }

  fn call(&mut self, callee: Value, this: Value, args: &[Value]) -> Result<Value, VmError> {
    self.call_internal(callee, this, args)
  }

  fn to_boolean(&mut self, value: Value) -> Result<bool, VmError> {
    Ok(match value {
      Value::Undefined | Value::Null => false,
      Value::Bool(b) => b,
      Value::Number(n) => !(n == 0.0 || n.is_nan()),
      Value::BigInt(n) => !n.is_zero(),
      Value::String(s) => !self.heap.get_string(s)?.is_empty(),
      Value::Symbol(_) | Value::Object(_) => true,
    })
  }

  fn to_number(&mut self, value: Value) -> Result<f64, VmError> {
    Ok(match value {
      Value::Number(n) => n,
      Value::Bool(b) => {
        if b {
          1.0
        } else {
          0.0
        }
      }
      Value::Null => 0.0,
      Value::Undefined => f64::NAN,
      Value::String(s) => self.to_number_from_string(s)?,
      Value::BigInt(_) => {
        return Err(self.throw_type_error("Cannot convert a BigInt value to a number"));
      }
      Value::Symbol(_) => {
        return Err(self.throw_type_error("Cannot convert a Symbol value to a number"));
      }
      Value::Object(obj) => {
        if let Some(string_data) = self.string_object_data(obj)? {
          self.to_number_from_string(string_data)?
        } else if let Some(boolean_data) = self.boolean_object_data(obj)? {
          if boolean_data {
            1.0
          } else {
            0.0
          }
        } else if let Some(number_data) = self.number_object_data(obj)? {
          number_data
        } else if self.bigint_object_data(obj)?.is_some() {
          return Err(self.throw_type_error("Cannot convert a BigInt value to a number"));
        } else if let Some(symbol_data) = self.symbol_object_data(obj)? {
          return Err(self.throw_symbol_to_number(symbol_data));
        } else {
          f64::NAN
        }
      }
    })
  }

  fn to_string(&mut self, value: Value) -> Result<Value, VmError> {
    self.sweep_dead_host_objects_if_needed();

    self.with_stack_roots([value], |rt| {
      let s = match value {
        Value::String(s) => s,
        Value::Undefined => rt.alloc_string_handle("undefined")?,
        Value::Null => rt.alloc_string_handle("null")?,
        Value::Bool(true) => rt.alloc_string_handle("true")?,
        Value::Bool(false) => rt.alloc_string_handle("false")?,
        Value::Number(n) => rt.to_string_from_number(n)?,
        Value::BigInt(n) => rt.alloc_string_handle(&n.to_decimal_string())?,
        Value::Symbol(_) => {
          return Err(rt.throw_type_error("Cannot convert a Symbol value to a string"));
        }
        Value::Object(obj) => {
          if let Some(string_data) = rt.string_object_data(obj)? {
            string_data
          } else if let Some(boolean_data) = rt.boolean_object_data(obj)? {
            if boolean_data {
              rt.alloc_string_handle("true")?
            } else {
              rt.alloc_string_handle("false")?
            }
          } else if let Some(number_data) = rt.number_object_data(obj)? {
            rt.to_string_from_number(number_data)?
          } else if let Some(bigint_data) = rt.bigint_object_data(obj)? {
            rt.alloc_string_handle(&bigint_data.to_decimal_string())?
          } else if let Some(symbol_data) = rt.symbol_object_data(obj)? {
            return Err(rt.throw_symbol_to_string(symbol_data));
          } else if rt.heap.is_valid_object(obj) {
            match rt.host_objects.get(&WeakGcObject::from(obj)) {
              Some(HostObjectKind::Error { name }) => {
                let name_str = (*name).to_string();
                let message_key = rt.property_key_from_str("message")?;
                let message_value = rt
                  .heap
                  .object_get_own_data_property_value(obj, &message_key)
                  .unwrap_or(None)
                  .unwrap_or(Value::Undefined);
                let message = match message_value {
                  Value::String(s) => rt.heap.get_string(s)?.to_utf8_lossy(),
                  _ => String::new(),
                };
                let combined = if message.is_empty() {
                  name_str
                } else {
                  format!("{name_str}: {message}")
                };
                rt.alloc_string_handle(&combined)?
              }
              _ => rt.alloc_string_handle("[object Object]")?,
            }
          } else {
            rt.alloc_string_handle("[object Object]")?
          }
        }
      };
      Ok(Value::String(s))
    })
  }

  fn string_to_utf8_lossy(&mut self, string: Value) -> Result<String, VmError> {
    let handle = match string {
      Value::String(s) => s,
      Value::Object(obj) => self
        .string_object_data(obj)?
        .ok_or_else(|| self.throw_type_error("value is not a string"))?,
      _ => return Err(self.throw_type_error("value is not a string")),
    };
    Ok(self.heap.get_string(handle)?.to_utf8_lossy())
  }

  fn to_bigint(&mut self, value: Value) -> Result<Value, VmError> {
    match value {
      Value::BigInt(_) => Ok(value),
      Value::Object(obj) => self
        .bigint_object_data(obj)?
        .map(Value::BigInt)
        .ok_or_else(|| self.throw_type_error("Cannot convert value to a BigInt")),
      _ => Err(self.throw_type_error("Cannot convert value to a BigInt")),
    }
  }

  fn to_numeric(&mut self, value: Value) -> Result<Value, VmError> {
    if self.is_bigint(value) {
      return Ok(value);
    }
    Ok(Value::Number(self.to_number(value)?))
  }

  fn get(&mut self, obj: Value, key: PropertyKey) -> Result<Value, VmError> {
    let Value::Object(receiver) = obj else {
      return Err(self.throw_type_error("Get: receiver is not an object"));
    };
    self.sweep_dead_host_objects_if_needed();

    let Some(desc) = self.heap.get_property(receiver, &key)? else {
      return Ok(Value::Undefined);
    };
    match desc.kind {
      PropertyKind::Data { value, .. } => Ok(value),
      PropertyKind::Accessor { get, .. } => {
        if matches!(get, Value::Undefined) {
          return Ok(Value::Undefined);
        }
        if !self.is_callable(get) {
          return Err(self.throw_type_error("Getter is not callable"));
        }
        self.call_internal(get, Value::Object(receiver), &[])
      }
    }
  }

  fn own_property_keys(&mut self, obj: Value) -> Result<Vec<PropertyKey>, VmError> {
    let Value::Object(obj) = obj else {
      return Err(self.throw_type_error("OwnPropertyKeys: receiver is not an object"));
    };
    self.sweep_dead_host_objects_if_needed();

    let mut out = self.heap.own_property_keys(obj)?;
    out.retain(|k| !self.is_internal_key(k));
    Ok(out)
  }

  fn get_own_property(
    &mut self,
    obj: Value,
    key: PropertyKey,
  ) -> Result<Option<JsOwnPropertyDescriptor<Value>>, VmError> {
    let Value::Object(obj) = obj else {
      return Err(self.throw_type_error("GetOwnProperty: receiver is not an object"));
    };
    let Some(desc) = self.heap.object_get_own_property(obj, &key)? else {
      return Ok(None);
    };

    let kind = match desc.kind {
      PropertyKind::Data { value, .. } => JsPropertyKind::Data { value },
      PropertyKind::Accessor { get, set } => JsPropertyKind::Accessor { get, set },
    };

    Ok(Some(JsOwnPropertyDescriptor {
      enumerable: desc.enumerable,
      kind,
    }))
  }

  fn get_method(&mut self, obj: Value, key: PropertyKey) -> Result<Option<Value>, VmError> {
    let func = self.get(obj, key)?;
    if matches!(func, Value::Undefined | Value::Null) {
      return Ok(None);
    }
    if !self.is_callable(func) {
      return Err(self.throw_type_error("GetMethod: property is not callable"));
    }
    Ok(Some(func))
  }

  fn get_iterator_from_method(
    &mut self,
    iterable: Value,
    method: Value,
  ) -> Result<IteratorRecord<Value>, VmError> {
    let iterator = self.call_internal(method, iterable, &[])?;
    if !self.is_object(iterator) {
      return Err(self.throw_type_error("Iterator method did not return an object"));
    }

    self.with_stack_roots([iterator], |rt| {
      let next_key = rt.property_key_from_str("next")?;
      let next = rt.get(iterator, next_key)?;
      if !rt.is_callable(next) {
        return Err(rt.throw_type_error("Iterator.next is not callable"));
      }

      Ok(IteratorRecord {
        iterator,
        next_method: next,
        done: false,
      })
    })
  }

  fn iterator_step_value(
    &mut self,
    iterator_record: &mut IteratorRecord<Value>,
  ) -> Result<Option<Value>, VmError> {
    if iterator_record.done {
      return Ok(None);
    }

    let iterator = iterator_record.iterator;
    let next_method = iterator_record.next_method;
    self.with_stack_roots([iterator, next_method], |rt| {
      let result = rt.call_internal(next_method, iterator, &[])?;
      if !rt.is_object(result) {
        return Err(rt.throw_type_error("Iterator.next() did not return an object"));
      }

      rt.with_stack_roots([result], |rt| {
        let done_key = rt.property_key_from_str("done")?;
        let done = rt.get(result, done_key)?;
        let done = rt.to_boolean(done)?;
        if done {
          iterator_record.done = true;
          return Ok(None);
        }

        let value_key = rt.property_key_from_str("value")?;
        let value = rt.get(result, value_key)?;
        Ok(Some(value))
      })
    })
  }
}

impl WebIdlJsRuntime for VmJsRuntime {
  fn limits(&self) -> WebIdlLimits {
    self.webidl_limits
  }

  fn hooks(&self) -> &dyn WebIdlHooks<Value> {
    self
  }

  fn symbol_iterator(&mut self) -> Result<PropertyKey, VmError> {
    if let Some(sym) = self.well_known_iterator {
      return Ok(PropertyKey::Symbol(sym));
    }
    let key = self.alloc_string_handle("Symbol.iterator")?;
    let sym = self.heap.symbol_for(key)?;
    self.well_known_iterator = Some(sym);
    Ok(PropertyKey::Symbol(sym))
  }

  fn symbol_async_iterator(&mut self) -> Result<PropertyKey, VmError> {
    if let Some(sym) = self.well_known_async_iterator {
      return Ok(PropertyKey::Symbol(sym));
    }
    let key = self.alloc_string_handle("Symbol.asyncIterator")?;
    let sym = self.heap.symbol_for(key)?;
    self.well_known_async_iterator = Some(sym);
    Ok(PropertyKey::Symbol(sym))
  }

  fn symbol_to_property_key(&mut self, symbol: Value) -> Result<PropertyKey, VmError> {
    let Value::Symbol(sym) = symbol else {
      return Err(self.throw_type_error("expected a Symbol value"));
    };
    Ok(PropertyKey::Symbol(sym))
  }

  fn implements_interface(&self, value: Value, interface: &str) -> bool {
    let Value::Object(obj) = value else {
      return false;
    };
    if !self.heap.is_valid_object(obj) {
      return false;
    }
    let Some(HostObjectKind::PlatformObject {
      primary_interface,
      implements,
      ..
    }) = self.host_objects.get(&WeakGcObject::from(obj))
    else {
      return false;
    };
    *primary_interface == interface || implements.iter().any(|i| *i == interface)
  }

  fn platform_object_opaque(&self, value: Value) -> Option<u64> {
    VmJsRuntime::platform_object_opaque(self, value)
  }

  fn is_string_object(&self, value: Value) -> bool {
    let Value::Object(obj) = value else {
      return false;
    };
    if !self.heap.is_valid_object(obj) {
      return false;
    }
    matches!(self.string_object_data(obj), Ok(Some(_)))
  }

  fn is_platform_object(&self, value: Value) -> bool {
    let Value::Object(obj) = value else {
      return false;
    };
    if !self.heap.is_valid_object(obj) {
      return false;
    }
    matches!(
      self.host_objects.get(&WeakGcObject::from(obj)),
      Some(HostObjectKind::PlatformObject { .. })
    )
  }

  fn is_array_buffer(&self, value: Value) -> bool {
    let Value::Object(obj) = value else {
      return false;
    };
    if !self.heap.is_valid_object(obj) {
      return false;
    }
    matches!(
      self.host_objects.get(&WeakGcObject::from(obj)),
      Some(HostObjectKind::ArrayBuffer { .. })
    )
  }

  fn is_shared_array_buffer(&self, value: Value) -> bool {
    let Value::Object(obj) = value else {
      return false;
    };
    if !self.heap.is_valid_object(obj) {
      return false;
    }
    matches!(
      self.host_objects.get(&WeakGcObject::from(obj)),
      Some(HostObjectKind::ArrayBuffer { shared: true })
    )
  }

  fn is_data_view(&self, value: Value) -> bool {
    let Value::Object(obj) = value else {
      return false;
    };
    if !self.heap.is_valid_object(obj) {
      return false;
    }
    matches!(
      self.host_objects.get(&WeakGcObject::from(obj)),
      Some(HostObjectKind::DataView)
    )
  }

  fn typed_array_name(&self, value: Value) -> Option<&'static str> {
    let Value::Object(obj) = value else {
      return None;
    };
    if !self.heap.is_valid_object(obj) {
      return None;
    }
    match self.host_objects.get(&WeakGcObject::from(obj))? {
      HostObjectKind::TypedArray { name } => Some(name),
      _ => None,
    }
  }

  fn platform_object_to_js_value(&mut self, value: &webidl_ir::PlatformObject) -> Option<Value> {
    value.downcast_ref::<Value>().copied()
  }

  fn throw_type_error(&mut self, message: &str) -> VmError {
    VmError::Throw(self.create_error_object("TypeError", message))
  }

  fn throw_range_error(&mut self, message: &str) -> VmError {
    VmError::Throw(self.create_error_object("RangeError", message))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn as_utf8_lossy(rt: &VmJsRuntime, v: Value) -> String {
    let Value::String(s) = v else {
      panic!("expected string");
    };
    rt.heap.get_string(s).unwrap().to_utf8_lossy()
  }

  #[test]
  fn to_string_primitives() {
    let mut rt = VmJsRuntime::with_limits(HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));
    let s = rt.to_string(Value::Undefined).unwrap();
    assert_eq!(as_utf8_lossy(&rt, s), "undefined");
    let s = rt.to_string(Value::Null).unwrap();
    assert_eq!(as_utf8_lossy(&rt, s), "null");
    let s = rt.to_string(Value::Bool(true)).unwrap();
    assert_eq!(as_utf8_lossy(&rt, s), "true");
    let s = rt.to_string(Value::Bool(false)).unwrap();
    assert_eq!(as_utf8_lossy(&rt, s), "false");
    let s = rt.to_string(Value::Number(42.0)).unwrap();
    assert_eq!(as_utf8_lossy(&rt, s), "42");
    let s = rt.to_string(Value::Number(-0.0)).unwrap();
    assert_eq!(as_utf8_lossy(&rt, s), "0");
    let s = rt.to_string(Value::BigInt(JsBigInt::from_u128(42))).unwrap();
    assert_eq!(as_utf8_lossy(&rt, s), "42");
  }

  #[test]
  fn to_number_primitives() {
    let mut rt = VmJsRuntime::with_limits(HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));
    assert!(rt.to_number(Value::Undefined).unwrap().is_nan());
    assert_eq!(rt.to_number(Value::Null).unwrap(), 0.0);
    assert_eq!(rt.to_number(Value::Bool(true)).unwrap(), 1.0);
    assert_eq!(rt.to_number(Value::Bool(false)).unwrap(), 0.0);
    let s = rt.alloc_string_value("  123  ").unwrap();
    assert_eq!(rt.to_number(s).unwrap(), 123.0);
    assert!(matches!(
      rt.to_number(Value::BigInt(JsBigInt::from_u128(1))),
      Err(VmError::Throw(_))
    ));

    // Per ECMA-262 StringToNumber, signed hex strings are not valid numeric literals.
    let s = rt.alloc_string_value("-0x10").unwrap();
    assert!(rt.to_number(s).unwrap().is_nan());
    let s = rt.alloc_string_value("+0x10").unwrap();
    assert!(rt.to_number(s).unwrap().is_nan());
  }

  #[test]
  fn to_string_and_to_number_on_string_object() {
    let mut rt = VmJsRuntime::with_limits(HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));
    let obj = rt.alloc_string_object_value("456").unwrap();
    let s = rt.to_string(obj).unwrap();
    assert_eq!(as_utf8_lossy(&rt, s), "456");
    assert_eq!(rt.to_number(obj).unwrap(), 456.0);
    assert!(rt.is_string_object(obj));
  }

  #[test]
  fn get_method_invokes_getter_once() {
    let mut rt = VmJsRuntime::with_limits(HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));

    let calls = std::rc::Rc::new(std::cell::Cell::new(0u32));
    let calls_for_getter = calls.clone();

    let method_key = rt.property_key_from_str("method").unwrap();
    let method = rt
      .alloc_function_value(|_rt, _this, _args| Ok(Value::Undefined))
      .unwrap();

    let getter = rt
      .alloc_function_value(move |rt, this, _args| {
        calls_for_getter.set(calls_for_getter.get() + 1);
        rt.get(this, method_key)
      })
      .unwrap();

    let obj = rt.alloc_object_value().unwrap();
    rt.define_data_property(obj, method_key, method, true)
      .unwrap();
    let key = rt.property_key_from_str("m").unwrap();
    rt.define_accessor_property(obj, key, getter, Value::Undefined, true)
      .unwrap();

    let got = rt.get_method(obj, key).unwrap();
    assert!(got.is_some());
    assert_eq!(calls.get(), 1);
  }

  #[test]
  fn call_function_invokes_host_function_with_this_and_args() {
    let mut rt = VmJsRuntime::new();

    let this = rt.alloc_object_value().unwrap();
    let arg0 = rt.alloc_string_value("arg0").unwrap();
    let arg1 = Value::Number(123.0);

    let callee = rt
      .alloc_function_value(move |rt, got_this, got_args| {
        assert_eq!(got_this, this);
        assert_eq!(got_args, &[arg0, arg1]);
        rt.alloc_string_value("ret")
      })
      .unwrap();

    let ret = rt.call_function(callee, this, &[arg0, arg1]).unwrap();
    assert_eq!(as_utf8_lossy(&rt, ret), "ret");
  }

  #[test]
  fn call_function_non_callable_throws_type_error() {
    let mut rt = VmJsRuntime::new();

    let err = rt
      .call_function(Value::Number(1.0), Value::Undefined, &[])
      .unwrap_err();
    let thrown = match err {
      VmError::Throw(v) => v,
      other => panic!("expected thrown TypeError, got {other:?}"),
    };

    let name_key = rt.prop_key_str("name").unwrap();
    let name_value = rt.get(thrown, name_key).unwrap();
    assert_eq!(as_utf8_lossy(&rt, name_value), "TypeError");
  }

  #[test]
  fn gc_collects_unreachable_values() -> Result<(), VmError> {
    let mut rt = VmJsRuntime::with_limits(HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024));
    let baseline = rt.heap().used_bytes();

    let mut weak_objects: Vec<WeakGcObject> = Vec::new();
    let mut strings: Vec<GcString> = Vec::new();

    for i in 0..10_000u32 {
      let obj = rt.alloc_object_value()?;
      let Value::Object(obj) = obj else {
        unreachable!();
      };
      weak_objects.push(WeakGcObject::from(obj));

      let s = rt.alloc_string_value(&format!("s{i}"))?;
      let Value::String(s) = s else {
        unreachable!();
      };
      strings.push(s);
    }

    let before = rt.heap().used_bytes();
    assert!(before > baseline);

    rt.heap_mut().collect_garbage();
    let after = rt.heap().used_bytes();
    assert!(after < before);

    for weak in weak_objects {
      assert_eq!(weak.upgrade(rt.heap()), None);
    }
    for s in strings {
      assert!(!rt.heap().is_valid_string(s));
    }

    Ok(())
  }

  #[test]
  fn property_reachability_keeps_values_alive() -> Result<(), VmError> {
    let mut rt = VmJsRuntime::with_limits(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));

    let obj = rt.alloc_object_value()?;
    let Value::Object(obj_handle) = obj else {
      unreachable!();
    };
    let weak = WeakGcObject::from(obj_handle);

    let value = rt.alloc_string_value("hello")?;
    let Value::String(s_handle) = value else {
      unreachable!();
    };

    let key = rt.property_key_from_str("x")?;
    rt.define_data_property(obj, key, value, true)?;

    let root = rt.heap_mut().add_root(Value::Object(obj_handle))?;
    rt.heap_mut().collect_garbage();

    assert_eq!(weak.upgrade(rt.heap()), Some(obj_handle));
    assert!(rt.heap().is_valid_string(s_handle));

    let got = rt.get(obj, key)?;
    assert_eq!(as_utf8_lossy(&rt, got), "hello");

    rt.heap_mut().remove_root(root);
    rt.heap_mut().collect_garbage();

    assert_eq!(weak.upgrade(rt.heap()), None);
    assert!(!rt.heap().is_valid_string(s_handle));

    Ok(())
  }

  #[test]
  fn own_property_keys_orders_indices_then_strings_then_symbols() -> Result<(), VmError> {
    let mut rt = VmJsRuntime::with_limits(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));

    let obj = rt.alloc_object_value()?;

    let k_b = rt.property_key_from_str("b")?;
    let k_1 = rt.property_key_from_str("1")?;
    let k_a = rt.property_key_from_str("a")?;
    let k_0 = rt.property_key_from_str("0")?;
    let k_01 = rt.property_key_from_str("01")?;

    let sym1 = {
      let mut scope = rt.heap_mut().scope();
      scope.alloc_symbol(Some("s1"))?
    };
    let sym2 = {
      let mut scope = rt.heap_mut().scope();
      scope.alloc_symbol(Some("s2"))?
    };

    rt.define_data_property(obj, k_b, Value::Number(0.0), true)?;
    rt.define_data_property(obj, k_1, Value::Number(0.0), true)?;
    rt.define_data_property(obj, k_a, Value::Number(0.0), true)?;
    rt.define_data_property(obj, PropertyKey::Symbol(sym1), Value::Number(0.0), true)?;
    rt.define_data_property(obj, k_0, Value::Number(0.0), true)?;
    rt.define_data_property(obj, PropertyKey::Symbol(sym2), Value::Number(0.0), true)?;
    rt.define_data_property(obj, k_01, Value::Number(0.0), true)?;

    let got = rt.own_property_keys(obj)?;
    let expected = vec![
      k_0,
      k_1,
      k_b,
      k_a,
      k_01,
      PropertyKey::Symbol(sym1),
      PropertyKey::Symbol(sym2),
    ];
    assert_eq!(got.len(), expected.len());
    for (got, expected) in got.iter().zip(expected.iter()) {
      assert!(rt.heap().property_key_eq(got, expected));
    }

    Ok(())
  }

  #[test]
  fn inherited_accessor_get_uses_receiver_as_this() -> Result<(), VmError> {
    let mut rt = VmJsRuntime::with_limits(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));

    let proto = rt.alloc_object_value()?;
    let instance = rt.alloc_object_value()?;
    rt.set_prototype(instance, Some(proto))?;

    let seen_this = std::rc::Rc::new(std::cell::Cell::new(Value::Undefined));
    let seen_this_for_getter = seen_this.clone();
    let getter = rt.alloc_function_value(move |_rt, this, _args| {
      seen_this_for_getter.set(this);
      Ok(Value::Undefined)
    })?;

    let key = rt.property_key_from_str("x")?;
    rt.define_accessor_property(proto, key, getter, Value::Undefined, true)?;

    let key = rt.property_key_from_str("x")?;
    let _ = rt.get(instance, key)?;
    assert_eq!(seen_this.get(), instance);
    Ok(())
  }
}
