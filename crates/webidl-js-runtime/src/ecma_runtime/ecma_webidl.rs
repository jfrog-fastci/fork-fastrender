use super::VmJsRuntime;
use crate::runtime::{JsRuntime as LegacyJsRuntime, WebIdlJsRuntime as LegacyWebIdlJsRuntime};
use vm_js::{GcObject, GcString, GcSymbol, PropertyKey as VmPropertyKey, Value, VmError};

use webidl::{
  IteratorResult, JsOwnPropertyDescriptor, PropertyKey as WebIdlPropertyKey, WellKnownSymbol,
};

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

const ITER_REC_ITERATOR: &str = "VmJsRuntime.webidl.iterator_record.iterator";
const ITER_REC_NEXT: &str = "VmJsRuntime.webidl.iterator_record.next_method";
const ITER_REC_DONE: &str = "VmJsRuntime.webidl.iterator_record.done";

impl webidl::JsRuntime for VmJsRuntime {
  type Value = Value;
  type String = GcString;
  type Object = GcObject;
  type Symbol = GcSymbol;
  type Error = VmError;

  fn limits(&self) -> webidl::WebIdlLimits {
    // Keep this aligned with the legacy `WebIdlJsRuntime` implementation.
    self.webidl_limits
  }

  fn hooks(&self) -> &dyn webidl::WebIdlHooks<Self::Value> {
    self
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
    <Self as LegacyWebIdlJsRuntime>::is_string_object(self, value)
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
    <Self as LegacyJsRuntime>::to_boolean(self, value)
  }

