use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;

use fastrender::js::bindings::{install_window_bindings, BindingValue, WebHostBindings};
use fastrender::js::{UrlLimits, UrlSearchParams};
use vm_js::{GcObject, PropertyKey, Value, VmError};
use webidl_js_runtime::{JsRuntime as _, VmJsRuntime, WebIdlBindingsRuntime, WebIdlJsRuntime as _};

#[derive(Default)]
struct UrlSearchParamsHost {
  limits: UrlLimits,
  params: HashMap<GcObject, UrlSearchParams>,
}

impl UrlSearchParamsHost {
  fn prototype_for(&mut self, rt: &mut VmJsRuntime, name: &str) -> Result<Value, VmError> {
    let global = <VmJsRuntime as WebIdlBindingsRuntime<Self>>::global_object(rt)?;
    let ctor_key: PropertyKey = rt.property_key_from_str(name)?;
    let ctor = rt.get(global, ctor_key)?;
    let proto_key: PropertyKey = rt.property_key_from_str("prototype")?;
    rt.get(ctor, proto_key)
  }

  fn require_params(
    &self,
    rt: &mut VmJsRuntime,
    receiver: Option<Value>,
  ) -> Result<&UrlSearchParams, VmError> {
    let Some(Value::Object(obj)) = receiver else {
      return Err(rt.throw_type_error("Illegal invocation"));
    };
    self
      .params
      .get(&obj)
      .ok_or_else(|| rt.throw_type_error("Illegal invocation"))
  }

  fn value_to_rust_string(rt: &mut VmJsRuntime, value: Value) -> Result<String, VmError> {
    let s = rt.to_string(value)?;
    rt.string_to_utf8_lossy(s)
  }

  fn make_iterator_result(
    rt: &mut VmJsRuntime,
    done: bool,
    value: Value,
  ) -> Result<Value, VmError> {
    let obj = rt.alloc_object_value()?;
    let obj_root = rt.heap_mut().add_root(obj)?;

    let done_key = rt.property_key_from_str("done")?;
    let value_key = rt.property_key_from_str("value")?;
    rt.define_data_property(obj, done_key, Value::Bool(done), true)?;
    rt.define_data_property(obj, value_key, value, true)?;

    rt.heap_mut().remove_root(obj_root);
    Ok(obj)
  }

  fn make_string_iterator(
    rt: &mut VmJsRuntime,
    items: Vec<String>,
  ) -> Result<Value, VmError> {
    let items = Rc::new(items);
    let idx = Rc::new(Cell::new(0usize));

    let next_func = rt.alloc_function_value({
      let items = items.clone();
      let idx = idx.clone();
      move |rt, _this, _args| {
        let i = idx.get();
        if i >= items.len() {
          return Self::make_iterator_result(rt, true, Value::Undefined);
        }
        idx.set(i + 1);
        let value = rt.alloc_string_value(&items[i])?;
        Self::make_iterator_result(rt, false, value)
      }
    })?;

    let iterator = rt.alloc_object_value()?;
    let iterator_root = rt.heap_mut().add_root(iterator)?;

    let next_key = rt.property_key_from_str("next")?;
    rt.define_data_property(iterator, next_key, next_func, false)?;

    // Iterator objects should be iterable: %Symbol.iterator% returns the iterator itself.
    let iter_func = rt.alloc_function_value(|_rt, this, _args| Ok(this))?;
    let sym_iter = rt.symbol_iterator()?;
    rt.define_data_property(iterator, sym_iter, iter_func, false)?;

    rt.heap_mut().remove_root(iterator_root);
    Ok(iterator)
  }

