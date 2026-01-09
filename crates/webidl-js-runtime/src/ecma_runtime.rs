use crate::runtime::{
  IteratorRecord, JsOwnPropertyDescriptor, JsPropertyKind, JsRuntime, WebIdlJsRuntime,
};
use std::collections::HashMap;
use std::rc::Rc;
use vm_js::{
  GcObject, GcString, GcSymbol, Heap, HeapLimits, PropertyDescriptor, PropertyKey, PropertyKind,
  Value, VmError,
};

type HostFn = Rc<dyn Fn(&mut VmJsRuntime, Value, &[Value]) -> Result<Value, VmError>>;

#[derive(Clone)]
enum HostObjectKind {
  Ordinary,
  PlatformObject {
    primary_interface: &'static str,
    implements: &'static [&'static str],
    opaque: u64,
  },
  Function(HostFn),
  StringObject { string_data: GcString },
  BooleanObject { boolean_data: bool },
  NumberObject { number_data: f64 },
  SymbolObject { symbol_data: GcSymbol },
  Error { name: &'static str, message: GcString },
  // Built-in internal-slot stubs (not yet provided by `vm-js`).
  ArrayBuffer {
    shared: bool,
  },
  DataView,
  TypedArray {
    name: &'static str,
  },
}

struct HostObject {
  kind: HostObjectKind,
  prototype: Option<GcObject>,
  properties: Vec<(PropertyKey, PropertyDescriptor)>,
}

fn root_value(heap: &mut Heap, value: Value) {
  // For now we keep all values ever created by this runtime alive by pinning them in the heap's
  // persistent root set. This avoids stale-handle bugs if the heap runs GC while the host keeps
  // references in `VmJsRuntime::objects`.
  //
  // Once `vm-js` grows a native object model, this adapter can shed most of this rooting logic.
  let _ = heap.add_root(value);
}

fn upsert_property(heap: &Heap, host: &mut HostObject, key: PropertyKey, desc: PropertyDescriptor) {
  for (existing_key, existing_desc) in &mut host.properties {
    if heap.property_key_eq(existing_key, &key) {
      *existing_desc = desc;
      return;
    }
  }
  host.properties.push((key, desc));
}

/// A concrete [`WebIdlJsRuntime`] implementation backed by `ecma-rs`'s `vm-js` value types.
///
/// `vm-js` is currently a GC/value/interrupt scaffolding crate. For Web IDL conversions and
/// overload resolution we only need a small subset of ECMAScript abstract operations and an
/// object/property model that host bindings can use. This runtime provides that adapter layer.
pub struct VmJsRuntime {
  heap: Heap,
  // Host-owned object model keyed by `vm-js` object handles.
  objects: HashMap<GcObject, HostObject>,
  // Intern pool for common strings used as property keys.
  interned_strings: HashMap<String, GcString>,
  well_known_iterator: Option<GcSymbol>,
  well_known_async_iterator: Option<GcSymbol>,
}

impl VmJsRuntime {
  /// Creates a new runtime with conservative heap limits suitable for unit tests.
  pub fn new() -> Self {
    // Keep limits high enough that adapter-level tests won't trigger GC. When this runtime is used
    // for real JS execution, the embedding should plumb in renderer-level memory budgets.
    let limits = HeapLimits::new(128 * 1024 * 1024, 128 * 1024 * 1024);
    Self {
      heap: Heap::new(limits),
      objects: HashMap::new(),
      interned_strings: HashMap::new(),
      well_known_iterator: None,
      well_known_async_iterator: None,
    }
  }

  pub fn heap(&self) -> &Heap {
    &self.heap
  }

  pub fn heap_mut(&mut self) -> &mut Heap {
    &mut self.heap
  }

  fn intern_string(&mut self, s: &str) -> Result<GcString, VmError> {
    if let Some(existing) = self.interned_strings.get(s).copied() {
      return Ok(existing);
    }
    let handle = {
      let mut scope = self.heap.scope();
      scope.alloc_string(s)?
    };
    // Keep the string alive even if GC runs.
    root_value(&mut self.heap, Value::String(handle));
    self.interned_strings.insert(s.to_string(), handle);
    Ok(handle)
  }

