use std::collections::HashMap;

use fastrender::js::bindings::{install_worker_bindings, BindingValue, WebHostBindings};
use fastrender::js::webidl::{
  DataPropertyAttributes, VmJsWebIdlBindingsCx, VmJsWebIdlBindingsState, WebIdlBindingsRuntime,
};
use fastrender::js::{UrlLimits, UrlSearchParams};
use vm_js::{
  Heap, HeapLimits, MicrotaskQueue, PropertyKey, Realm, Scope, Value, Vm, VmError, VmOptions,
  WeakGcObject,
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

#[derive(Debug)]
enum IterItem {
  Key(String),
  Value(String),
  Pair(String, String),
}

#[derive(Debug)]
struct IteratorState {
  idx: usize,
  items: Vec<IterItem>,
}

#[derive(Default)]
struct UrlSearchParamsHost {
  limits: UrlLimits,
  params: HashMap<WeakGcObject, UrlSearchParams>,
  iterators: HashMap<WeakGcObject, IteratorState>,
}

fn iterator_result<Host, R>(
  rt: &mut R,
  done: bool,
  value: R::JsValue,
) -> Result<R::JsValue, R::Error>
where
  R: WebIdlBindingsRuntime<Host>,
{
  let obj = rt.create_object()?;
  let attrs = DataPropertyAttributes::new(true, true, true);
  rt.define_data_property_str(obj, "done", rt.js_bool(done), attrs)?;
  rt.define_data_property_str(obj, "value", value, attrs)?;
  Ok(obj)
}

fn iterator_return_this<Host, R>(
  _rt: &mut R,
  _host: &mut Host,
  this: R::JsValue,
  _args: &[R::JsValue],
) -> Result<R::JsValue, R::Error>
where
  R: WebIdlBindingsRuntime<Host>,
{
  Ok(this)
}

fn url_search_params_iterator_next<R>(
  rt: &mut R,
  host: &mut UrlSearchParamsHost,
  this: R::JsValue,
  _args: &[R::JsValue],
) -> Result<R::JsValue, R::Error>
where
  R: WebIdlBindingsRuntime<UrlSearchParamsHost, JsValue = Value, Error = VmError>,
{
  let Value::Object(obj) = this else {
    return Err(rt.throw_type_error("Illegal invocation"));
  };
  let Some(state) = host.iterators.get_mut(&WeakGcObject::from(obj)) else {
    return Err(rt.throw_type_error("Illegal invocation"));
  };

  if state.idx >= state.items.len() {
    return iterator_result::<UrlSearchParamsHost, R>(rt, true, rt.js_undefined());
  }

  let item = &state.items[state.idx];
  state.idx += 1;

  let value = match item {
    IterItem::Key(s) | IterItem::Value(s) => rt.js_string(s)?,
    IterItem::Pair(k, v) => {
      let pair = rt.create_object()?;
      let attrs = DataPropertyAttributes::new(true, true, true);
      let key_value = rt.js_string(k)?;
      rt.define_data_property_str(pair, "0", key_value, attrs)?;
      let value_value = rt.js_string(v)?;
      rt.define_data_property_str(pair, "1", value_value, attrs)?;
      pair
    }
  };

  iterator_result::<UrlSearchParamsHost, R>(rt, false, value)
}

impl UrlSearchParamsHost {
  fn require_params<'a>(
    &'a self,
    rt: &mut VmJsWebIdlBindingsCx<'_, UrlSearchParamsHost>,
    receiver: Option<Value>,
  ) -> Result<&'a UrlSearchParams, VmError> {
    let Some(Value::Object(obj)) = receiver else {
      return Err(rt.throw_type_error("Illegal invocation"));
    };
    self
      .params
      .get(&WeakGcObject::from(obj))
      .ok_or_else(|| rt.throw_type_error("Illegal invocation"))
  }

  fn make_iterator<'a>(
    &mut self,
    rt: &mut VmJsWebIdlBindingsCx<'a, UrlSearchParamsHost>,
    items: Vec<IterItem>,
  ) -> Result<Value, VmError> {
    let iter = rt.create_object()?;
    let next = rt.create_function(
      "next",
      0,
      url_search_params_iterator_next::<VmJsWebIdlBindingsCx<'a, UrlSearchParamsHost>>,
    )?;
    rt.define_data_property_str(iter, "next", next, DataPropertyAttributes::METHOD)?;

    // Iterator objects should be iterable: %Symbol.iterator% returns the iterator itself.
    let iter_func = rt.create_function(
      "Symbol.iterator",
      0,
      iterator_return_this::<UrlSearchParamsHost, VmJsWebIdlBindingsCx<'a, UrlSearchParamsHost>>,
    )?;
    let iter_key = rt.symbol_iterator()?;
    rt.define_data_property(iter, iter_key, iter_func, DataPropertyAttributes::METHOD)?;

    let Value::Object(obj) = iter else {
      return Err(rt.throw_type_error("iterator allocation did not produce an object"));
    };
    self
      .iterators
      .insert(WeakGcObject::from(obj), IteratorState { idx: 0, items });
    Ok(iter)
  }
}