  fn make_pair_iterator(
    rt: &mut VmJsRuntime,
    items: Vec<(String, String)>,
  ) -> Result<Value, VmError> {
    let items = Rc::new(items);
    let idx = Rc::new(Cell::new(0usize));

    let next_func = rt.alloc_function_value({
      let items = items.clone();
      let idx = idx.clone();
      move |rt, _this, _args| {
        let i = idx.get();
        if i >= items.len() {
          return Self::make_iterator_result(rt, true, Value::Undefined);
        }
        idx.set(i + 1);

        let (key, value) = &items[i];
        let arr = rt.alloc_array()?;
        let arr_root = rt.heap_mut().add_root(arr)?;

        let k = rt.alloc_string_value(key)?;
        let v = rt.alloc_string_value(value)?;

        let k0 = rt.property_key_from_u32(0)?;
        rt.define_data_property(arr, k0, k, true)?;
        let k1 = rt.property_key_from_u32(1)?;
        rt.define_data_property(arr, k1, v, true)?;

        rt.heap_mut().remove_root(arr_root);
        Self::make_iterator_result(rt, false, arr)
      }
    })?;

    let iterator = rt.alloc_object_value()?;
    let iterator_root = rt.heap_mut().add_root(iterator)?;

    let next_key = rt.property_key_from_str("next")?;
    rt.define_data_property(iterator, next_key, next_func, false)?;

    let iter_func = rt.alloc_function_value(|_rt, this, _args| Ok(this))?;
    let sym_iter = rt.symbol_iterator()?;
    rt.define_data_property(iterator, sym_iter, iter_func, false)?;

    rt.heap_mut().remove_root(iterator_root);
    Ok(iterator)
  }

  fn make_indexed_iterable(rt: &mut VmJsRuntime, iterable: Value, len: usize) -> Result<(), VmError> {
    let idx = Rc::new(Cell::new(0usize));

    let next_key = rt.property_key_from_str("next")?;
    let next_func = rt.alloc_function_value({
      let idx = idx.clone();
      move |rt, this, _args| {
        let i = idx.get();
        if i >= len {
          return Self::make_iterator_result(rt, true, Value::Undefined);
        }
        idx.set(i + 1);
        let key = rt.property_key_from_u32(i as u32)?;
        let value = rt.get(this, key)?;
        Self::make_iterator_result(rt, false, value)
      }
    })?;

    rt.define_data_property(iterable, next_key, next_func, false)?;

    // Iterator objects should be iterable: %Symbol.iterator% returns the iterator itself.
    let iter_func = rt.alloc_function_value(|_rt, this, _args| Ok(this))?;
    let sym_iter = rt.symbol_iterator()?;
    rt.define_data_property(iterable, sym_iter, iter_func, false)?;
    Ok(())
  }
}