  /// Creates a string [`PropertyKey`] from a Rust `&str`.
  ///
  /// This is a convenience for embeddings (e.g. DOM bindings) that need to define/read properties
  /// by name without having to pattern-match a [`Value::String`] just to construct
  /// `PropertyKey::String`.
  ///
  /// # GC / rooting
  ///
  /// `Value`/`PropertyKey` are GC handles. Today, this adapter pins all values it allocates in the
  /// heap's persistent root set (see `root_value`), so returned keys remain valid for the lifetime
  /// of the `VmJsRuntime`. Callers should still treat returned handles as GC-managed; a future
  /// engine integration may require explicit rooting or scoped handles instead of globally pinning
  /// everything.
  pub fn prop_key_str(&mut self, s: &str) -> Result<PropertyKey, VmError> {
    Ok(PropertyKey::String(self.intern_string(s)?))
  }

  pub fn alloc_string_value(&mut self, s: &str) -> Result<Value, VmError> {
    Ok(Value::String(self.intern_string(s)?))
  }

  pub fn alloc_object_value(&mut self) -> Result<Value, VmError> {
    let obj = {
      let mut scope = self.heap.scope();
      scope.alloc_object()?
    };
    root_value(&mut self.heap, Value::Object(obj));
    self.objects.insert(
      obj,
      HostObject {
        kind: HostObjectKind::Ordinary,
        prototype: None,
        properties: Vec::new(),
      },
    );
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
    root_value(&mut self.heap, Value::Object(obj));
    self.objects.insert(
      obj,
      HostObject {
        kind: HostObjectKind::PlatformObject {
          primary_interface,
          implements,
          opaque,
        },
        prototype: None,
        properties: Vec::new(),
      },
    );
    Ok(Value::Object(obj))
  }

  pub fn platform_object_opaque(&self, v: Value) -> Option<u64> {
    let Value::Object(obj) = v else {
      return None;
    };
    let host = self.objects.get(&obj)?;
    match &host.kind {
      HostObjectKind::PlatformObject { opaque, .. } => Some(*opaque),
      _ => None,
    }
  }

