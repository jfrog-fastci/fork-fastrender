use super::VmJsRuntime;
use crate::runtime::{JsRuntime as LegacyJsRuntime, WebIdlJsRuntime as LegacyWebIdlJsRuntime};
use vm_js::{GcObject, GcString, GcSymbol, PropertyKey as VmPropertyKey, Value, VmError};

use webidl::{IteratorResult, PropertyKey as WebIdlPropertyKey, WellKnownSymbol};

/// GC-rooting conversion context for invoking `vendor/ecma-rs/webidl` algorithms on the legacy
/// heap-only [`VmJsRuntime`].
///
/// `vm-js` GC handles are only valid while the underlying allocation is reachable from the VM root
/// set. Unlike real `vm-js` realms (where JS values are naturally stack-rooted during execution),
/// this legacy runtime often manipulates values directly from Rust.
///
/// The `webidl` crate's conversions/overload resolution keep JS handles in locals across multiple
/// runtime calls and allocations, so using `VmJsRuntime` directly is not GC-safe under pressure.
///
/// This context solves that by recording the heap's current stack-root length and pushing roots for
/// every produced/consumed handle for the lifetime of the context. On drop, the root stack is
/// truncated back to the entry length.
pub struct VmJsWebIdlCx<'a> {
  rt: &'a mut VmJsRuntime,
  root_stack_len_at_entry: usize,
}

impl<'a> VmJsWebIdlCx<'a> {
  pub(crate) fn new(rt: &'a mut VmJsRuntime) -> Self {
    let root_stack_len_at_entry = rt.heap.stack_root_len();
    Self {
      rt,
      root_stack_len_at_entry,
    }
  }

  #[inline]
  fn root_values(&mut self, values: &[Value]) -> Result<(), VmError> {
    // `vm-js` only debug-asserts root validity when pushing stack roots. Ensure we return an error
    // in release builds rather than silently enqueuing stale handles.
    for &v in values {
      if !self.rt.value_is_valid_or_primitive(v) {
        return Err(VmError::invalid_handle());
      }
    }
    self.rt.heap.push_stack_roots(values)
  }

  #[inline]
  fn root_value(&mut self, value: Value) -> Result<(), VmError> {
    self.root_values(&[value])
  }
}

impl Drop for VmJsWebIdlCx<'_> {
  fn drop(&mut self) {
    self.rt.heap.truncate_stack_roots(self.root_stack_len_at_entry);
  }
}

fn to_vm_property_key(key: WebIdlPropertyKey<GcString, GcSymbol>) -> VmPropertyKey {
  match key {
    WebIdlPropertyKey::String(s) => VmPropertyKey::String(s),
    WebIdlPropertyKey::Symbol(s) => VmPropertyKey::Symbol(s),
  }
}

fn from_vm_property_key(key: VmPropertyKey) -> WebIdlPropertyKey<GcString, GcSymbol> {
  match key {
    VmPropertyKey::String(s) => WebIdlPropertyKey::String(s),
    VmPropertyKey::Symbol(s) => WebIdlPropertyKey::Symbol(s),
  }
}