impl WebHostBindings<VmJsRuntime> for UrlSearchParamsHost {
  fn call_operation(
    &mut self,
    rt: &mut VmJsRuntime,
    receiver: Option<Value>,
    interface: &'static str,
    operation: &'static str,
    _overload: usize,
    args: Vec<BindingValue<Value>>,
  ) -> Result<BindingValue<Value>, VmError> {
    match (interface, operation) {
      ("URLSearchParams", "constructor") => {
        let params = match args.get(0) {
          None => UrlSearchParams::new(&self.limits),

          // URLSearchParamsInit string.
          Some(BindingValue::String(s)) => {
            if s.is_empty() {
              UrlSearchParams::new(&self.limits)
            } else {
              UrlSearchParams::parse(s, &self.limits).map_err(|e| {
                rt.throw_type_error(&format!("URLSearchParams constructor failed: {e}"))
              })?
            }
          }

          // record<USVString, USVString>.
          Some(BindingValue::Dictionary(map)) => {
            let params = UrlSearchParams::new(&self.limits);
            for (k, v) in map {
              let BindingValue::String(v) = v else {
                return Err(rt.throw_type_error(
                  "URLSearchParams constructor failed: record value is not a string",
                ));
              };
              params.append(k, v).map_err(|e| {
                rt.throw_type_error(&format!("URLSearchParams constructor failed: {e}"))
              })?;
            }
            params
          }

          // sequence<sequence<USVString>>.
          Some(BindingValue::Sequence(pairs)) => {
            let params = UrlSearchParams::new(&self.limits);
            for pair in pairs {
              let pair = match pair {
                BindingValue::Sequence(pair) | BindingValue::FrozenArray(pair) => pair,
                _ => {
                  return Err(rt.throw_type_error(
                    "URLSearchParams constructor failed: expected pair sequence",
                  ))
                }
              };
              if pair.len() != 2 {
                return Err(rt.throw_type_error(
                  "URLSearchParams constructor failed: expected [name, value] pair",
                ));
              }
              let BindingValue::String(k) = &pair[0] else {
                return Err(rt.throw_type_error(
                  "URLSearchParams constructor failed: pair key is not a string",
                ));
              };
              let BindingValue::String(v) = &pair[1] else {
                return Err(rt.throw_type_error(
                  "URLSearchParams constructor failed: pair value is not a string",
                ));
              };
              params.append(k, v).map_err(|e| {
                rt.throw_type_error(&format!("URLSearchParams constructor failed: {e}"))
              })?;
            }
            params
          }

          // Legacy escape hatch used by older bindings: attempt `ToString` on opaque JS values.
          Some(BindingValue::Object(v)) => {
            let init = Self::value_to_rust_string(rt, *v)?;
            if init.is_empty() {
              UrlSearchParams::new(&self.limits)
            } else {
              UrlSearchParams::parse(&init, &self.limits).map_err(|e| {
                rt.throw_type_error(&format!("URLSearchParams constructor failed: {e}"))
              })?
            }
          }

          Some(_) => {
            return Err(rt.throw_type_error(
              "URLSearchParams constructor failed: unsupported init type",
            ))
          }
        };

        let obj = rt.alloc_object_value()?;
        let proto = self.prototype_for(rt, "URLSearchParams")?;
        rt.set_prototype(obj, Some(proto))?;

        let Value::Object(obj_handle) = obj else {
          return Err(rt.throw_type_error("URLSearchParams constructor did not create an object"));
        };
        let _ = rt.heap_mut().add_root(obj)?;
        self.params.insert(obj_handle, params);

        Ok(BindingValue::Object(obj))
      }
      ("URLSearchParams", "entries") => {
        let params = self.require_params(rt, receiver)?;
        let pairs = params
          .pairs()
          .map_err(|e| rt.throw_type_error(&format!("URLSearchParams.entries failed: {e}")))?;
        let iter = Self::make_pair_iterator(rt, pairs)?;
        Ok(BindingValue::Object(iter))
      }
      ("URLSearchParams", "keys") => {
        let params = self.require_params(rt, receiver)?;
        let pairs = params
          .pairs()
          .map_err(|e| rt.throw_type_error(&format!("URLSearchParams.keys failed: {e}")))?;
        let keys = pairs.into_iter().map(|(k, _v)| k).collect();
        let iter = Self::make_string_iterator(rt, keys)?;
        Ok(BindingValue::Object(iter))
      }
      ("URLSearchParams", "values") => {
        let params = self.require_params(rt, receiver)?;
        let pairs = params
          .pairs()
          .map_err(|e| rt.throw_type_error(&format!("URLSearchParams.values failed: {e}")))?;
        let values = pairs.into_iter().map(|(_k, v)| v).collect();
        let iter = Self::make_string_iterator(rt, values)?;
        Ok(BindingValue::Object(iter))
      }
      ("URLSearchParams", "forEach") => {
        let params = self.require_params(rt, receiver)?;
        let Some(receiver) = receiver else {
          return Err(rt.throw_type_error("Illegal invocation"));
        };

        let callback = match args.get(0) {
          Some(BindingValue::Object(v)) if rt.is_callable(*v) => *v,
          _ => return Err(rt.throw_type_error("URLSearchParams.forEach: expected callback")),
        };
        let this_arg = match args.get(1) {
          None | Some(BindingValue::Undefined) => Value::Undefined,
          Some(BindingValue::Object(v)) => *v,
          Some(_) => Value::Undefined,
        };

        let pairs = params
          .pairs()
          .map_err(|e| rt.throw_type_error(&format!("URLSearchParams.forEach failed: {e}")))?;
        for (key, value) in pairs {
          let key = rt.alloc_string_value(&key)?;
          let value = rt.alloc_string_value(&value)?;
          rt.call(callback, this_arg, &[value, key, receiver])?;
        }
        Ok(BindingValue::Undefined)
      }
      _ => Err(rt.throw_type_error("unimplemented host operation")),
    }
  }
}

fn get(rt: &mut VmJsRuntime, obj: Value, name: &str) -> Result<Value, VmError> {
  let key: PropertyKey = rt.property_key_from_str(name)?;
  rt.get(obj, key)
}