  pub fn platform_object_primary_interface(&self, v: Value) -> Option<&'static str> {
    let Value::Object(obj) = v else {
      return None;
    };
    let host = self.objects.get(&obj)?;
    match &host.kind {
      HostObjectKind::PlatformObject {
        primary_interface, ..
      } => Some(*primary_interface),
      _ => None,
    }
  }

  pub fn implements_interface(&self, v: Value, interface: &str) -> bool {
    let Value::Object(obj) = v else {
      return false;
    };
    let Some(host) = self.objects.get(&obj) else {
      return false;
    };
    match &host.kind {
      HostObjectKind::PlatformObject {
        primary_interface,
        implements,
        ..
      } => *primary_interface == interface || implements.iter().any(|i| *i == interface),
      _ => false,
    }
  }

  pub fn alloc_string_object_value(&mut self, s: &str) -> Result<Value, VmError> {
    let string_data = self.intern_string(s)?;
    self.alloc_string_object_from_handle(string_data)
  }

  fn alloc_string_object_from_handle(&mut self, string_data: GcString) -> Result<Value, VmError> {
    let obj = {
      let mut scope = self.heap.scope();
      scope.alloc_object()?
    };
    root_value(&mut self.heap, Value::Object(obj));
    root_value(&mut self.heap, Value::String(string_data));
    self.objects.insert(
      obj,
      HostObject {
        kind: HostObjectKind::StringObject { string_data },
        prototype: None,
        properties: Vec::new(),
      },
    );
    Ok(Value::Object(obj))
  }

  fn alloc_boolean_object_value(&mut self, boolean_data: bool) -> Result<Value, VmError> {
    let obj = {
      let mut scope = self.heap.scope();
      scope.alloc_object()?
    };
    root_value(&mut self.heap, Value::Object(obj));
    self.objects.insert(
      obj,
      HostObject {
        kind: HostObjectKind::BooleanObject { boolean_data },
        prototype: None,
        properties: Vec::new(),
      },
    );
    Ok(Value::Object(obj))
  }

  fn alloc_number_object_value(&mut self, number_data: f64) -> Result<Value, VmError> {
    let obj = {
      let mut scope = self.heap.scope();
      scope.alloc_object()?
    };
    root_value(&mut self.heap, Value::Object(obj));
    self.objects.insert(
      obj,
      HostObject {
        kind: HostObjectKind::NumberObject { number_data },
        prototype: None,
        properties: Vec::new(),
      },
    );
    Ok(Value::Object(obj))
  }

  fn alloc_symbol_object_value(&mut self, symbol_data: GcSymbol) -> Result<Value, VmError> {
    let obj = {
      let mut scope = self.heap.scope();
      scope.alloc_object()?
    };
    root_value(&mut self.heap, Value::Object(obj));
    root_value(&mut self.heap, Value::Symbol(symbol_data));
    self.objects.insert(
      obj,
      HostObject {
        kind: HostObjectKind::SymbolObject { symbol_data },
        prototype: None,
        properties: Vec::new(),
      },
    );
    Ok(Value::Object(obj))
  }

  pub fn alloc_function_value<F>(&mut self, f: F) -> Result<Value, VmError>
  where
    F: Fn(&mut VmJsRuntime, Value, &[Value]) -> Result<Value, VmError> + 'static,
  {
    let obj = {
      let mut scope = self.heap.scope();
      scope.alloc_object()?
    };
    root_value(&mut self.heap, Value::Object(obj));
    self.objects.insert(
      obj,
      HostObject {
        kind: HostObjectKind::Function(Rc::new(f)),
        prototype: None,
        properties: Vec::new(),
      },
    );
    Ok(Value::Object(obj))
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
    let Some(host) = self.objects.get_mut(&obj) else {
      return Err(VmError::Unimplemented(
        "set_prototype on non-host object is not supported",
      ));
    };
    host.prototype = proto_obj;
    Ok(())
  }

  pub fn define_data_property(
    &mut self,
    obj: Value,
    key: PropertyKey,
    value: Value,
    enumerable: bool,
  ) -> Result<(), VmError> {
    let Value::Object(obj) = obj else {
      return Err(self.throw_type_error("define_data_property: receiver is not an object"));
    };
    let Some(host) = self.objects.get_mut(&obj) else {
      return Err(VmError::Unimplemented(
        "define_data_property on non-host object is not supported",
      ));
    };

    // Root the stored values so GC cannot invalidate handles that the host holds in `self.objects`.
    match key {
      PropertyKey::String(s) => root_value(&mut self.heap, Value::String(s)),
      PropertyKey::Symbol(sym) => root_value(&mut self.heap, Value::Symbol(sym)),
    }
    root_value(&mut self.heap, value);

    let desc = PropertyDescriptor {
      enumerable,
      configurable: true,
      kind: PropertyKind::Data {
        value,
        writable: true,
      },
    };
    upsert_property(&self.heap, host, key, desc);
    Ok(())
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
    let Some(host) = self.objects.get_mut(&obj) else {
      return Err(VmError::Unimplemented(
        "define_accessor_property on non-host object is not supported",
      ));
    };

    match key {
      PropertyKey::String(s) => root_value(&mut self.heap, Value::String(s)),
      PropertyKey::Symbol(sym) => root_value(&mut self.heap, Value::Symbol(sym)),
    }
    root_value(&mut self.heap, get);
    root_value(&mut self.heap, set);

    let desc = PropertyDescriptor {
      enumerable,
      configurable: true,
      kind: PropertyKind::Accessor { get, set },
    };
    upsert_property(&self.heap, host, key, desc);
    Ok(())
  }

  fn call_internal(&mut self, callee: Value, this: Value, args: &[Value]) -> Result<Value, VmError> {
    let Value::Object(func) = callee else {
      return Err(self.throw_type_error("value is not callable"));
    };
    let Some(obj) = self.objects.get(&func) else {
      return Err(VmError::Unimplemented(
        "Call: calling non-host functions is not supported",
      ));
    };
    let HostObjectKind::Function(f) = &obj.kind else {
      return Err(self.throw_type_error("value is not callable"));
    };

    let f = f.clone();
    f(self, this, args)
  }

  /// Invokes `callee` as a function with an explicit `this` value and argument list.
  ///
  /// This is the minimal embedding API needed by DOM/WebIDL bindings to call event listeners and
  /// callback interfaces.
  ///
  /// # Current limitations
  ///
  /// Only host-callable objects created via [`VmJsRuntime::alloc_function_value`] are supported
  /// today. Attempting to call an arbitrary JS value will either throw a `TypeError` (if it is not
  /// callable) or return [`VmError::Unimplemented`] (if it is some non-host callable we don't know
  /// how to invoke yet).
  ///
  /// # GC / rooting
  ///
  /// `Value`/`PropertyKey` are GC handles. This runtime currently pins all values it allocates in
  /// the heap's persistent root set, allowing callers to store handles without additional rooting.
  /// Do not rely on this long-term: a future engine integration may require explicit rooting or
  /// handle scopes.
  pub fn call_function(&mut self, callee: Value, this: Value, args: &[Value]) -> Result<Value, VmError> {
    <Self as JsRuntime>::call(self, callee, this, args)
  }
  fn find_own_property(&self, obj: GcObject, key: &PropertyKey) -> Option<PropertyDescriptor> {
    let host = self.objects.get(&obj)?;
    for (k, desc) in &host.properties {
      if self.heap.property_key_eq(k, key) {
        return Some(*desc);
      }
    }
    None
  }

  fn string_to_array_index(&self, s: GcString) -> Option<u32> {
    let js = self.heap.get_string(s).ok()?;
    let units = js.as_code_units();
    if units.is_empty() {
      return None;
    }
    if units.len() > 1 && units[0] == b'0' as u16 {
      return None;
    }
    let mut n: u64 = 0;
    for &u in units {
      if !(b'0' as u16..=b'9' as u16).contains(&u) {
        return None;
      }
      n = n.checked_mul(10)?;
      n = n.checked_add((u - b'0' as u16) as u64)?;
      if n > u32::MAX as u64 {
        return None;
      }
    }
    // Array index is uint32 < 2^32 - 1.
    if n == u32::MAX as u64 {
      return None;
    }
    Some(n as u32)
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
    let (sign, digits) = if let Some(rest) = trimmed.strip_prefix('-') {
      (-1.0, rest)
    } else if let Some(rest) = trimmed.strip_prefix('+') {
      (1.0, rest)
    } else {
      (1.0, trimmed)
    };
    if let Some(rest) = digits
      .strip_prefix("0x")
      .or_else(|| digits.strip_prefix("0X"))
    {
      if rest.is_empty() {
        return Ok(f64::NAN);
      }
      if let Ok(v) = u64::from_str_radix(rest, 16) {
        return Ok((v as f64) * sign);
      }
      return Ok(f64::NAN);
    }
    if let Some(rest) = digits
      .strip_prefix("0b")
      .or_else(|| digits.strip_prefix("0B"))
    {
      if rest.is_empty() {
        return Ok(f64::NAN);
      }
      if let Ok(v) = u64::from_str_radix(rest, 2) {
        return Ok((v as f64) * sign);
      }
      return Ok(f64::NAN);
    }
    if let Some(rest) = digits
      .strip_prefix("0o")
      .or_else(|| digits.strip_prefix("0O"))
    {
      if rest.is_empty() {
        return Ok(f64::NAN);
      }
      if let Ok(v) = u64::from_str_radix(rest, 8) {
        return Ok((v as f64) * sign);
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
      return self.intern_string("NaN");
    }
    if n == 0.0 {
      // Covers +0 and -0.
      return self.intern_string("0");
    }
    if n == f64::INFINITY {
      return self.intern_string("Infinity");
    }
    if n == f64::NEG_INFINITY {
      return self.intern_string("-Infinity");
    }

    // Note: This is not a full implementation of ECMA-262 `Number::toString` formatting, but it is
    // sufficient for WebIDL conversions which generally do not depend on the exact exponent
    // formatting. We intentionally use Rust's shortest-roundtrip formatting here.
    self.intern_string(&n.to_string())
  }

  fn create_error_object(&mut self, name: &'static str, message: &str) -> Value {
    // Allocate error object.
    let obj = match {
      let mut scope = self.heap.scope();
      scope.alloc_object()
    } {
      Ok(obj) => obj,
      Err(_) => {
        // If allocation fails, fall back to throwing a primitive.
        return Value::Undefined;
      }
    };
    root_value(&mut self.heap, Value::Object(obj));

    let message_handle = match self
      .intern_string(message)
      .or_else(|_| self.intern_string("error"))
    {
      Ok(s) => s,
      Err(_) => {
        // If we cannot allocate the message string, fall back to throwing a primitive.
        return Value::Undefined;
      }
    };
    root_value(&mut self.heap, Value::String(message_handle));

    self.objects.insert(
      obj,
      HostObject {
        kind: HostObjectKind::Error {
          name,
          message: message_handle,
        },
        prototype: None,
        properties: Vec::new(),
      },
    );

    // Best-effort "name" + "message" own properties (non-enumerable per spec, but not critical).
    if let (Ok(name_key), Ok(name_value)) = (self.intern_string("name"), self.intern_string(name)) {
      let _ = self.define_data_property(
        Value::Object(obj),
        PropertyKey::String(name_key),
        Value::String(name_value),
        false,
      );
    }
    if let Ok(message_key) = self.intern_string("message") {
      let _ = self.define_data_property(
        Value::Object(obj),
        PropertyKey::String(message_key),
        Value::String(message_handle),
        false,
      );
    }

    Value::Object(obj)
  }
}