impl webidl::JsRuntime for VmJsWebIdlCx<'_> {
  type Value = Value;
  type String = GcString;
  type Object = GcObject;
  type Symbol = GcSymbol;
  type Error = VmError;

  fn limits(&self) -> webidl::WebIdlLimits {
    // Keep this aligned with the legacy `WebIdlJsRuntime` implementation.
    self.rt.webidl_limits
  }

  fn hooks(&self) -> &dyn webidl::WebIdlHooks<Self::Value> {
    self.rt
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

  fn is_string_object(&self, value: Self::Value) -> bool {
    <VmJsRuntime as LegacyWebIdlJsRuntime>::is_string_object(self.rt, value)
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
    self.root_value(value)?;
    <VmJsRuntime as LegacyJsRuntime>::to_boolean(self.rt, value)
  }

  fn to_string(&mut self, value: Self::Value) -> Result<Self::String, Self::Error> {
    self.root_value(value)?;
    let v = <VmJsRuntime as LegacyJsRuntime>::to_string(self.rt, value)?;
    match v {
      Value::String(s) => {
        self.root_value(Value::String(s))?;
        Ok(s)
      }
      other => Err(VmError::InvariantViolation(match other {
        Value::Undefined => "ToString returned undefined",
        Value::Null => "ToString returned null",
        Value::Bool(_) => "ToString returned boolean",
        Value::Number(_) => "ToString returned number",
        Value::BigInt(_) => "ToString returned BigInt",
        Value::String(_) => unreachable!(),
        Value::Symbol(_) => "ToString returned symbol",
        Value::Object(_) => "ToString returned object",
      })),
    }
  }

  fn to_number(&mut self, value: Self::Value) -> Result<f64, Self::Error> {
    self.root_value(value)?;
    <VmJsRuntime as LegacyJsRuntime>::to_number(self.rt, value)
  }

  fn type_error(&mut self, message: &'static str) -> Self::Error {
    <VmJsRuntime as LegacyWebIdlJsRuntime>::throw_type_error(self.rt, message)
  }

  fn get(
    &mut self,
    object: Self::Object,
    key: WebIdlPropertyKey<Self::String, Self::Symbol>,
  ) -> Result<Self::Value, Self::Error> {
    let key_value = match key {
      WebIdlPropertyKey::String(s) => Value::String(s),
      WebIdlPropertyKey::Symbol(s) => Value::Symbol(s),
    };
    self.root_values(&[Value::Object(object), key_value])?;
    let key = to_vm_property_key(key);
    let v = <VmJsRuntime as LegacyJsRuntime>::get(self.rt, Value::Object(object), key)?;
    self.root_value(v)?;
    Ok(v)
  }

  fn get_method(
    &mut self,
    object: Self::Object,
    key: WebIdlPropertyKey<Self::String, Self::Symbol>,
  ) -> Result<Option<Self::Value>, Self::Error> {
    let key_value = match key {
      WebIdlPropertyKey::String(s) => Value::String(s),
      WebIdlPropertyKey::Symbol(s) => Value::Symbol(s),
    };
    self.root_values(&[Value::Object(object), key_value])?;
    let key = to_vm_property_key(key);
    let v = <VmJsRuntime as LegacyJsRuntime>::get_method(self.rt, Value::Object(object), key)?;
    if let Some(value) = v {
      self.root_value(value)?;
    }
    Ok(v)
  }

  fn own_property_keys(
    &mut self,
    object: Self::Object,
  ) -> Result<Vec<WebIdlPropertyKey<Self::String, Self::Symbol>>, Self::Error> {
    self.root_value(Value::Object(object))?;
    let keys = <VmJsRuntime as LegacyJsRuntime>::own_property_keys(self.rt, Value::Object(object))?;
    let out = keys.into_iter().map(from_vm_property_key).collect::<Vec<_>>();

    // Root the returned key handles so callers can hold them across allocations.
    let mut roots = Vec::new();
    roots
      .try_reserve_exact(out.len())
      .map_err(|_| VmError::OutOfMemory)?;
    for key in &out {
      roots.push(match key {
        WebIdlPropertyKey::String(s) => Value::String(*s),
        WebIdlPropertyKey::Symbol(s) => Value::Symbol(*s),
      });
    }
    self.root_values(&roots)?;

    Ok(out)
  }

  fn alloc_string_from_code_units(&mut self, units: &[u16]) -> Result<Self::String, Self::Error> {
    let s = {
      let mut scope = self.rt.heap_mut().scope();
      scope.alloc_string_from_code_units(units)?
    };
    self.root_value(Value::String(s))?;
    Ok(s)
  }

  fn alloc_object(&mut self) -> Result<Self::Object, Self::Error> {
    let obj = {
      let mut scope = self.rt.heap_mut().scope();
      scope.alloc_object()?
    };
    self.root_value(Value::Object(obj))?;
    Ok(obj)
  }

  fn alloc_array(&mut self, len: usize) -> Result<Self::Object, Self::Error> {
    let obj = {
      let mut scope = self.rt.heap_mut().scope();
      scope.alloc_array(len)?
    };
    self.root_value(Value::Object(obj))?;
    Ok(obj)
  }

  fn create_data_property_or_throw(
    &mut self,
    object: Self::Object,
    key: WebIdlPropertyKey<Self::String, Self::Symbol>,
    value: Self::Value,
  ) -> Result<(), Self::Error> {
    let key_value = match key {
      WebIdlPropertyKey::String(s) => Value::String(s),
      WebIdlPropertyKey::Symbol(s) => Value::Symbol(s),
    };
    self.root_values(&[Value::Object(object), key_value, value])?;
    let key = to_vm_property_key(key);
    let ok = self.rt.heap_mut().create_data_property(object, key, value)?;
    if ok {
      Ok(())
    } else {
      Err(<VmJsRuntime as LegacyWebIdlJsRuntime>::throw_type_error(
        self.rt,
        "CreateDataProperty rejected",
      ))
    }
  }

  fn well_known_symbol(&mut self, sym: WellKnownSymbol) -> Result<Self::Symbol, Self::Error> {
    let key = match sym {
      WellKnownSymbol::Iterator => <VmJsRuntime as LegacyWebIdlJsRuntime>::symbol_iterator(self.rt)?,
      WellKnownSymbol::AsyncIterator => {
        <VmJsRuntime as LegacyWebIdlJsRuntime>::symbol_async_iterator(self.rt)?
      }
    };
    let VmPropertyKey::Symbol(sym) = key else {
      return Err(VmError::InvariantViolation(
        "well_known_symbol did not return a symbol key",
      ));
    };
    self.root_value(Value::Symbol(sym))?;
    Ok(sym)
  }

  fn get_iterator(&mut self, value: Self::Value) -> Result<Self::Object, Self::Error> {
    // Spec: https://tc39.es/ecma262/#sec-getiterator (partial).
    let Value::Object(obj) = value else {
      return Err(self.type_error("GetIterator: value is not an object"));
    };
    self.root_value(Value::Object(obj))?;

    let iter_sym = self.well_known_symbol(WellKnownSymbol::Iterator)?;
    let method =
      <Self as webidl::JsRuntime>::get_method(self, obj, WebIdlPropertyKey::Symbol(iter_sym))?
        .ok_or_else(|| self.type_error("GetIterator: object is not iterable"))?;
    <Self as webidl::JsRuntime>::get_iterator_from_method(self, obj, method)
  }

  fn get_iterator_from_method(
    &mut self,
    object: Self::Object,
    method: Self::Value,
  ) -> Result<Self::Object, Self::Error> {
    // Spec: https://tc39.es/ecma262/#sec-getiteratorfrommethod.
    //
    // The ECMAScript spec models this as an IteratorRecord (iterator + next method). For this
    // heap-only runtime we keep the representation minimal and return the iterator object itself.
    // `iterator_next` is responsible for fetching/calling the `next` method.
    self.root_values(&[Value::Object(object), method])?;

    let iterator =
      <VmJsRuntime as LegacyJsRuntime>::call(self.rt, method, Value::Object(object), &[])?;
    let Value::Object(iterator_obj) = iterator else {
      return Err(self.type_error("Iterator method did not return an object"));
    };

    self.root_value(Value::Object(iterator_obj))?;
    Ok(iterator_obj)
  }

  fn iterator_next(
    &mut self,
    iterator: Self::Object,
  ) -> Result<IteratorResult<Self::Value>, Self::Error> {
    self.root_value(Value::Object(iterator))?;
    let next_key = {
      let s = self.rt.alloc_string_handle("next")?;
      WebIdlPropertyKey::String(s)
    };
    let next_method = self
      .get_method(iterator, next_key)?
      .ok_or_else(|| self.type_error("IteratorNext(iterator): next is undefined/null"))?;

    let result =
      <VmJsRuntime as LegacyJsRuntime>::call(self.rt, next_method, Value::Object(iterator), &[])?;
    let Value::Object(result_obj) = result else {
      return Err(self.type_error("IteratorNext(iterator): next() did not return an object"));
    };
    self.root_value(Value::Object(result_obj))?;

    let done_key = {
      let s = self.rt.alloc_string_handle("done")?;
      WebIdlPropertyKey::String(s)
    };
    let done_value = self.get(result_obj, done_key)?;
    let done = self.to_boolean(done_value)?;

    let value = if done {
      Value::Undefined
    } else {
      let value_key = {
        let s = self.rt.alloc_string_handle("value")?;
        WebIdlPropertyKey::String(s)
      };
      self.get(result_obj, value_key)?
    };
    self.root_value(value)?;

    Ok(IteratorResult { value, done })
  }
}

// Note: older versions of the `webidl` crate exposed an additional `WebIdlJsRuntime` trait. The
// current WebIDL scaffolding only requires `webidl::JsRuntime`, so keep this adapter minimal.