fn get_method(rt: &mut VmJsRuntime, obj: Value, name: &str) -> Result<Value, VmError> {
  let func = get(rt, obj, name)?;
  if !rt.is_callable(func) {
    return Err(rt.throw_type_error(&format!("{name} is not callable")));
  }
  Ok(func)
}

fn collect_string_iterable(
  rt: &mut VmJsRuntime,
  host: &mut UrlSearchParamsHost,
  params: Value,
  method_name: &str,
) -> Result<Vec<String>, VmError> {
  // Call `params.{method_name}()` (requires host context; method is a generated wrapper).
  let method = get_method(rt, params, method_name)?;
  let iterator = rt.with_host_context(host, |rt| rt.call(method, params, &[]))?;

  // Emulate `Array.from(iterator)` by iterating via `iterator[Symbol.iterator]()`.
  let sym_iter = rt.symbol_iterator()?;
  let Some(iter_method) = rt.get_method(iterator, sym_iter)? else {
    return Err(rt.throw_type_error("iterator is missing [Symbol.iterator]"));
  };
  let mut record = rt.get_iterator_from_method(iterator, iter_method)?;

  let mut out = Vec::new();
  while let Some(v) = rt.iterator_step_value(&mut record)? {
    out.push(UrlSearchParamsHost::value_to_rust_string(rt, v)?);
  }
  Ok(out)
}

fn collect_pairs_iterable(
  rt: &mut VmJsRuntime,
  host: &mut UrlSearchParamsHost,
  params: Value,
) -> Result<Vec<(String, String)>, VmError> {
  let sym_iter = rt.symbol_iterator()?;
  let Some(iter_method) = rt.get_method(params, sym_iter)? else {
    return Err(rt.throw_type_error("URLSearchParams is missing [Symbol.iterator]"));
  };

  // [Symbol.iterator] is a generated wrapper (aliases `entries`).
  let mut record = rt.with_host_context(host, |rt| rt.get_iterator_from_method(params, iter_method))?;

  let mut out = Vec::new();
  while let Some(pair) = rt.iterator_step_value(&mut record)? {
    let key0 = rt.property_key_from_u32(0)?;
    let key1 = rt.property_key_from_u32(1)?;
    let k = rt.get(pair, key0)?;
    let v = rt.get(pair, key1)?;
    out.push((
      UrlSearchParamsHost::value_to_rust_string(rt, k)?,
      UrlSearchParamsHost::value_to_rust_string(rt, v)?,
    ));
  }
  Ok(out)
}

fn assert_type_error(rt: &mut VmJsRuntime, err: VmError) {
  let Some(thrown) = err.thrown_value() else {
    panic!("expected thrown error, got {err:?}");
  };
  let s = rt.to_string(thrown).unwrap();
  let msg = rt.string_to_utf8_lossy(s).unwrap();
  assert!(msg.starts_with("TypeError:"), "expected TypeError, got {msg:?}");
}