impl Default for VmJsRuntime {
  fn default() -> Self {
    Self::new()
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
  fn property_key_from_str(&mut self, s: &str) -> Result<PropertyKey, VmError> {
    self.prop_key_str(s)
  }

  fn property_key_from_u32(&mut self, index: u32) -> Result<PropertyKey, VmError> {
    self.prop_key_str(&index.to_string())
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
      PropertyKey::Symbol(_) => Err(self.throw_type_error(
        "Cannot convert a Symbol property key to a string",
      )),
    }
  }

  fn alloc_object(&mut self) -> Result<Value, VmError> {
    self.alloc_object_value()
  }

  fn alloc_array(&mut self) -> Result<Value, VmError> {
    // `vm-js` does not yet provide Array exotic objects. For WebIDL conversions we only require an
    // object with indexed own properties.
    self.alloc_object_value()
  }

  fn define_data_property(
    &mut self,
    obj: Value,
    key: PropertyKey,
    value: Value,
    enumerable: bool,
  ) -> Result<(), VmError> {
    VmJsRuntime::define_data_property(self, obj, key, value, enumerable)
  }

  fn is_object(&self, value: Value) -> bool {
    matches!(value, Value::Object(_))
  }

  fn is_callable(&self, value: Value) -> bool {
    let Value::Object(obj) = value else {
      return false;
    };
    matches!(
      self.objects.get(&obj).map(|o| &o.kind),
      Some(HostObjectKind::Function(_))
    )
  }