  fn to_string(&mut self, value: Self::Value) -> Result<Self::String, Self::Error> {
    let v = <Self as LegacyJsRuntime>::to_string(self, value)?;
    match v {
      Value::String(s) => Ok(s),
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
    <Self as LegacyJsRuntime>::to_number(self, value)
  }

  fn type_error(&mut self, message: &'static str) -> Self::Error {
    <Self as LegacyWebIdlJsRuntime>::throw_type_error(self, message)
  }

  fn get(
    &mut self,
    object: Self::Object,
    key: WebIdlPropertyKey<Self::String, Self::Symbol>,
  ) -> Result<Self::Value, Self::Error> {
    let key = to_vm_property_key(key);
    <Self as LegacyJsRuntime>::get(self, Value::Object(object), key)
  }

  fn get_method(
    &mut self,
    object: Self::Object,
    key: WebIdlPropertyKey<Self::String, Self::Symbol>,
  ) -> Result<Option<Self::Value>, Self::Error> {
    let key = to_vm_property_key(key);
    <Self as LegacyJsRuntime>::get_method(self, Value::Object(object), key)
  }

  fn own_property_keys(
    &mut self,
    object: Self::Object,
  ) -> Result<Vec<WebIdlPropertyKey<Self::String, Self::Symbol>>, Self::Error> {
    let keys = <Self as LegacyJsRuntime>::own_property_keys(self, Value::Object(object))?;
    Ok(keys.into_iter().map(from_vm_property_key).collect())
  }

  fn alloc_string_from_code_units(&mut self, units: &[u16]) -> Result<Self::String, Self::Error> {
    let mut scope = self.heap_mut().scope();
    scope.alloc_string_from_code_units(units)
  }

  fn alloc_object(&mut self) -> Result<Self::Object, Self::Error> {
    let mut scope = self.heap_mut().scope();
    scope.alloc_object()
  }

  fn alloc_array(&mut self, len: usize) -> Result<Self::Object, Self::Error> {
    let mut scope = self.heap_mut().scope();
    scope.alloc_array(len)
  }

  fn create_data_property_or_throw(
    &mut self,
    object: Self::Object,
    key: WebIdlPropertyKey<Self::String, Self::Symbol>,
    value: Self::Value,
  ) -> Result<(), Self::Error> {
    let key = to_vm_property_key(key);
    let ok = self.heap_mut().create_data_property(object, key, value)?;
    if ok {
      Ok(())
    } else {
      Err(<Self as LegacyWebIdlJsRuntime>::throw_type_error(
        self,
        "CreateDataProperty rejected",
      ))
    }
  }

  fn well_known_symbol(&mut self, sym: WellKnownSymbol) -> Result<Self::Symbol, Self::Error> {
    let key = match sym {
      WellKnownSymbol::Iterator => <Self as LegacyWebIdlJsRuntime>::symbol_iterator(self)?,
      WellKnownSymbol::AsyncIterator => <Self as LegacyWebIdlJsRuntime>::symbol_async_iterator(self)?,
    };
    let VmPropertyKey::Symbol(sym) = key else {
      return Err(VmError::InvariantViolation("well_known_symbol did not return a symbol key"));
    };
    Ok(sym)
  }

  fn get_iterator(&mut self, value: Self::Value) -> Result<Self::Object, Self::Error> {
    // Spec: https://tc39.es/ecma262/#sec-getiterator (partial).
    let Value::Object(obj) = value else {
      return Err(self.type_error("GetIterator: value is not an object"));
    };

    let iter_sym = self.well_known_symbol(WellKnownSymbol::Iterator)?;
    let method = <Self as webidl::JsRuntime>::get_method(
      self,
      obj,
      WebIdlPropertyKey::Symbol(iter_sym),
    )?
    .ok_or_else(|| self.type_error("GetIterator: object is not iterable"))?;
    <Self as webidl::JsRuntime>::get_iterator_from_method(self, obj, method)
  }

  fn get_iterator_from_method(
    &mut self,
    object: Self::Object,
    method: Self::Value,
  ) -> Result<Self::Object, Self::Error> {
    // Spec: https://tc39.es/ecma262/#sec-getiteratorfrommethod (modeled as an IteratorRecord).

    let iterator = <Self as LegacyJsRuntime>::call(self, method, Value::Object(object), &[])?;
    let Value::Object(_iterator_obj) = iterator else {
      return Err(self.type_error("Iterator method did not return an object"));
    };

    // Cache `next_method` up front (important: avoid re-invoking side-effectful getters on each step).
    self.with_stack_roots([iterator], |rt| {
      let next_key = <Self as LegacyJsRuntime>::property_key_from_str(rt, "next")?;
      let next = <Self as LegacyJsRuntime>::get(rt, iterator, next_key)?;
      if !<Self as LegacyJsRuntime>::is_callable(rt, next) {
        return Err(rt.type_error("Iterator.next is not callable"));
      }

      let record_obj = {
        let mut scope = rt.heap.scope();
        scope.alloc_object()?
      };

      rt.with_stack_roots([Value::Object(record_obj), iterator, next], |rt| {
        let key_iter = <Self as LegacyJsRuntime>::property_key_from_str(rt, ITER_REC_ITERATOR)?;
        rt.heap.create_data_property_or_throw(record_obj, key_iter, iterator)?;

        let key_next = <Self as LegacyJsRuntime>::property_key_from_str(rt, ITER_REC_NEXT)?;
        rt.heap.create_data_property_or_throw(record_obj, key_next, next)?;

        let key_done = <Self as LegacyJsRuntime>::property_key_from_str(rt, ITER_REC_DONE)?;
        rt.heap
          .create_data_property_or_throw(record_obj, key_done, Value::Bool(false))?;
        Ok(record_obj)
      })
    })
  }

  fn iterator_next(
    &mut self,
    iterator: Self::Object,
  ) -> Result<IteratorResult<Self::Value>, Self::Error> {
    self.with_stack_roots([Value::Object(iterator)], |rt| {
      // If already done, return { value: undefined, done: true }.
      let done_key = <Self as LegacyJsRuntime>::property_key_from_str(rt, ITER_REC_DONE)?;
      let done_value = rt
        .heap
        .object_get_own_data_property_value(iterator, &done_key)?
        .unwrap_or(Value::Bool(false));
      if matches!(done_value, Value::Bool(true)) {
        return Ok(IteratorResult {
          value: Value::Undefined,
          done: true,
        });
      }

      let iter_key = <Self as LegacyJsRuntime>::property_key_from_str(rt, ITER_REC_ITERATOR)?;
      let next_key = <Self as LegacyJsRuntime>::property_key_from_str(rt, ITER_REC_NEXT)?;

      let iter_value = rt
        .heap
        .object_get_own_data_property_value(iterator, &iter_key)?
        .ok_or(VmError::InvariantViolation(
          "IteratorRecord missing iterator field",
        ))?;
      let next_method = rt
        .heap
        .object_get_own_data_property_value(iterator, &next_key)?
        .ok_or(VmError::InvariantViolation(
          "IteratorRecord missing next_method field",
        ))?;

      rt.with_stack_roots([iter_value, next_method], |rt| {
        let result = <Self as LegacyJsRuntime>::call(rt, next_method, iter_value, &[])?;
        let Value::Object(_result_obj) = result else {
          return Err(rt.type_error("Iterator.next() did not return an object"));
        };

        rt.with_stack_roots([result], |rt| {
          // done = ToBoolean(Get(result, "done"))
          let done_key = <Self as LegacyJsRuntime>::property_key_from_str(rt, "done")?;
          let done_value = <Self as LegacyJsRuntime>::get(rt, result, done_key)?;
          let done = <Self as LegacyJsRuntime>::to_boolean(rt, done_value)?;
          if done {
            // iterator_record.done = true
            let record_done_key = <Self as LegacyJsRuntime>::property_key_from_str(rt, ITER_REC_DONE)?;
            rt.heap.object_set_existing_data_property_value(
              iterator,
              &record_done_key,
              Value::Bool(true),
            )?;
            return Ok(IteratorResult {
              value: Value::Undefined,
              done: true,
            });
          }

          // value = Get(result, "value")
          let value_key = <Self as LegacyJsRuntime>::property_key_from_str(rt, "value")?;
          let value = <Self as LegacyJsRuntime>::get(rt, result, value_key)?;

          Ok(IteratorResult { value, done: false })
        })
      })
    })
  }
}

impl webidl::WebIdlJsRuntime for VmJsRuntime {
  fn is_callable(&self, value: Self::Value) -> bool {
    <Self as LegacyJsRuntime>::is_callable(self, value)
  }