impl<'a> WebHostBindings<VmJsWebIdlBindingsCx<'a, UrlSearchParamsHost>> for UrlSearchParamsHost {
  fn call_operation(
    &mut self,
    rt: &mut VmJsWebIdlBindingsCx<'a, UrlSearchParamsHost>,
    receiver: Option<Value>,
    interface: &'static str,
    operation: &'static str,
    _overload: usize,
    _args: Vec<BindingValue<Value>>,
  ) -> Result<BindingValue<Value>, VmError> {
    match (interface, operation) {
      ("URLSearchParams", "entries") => {
        let params = self.require_params(rt, receiver)?;
        let pairs = params
          .pairs()
          .map_err(|e| rt.throw_type_error(&format!("URLSearchParams.entries failed: {e}")))?;
        let items = pairs
          .into_iter()
          .map(|(k, v)| IterItem::Pair(k, v))
          .collect();
        Ok(BindingValue::Object(self.make_iterator(rt, items)?))
      }
      ("URLSearchParams", "keys") => {
        let params = self.require_params(rt, receiver)?;
        let pairs = params
          .pairs()
          .map_err(|e| rt.throw_type_error(&format!("URLSearchParams.keys failed: {e}")))?;
        let items = pairs.into_iter().map(|(k, _v)| IterItem::Key(k)).collect();
        Ok(BindingValue::Object(self.make_iterator(rt, items)?))
      }
      ("URLSearchParams", "values") => {
        let params = self.require_params(rt, receiver)?;
        let pairs = params
          .pairs()
          .map_err(|e| rt.throw_type_error(&format!("URLSearchParams.values failed: {e}")))?;
        let items = pairs
          .into_iter()
          .map(|(_k, v)| IterItem::Value(v))
          .collect();
        Ok(BindingValue::Object(self.make_iterator(rt, items)?))
      }
      _ => Err(rt.throw_type_error("unimplemented host operation")),
    }
  }
}

fn alloc_key(scope: &mut Scope<'_>, name: &str) -> Result<PropertyKey, VmError> {
  let s = scope.alloc_string(name)?;
  scope.push_root(Value::String(s))?;
  Ok(PropertyKey::from_string(s))
}

fn value_to_rust_string(scope: &mut Scope<'_>, value: Value) -> Result<String, VmError> {
  let s = scope.heap_mut().to_string(value)?;
  Ok(scope.heap().get_string(s)?.to_utf8_lossy())
}

fn collect_string_iterator(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  hooks: &mut MicrotaskQueue,
  host: &mut UrlSearchParamsHost,
  iterator: Value,
  next_key: PropertyKey,
  done_key: PropertyKey,
  value_key: PropertyKey,
) -> Result<Vec<String>, VmError> {
  scope.push_root(iterator)?;
  let Value::Object(iterator_obj) = iterator else {
    return Err(VmError::TypeError("expected iterator object"));
  };
  let next = vm.get(scope, iterator_obj, next_key)?;
  scope.push_root(next)?;
  let mut out = Vec::new();
  loop {
    let result = vm.call_with_host_and_hooks(host, scope, hooks, next, iterator, &[])?;
    scope.push_root(result)?;
    let Value::Object(result_obj) = result else {
      return Err(VmError::TypeError("expected iterator result object"));
    };
    let done = vm.get(scope, result_obj, done_key)?;
    if matches!(done, Value::Bool(true)) {
      break;
    }
    let v = vm.get(scope, result_obj, value_key)?;
    out.push(value_to_rust_string(scope, v)?);
  }
  Ok(out)
}