  fn is_boolean(&self, value: Value) -> bool {
    matches!(value, Value::Bool(_))
  }

  fn is_number(&self, value: Value) -> bool {
    matches!(value, Value::Number(_))
  }

  fn is_bigint(&self, _value: Value) -> bool {
    false
  }

  fn is_string(&self, value: Value) -> bool {
    matches!(value, Value::String(_))
  }

  fn is_symbol(&self, value: Value) -> bool {
    matches!(value, Value::Symbol(_))
  }

  fn to_object(&mut self, value: Value) -> Result<Value, VmError> {
    match value {
      Value::Undefined | Value::Null => Err(self.throw_type_error(
        "ToObject: cannot convert null or undefined to object",
      )),
      Value::Object(_) => Ok(value),
      Value::String(string_data) => Ok(self.alloc_string_object_from_handle(string_data)?),
      Value::Bool(boolean_data) => Ok(self.alloc_boolean_object_value(boolean_data)?),
      Value::Number(number_data) => Ok(self.alloc_number_object_value(number_data)?),
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
      Value::Symbol(_) => {
        return Err(self.throw_type_error("Cannot convert a Symbol value to a number"));
      }
      Value::Object(obj) => match self.objects.get(&obj).map(|o| &o.kind) {
        Some(HostObjectKind::StringObject { string_data }) => self.to_number_from_string(*string_data)?,
        Some(HostObjectKind::BooleanObject { boolean_data }) => {
          if *boolean_data { 1.0 } else { 0.0 }
        }
        Some(HostObjectKind::NumberObject { number_data }) => *number_data,
        Some(HostObjectKind::SymbolObject { symbol_data }) => {
          let message = match self
            .heap
            .symbol_description(*symbol_data)
            .and_then(|s| self.heap.get_string(s).ok())
            .map(|s| s.to_utf8_lossy())
          {
            Some(desc) => format!("Cannot convert a Symbol({desc}) value to a number"),
            None => "Cannot convert a Symbol value to a number".to_string(),
          };
          return Err(self.throw_type_error(&message));
        }
        _ => f64::NAN,
      },
    })
  }

  fn to_string(&mut self, value: Value) -> Result<Value, VmError> {
    let s = match value {
      Value::String(s) => s,
      Value::Undefined => self.intern_string("undefined")?,
      Value::Null => self.intern_string("null")?,
      Value::Bool(true) => self.intern_string("true")?,
      Value::Bool(false) => self.intern_string("false")?,
      Value::Number(n) => self.to_string_from_number(n)?,
      Value::Symbol(_) => {
        return Err(self.throw_type_error("Cannot convert a Symbol value to a string"));
      }
      Value::Object(obj) => match self.objects.get(&obj).map(|o| &o.kind) {
        Some(HostObjectKind::StringObject { string_data }) => *string_data,
        Some(HostObjectKind::BooleanObject { boolean_data }) => {
          if *boolean_data {
            self.intern_string("true")?
          } else {
            self.intern_string("false")?
          }
        }
        Some(HostObjectKind::NumberObject { number_data }) => self.to_string_from_number(*number_data)?,
        Some(HostObjectKind::SymbolObject { symbol_data }) => {
          let message = match self
            .heap
            .symbol_description(*symbol_data)
            .and_then(|s| self.heap.get_string(s).ok())
            .map(|s| s.to_utf8_lossy())
          {
            Some(desc) => format!("Cannot convert a Symbol({desc}) value to a string"),
            None => "Cannot convert a Symbol value to a string".to_string(),
          };
          return Err(self.throw_type_error(&message));
        }
        Some(HostObjectKind::Error { name, message }) => {
          // A minimal `Error.prototype.toString`-like formatting.
          let name = (*name).to_string();
          let message = self.heap.get_string(*message)?.to_utf8_lossy();
          let combined = if message.is_empty() {
            name
          } else {
            format!("{name}: {message}")
          };
          self.intern_string(&combined)?
        }
        _ => self.intern_string("[object Object]")?,
      },
    };
    Ok(Value::String(s))
  }

  fn to_bigint(&mut self, _value: Value) -> Result<Value, VmError> {
    Err(self.throw_type_error("BigInt is not supported by vm-js yet"))
  }

  fn to_numeric(&mut self, value: Value) -> Result<Value, VmError> {
    if self.is_bigint(value) {
      return Ok(value);
    }
    Ok(Value::Number(self.to_number(value)?))
  }

  fn get(&mut self, obj: Value, key: PropertyKey) -> Result<Value, VmError> {
    // Per ECMAScript `[[Get]]`, accessor properties are invoked with the original receiver, not the
    // object in the prototype chain where the property was found.
    let receiver = obj;
    let Value::Object(mut current) = obj else {
      return Err(self.throw_type_error("Get: receiver is not an object"));
    };

    loop {
      let Some(host) = self.objects.get(&current) else {
        return Err(VmError::Unimplemented(
          "Get on non-host objects is not supported",
        ));
      };

      if let Some(desc) = self.find_own_property(current, &key) {
        return match desc.kind {
          PropertyKind::Data { value, .. } => Ok(value),
          PropertyKind::Accessor { get, .. } => {
            if matches!(get, Value::Undefined) {
              return Ok(Value::Undefined);
            }
            if !self.is_callable(get) {
              return Err(self.throw_type_error("Getter is not callable"));
            }
            self.call(get, receiver, &[])
          }
        };
      }

      let Some(proto) = host.prototype else {
        return Ok(Value::Undefined);
      };
      current = proto;
    }
  }

  fn own_property_keys(&mut self, obj: Value) -> Result<Vec<PropertyKey>, VmError> {
    let Value::Object(obj) = obj else {
      return Err(self.throw_type_error("OwnPropertyKeys: receiver is not an object"));
    };
    let Some(host) = self.objects.get(&obj) else {
      return Err(VmError::Unimplemented(
        "OwnPropertyKeys on non-host objects is not supported",
      ));
    };

    let mut array_keys: Vec<(u32, PropertyKey)> = Vec::new();
    let mut string_keys: Vec<PropertyKey> = Vec::new();
    let mut symbol_keys: Vec<PropertyKey> = Vec::new();

    for (k, _desc) in &host.properties {
      match k {
        PropertyKey::String(s) => {
          if let Some(idx) = self.string_to_array_index(*s) {
            array_keys.push((idx, *k));
          } else {
            string_keys.push(*k);
          }
        }
        PropertyKey::Symbol(_) => symbol_keys.push(*k),
      }
    }

    array_keys.sort_by_key(|(idx, _)| *idx);
    let mut out = Vec::with_capacity(array_keys.len() + string_keys.len() + symbol_keys.len());
    out.extend(array_keys.into_iter().map(|(_idx, k)| k));
    out.extend(string_keys);
    out.extend(symbol_keys);
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
    let Some(desc) = self.find_own_property(obj, &key) else {
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

    let next_key = self.prop_key_str("next")?;
    let next = self.get(iterator, next_key)?;
    if !self.is_callable(next) {
      return Err(self.throw_type_error("Iterator.next is not callable"));
    }

    Ok(IteratorRecord {
      iterator,
      next_method: next,
      done: false,
    })
  }

  fn iterator_step_value(
    &mut self,
    iterator_record: &mut IteratorRecord<Value>,
  ) -> Result<Option<Value>, VmError> {
    if iterator_record.done {
      return Ok(None);
    }

    let result = self.call_internal(iterator_record.next_method, iterator_record.iterator, &[])?;
    if !self.is_object(result) {
      return Err(self.throw_type_error("Iterator.next() did not return an object"));
    }

    let done_key = self.prop_key_str("done")?;
    let done = self.get(result, done_key)?;
    let done = self.to_boolean(done)?;
    if done {
      iterator_record.done = true;
      return Ok(None);
    }

    let value_key = self.prop_key_str("value")?;
    let value = self.get(result, value_key)?;
    Ok(Some(value))
  }
}