#[test]
fn generated_webidl_bindings_install_iterable_url_search_params() -> Result<(), VmError> {
  let mut rt = VmJsRuntime::new();
  let mut host = UrlSearchParamsHost::default();

  install_window_bindings(&mut rt, &mut host)?;

  let global = <VmJsRuntime as WebIdlBindingsRuntime<UrlSearchParamsHost>>::global_object(&mut rt)?;
  let ctor = get_method(&mut rt, global, "URLSearchParams")?;
  let init = rt.alloc_string_value("a=1&b=2")?;
  let params = rt.with_host_context(&mut host, |rt| rt.call(ctor, Value::Undefined, &[init]))?;

  // typeof URLSearchParams.prototype[Symbol.iterator] === "function"
  let proto = get(&mut rt, ctor, "prototype")?;
  let sym_iter = rt.symbol_iterator()?;
  let iter = rt.get(proto, sym_iter)?;
  assert!(rt.is_callable(iter));

  // URLSearchParams.prototype[Symbol.iterator] should alias `entries`.
  let entries = get_method(&mut rt, proto, "entries")?;
  assert_eq!(iter, entries);

  // Array.from(new URLSearchParams("a=1&b=2").keys()) -> ["a", "b"]
  let keys = collect_string_iterable(&mut rt, &mut host, params, "keys")?;
  assert_eq!(keys, vec!["a".to_string(), "b".to_string()]);

  // Array.from(new URLSearchParams("a=1&b=2").values()) -> ["1", "2"]
  let values = collect_string_iterable(&mut rt, &mut host, params, "values")?;
  assert_eq!(values, vec!["1".to_string(), "2".to_string()]);

  // Array.from(new URLSearchParams("a=1&b=2")) -> [["a","1"],["b","2"]]
  let pairs = collect_pairs_iterable(&mut rt, &mut host, params)?;
  assert_eq!(
    pairs,
    vec![("a".to_string(), "1".to_string()), ("b".to_string(), "2".to_string())]
  );

  // `new URLSearchParams({ a: "1", b: "2" })` (record init).
  let init_record = rt.alloc_object_value()?;
  let record_root = rt.heap_mut().add_root(init_record)?;
  let a_key = rt.property_key_from_str("a")?;
  let b_key = rt.property_key_from_str("b")?;
  let a_val = rt.alloc_string_value("1")?;
  let b_val = rt.alloc_string_value("2")?;
  rt.define_data_property(init_record, a_key, a_val, true)?;
  rt.define_data_property(init_record, b_key, b_val, true)?;
  let params_record =
    rt.with_host_context(&mut host, |rt| rt.call(ctor, Value::Undefined, &[init_record]))?;
  rt.heap_mut().remove_root(record_root);
  let pairs = collect_pairs_iterable(&mut rt, &mut host, params_record)?;
  assert_eq!(
    pairs,
    vec![("a".to_string(), "1".to_string()), ("b".to_string(), "2".to_string())]
  );

  // `new URLSearchParams([ ["a","1"], ["b","2"] ])` (sequence-of-sequence init).
  let outer = rt.alloc_array()?;
  let outer_root = rt.heap_mut().add_root(outer)?;
  for (idx, (k, v)) in [("a", "1"), ("b", "2")].into_iter().enumerate() {
    let inner = rt.alloc_array()?;
    let inner_root = rt.heap_mut().add_root(inner)?;
    let k0 = rt.property_key_from_u32(0)?;
    let k1 = rt.property_key_from_u32(1)?;
    let key = rt.alloc_string_value(k)?;
    let value = rt.alloc_string_value(v)?;
    rt.define_data_property(inner, k0, key, true)?;
    rt.define_data_property(inner, k1, value, true)?;
    UrlSearchParamsHost::make_indexed_iterable(&mut rt, inner, 2)?;
    rt.heap_mut().remove_root(inner_root);
    let outer_key = rt.property_key_from_u32(idx as u32)?;
    rt.define_data_property(outer, outer_key, inner, true)?;
  }
  UrlSearchParamsHost::make_indexed_iterable(&mut rt, outer, 2)?;
  let params_seq =
    rt.with_host_context(&mut host, |rt| rt.call(ctor, Value::Undefined, &[outer]))?;
  rt.heap_mut().remove_root(outer_root);
  let pairs = collect_pairs_iterable(&mut rt, &mut host, params_seq)?;
  assert_eq!(
    pairs,
    vec![("a".to_string(), "1".to_string()), ("b".to_string(), "2".to_string())]
  );

  // Invalid init should throw TypeError: `new URLSearchParams([{}])`.
  let bad_outer = rt.alloc_array()?;
  let bad_outer_root = rt.heap_mut().add_root(bad_outer)?;
  let bad_elem = rt.alloc_object_value()?;
  let key0 = rt.property_key_from_u32(0)?;
  rt.define_data_property(bad_outer, key0, bad_elem, true)?;
  UrlSearchParamsHost::make_indexed_iterable(&mut rt, bad_outer, 1)?;
  let err = rt
    .with_host_context(&mut host, |rt| rt.call(ctor, Value::Undefined, &[bad_outer]))
    .unwrap_err();
  rt.heap_mut().remove_root(bad_outer_root);
  assert_type_error(&mut rt, err);

  Ok(())
}