fn collect_pair_iterator(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  hooks: &mut MicrotaskQueue,
  host: &mut UrlSearchParamsHost,
  iterator: Value,
  next_key: PropertyKey,
  done_key: PropertyKey,
  value_key: PropertyKey,
  key0: PropertyKey,
  key1: PropertyKey,
) -> Result<Vec<(String, String)>, VmError> {
  scope.push_root(iterator)?;
  let Value::Object(iterator_obj) = iterator else {
    return Err(VmError::TypeError("expected iterator object"));
  };
  let next = vm.get(scope, iterator_obj, next_key)?;
  scope.push_root(next)?;
  let mut out = Vec::new();
  loop {
    let result = vm.call_with_host_and_hooks(host, scope, hooks, next, iterator, &[])?;
    scope.push_root(result)?;
    let Value::Object(result_obj) = result else {
      return Err(VmError::TypeError("expected iterator result object"));
    };
    let done = vm.get(scope, result_obj, done_key)?;
    if matches!(done, Value::Bool(true)) {
      break;
    }
    let pair = vm.get(scope, result_obj, value_key)?;
    let Value::Object(pair_obj) = pair else {
      return Err(VmError::TypeError("expected pair object"));
    };
    let k = vm.get(scope, pair_obj, key0)?;
    let v = vm.get(scope, pair_obj, key1)?;
    out.push((
      value_to_rust_string(scope, k)?,
      value_to_rust_string(scope, v)?,
    ));
  }
  Ok(out)
}