impl WebIdlJsRuntime for VmJsRuntime {
  fn symbol_iterator(&mut self) -> Result<Value, VmError> {
    if let Some(sym) = self.well_known_iterator {
      return Ok(Value::Symbol(sym));
    }
    let key = self.intern_string("Symbol.iterator")?;
    let sym = self.heap.symbol_for(key)?;
    root_value(&mut self.heap, Value::Symbol(sym));
    self.well_known_iterator = Some(sym);
    Ok(Value::Symbol(sym))
  }

  fn symbol_async_iterator(&mut self) -> Result<Value, VmError> {
    if let Some(sym) = self.well_known_async_iterator {
      return Ok(Value::Symbol(sym));
    }
    let key = self.intern_string("Symbol.asyncIterator")?;
    let sym = self.heap.symbol_for(key)?;
    root_value(&mut self.heap, Value::Symbol(sym));
    self.well_known_async_iterator = Some(sym);
    Ok(Value::Symbol(sym))
  }

  fn implements_interface(&self, value: Value, interface: &str) -> bool {
    VmJsRuntime::implements_interface(self, value, interface)
  }

  fn platform_object_opaque(&self, value: Value) -> Option<u64> {
    VmJsRuntime::platform_object_opaque(self, value)
  }

  fn is_string_object(&self, value: Value) -> bool {
    let Value::Object(obj) = value else {
      return false;
    };
    matches!(
      self.objects.get(&obj).map(|o| &o.kind),
      Some(HostObjectKind::StringObject { .. })
    )
  }