  fn is_bigint(&self, value: Self::Value) -> bool {
    <Self as LegacyJsRuntime>::is_bigint(self, value)
  }

  fn to_bigint(&mut self, value: Self::Value) -> Result<Self::Value, Self::Error> {
    <Self as LegacyJsRuntime>::to_bigint(self, value)
  }

  fn to_numeric(&mut self, value: Self::Value) -> Result<Self::Value, Self::Error> {
    <Self as LegacyJsRuntime>::to_numeric(self, value)
  }

  fn get_own_property(
    &mut self,
    object: Self::Object,
    key: WebIdlPropertyKey<Self::String, Self::Symbol>,
  ) -> Result<Option<JsOwnPropertyDescriptor<Self::Value>>, Self::Error> {
    let key = to_vm_property_key(key);
    <Self as LegacyJsRuntime>::get_own_property(self, Value::Object(object), key)
  }

  fn throw_type_error(&mut self, message: &str) -> Self::Error {
    <Self as LegacyWebIdlJsRuntime>::throw_type_error(self, message)
  }

  fn throw_range_error(&mut self, message: &str) -> Self::Error {
    <Self as LegacyWebIdlJsRuntime>::throw_range_error(self, message)
  }

  fn is_array_buffer(&self, value: Self::Value) -> bool {
    <Self as LegacyWebIdlJsRuntime>::is_array_buffer(self, value)
  }

  fn is_shared_array_buffer(&self, value: Self::Value) -> bool {
    <Self as LegacyWebIdlJsRuntime>::is_shared_array_buffer(self, value)
  }

  fn is_data_view(&self, value: Self::Value) -> bool {
    <Self as LegacyWebIdlJsRuntime>::is_data_view(self, value)
  }

  fn typed_array_name(&self, value: Self::Value) -> Option<&'static str> {
    <Self as LegacyWebIdlJsRuntime>::typed_array_name(self, value)
  }
}