#[test]
fn generated_webidl_bindings_install_iterable_url_search_params_in_realm() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(32 * 1024 * 1024, 32 * 1024 * 1024));
  let mut vm = Vm::new(VmOptions::default());
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let state = Box::new(VmJsWebIdlBindingsState::<UrlSearchParamsHost>::new(
    realm.global_object(),
    WebIdlLimits::default(),
    Box::new(NoHooks),
  ));

  let mut host = UrlSearchParamsHost::default();
  {
    let mut cx = VmJsWebIdlBindingsCx::new(&mut vm, &mut heap, &state);
    install_worker_bindings(&mut cx, &mut host)?;
  }

  let mut hooks = MicrotaskQueue::new();
  let mut scope = heap.scope();

  let global = realm.global_object();
  scope.push_root(Value::Object(global))?;

  let ctor_key = alloc_key(&mut scope, "URLSearchParams")?;
  let ctor = vm.get(&mut scope, global, ctor_key)?;
  scope.push_root(ctor)?;
  let Value::Object(ctor_obj) = ctor else {
    return Err(VmError::TypeError(
      "URLSearchParams constructor is not an object",
    ));
  };

  let proto_key = alloc_key(&mut scope, "prototype")?;
  let proto = vm.get(&mut scope, ctor_obj, proto_key)?;
  scope.push_root(proto)?;
  let Value::Object(proto_obj) = proto else {
    return Err(VmError::TypeError(
      "URLSearchParams.prototype is not an object",
    ));
  };

  // typeof URLSearchParams.prototype[Symbol.iterator] === "function"
  let sym_iter = realm.intrinsics().well_known_symbols().iterator;
  let sym_iter_key = PropertyKey::from_symbol(sym_iter);
  let iter = vm.get(&mut scope, proto_obj, sym_iter_key)?;
  let Value::Object(iter_obj) = iter else {
    return Err(VmError::TypeError(
      "URLSearchParams.prototype[Symbol.iterator] is not an object",
    ));
  };
  scope
    .heap()
    .get_function_native_slots(iter_obj)
    .map_err(|_| {
      VmError::TypeError("URLSearchParams.prototype[Symbol.iterator] is not callable")
    })?;

  // URLSearchParams.prototype[Symbol.iterator] should alias `entries`.
  let entries_key = alloc_key(&mut scope, "entries")?;
  let entries = vm.get(&mut scope, proto_obj, entries_key)?;
  assert_eq!(iter, entries);

  // Ensure WebIDL iterable-shape methods have the correct function `length`.
  let length_key = alloc_key(&mut scope, "length")?;
  let for_each_key = alloc_key(&mut scope, "forEach")?;
  let for_each = vm.get(&mut scope, proto_obj, for_each_key)?;
  let Value::Object(for_each_obj) = for_each else {
    return Err(VmError::TypeError(
      "URLSearchParams.prototype.forEach is not an object",
    ));
  };
  let for_each_len = vm.get(&mut scope, for_each_obj, length_key)?;
  assert_eq!(for_each_len, Value::Number(1.0));

  let keys_key = alloc_key(&mut scope, "keys")?;
  let values_key = alloc_key(&mut scope, "values")?;
  for (name, key) in [
    ("entries", entries_key),
    ("keys", keys_key),
    ("values", values_key),
  ] {
    let func = vm.get(&mut scope, proto_obj, key)?;
    let Value::Object(func_obj) = func else {
      return Err(VmError::TypeError("expected function object"));
    };
    let len = vm.get(&mut scope, func_obj, length_key)?;
    assert_eq!(len, Value::Number(0.0), "{name}.length");
  }

  // Create a params object branded by the generated prototype and attach host state.
  let params_obj = scope.alloc_object_with_prototype(Some(proto_obj))?;
  scope.push_root(Value::Object(params_obj))?;
  let params = UrlSearchParams::parse("a=1&b=2", &host.limits)
    .expect("UrlSearchParams parse should succeed for fixed input");
  host.params.insert(WeakGcObject::from(params_obj), params);
  let params_val = Value::Object(params_obj);

  // Iterate keys/values/entries by repeatedly calling `.next()`.
  let next_key = alloc_key(&mut scope, "next")?;
  let done_key = alloc_key(&mut scope, "done")?;
  let value_key = alloc_key(&mut scope, "value")?;
  let key0 = alloc_key(&mut scope, "0")?;
  let key1 = alloc_key(&mut scope, "1")?;

  let keys_method = vm.get(&mut scope, params_obj, keys_key)?;
  let keys_iter = vm.call_with_host_and_hooks(
    &mut host,
    &mut scope,
    &mut hooks,
    keys_method,
    params_val,
    &[],
  )?;
  let keys = collect_string_iterator(
    &mut vm, &mut scope, &mut hooks, &mut host, keys_iter, next_key, done_key, value_key,
  )?;
  assert_eq!(keys, vec!["a".to_string(), "b".to_string()]);

  let values_method = vm.get(&mut scope, params_obj, values_key)?;
  let values_iter = vm.call_with_host_and_hooks(
    &mut host,
    &mut scope,
    &mut hooks,
    values_method,
    params_val,
    &[],
  )?;
  let values = collect_string_iterator(
    &mut vm,
    &mut scope,
    &mut hooks,
    &mut host,
    values_iter,
    next_key,
    done_key,
    value_key,
  )?;
  assert_eq!(values, vec!["1".to_string(), "2".to_string()]);

  let iter_method = vm.get(&mut scope, params_obj, sym_iter_key)?;
  let pairs_iter = vm.call_with_host_and_hooks(
    &mut host,
    &mut scope,
    &mut hooks,
    iter_method,
    params_val,
    &[],
  )?;
  let pairs = collect_pair_iterator(
    &mut vm, &mut scope, &mut hooks, &mut host, pairs_iter, next_key, done_key, value_key, key0,
    key1,
  )?;
  assert_eq!(
    pairs,
    vec![
      ("a".to_string(), "1".to_string()),
      ("b".to_string(), "2".to_string())
    ]
  );

  drop(scope);
  realm.teardown(&mut heap);
  Ok(())
}