  fn is_array_buffer(&self, value: Value) -> bool {
    let Value::Object(obj) = value else {
      return false;
    };
    matches!(
      self.objects.get(&obj).map(|o| &o.kind),
      Some(HostObjectKind::ArrayBuffer { .. })
    )
  }

  fn is_shared_array_buffer(&self, value: Value) -> bool {
    let Value::Object(obj) = value else {
      return false;
    };
    matches!(
      self.objects.get(&obj).map(|o| &o.kind),
      Some(HostObjectKind::ArrayBuffer { shared: true })
    )
  }

  fn is_data_view(&self, value: Value) -> bool {
    let Value::Object(obj) = value else {
      return false;
    };
    matches!(
      self.objects.get(&obj).map(|o| &o.kind),
      Some(HostObjectKind::DataView)
    )
  }

  fn typed_array_name(&self, value: Value) -> Option<&'static str> {
    let Value::Object(obj) = value else {
      return None;
    };
    match self.objects.get(&obj)?.kind {
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
    let mut rt = VmJsRuntime::new();
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
  }

  #[test]
  fn to_number_primitives() {
    let mut rt = VmJsRuntime::new();
    assert!(rt.to_number(Value::Undefined).unwrap().is_nan());
    assert_eq!(rt.to_number(Value::Null).unwrap(), 0.0);
    assert_eq!(rt.to_number(Value::Bool(true)).unwrap(), 1.0);
    assert_eq!(rt.to_number(Value::Bool(false)).unwrap(), 0.0);
    let s = rt.alloc_string_value("  123  ").unwrap();
    assert_eq!(rt.to_number(s).unwrap(), 123.0);
  }

  #[test]
  fn to_string_and_to_number_on_string_object() {
    let mut rt = VmJsRuntime::new();
    let obj = rt.alloc_string_object_value("456").unwrap();
    let s = rt.to_string(obj).unwrap();
    assert_eq!(as_utf8_lossy(&rt, s), "456");
    assert_eq!(rt.to_number(obj).unwrap(), 456.0);
    assert!(rt.is_string_object(obj));
  }

  #[test]
  fn get_method_invokes_getter_once() {
    let mut rt = VmJsRuntime::new();

    let calls = std::rc::Rc::new(std::cell::Cell::new(0u32));
    let calls_for_getter = calls.clone();

    let method = rt
      .alloc_function_value(|_rt, _this, _args| Ok(Value::Undefined))
      .unwrap();

    let getter = rt
      .alloc_function_value(move |_rt, _this, _args| {
        calls_for_getter.set(calls_for_getter.get() + 1);
        Ok(method)
      })
      .unwrap();

    let obj = rt.alloc_object_value().unwrap();
    let key = rt.prop_key_str("m").unwrap();
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
}
