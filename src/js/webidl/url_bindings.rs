use crate::js::url::{Url, UrlError, UrlLimits, UrlSearchParams};
use std::cell::RefCell;
use std::char::decode_utf16;
use std::collections::HashMap;
use std::rc::Rc;
use vm_js::{GcObject, PropertyDescriptor, PropertyKey, PropertyKind, RootId, Value, VmError};
use webidl_js_runtime::runtime::JsPropertyKind;
use webidl_js_runtime::{JsRuntime as _, WebIdlJsRuntime as _};

#[derive(Default)]
struct UrlBindingState {
  limits: UrlLimits,
  urls: HashMap<GcObject, UrlInstance>,
  search_params: HashMap<GcObject, UrlSearchParams>,
}

struct UrlInstance {
  url: Url,
}

fn type_error(rt: &mut webidl_js_runtime::VmJsRuntime, message: &str) -> VmError {
  rt.throw_type_error(message)
}

fn to_property_key(
  rt: &mut webidl_js_runtime::VmJsRuntime,
  name: &str,
) -> Result<PropertyKey, VmError> {
  let v = rt.alloc_string_value(name)?;
  let Value::String(s) = v else {
    return Err(type_error(rt, "failed to allocate property key string"));
  };
  Ok(PropertyKey::String(s))
}

fn string_to_rust_string_limited(
  rt: &mut webidl_js_runtime::VmJsRuntime,
  handle: vm_js::GcString,
  max_bytes: usize,
  context: &str,
) -> Result<String, VmError> {
  let js = rt.heap().get_string(handle)?;

  let code_units_len = js.len_code_units();
  // UTF-8 output bytes are always >= UTF-16 code unit length, even accounting for surrogate pairs
  // and replacement characters produced by lossy decoding. Use this to reject pathological strings
  // without iterating them.
  if code_units_len > max_bytes {
    return Err(type_error(
      rt,
      &format!("{context} exceeded max bytes (len_code_units={code_units_len}, limit={max_bytes})"),
    ));
  }

  // `JsString::to_utf8_lossy` uses `String::from_utf16_lossy`, which can allocate an output string
  // much larger than the UTF-16 input (up to 3 bytes per code unit). Build the string manually so
  // we can stop once the configured byte limit would be exceeded.
  let capacity = code_units_len.saturating_mul(3).min(max_bytes);
  let mut out = String::with_capacity(capacity);
  let mut out_len = 0usize;

  for decoded in decode_utf16(js.as_code_units().iter().copied()) {
    let ch = decoded.unwrap_or('\u{FFFD}');
    let ch_len = ch.len_utf8();
    let next_len = out_len.checked_add(ch_len).unwrap_or(usize::MAX);
    if next_len > max_bytes {
      return Err(type_error(
        rt,
        &format!("{context} exceeded max bytes (limit={max_bytes})"),
      ));
    }
    out.push(ch);
    out_len = next_len;
  }

  Ok(out)
}

fn to_rust_string_limited(
  rt: &mut webidl_js_runtime::VmJsRuntime,
  value: Value,
  max_bytes: usize,
  context: &str,
) -> Result<String, VmError> {
  let v = rt.to_string(value)?;
  let Value::String(s) = v else {
    return Err(type_error(rt, "ToString did not return a string"));
  };
  string_to_rust_string_limited(rt, s, max_bytes, context)
}

fn url_setter_result(
  rt: &mut webidl_js_runtime::VmJsRuntime,
  result: Result<(), UrlError>,
) -> Result<(), VmError> {
  match result {
    Ok(()) => Ok(()),
    // WHATWG URL setters (other than `href`) do not throw on parse failures; they simply do
    // nothing. Preserve that behaviour while still surfacing resource-limit failures.
    Err(UrlError::SetterFailure { .. }) => Ok(()),
    Err(e) => Err(type_error(rt, &e.to_string())),
  }
}

fn define_method(
  rt: &mut webidl_js_runtime::VmJsRuntime,
  obj: Value,
  name: &str,
  func: Value,
) -> Result<(), VmError> {
  let key = to_property_key(rt, name)?;
  rt.define_data_property(obj, key, func, false)
}

fn define_accessor(
  rt: &mut webidl_js_runtime::VmJsRuntime,
  obj: Value,
  name: &str,
  get: Value,
  set: Value,
) -> Result<(), VmError> {
  let key = to_property_key(rt, name)?;
  rt.define_accessor_property(obj, key, get, set, false)
}

fn array_to_iterator(
  rt: &mut webidl_js_runtime::VmJsRuntime,
  arr: Value,
  len: usize,
) -> Result<Value, VmError> {
  // `vm-js` arrays do not have interpreter-backed iterator methods yet, so build a small host
  // iterator object that yields values from `arr`.
  let iter = rt.alloc_object_value()?;
  let mut roots: Vec<RootId> = Vec::new();
  let result = (|| -> Result<Value, VmError> {
    roots.push(rt.heap_mut().add_root(iter)?);
    roots.push(rt.heap_mut().add_root(arr)?);

    let values_key = rt.property_key_from_str("__fastrender_iter_values")?;
    rt.define_data_property(iter, values_key, arr, false)?;

    let index_key = rt.property_key_from_str("__fastrender_iter_index")?;
    rt.define_data_property(iter, index_key, Value::Number(0.0), false)?;

    let len_key = rt.property_key_from_str("__fastrender_iter_len")?;
    rt.define_data_property(iter, len_key, Value::Number(len as f64), false)?;

    let next = rt.alloc_function_value(move |rt, this, _args| {
      let obj = expect_object(rt, this, "Iterator")?;

      let values_key = rt.property_key_from_str("__fastrender_iter_values")?;
      let values = rt.get(Value::Object(obj), values_key)?;

      let index_key = rt.property_key_from_str("__fastrender_iter_index")?;
      let index = rt.get(Value::Object(obj), index_key)?;
      let Value::Number(index) = index else {
        return Err(type_error(rt, "Iterator: invalid index"));
      };
      if !index.is_finite() || index < 0.0 || index.fract() != 0.0 || index > u32::MAX as f64 {
        return Err(type_error(rt, "Iterator: invalid index"));
      }
      let idx_u32: u32 = index as u32;
      let idx_usize = idx_u32 as usize;

      let len_key = rt.property_key_from_str("__fastrender_iter_len")?;
      let len = rt.get(Value::Object(obj), len_key)?;
      let Value::Number(len) = len else {
        return Err(type_error(rt, "Iterator: invalid length"));
      };
      if !len.is_finite() || len < 0.0 || len.fract() != 0.0 || len > u32::MAX as f64 {
        return Err(type_error(rt, "Iterator: invalid length"));
      }
      let len_u32: u32 = len as u32;
      let len_usize = len_u32 as usize;

      let (done, value) = if idx_usize >= len_usize {
        (true, Value::Undefined)
      } else {
        let key = rt.property_key_from_u32(idx_u32)?;
        let value = rt.get(values, key)?;

        // Update `__fastrender_iter_index`.
        let index_key = rt.property_key_from_str("__fastrender_iter_index")?;
        rt.define_data_property(
          Value::Object(obj),
          index_key,
          Value::Number((idx_usize + 1) as f64),
          false,
        )?;
        (false, value)
      };

      let result = rt.alloc_object_value()?;
      let result_root = rt.heap_mut().add_root(result)?;
      let done_key = rt.property_key_from_str("done")?;
      rt.define_data_property(result, done_key, Value::Bool(done), true)?;
      let value_key = rt.property_key_from_str("value")?;
      rt.define_data_property(result, value_key, value, true)?;
      rt.heap_mut().remove_root(result_root);
      Ok(result)
    })?;
    roots.push(rt.heap_mut().add_root(next)?);
    let next_key = rt.property_key_from_str("next")?;
    rt.define_data_property(iter, next_key, next, false)?;

    // Iterator objects returned by URLSearchParams.{entries,keys,values} must be iterable:
    // https://tc39.es/ecma262/#sec-%arrayiteratorprototype%-@@iterator
    let iterator = rt.alloc_function_value(|_rt, this, _args| Ok(this))?;
    roots.push(rt.heap_mut().add_root(iterator)?);
    let iter_key = rt.symbol_iterator()?;
    rt.define_data_property(iter, iter_key, iterator, false)?;

    Ok(iter)
  })();

  for id in roots {
    rt.heap_mut().remove_root(id);
  }
  result
}

fn expect_object(
  rt: &mut webidl_js_runtime::VmJsRuntime,
  this: Value,
  class_name: &str,
) -> Result<GcObject, VmError> {
  let Value::Object(obj) = this else {
    return Err(type_error(
      rt,
      &format!("{class_name}: illegal invocation (this is not an object)"),
    ));
  };
  Ok(obj)
}

fn array_like_length(
  rt: &mut webidl_js_runtime::VmJsRuntime,
  value: Value,
) -> Result<Option<u32>, VmError> {
  if !rt.is_object(value) {
    return Ok(None);
  }
  let length_key = rt.property_key_from_str("length")?;
  let Some(desc) = rt.get_own_property(value, length_key)? else {
    return Ok(None);
  };
  if desc.enumerable {
    return Ok(None);
  }
  let JsPropertyKind::Data { value } = desc.kind else {
    return Ok(None);
  };
  let Value::Number(n) = value else {
    return Ok(None);
  };
  if !n.is_finite() || n < 0.0 || n.fract() != 0.0 || n > u32::MAX as f64 {
    return Ok(None);
  }
  Ok(Some(n as u32))
}

fn init_url_instance(
  rt: &mut webidl_js_runtime::VmJsRuntime,
  state: Rc<RefCell<UrlBindingState>>,
  obj: Value,
) -> Result<(), VmError> {
  // When initialising an instance we temporarily keep the partially-initialized object and the
  // function objects for its accessors/methods rooted, so a GC triggered by intermediate
  // allocations cannot collect them.
  let mut roots: Vec<RootId> = Vec::new();
  let result = (|| -> Result<(), VmError> {
    roots.push(rt.heap_mut().add_root(obj)?);
    let max_input_bytes = { state.borrow().limits.max_input_bytes };

    let href_get = rt.alloc_function_value({
      let state = state.clone();
      move |rt, this, _args| {
        let obj = expect_object(rt, this, "URL")?;
        let url = state
          .borrow()
          .urls
          .get(&obj)
          .ok_or_else(|| type_error(rt, "URL: illegal invocation"))?
          .url
          .clone();
        let href = url.href().map_err(|e| type_error(rt, &e.to_string()))?;
        rt.alloc_string_value(&href)
      }
    })?;
    roots.push(rt.heap_mut().add_root(href_get)?);

    let href_set = rt.alloc_function_value({
      let state = state.clone();
      move |rt, this, args| {
        let obj = expect_object(rt, this, "URL")?;
        let value = args.get(0).copied().unwrap_or(Value::Undefined);
        let value = to_rust_string_limited(rt, value, max_input_bytes, "URL.href")?;
        let url = state
          .borrow()
          .urls
          .get(&obj)
          .ok_or_else(|| type_error(rt, "URL: illegal invocation"))?
          .url
          .clone();
        url
          .set_href(&value)
          .map_err(|e| type_error(rt, &e.to_string()))?;
        Ok(Value::Undefined)
      }
    })?;
    roots.push(rt.heap_mut().add_root(href_set)?);

    let origin_get = rt.alloc_function_value({
      let state = state.clone();
      move |rt, this, _args| {
        let obj = expect_object(rt, this, "URL")?;
        let url = state
          .borrow()
          .urls
          .get(&obj)
          .ok_or_else(|| type_error(rt, "URL: illegal invocation"))?
          .url
          .clone();
        let origin = url.origin();
        rt.alloc_string_value(&origin)
      }
    })?;
    roots.push(rt.heap_mut().add_root(origin_get)?);

    let protocol_get = rt.alloc_function_value({
      let state = state.clone();
      move |rt, this, _args| {
        let obj = expect_object(rt, this, "URL")?;
        let url = state
          .borrow()
          .urls
          .get(&obj)
          .ok_or_else(|| type_error(rt, "URL: illegal invocation"))?
          .url
          .clone();
        let protocol = url.protocol().map_err(|e| type_error(rt, &e.to_string()))?;
        rt.alloc_string_value(&protocol)
      }
    })?;
    roots.push(rt.heap_mut().add_root(protocol_get)?);

    let protocol_set = rt.alloc_function_value({
      let state = state.clone();
      move |rt, this, args| {
        let obj = expect_object(rt, this, "URL")?;
        let value = args.get(0).copied().unwrap_or(Value::Undefined);
        let value = to_rust_string_limited(rt, value, max_input_bytes, "URL.protocol")?;
        let url = state
          .borrow()
          .urls
          .get(&obj)
          .ok_or_else(|| type_error(rt, "URL: illegal invocation"))?
          .url
          .clone();
        url_setter_result(rt, url.set_protocol(&value))?;
        Ok(Value::Undefined)
      }
    })?;
    roots.push(rt.heap_mut().add_root(protocol_set)?);

    let username_get = rt.alloc_function_value({
      let state = state.clone();
      move |rt, this, _args| {
        let obj = expect_object(rt, this, "URL")?;
        let url = state
          .borrow()
          .urls
          .get(&obj)
          .ok_or_else(|| type_error(rt, "URL: illegal invocation"))?
          .url
          .clone();
        let username = url.username().map_err(|e| type_error(rt, &e.to_string()))?;
        rt.alloc_string_value(&username)
      }
    })?;
    roots.push(rt.heap_mut().add_root(username_get)?);

    let username_set = rt.alloc_function_value({
      let state = state.clone();
      move |rt, this, args| {
        let obj = expect_object(rt, this, "URL")?;
        let value = args.get(0).copied().unwrap_or(Value::Undefined);
        let value = to_rust_string_limited(rt, value, max_input_bytes, "URL.username")?;
        let url = state
          .borrow()
          .urls
          .get(&obj)
          .ok_or_else(|| type_error(rt, "URL: illegal invocation"))?
          .url
          .clone();
        url_setter_result(rt, url.set_username(&value))?;
        Ok(Value::Undefined)
      }
    })?;
    roots.push(rt.heap_mut().add_root(username_set)?);

    let password_get = rt.alloc_function_value({
      let state = state.clone();
      move |rt, this, _args| {
        let obj = expect_object(rt, this, "URL")?;
        let url = state
          .borrow()
          .urls
          .get(&obj)
          .ok_or_else(|| type_error(rt, "URL: illegal invocation"))?
          .url
          .clone();
        let password = url.password().map_err(|e| type_error(rt, &e.to_string()))?;
        rt.alloc_string_value(&password)
      }
    })?;
    roots.push(rt.heap_mut().add_root(password_get)?);

    let password_set = rt.alloc_function_value({
      let state = state.clone();
      move |rt, this, args| {
        let obj = expect_object(rt, this, "URL")?;
        let value = args.get(0).copied().unwrap_or(Value::Undefined);
        let value = to_rust_string_limited(rt, value, max_input_bytes, "URL.password")?;
        let url = state
          .borrow()
          .urls
          .get(&obj)
          .ok_or_else(|| type_error(rt, "URL: illegal invocation"))?
          .url
          .clone();
        url_setter_result(rt, url.set_password(&value))?;
        Ok(Value::Undefined)
      }
    })?;
    roots.push(rt.heap_mut().add_root(password_set)?);

    let host_get = rt.alloc_function_value({
      let state = state.clone();
      move |rt, this, _args| {
        let obj = expect_object(rt, this, "URL")?;
        let url = state
          .borrow()
          .urls
          .get(&obj)
          .ok_or_else(|| type_error(rt, "URL: illegal invocation"))?
          .url
          .clone();
        let host = url.host().map_err(|e| type_error(rt, &e.to_string()))?;
        rt.alloc_string_value(&host)
      }
    })?;
    roots.push(rt.heap_mut().add_root(host_get)?);

    let host_set = rt.alloc_function_value({
      let state = state.clone();
      move |rt, this, args| {
        let obj = expect_object(rt, this, "URL")?;
        let value = args.get(0).copied().unwrap_or(Value::Undefined);
        let value = to_rust_string_limited(rt, value, max_input_bytes, "URL.host")?;
        let url = state
          .borrow()
          .urls
          .get(&obj)
          .ok_or_else(|| type_error(rt, "URL: illegal invocation"))?
          .url
          .clone();
        url_setter_result(rt, url.set_host(&value))?;
        Ok(Value::Undefined)
      }
    })?;
    roots.push(rt.heap_mut().add_root(host_set)?);

    let hostname_get = rt.alloc_function_value({
      let state = state.clone();
      move |rt, this, _args| {
        let obj = expect_object(rt, this, "URL")?;
        let url = state
          .borrow()
          .urls
          .get(&obj)
          .ok_or_else(|| type_error(rt, "URL: illegal invocation"))?
          .url
          .clone();
        let hostname = url.hostname().map_err(|e| type_error(rt, &e.to_string()))?;
        rt.alloc_string_value(&hostname)
      }
    })?;
    roots.push(rt.heap_mut().add_root(hostname_get)?);

    let hostname_set = rt.alloc_function_value({
      let state = state.clone();
      move |rt, this, args| {
        let obj = expect_object(rt, this, "URL")?;
        let value = args.get(0).copied().unwrap_or(Value::Undefined);
        let value = to_rust_string_limited(rt, value, max_input_bytes, "URL.hostname")?;
        let url = state
          .borrow()
          .urls
          .get(&obj)
          .ok_or_else(|| type_error(rt, "URL: illegal invocation"))?
          .url
          .clone();
        url_setter_result(rt, url.set_hostname(&value))?;
        Ok(Value::Undefined)
      }
    })?;
    roots.push(rt.heap_mut().add_root(hostname_set)?);

    let port_get = rt.alloc_function_value({
      let state = state.clone();
      move |rt, this, _args| {
        let obj = expect_object(rt, this, "URL")?;
        let url = state
          .borrow()
          .urls
          .get(&obj)
          .ok_or_else(|| type_error(rt, "URL: illegal invocation"))?
          .url
          .clone();
        let port = url.port().map_err(|e| type_error(rt, &e.to_string()))?;
        rt.alloc_string_value(&port)
      }
    })?;
    roots.push(rt.heap_mut().add_root(port_get)?);

    let port_set = rt.alloc_function_value({
      let state = state.clone();
      move |rt, this, args| {
        let obj = expect_object(rt, this, "URL")?;
        let value = args.get(0).copied().unwrap_or(Value::Undefined);
        let value = to_rust_string_limited(rt, value, max_input_bytes, "URL.port")?;
        let url = state
          .borrow()
          .urls
          .get(&obj)
          .ok_or_else(|| type_error(rt, "URL: illegal invocation"))?
          .url
          .clone();
        url_setter_result(rt, url.set_port(&value))?;
        Ok(Value::Undefined)
      }
    })?;
    roots.push(rt.heap_mut().add_root(port_set)?);

    let pathname_get = rt.alloc_function_value({
      let state = state.clone();
      move |rt, this, _args| {
        let obj = expect_object(rt, this, "URL")?;
        let url = state
          .borrow()
          .urls
          .get(&obj)
          .ok_or_else(|| type_error(rt, "URL: illegal invocation"))?
          .url
          .clone();
        let pathname = url.pathname().map_err(|e| type_error(rt, &e.to_string()))?;
        rt.alloc_string_value(&pathname)
      }
    })?;
    roots.push(rt.heap_mut().add_root(pathname_get)?);

    let pathname_set = rt.alloc_function_value({
      let state = state.clone();
      move |rt, this, args| {
        let obj = expect_object(rt, this, "URL")?;
        let value = args.get(0).copied().unwrap_or(Value::Undefined);
        let value = to_rust_string_limited(rt, value, max_input_bytes, "URL.pathname")?;
        let url = state
          .borrow()
          .urls
          .get(&obj)
          .ok_or_else(|| type_error(rt, "URL: illegal invocation"))?
          .url
          .clone();
        url_setter_result(rt, url.set_pathname(&value))?;
        Ok(Value::Undefined)
      }
    })?;
    roots.push(rt.heap_mut().add_root(pathname_set)?);

    let search_get = rt.alloc_function_value({
      let state = state.clone();
      move |rt, this, _args| {
        let obj = expect_object(rt, this, "URL")?;
        let url = state
          .borrow()
          .urls
          .get(&obj)
          .ok_or_else(|| type_error(rt, "URL: illegal invocation"))?
          .url
          .clone();
        let search = url.search().map_err(|e| type_error(rt, &e.to_string()))?;
        rt.alloc_string_value(&search)
      }
    })?;
    roots.push(rt.heap_mut().add_root(search_get)?);

    let search_set = rt.alloc_function_value({
      let state = state.clone();
      move |rt, this, args| {
        let obj = expect_object(rt, this, "URL")?;
        let value = args.get(0).copied().unwrap_or(Value::Undefined);
        let value = to_rust_string_limited(rt, value, max_input_bytes, "URL.search")?;
        let url = state
          .borrow()
          .urls
          .get(&obj)
          .ok_or_else(|| type_error(rt, "URL: illegal invocation"))?
          .url
          .clone();
        url
          .set_search(&value)
          .map_err(|e| type_error(rt, &e.to_string()))?;
        Ok(Value::Undefined)
      }
    })?;
    roots.push(rt.heap_mut().add_root(search_set)?);

    let hash_get = rt.alloc_function_value({
      let state = state.clone();
      move |rt, this, _args| {
        let obj = expect_object(rt, this, "URL")?;
        let url = state
          .borrow()
          .urls
          .get(&obj)
          .ok_or_else(|| type_error(rt, "URL: illegal invocation"))?
          .url
          .clone();
        let hash = url.hash().map_err(|e| type_error(rt, &e.to_string()))?;
        rt.alloc_string_value(&hash)
      }
    })?;
    roots.push(rt.heap_mut().add_root(hash_get)?);

    let hash_set = rt.alloc_function_value({
      let state = state.clone();
      move |rt, this, args| {
        let obj = expect_object(rt, this, "URL")?;
        let value = args.get(0).copied().unwrap_or(Value::Undefined);
        let value = to_rust_string_limited(rt, value, max_input_bytes, "URL.hash")?;
        let url = state
          .borrow()
          .urls
          .get(&obj)
          .ok_or_else(|| type_error(rt, "URL: illegal invocation"))?
          .url
          .clone();
        url
          .set_hash(&value)
          .map_err(|e| type_error(rt, &e.to_string()))?;
        Ok(Value::Undefined)
      }
    })?;
    roots.push(rt.heap_mut().add_root(hash_set)?);

    let search_params_get = rt.alloc_function_value({
      let state = state.clone();
      move |rt, this, _args| {
        let obj = expect_object(rt, this, "URL")?;
        // Internal slot used to keep the associated URLSearchParams wrapper alive and stable.
        //
        // This intentionally uses a non-enumerable own property (instead of a Rust-side cache) so
        // the vm-js GC can trace the cached object. This preserves `[SameObject]` semantics:
        // repeated reads of `.searchParams` return the same object *as long as the URL object is
        // alive*.
        let slot_key = rt.property_key_from_str("__fastrender_url_searchParams")?;
        let cached = rt.get(Value::Object(obj), slot_key)?;
        if !matches!(cached, Value::Undefined) {
          return Ok(cached);
        }

        let url = state
          .borrow()
          .urls
          .get(&obj)
          .ok_or_else(|| type_error(rt, "URL: illegal invocation"))?
          .url
          .clone();

        let params = url.search_params();
        let params_obj = rt.alloc_object_value()?;
        // Root while we allocate functions/properties and attach to the URL object.
        let params_root = rt.heap_mut().add_root(params_obj)?;
        let init_result = init_urlsearchparams_instance(rt, state.clone(), params_obj, params);
        if let Err(err) = init_result {
          rt.heap_mut().remove_root(params_root);
          return Err(err);
        }

        // Define the internal cache slot on the URL instance (non-enumerable, non-writable,
        // non-configurable).
        //
        // Note: allocate a fresh key here rather than reusing `slot_key` so we never hold a
        // non-rooted string handle across allocations that could trigger GC.
        let slot_key = rt.property_key_from_str("__fastrender_url_searchParams")?;
        let Value::Object(params_handle) = params_obj else {
          rt.heap_mut().remove_root(params_root);
          return Err(type_error(rt, "URLSearchParams: expected object"));
        };
        let define_result = {
          let mut scope = rt.heap_mut().scope();
          scope.define_property(
            obj,
            slot_key,
            PropertyDescriptor {
              enumerable: false,
              configurable: false,
              kind: PropertyKind::Data {
                value: Value::Object(params_handle),
                writable: false,
              },
            },
          )
        };
        rt.heap_mut().remove_root(params_root);
        define_result?;
        Ok(params_obj)
      }
    })?;
    roots.push(rt.heap_mut().add_root(search_params_get)?);

    let to_json = rt.alloc_function_value({
      let state = state.clone();
      move |rt, this, _args| {
        let obj = expect_object(rt, this, "URL")?;
        let url = state
          .borrow()
          .urls
          .get(&obj)
          .ok_or_else(|| type_error(rt, "URL: illegal invocation"))?
          .url
          .clone();
        let json = url.to_json().map_err(|e| type_error(rt, &e.to_string()))?;
        rt.alloc_string_value(&json)
      }
    })?;
    roots.push(rt.heap_mut().add_root(to_json)?);

    let to_string = rt.alloc_function_value({
      let state = state.clone();
      move |rt, this, _args| {
        let obj = expect_object(rt, this, "URL")?;
        let url = state
          .borrow()
          .urls
          .get(&obj)
          .ok_or_else(|| type_error(rt, "URL: illegal invocation"))?
          .url
          .clone();
        let href = url.href().map_err(|e| type_error(rt, &e.to_string()))?;
        rt.alloc_string_value(&href)
      }
    })?;
    roots.push(rt.heap_mut().add_root(to_string)?);

    define_accessor(rt, obj, "href", href_get, href_set)?;
    define_accessor(rt, obj, "origin", origin_get, Value::Undefined)?;
    define_accessor(rt, obj, "protocol", protocol_get, protocol_set)?;
    define_accessor(rt, obj, "username", username_get, username_set)?;
    define_accessor(rt, obj, "password", password_get, password_set)?;
    define_accessor(rt, obj, "host", host_get, host_set)?;
    define_accessor(rt, obj, "hostname", hostname_get, hostname_set)?;
    define_accessor(rt, obj, "port", port_get, port_set)?;
    define_accessor(rt, obj, "pathname", pathname_get, pathname_set)?;
    define_accessor(rt, obj, "search", search_get, search_set)?;
    define_accessor(rt, obj, "hash", hash_get, hash_set)?;
    define_accessor(rt, obj, "searchParams", search_params_get, Value::Undefined)?;
    define_method(rt, obj, "toJSON", to_json)?;
    define_method(rt, obj, "toString", to_string)?;

    Ok(())
  })();
  for id in roots {
    rt.heap_mut().remove_root(id);
  }
  result
}

fn init_urlsearchparams_instance(
  rt: &mut webidl_js_runtime::VmJsRuntime,
  state: Rc<RefCell<UrlBindingState>>,
  obj: Value,
  params: UrlSearchParams,
) -> Result<(), VmError> {
  // Like `init_url_instance`, keep the wrapper object and its method functions rooted while the
  // instance is being populated to prevent GC from collecting them mid-initialization.
  let mut roots: Vec<RootId> = Vec::new();
  let result = (|| -> Result<(), VmError> {
    roots.push(rt.heap_mut().add_root(obj)?);
    let max_total_query_bytes = { state.borrow().limits.max_total_query_bytes };

    let Value::Object(obj_handle) = obj else {
      return Err(type_error(rt, "URLSearchParams: expected object"));
    };
    state.borrow_mut().search_params.insert(obj_handle, params);

    let append = rt.alloc_function_value({
      let state = state.clone();
      move |rt, this, args| {
        let obj = expect_object(rt, this, "URLSearchParams")?;
        let name = to_rust_string_limited(
          rt,
          args.get(0).copied().unwrap_or(Value::Undefined),
          max_total_query_bytes,
          "URLSearchParams.append",
        )?;
        let value = to_rust_string_limited(
          rt,
          args.get(1).copied().unwrap_or(Value::Undefined),
          max_total_query_bytes,
          "URLSearchParams.append",
        )?;
        let params = state
          .borrow()
          .search_params
          .get(&obj)
          .ok_or_else(|| type_error(rt, "URLSearchParams: illegal invocation"))?
          .clone();
        params
          .append(&name, &value)
          .map_err(|e| type_error(rt, &e.to_string()))?;
        Ok(Value::Undefined)
      }
    })?;
    roots.push(rt.heap_mut().add_root(append)?);

    let delete = rt.alloc_function_value({
      let state = state.clone();
      move |rt, this, args| {
        let obj = expect_object(rt, this, "URLSearchParams")?;
        let name = to_rust_string_limited(
          rt,
          args.get(0).copied().unwrap_or(Value::Undefined),
          max_total_query_bytes,
          "URLSearchParams.delete",
        )?;
        let value_arg = args.get(1).copied();
        let value = match value_arg {
          None | Some(Value::Undefined) => None,
          Some(v) => Some(to_rust_string_limited(
            rt,
            v,
            max_total_query_bytes,
            "URLSearchParams.delete",
          )?),
        };
        let params = state
          .borrow()
          .search_params
          .get(&obj)
          .ok_or_else(|| type_error(rt, "URLSearchParams: illegal invocation"))?
          .clone();
        params
          .delete(&name, value.as_deref())
          .map_err(|e| type_error(rt, &e.to_string()))?;
        Ok(Value::Undefined)
      }
    })?;
    roots.push(rt.heap_mut().add_root(delete)?);

    let get = rt.alloc_function_value({
      let state = state.clone();
      move |rt, this, args| {
        let obj = expect_object(rt, this, "URLSearchParams")?;
        let name = to_rust_string_limited(
          rt,
          args.get(0).copied().unwrap_or(Value::Undefined),
          max_total_query_bytes,
          "URLSearchParams.get",
        )?;
        let params = state
          .borrow()
          .search_params
          .get(&obj)
          .ok_or_else(|| type_error(rt, "URLSearchParams: illegal invocation"))?
          .clone();
        let value = params
          .get(&name)
          .map_err(|e| type_error(rt, &e.to_string()))?;
        match value {
          Some(v) => rt.alloc_string_value(&v),
          None => Ok(Value::Null),
        }
      }
    })?;
    roots.push(rt.heap_mut().add_root(get)?);

    let get_all = rt.alloc_function_value({
      let state = state.clone();
      move |rt, this, args| {
        let obj = expect_object(rt, this, "URLSearchParams")?;
        let name = to_rust_string_limited(
          rt,
          args.get(0).copied().unwrap_or(Value::Undefined),
          max_total_query_bytes,
          "URLSearchParams.getAll",
        )?;
        let params = state
          .borrow()
          .search_params
          .get(&obj)
          .ok_or_else(|| type_error(rt, "URLSearchParams: illegal invocation"))?
          .clone();
        let values = params
          .get_all(&name)
          .map_err(|e| type_error(rt, &e.to_string()))?;

        // WHATWG `URLSearchParams.getAll()` returns a sequence<string>, which maps to a JS Array.
        let arr = rt.alloc_array()?;
        for (idx, value) in values.iter().enumerate() {
          let idx_u32: u32 = idx
            .try_into()
            .map_err(|_| type_error(rt, "URLSearchParams.getAll: index exceeds u32"))?;
          let value = rt.alloc_string_value(value)?;
          let key = rt.property_key_from_u32(idx_u32)?;
          rt.define_data_property(arr, key, value, true)?;
        }
        Ok(arr)
      }
    })?;
    roots.push(rt.heap_mut().add_root(get_all)?);

    let has = rt.alloc_function_value({
      let state = state.clone();
      move |rt, this, args| {
        let obj = expect_object(rt, this, "URLSearchParams")?;
        let name = to_rust_string_limited(
          rt,
          args.get(0).copied().unwrap_or(Value::Undefined),
          max_total_query_bytes,
          "URLSearchParams.has",
        )?;
        let value_arg = args.get(1).copied();
        let value = match value_arg {
          None | Some(Value::Undefined) => None,
          Some(v) => Some(to_rust_string_limited(
            rt,
            v,
            max_total_query_bytes,
            "URLSearchParams.has",
          )?),
        };
        let params = state
          .borrow()
          .search_params
          .get(&obj)
          .ok_or_else(|| type_error(rt, "URLSearchParams: illegal invocation"))?
          .clone();
        let has = params
          .has(&name, value.as_deref())
          .map_err(|e| type_error(rt, &e.to_string()))?;
        Ok(Value::Bool(has))
      }
    })?;
    roots.push(rt.heap_mut().add_root(has)?);

    let set = rt.alloc_function_value({
      let state = state.clone();
      move |rt, this, args| {
        let obj = expect_object(rt, this, "URLSearchParams")?;
        let name = to_rust_string_limited(
          rt,
          args.get(0).copied().unwrap_or(Value::Undefined),
          max_total_query_bytes,
          "URLSearchParams.set",
        )?;
        let value = to_rust_string_limited(
          rt,
          args.get(1).copied().unwrap_or(Value::Undefined),
          max_total_query_bytes,
          "URLSearchParams.set",
        )?;
        let params = state
          .borrow()
          .search_params
          .get(&obj)
          .ok_or_else(|| type_error(rt, "URLSearchParams: illegal invocation"))?
          .clone();
        params
          .set(&name, &value)
          .map_err(|e| type_error(rt, &e.to_string()))?;
        Ok(Value::Undefined)
      }
    })?;
    roots.push(rt.heap_mut().add_root(set)?);

    let to_string = rt.alloc_function_value({
      let state = state.clone();
      move |rt, this, _args| {
        let obj = expect_object(rt, this, "URLSearchParams")?;
        let params = state
          .borrow()
          .search_params
          .get(&obj)
          .ok_or_else(|| type_error(rt, "URLSearchParams: illegal invocation"))?
          .clone();
        let s = params
          .serialize()
          .map_err(|e| type_error(rt, &e.to_string()))?;
        rt.alloc_string_value(&s)
      }
    })?;
    roots.push(rt.heap_mut().add_root(to_string)?);

    let size_get = rt.alloc_function_value({
      let state = state.clone();
      move |rt, this, _args| {
        let obj = expect_object(rt, this, "URLSearchParams")?;
        let params = state
          .borrow()
          .search_params
          .get(&obj)
          .ok_or_else(|| type_error(rt, "URLSearchParams: illegal invocation"))?
          .clone();
        let size = params.size().map_err(|e| type_error(rt, &e.to_string()))?;
        Ok(Value::Number(size as f64))
      }
    })?;
    roots.push(rt.heap_mut().add_root(size_get)?);

    let sort = rt.alloc_function_value({
      let state = state.clone();
      move |rt, this, _args| {
        let obj = expect_object(rt, this, "URLSearchParams")?;
        let params = state
          .borrow()
          .search_params
          .get(&obj)
          .ok_or_else(|| type_error(rt, "URLSearchParams: illegal invocation"))?
          .clone();
        params.sort().map_err(|e| type_error(rt, &e.to_string()))?;
        Ok(Value::Undefined)
      }
    })?;
    roots.push(rt.heap_mut().add_root(sort)?);

    let entries = rt.alloc_function_value({
      let state = state.clone();
      move |rt, this, _args| {
        let obj = expect_object(rt, this, "URLSearchParams")?;
        let params = state
          .borrow()
          .search_params
          .get(&obj)
          .ok_or_else(|| type_error(rt, "URLSearchParams: illegal invocation"))?
          .clone();
        let pairs = params.pairs().map_err(|e| type_error(rt, &e.to_string()))?;

        let arr = rt.alloc_array()?;
        let arr_root = rt.heap_mut().add_root(arr)?;
        for (idx, (name, value)) in pairs.iter().enumerate() {
          let idx_u32: u32 = idx
            .try_into()
            .map_err(|_| type_error(rt, "URLSearchParams.entries: index exceeds u32"))?;

          let pair = rt.alloc_array()?;
          let pair_root = rt.heap_mut().add_root(pair)?;
          let name_value = rt.alloc_string_value(name)?;
          let value_value = rt.alloc_string_value(value)?;
          let key0 = rt.property_key_from_u32(0)?;
          rt.define_data_property(pair, key0, name_value, true)?;
          let key1 = rt.property_key_from_u32(1)?;
          rt.define_data_property(pair, key1, value_value, true)?;
          rt.heap_mut().remove_root(pair_root);

          let key = rt.property_key_from_u32(idx_u32)?;
          rt.define_data_property(arr, key, pair, true)?;
        }
        let iter = array_to_iterator(rt, arr, pairs.len());
        rt.heap_mut().remove_root(arr_root);
        iter
      }
    })?;
    roots.push(rt.heap_mut().add_root(entries)?);

    let keys = rt.alloc_function_value({
      let state = state.clone();
      move |rt, this, _args| {
        let obj = expect_object(rt, this, "URLSearchParams")?;
        let params = state
          .borrow()
          .search_params
          .get(&obj)
          .ok_or_else(|| type_error(rt, "URLSearchParams: illegal invocation"))?
          .clone();
        let pairs = params.pairs().map_err(|e| type_error(rt, &e.to_string()))?;

        let arr = rt.alloc_array()?;
        let arr_root = rt.heap_mut().add_root(arr)?;
        for (idx, (name, _)) in pairs.iter().enumerate() {
          let idx_u32: u32 = idx
            .try_into()
            .map_err(|_| type_error(rt, "URLSearchParams.keys: index exceeds u32"))?;
          let name_value = rt.alloc_string_value(name)?;
          let key = rt.property_key_from_u32(idx_u32)?;
          rt.define_data_property(arr, key, name_value, true)?;
        }
        let iter = array_to_iterator(rt, arr, pairs.len());
        rt.heap_mut().remove_root(arr_root);
        iter
      }
    })?;
    roots.push(rt.heap_mut().add_root(keys)?);

    let values = rt.alloc_function_value({
      let state = state.clone();
      move |rt, this, _args| {
        let obj = expect_object(rt, this, "URLSearchParams")?;
        let params = state
          .borrow()
          .search_params
          .get(&obj)
          .ok_or_else(|| type_error(rt, "URLSearchParams: illegal invocation"))?
          .clone();
        let pairs = params.pairs().map_err(|e| type_error(rt, &e.to_string()))?;

        let arr = rt.alloc_array()?;
        let arr_root = rt.heap_mut().add_root(arr)?;
        for (idx, (_, value)) in pairs.iter().enumerate() {
          let idx_u32: u32 = idx
            .try_into()
            .map_err(|_| type_error(rt, "URLSearchParams.values: index exceeds u32"))?;
          let value_value = rt.alloc_string_value(value)?;
          let key = rt.property_key_from_u32(idx_u32)?;
          rt.define_data_property(arr, key, value_value, true)?;
        }
        let iter = array_to_iterator(rt, arr, pairs.len());
        rt.heap_mut().remove_root(arr_root);
        iter
      }
    })?;
    roots.push(rt.heap_mut().add_root(values)?);

    let for_each = rt.alloc_function_value({
      let state = state.clone();
      move |rt, this, args| {
        let obj = expect_object(rt, this, "URLSearchParams")?;
        let callback = args.get(0).copied().unwrap_or(Value::Undefined);
        if !rt.is_callable(callback) {
          return Err(type_error(
            rt,
            "URLSearchParams.forEach callback is not a function",
          ));
        }
        let this_arg = args.get(1).copied().unwrap_or(Value::Undefined);

        let params = state
          .borrow()
          .search_params
          .get(&obj)
          .ok_or_else(|| type_error(rt, "URLSearchParams: illegal invocation"))?
          .clone();
        let pairs = params.pairs().map_err(|e| type_error(rt, &e.to_string()))?;

        for (name, value) in pairs {
          let value_value = rt.alloc_string_value(&value)?;
          let name_value = rt.alloc_string_value(&name)?;
          rt.call_function(
            callback,
            this_arg,
            &[value_value, name_value, Value::Object(obj)],
          )?;
        }
        Ok(Value::Undefined)
      }
    })?;
    roots.push(rt.heap_mut().add_root(for_each)?);

    define_method(rt, obj, "append", append)?;
    define_method(rt, obj, "delete", delete)?;
    define_method(rt, obj, "get", get)?;
    define_method(rt, obj, "getAll", get_all)?;
    define_method(rt, obj, "has", has)?;
    define_method(rt, obj, "set", set)?;
    define_accessor(rt, obj, "size", size_get, Value::Undefined)?;
    define_method(rt, obj, "sort", sort)?;
    define_method(rt, obj, "toString", to_string)?;
    define_method(rt, obj, "entries", entries)?;
    define_method(rt, obj, "keys", keys)?;
    define_method(rt, obj, "values", values)?;
    define_method(rt, obj, "forEach", for_each)?;

    let iter_key = rt.symbol_iterator()?;
    rt.define_data_property(obj, iter_key, entries, false)?;

    Ok(())
  })();
  for id in roots {
    rt.heap_mut().remove_root(id);
  }
  result
}

/// Install WHATWG-shaped `URL` and `URLSearchParams` constructors onto `global`.
///
/// The bindings are backed by the deterministic Rust primitives in [`crate::js::url`].
pub fn install_url_bindings(
  rt: &mut webidl_js_runtime::VmJsRuntime,
  global: Value,
) -> Result<(), VmError> {
  install_url_bindings_with_limits(rt, global, UrlLimits::default())
}

/// Install WHATWG-shaped `URL` and `URLSearchParams` constructors onto `global`, using the provided
/// resource limits.
pub fn install_url_bindings_with_limits(
  rt: &mut webidl_js_runtime::VmJsRuntime,
  global: Value,
  limits: UrlLimits,
) -> Result<(), VmError> {
  // Root `global` while defining constructors: when the heap is under GC pressure, the intermediate
  // allocations in this function can trigger a collection, and `global` is otherwise just a raw
  // handle from the embedding.
  let mut roots: Vec<RootId> = Vec::new();
  roots.push(rt.heap_mut().add_root(global)?);

  let result = (|| -> Result<(), VmError> {
    let mut state = UrlBindingState::default();
    state.limits = limits;
    let state: Rc<RefCell<UrlBindingState>> = Rc::new(RefCell::new(state));

    let url_ctor = rt.alloc_function_value({
      let state = state.clone();
      move |rt, _this, args| {
        let limits = { state.borrow().limits.clone() };
        let input = to_rust_string_limited(
          rt,
          args.get(0).copied().unwrap_or(Value::Undefined),
          limits.max_input_bytes,
          "URL constructor input",
        )?;
        let base_value = args.get(1).copied();
        let base = match base_value {
          None | Some(Value::Undefined) => None,
          Some(Value::Object(obj)) => {
            // `vm-js` does not implement full object-to-string coercion yet (it produces
            // `"[object Object]"` for ordinary objects). To support `new URL(rel, baseUrlObj)` we
            // special-case bases that are themselves URL wrappers.
            let maybe_base = { state.borrow().urls.get(&obj).map(|u| u.url.clone()) };
            if let Some(base_url) = maybe_base {
              let href = base_url
                .href()
                .map_err(|e| type_error(rt, &e.to_string()))?;
              Some(href)
            } else {
              Some(to_rust_string_limited(
                rt,
                Value::Object(obj),
                limits.max_input_bytes,
                "URL constructor base",
              )?)
            }
          }
          Some(v) => Some(to_rust_string_limited(
            rt,
            v,
            limits.max_input_bytes,
            "URL constructor base",
          )?),
        };

        let url = Url::parse(&input, base.as_deref(), &limits)
          .map_err(|e| type_error(rt, &e.to_string()))?;
        let obj = rt.alloc_object_value()?;
        let Value::Object(obj_handle) = obj else {
          return Err(type_error(rt, "URL: expected object"));
        };

        state
          .borrow_mut()
          .urls
          .insert(obj_handle, UrlInstance { url });

        init_url_instance(rt, state.clone(), obj)?;
        Ok(obj)
      }
    })?;
    roots.push(rt.heap_mut().add_root(url_ctor)?);

    let url_parse = rt.alloc_function_value({
      let state = state.clone();
      move |rt, _this, args| {
        let limits = { state.borrow().limits.clone() };
        let input = to_rust_string_limited(
          rt,
          args.get(0).copied().unwrap_or(Value::Undefined),
          limits.max_input_bytes,
          "URL.parse input",
        )?;
        let base_value = args.get(1).copied();
        let base = match base_value {
          None | Some(Value::Undefined) => None,
          Some(Value::Object(obj)) => {
            let maybe_base = { state.borrow().urls.get(&obj).map(|u| u.url.clone()) };
            if let Some(base_url) = maybe_base {
              let href = base_url
                .href()
                .map_err(|e| type_error(rt, &e.to_string()))?;
              Some(href)
            } else {
              Some(to_rust_string_limited(
                rt,
                Value::Object(obj),
                limits.max_input_bytes,
                "URL.parse base",
              )?)
            }
          }
          Some(v) => Some(to_rust_string_limited(
            rt,
            v,
            limits.max_input_bytes,
            "URL.parse base",
          )?),
        };

        let url = match Url::parse_without_diagnostics(&input, base.as_deref(), &limits) {
          Ok(url) => url,
          Err(_) => return Ok(Value::Null),
        };

        let obj = rt.alloc_object_value()?;
        let Value::Object(obj_handle) = obj else {
          return Err(type_error(rt, "URL: expected object"));
        };

        state
          .borrow_mut()
          .urls
          .insert(obj_handle, UrlInstance { url });
        init_url_instance(rt, state.clone(), obj)?;
        Ok(obj)
      }
    })?;
    roots.push(rt.heap_mut().add_root(url_parse)?);

    let url_can_parse = rt.alloc_function_value({
      let state = state.clone();
      move |rt, _this, args| {
        let limits = { state.borrow().limits.clone() };
        let input = to_rust_string_limited(
          rt,
          args.get(0).copied().unwrap_or(Value::Undefined),
          limits.max_input_bytes,
          "URL.canParse input",
        )?;
        let base_value = args.get(1).copied();
        let base = match base_value {
          None | Some(Value::Undefined) => None,
          Some(Value::Object(obj)) => {
            let maybe_base = { state.borrow().urls.get(&obj).map(|u| u.url.clone()) };
            if let Some(base_url) = maybe_base {
              let href = base_url
                .href()
                .map_err(|e| type_error(rt, &e.to_string()))?;
              Some(href)
            } else {
              Some(to_rust_string_limited(
                rt,
                Value::Object(obj),
                limits.max_input_bytes,
                "URL.canParse base",
              )?)
            }
          }
          Some(v) => Some(to_rust_string_limited(
            rt,
            v,
            limits.max_input_bytes,
            "URL.canParse base",
          )?),
        };

        Ok(Value::Bool(Url::can_parse(
          &input,
          base.as_deref(),
          &limits,
        )))
      }
    })?;
    roots.push(rt.heap_mut().add_root(url_can_parse)?);

    let url_search_params_ctor = rt.alloc_function_value({
      let state = state.clone();
      move |rt, _this, args| {
        let init = args.get(0).copied();
        let limits = { state.borrow().limits.clone() };
        let params = match init {
          None | Some(Value::Undefined) => UrlSearchParams::new(&limits),
          // WebIDL treats String objects as string values when converting the constructor union.
          Some(v) if rt.is_string_object(v) => {
            let init =
              to_rust_string_limited(rt, v, limits.max_input_bytes, "URLSearchParams init")?;
            UrlSearchParams::parse(&init, &limits).map_err(|e| type_error(rt, &e.to_string()))?
          }
          Some(v) if rt.is_object(v) => {
            // The WHATWG constructor accepts:
            // - `USVString`
            // - `sequence<sequence<USVString>>` (iterable of pairs)
            // - `record<USVString, USVString>`
            //
            // `vm-js` arrays are not iterable yet, so we special-case them by detecting array-ish
            // objects via their non-enumerable `length` own property.
            let iter_key = rt.symbol_iterator()?;
            if let Some(method) = rt.get_method(v, iter_key)? {
              // Iterable of pairs.
              let mut record = rt.get_iterator_from_method(v, method)?;
              let mut roots: Vec<RootId> = Vec::new();
              let result = (|| -> Result<UrlSearchParams, VmError> {
                roots.push(rt.heap_mut().add_root(record.iterator)?);
                roots.push(rt.heap_mut().add_root(record.next_method)?);

                let params = UrlSearchParams::new(&limits);
                while let Some(pair) = rt.iterator_step_value(&mut record)? {
                  let pair_root = rt.heap_mut().add_root(pair)?;
                  let step = (|| -> Result<(), VmError> {
                    let Value::Object(pair_obj) = pair else {
                      return Err(type_error(
                        rt,
                        "URLSearchParams init: expected a [name, value] pair",
                      ));
                    };

                    let length_key = rt.property_key_from_str("length")?;
                    let len = rt.get(Value::Object(pair_obj), length_key)?;
                    if len != Value::Number(2.0) {
                      return Err(type_error(
                        rt,
                        "URLSearchParams init: expected a [name, value] pair",
                      ));
                    }

                    let name_value = {
                      let key = rt.property_key_from_u32(0)?;
                      rt.get(Value::Object(pair_obj), key)?
                    };
                    let name = to_rust_string_limited(
                      rt,
                      name_value,
                      limits.max_total_query_bytes,
                      "URLSearchParams init",
                    )?;

                    let value_value = {
                      let key = rt.property_key_from_u32(1)?;
                      rt.get(Value::Object(pair_obj), key)?
                    };
                    let value = to_rust_string_limited(
                      rt,
                      value_value,
                      limits.max_total_query_bytes,
                      "URLSearchParams init",
                    )?;

                    params
                      .append(&name, &value)
                      .map_err(|e| type_error(rt, &e.to_string()))?;
                    Ok(())
                  })();
                  rt.heap_mut().remove_root(pair_root);
                  step?;
                }
                Ok(params)
              })();
              for id in roots {
                rt.heap_mut().remove_root(id);
              }
              result?
            } else if let Some(len) = array_like_length(rt, v)? {
              // Array-of-pairs.
              let params = UrlSearchParams::new(&limits);
              for idx in 0..len {
                let pair = {
                  let key = rt.property_key_from_u32(idx)?;
                  rt.get(v, key)?
                };
                let Value::Object(pair_obj) = pair else {
                  return Err(type_error(
                    rt,
                    "URLSearchParams init: expected a [name, value] pair",
                  ));
                };

                let length_key = rt.property_key_from_str("length")?;
                let len = rt.get(Value::Object(pair_obj), length_key)?;
                if len != Value::Number(2.0) {
                  return Err(type_error(
                    rt,
                    "URLSearchParams init: expected a [name, value] pair",
                  ));
                }

                let name_value = {
                  let key = rt.property_key_from_u32(0)?;
                  rt.get(Value::Object(pair_obj), key)?
                };
                let name = to_rust_string_limited(
                  rt,
                  name_value,
                  limits.max_total_query_bytes,
                  "URLSearchParams init",
                )?;

                let value_value = {
                  let key = rt.property_key_from_u32(1)?;
                  rt.get(Value::Object(pair_obj), key)?
                };
                let value = to_rust_string_limited(
                  rt,
                  value_value,
                  limits.max_total_query_bytes,
                  "URLSearchParams init",
                )?;

                params
                  .append(&name, &value)
                  .map_err(|e| type_error(rt, &e.to_string()))?;
              }
              params
            } else {
              // Record/object of key/value pairs.
              let params = UrlSearchParams::new(&limits);
              let keys = rt.own_property_keys(v)?;
              for key in keys {
                let PropertyKey::String(s) = key else {
                  continue;
                };
                let Some(desc) = rt.get_own_property(v, key)? else {
                  continue;
                };
                if !desc.enumerable {
                  continue;
                }

                let name = string_to_rust_string_limited(
                  rt,
                  s,
                  limits.max_total_query_bytes,
                  "URLSearchParams init",
                )?;
                let value_value = rt.get(v, key)?;
                let value = to_rust_string_limited(
                  rt,
                  value_value,
                  limits.max_total_query_bytes,
                  "URLSearchParams init",
                )?;
                params
                  .append(&name, &value)
                  .map_err(|e| type_error(rt, &e.to_string()))?;
              }
              params
            }
          }
          Some(v) => {
            // String (and any non-object / non-String-object value).
            let init =
              to_rust_string_limited(rt, v, limits.max_input_bytes, "URLSearchParams init")?;
            UrlSearchParams::parse(&init, &limits).map_err(|e| type_error(rt, &e.to_string()))?
          }
        };
        let obj = rt.alloc_object_value()?;
        init_urlsearchparams_instance(rt, state.clone(), obj, params)?;
        Ok(obj)
      }
    })?;
    roots.push(rt.heap_mut().add_root(url_search_params_ctor)?);

    define_method(rt, url_ctor, "parse", url_parse)?;
    define_method(rt, url_ctor, "canParse", url_can_parse)?;

    let url_key = to_property_key(rt, "URL")?;
    rt.define_data_property(global, url_key, url_ctor, false)?;
    let usp_key = to_property_key(rt, "URLSearchParams")?;
    rt.define_data_property(global, usp_key, url_search_params_ctor, false)?;

    Ok(())
  })();

  for id in roots {
    rt.heap_mut().remove_root(id);
  }
  result
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::js::webidl::legacy::VmJsRuntime;
  use vm_js::HeapLimits;
  use webidl_js_runtime::{JsRuntime as _, WebIdlJsRuntime as _};

  fn key(rt: &mut VmJsRuntime, name: &str) -> PropertyKey {
    let v = rt.alloc_string_value(name).unwrap();
    let Value::String(s) = v else {
      panic!("expected string for key");
    };
    PropertyKey::String(s)
  }

  fn str_val(rt: &mut VmJsRuntime, s: &str) -> Value {
    rt.alloc_string_value(s).unwrap()
  }

  fn as_rust_string(rt: &VmJsRuntime, v: Value) -> String {
    let Value::String(s) = v else {
      panic!("expected string, got {v:?}");
    };
    rt.heap().get_string(s).unwrap().to_utf8_lossy()
  }

  fn get(rt: &mut VmJsRuntime, obj: Value, name: &str) -> Value {
    let k = key(rt, name);
    rt.get(obj, k).unwrap()
  }

  fn call(rt: &mut VmJsRuntime, func: Value, this: Value, args: &[Value]) -> Value {
    rt.call_function(func, this, args).unwrap()
  }

  fn call_method(rt: &mut VmJsRuntime, this: Value, name: &str, args: &[Value]) -> Value {
    let func = get(rt, this, name);
    call(rt, func, this, args)
  }

  fn set_accessor(rt: &mut VmJsRuntime, obj: Value, name: &str, value: Value) {
    // Keep the receiver and value rooted in case key allocation triggers GC.
    let obj_root = rt.heap_mut().add_root(obj).unwrap();
    let value_root = rt.heap_mut().add_root(value).unwrap();
    let key = key(rt, name);
    let desc = rt
      .get_own_property(obj, key)
      .unwrap()
      .unwrap_or_else(|| panic!("missing own property {name}"));
    let JsPropertyKind::Accessor { set, .. } = desc.kind else {
      panic!("{name} is not an accessor property");
    };
    call(rt, set, obj, &[value]);
    rt.heap_mut().remove_root(value_root);
    rt.heap_mut().remove_root(obj_root);
  }

  fn new_url(rt: &mut VmJsRuntime, global: Value, input: &str, base: Option<&str>) -> Value {
    let url_ctor = get(rt, global, "URL");
    let mut args = vec![str_val(rt, input)];
    if let Some(base) = base {
      args.push(str_val(rt, base));
    }
    call(rt, url_ctor, Value::Undefined, &args)
  }

  fn new_url_search_params(rt: &mut VmJsRuntime, global: Value, init: Option<&str>) -> Value {
    let ctor = get(rt, global, "URLSearchParams");
    let args = init.map(|s| vec![str_val(rt, s)]).unwrap_or_default();
    call(rt, ctor, Value::Undefined, &args)
  }

  fn new_url_search_params_value(rt: &mut VmJsRuntime, global: Value, init: Value) -> Value {
    let ctor = get(rt, global, "URLSearchParams");
    call(rt, ctor, Value::Undefined, &[init])
  }

  fn array(rt: &mut VmJsRuntime, items: &[Value]) -> Value {
    let arr = rt.alloc_array().unwrap();
    let arr_root = rt.heap_mut().add_root(arr).unwrap();
    for (idx, item) in items.iter().copied().enumerate() {
      let item_root = rt.heap_mut().add_root(item).unwrap();
      let idx_u32: u32 = idx.try_into().unwrap();
      let key = rt.property_key_from_u32(idx_u32).unwrap();
      rt.define_data_property(arr, key, item, true).unwrap();
      rt.heap_mut().remove_root(item_root);
    }
    rt.heap_mut().remove_root(arr_root);
    arr
  }

  fn record(rt: &mut VmJsRuntime, entries: &[(&str, &str)]) -> Value {
    let obj = rt.alloc_object_value().unwrap();
    let obj_root = rt.heap_mut().add_root(obj).unwrap();
    for (k, v) in entries {
      let key = key(rt, k);
      let key_root = match key {
        PropertyKey::String(s) => Some(rt.heap_mut().add_root(Value::String(s)).unwrap()),
        PropertyKey::Symbol(s) => Some(rt.heap_mut().add_root(Value::Symbol(s)).unwrap()),
      };
      let value = str_val(rt, v);
      rt.define_data_property(obj, key, value, true).unwrap();
      if let Some(id) = key_root {
        rt.heap_mut().remove_root(id);
      }
    }
    rt.heap_mut().remove_root(obj_root);
    obj
  }

  #[test]
  fn url_parse_and_can_parse() {
    let mut rt = VmJsRuntime::new();
    let global = rt.alloc_object_value().unwrap();
    install_url_bindings(&mut rt, global).unwrap();

    let url_ctor = get(&mut rt, global, "URL");
    let foo = str_val(&mut rt, "foo");
    let base = str_val(&mut rt, "https://example.com/base");
    let parsed = call_method(&mut rt, url_ctor, "parse", &[foo, base]);
    let href = get(&mut rt, parsed, "href");
    assert_eq!(as_rust_string(&rt, href), "https://example.com/foo");

    let not_a_url = str_val(&mut rt, "not a url");
    let invalid = call_method(&mut rt, url_ctor, "parse", &[not_a_url]);
    assert_eq!(invalid, Value::Null);

    let foo = str_val(&mut rt, "foo");
    let base = str_val(&mut rt, "https://example.com/base");
    let can_parse = call_method(&mut rt, url_ctor, "canParse", &[foo, base]);
    assert_eq!(can_parse, Value::Bool(true));

    let not_a_url = str_val(&mut rt, "not a url");
    let can_parse = call_method(&mut rt, url_ctor, "canParse", &[not_a_url]);
    assert_eq!(can_parse, Value::Bool(false));
  }

  #[test]
  fn url_stringification_and_base_url_object() {
    let mut rt = VmJsRuntime::new();
    let global = rt.alloc_object_value().unwrap();
    install_url_bindings(&mut rt, global).unwrap();

    let base_url = new_url(&mut rt, global, "https://example.com/base", None);
    let base_url_root = rt.heap_mut().add_root(base_url).unwrap();
    let base_str = call_method(&mut rt, base_url, "toString", &[]);
    assert_eq!(as_rust_string(&rt, base_str), "https://example.com/base");

    let url_ctor = get(&mut rt, global, "URL");
    let foo = str_val(&mut rt, "foo");
    let url = call(&mut rt, url_ctor, Value::Undefined, &[foo, base_url]);
    let href = get(&mut rt, url, "href");
    assert_eq!(as_rust_string(&rt, href), "https://example.com/foo");

    rt.heap_mut().remove_root(base_url_root);
  }

  #[test]
  fn relative_parsing_with_base() {
    let mut rt = VmJsRuntime::new();
    let global = rt.alloc_object_value().unwrap();
    install_url_bindings(&mut rt, global).unwrap();

    let url = new_url(&mut rt, global, "foo", Some("https://example.com/bar/baz"));
    let href = get(&mut rt, url, "href");
    assert_eq!(as_rust_string(&rt, href), "https://example.com/bar/foo");
  }

  #[test]
  fn url_setters_update_href() {
    let mut rt = VmJsRuntime::new();
    let global = rt.alloc_object_value().unwrap();
    install_url_bindings(&mut rt, global).unwrap();

    let url = new_url(&mut rt, global, "https://example.com/", None);
    let user = str_val(&mut rt, "user");
    let pass = str_val(&mut rt, "pass");
    let host = str_val(&mut rt, "example.org:8080");
    let pathname = str_val(&mut rt, "/a/b");
    let protocol = str_val(&mut rt, "http:");
    set_accessor(&mut rt, url, "username", user);
    set_accessor(&mut rt, url, "password", pass);
    set_accessor(&mut rt, url, "host", host);
    set_accessor(&mut rt, url, "pathname", pathname);
    set_accessor(&mut rt, url, "protocol", protocol);

    let href = get(&mut rt, url, "href");
    assert_eq!(
      as_rust_string(&rt, href),
      "http://user:pass@example.org:8080/a/b"
    );
  }

  #[test]
  fn searchparams_mutation_updates_href() {
    let mut rt = VmJsRuntime::new();
    let global = rt.alloc_object_value().unwrap();
    install_url_bindings(&mut rt, global).unwrap();

    let url = new_url(&mut rt, global, "https://example.com/?a=b%20~", None);
    let search_params_1 = get(&mut rt, url, "searchParams");
    let search_params_2 = get(&mut rt, url, "searchParams");
    assert_eq!(
      search_params_1, search_params_2,
      "expected URL.searchParams to return the same object each time"
    );

    let c = str_val(&mut rt, "c");
    let d = str_val(&mut rt, "d");
    let args = [c, d];
    call_method(&mut rt, search_params_1, "append", &args);

    let href = get(&mut rt, url, "href");
    assert_eq!(
      as_rust_string(&rt, href),
      "https://example.com/?a=b+%7E&c=d"
    );
  }

  #[test]
  fn setting_search_updates_associated_searchparams() {
    let mut rt = VmJsRuntime::new();
    let global = rt.alloc_object_value().unwrap();
    install_url_bindings(&mut rt, global).unwrap();

    let url = new_url(&mut rt, global, "https://example.com/", None);
    let search_params = get(&mut rt, url, "searchParams");

    let search_value = str_val(&mut rt, "?q=a+b");
    set_accessor(&mut rt, url, "search", search_value);
    let q = str_val(&mut rt, "q");
    let args = [q];
    let q_value = call_method(&mut rt, search_params, "get", &args);
    assert_eq!(as_rust_string(&rt, q_value), "a b");
  }

  #[test]
  fn setting_and_clearing_hash() {
    let mut rt = VmJsRuntime::new();
    let global = rt.alloc_object_value().unwrap();
    install_url_bindings(&mut rt, global).unwrap();

    let url = new_url(&mut rt, global, "https://example.com/#a", None);
    let hash = get(&mut rt, url, "hash");
    assert_eq!(as_rust_string(&rt, hash), "#a");

    let hash_b = str_val(&mut rt, "#b");
    set_accessor(&mut rt, url, "hash", hash_b);
    let href = get(&mut rt, url, "href");
    assert_eq!(as_rust_string(&rt, href), "https://example.com/#b");

    let empty = str_val(&mut rt, "");
    set_accessor(&mut rt, url, "hash", empty);
    let href = get(&mut rt, url, "href");
    assert_eq!(as_rust_string(&rt, href), "https://example.com/");
  }

  #[test]
  fn url_origin_reflects_serialized_origin() {
    let mut rt = VmJsRuntime::new();
    let global = rt.alloc_object_value().unwrap();
    install_url_bindings(&mut rt, global).unwrap();

    let url = new_url(&mut rt, global, "https://example.com/path", None);
    let origin = get(&mut rt, url, "origin");
    assert_eq!(as_rust_string(&rt, origin), "https://example.com");

    let url = new_url(&mut rt, global, "http://example.com:8080/path", None);
    let origin = get(&mut rt, url, "origin");
    assert_eq!(as_rust_string(&rt, origin), "http://example.com:8080");
  }

  #[test]
  fn url_origin_for_opaque_and_blob_schemes() {
    let mut rt = VmJsRuntime::new();
    let global = rt.alloc_object_value().unwrap();
    install_url_bindings(&mut rt, global).unwrap();

    let url = new_url(&mut rt, global, "file:///tmp/x", None);
    let origin = get(&mut rt, url, "origin");
    assert_eq!(as_rust_string(&rt, origin), "null");

    let url = new_url(&mut rt, global, "data:text/plain,hello", None);
    let origin = get(&mut rt, url, "origin");
    assert_eq!(as_rust_string(&rt, origin), "null");

    let url = new_url(&mut rt, global, "blob:https://example.com/uuid", None);
    let origin = get(&mut rt, url, "origin");
    assert_eq!(as_rust_string(&rt, origin), "https://example.com");

    let url = new_url(&mut rt, global, "blob:file:///tmp/x", None);
    let origin = get(&mut rt, url, "origin");
    assert_eq!(as_rust_string(&rt, origin), "null");
  }

  #[test]
  fn searchparams_cached_object_survives_gc() {
    let mut rt = VmJsRuntime::new();
    let global = rt.alloc_object_value().unwrap();
    install_url_bindings(&mut rt, global).unwrap();

    // Root the global + URL object so `collect_garbage()` doesn't sweep them.
    let global_root = rt.heap_mut().add_root(global).unwrap();

    let url = new_url(&mut rt, global, "https://example.com/?a=b", None);
    let url_root = rt.heap_mut().add_root(url).unwrap();

    let search_params_1 = get(&mut rt, url, "searchParams");
    rt.heap_mut().collect_garbage();
    let search_params_2 = get(&mut rt, url, "searchParams");
    assert_eq!(
      search_params_1, search_params_2,
      "URL.searchParams should keep the cached object alive while the URL object is alive"
    );

    rt.heap_mut().remove_root(url_root);
    rt.heap_mut().remove_root(global_root);
  }

  #[test]
  fn searchparams_get_all_returns_array_with_length_semantics() {
    let mut rt = VmJsRuntime::new();
    let global = rt.alloc_object_value().unwrap();
    install_url_bindings(&mut rt, global).unwrap();

    let url = new_url(&mut rt, global, "https://example.com/?a=1&a=2", None);
    let params = get(&mut rt, url, "searchParams");

    let a = str_val(&mut rt, "a");
    let values = call_method(&mut rt, params, "getAll", &[a]);

    let length = get(&mut rt, values, "length");
    assert_eq!(length, Value::Number(2.0));

    // Array exotic objects update `length` when defining an element beyond the current length.
    let idx_key = key(&mut rt, "5");
    let x = str_val(&mut rt, "x");
    rt.define_data_property(values, idx_key, x, true).unwrap();
    let length = get(&mut rt, values, "length");
    assert_eq!(length, Value::Number(6.0));
  }

  #[test]
  fn urlsearchparams_constructor_variants() {
    let mut rt = VmJsRuntime::new();
    let global = rt.alloc_object_value().unwrap();
    install_url_bindings(&mut rt, global).unwrap();

    // sequence<sequence<USVString>> (array-of-pairs)
    let a = str_val(&mut rt, "a");
    let b = str_val(&mut rt, "b");
    let c = str_val(&mut rt, "c");
    let d = str_val(&mut rt, "d");
    let pair1 = array(&mut rt, &[a, b]);
    let pair2 = array(&mut rt, &[c, d]);
    let init = array(&mut rt, &[pair1, pair2]);
    let params = new_url_search_params_value(&mut rt, global, init);
    let s = call_method(&mut rt, params, "toString", &[]);
    assert_eq!(as_rust_string(&rt, s), "a=b&c=d");

    // record<USVString, USVString> (plain object)
    let init = record(&mut rt, &[("a", "b"), ("c", "d")]);
    let params = new_url_search_params_value(&mut rt, global, init);
    let s = call_method(&mut rt, params, "toString", &[]);
    assert_eq!(as_rust_string(&rt, s), "a=b&c=d");

    // iterable (URLSearchParams itself implements @@iterator)
    let original = new_url_search_params(&mut rt, global, Some("a=b&c=d"));
    let params = new_url_search_params_value(&mut rt, global, original);
    let s = call_method(&mut rt, params, "toString", &[]);
    assert_eq!(as_rust_string(&rt, s), "a=b&c=d");
  }

  #[test]
  fn urlsearchparams_size_sort_and_iteration() {
    let mut rt = VmJsRuntime::new();
    let global = rt.alloc_object_value().unwrap();
    install_url_bindings(&mut rt, global).unwrap();

    let params = new_url_search_params(&mut rt, global, Some("b=2&a=1&a=0"));
    let size = get(&mut rt, params, "size");
    assert_eq!(size, Value::Number(3.0));

    // Symbol.iterator should alias `entries`.
    let iter_key = rt.symbol_iterator().unwrap();
    let iter_method = rt.get(params, iter_key).unwrap();
    let entries = get(&mut rt, params, "entries");
    assert_eq!(iter_method, entries);

    // Iterate via the WebIDL iterator hooks (equivalent to `for...of`).
    let mut record = rt.get_iterator_from_method(params, iter_method).unwrap();
    let mut out: Vec<String> = Vec::new();
    while let Some(pair) = rt.iterator_step_value(&mut record).unwrap() {
      let key = get(&mut rt, pair, "0");
      let value = get(&mut rt, pair, "1");
      out.push(format!(
        "{}={}",
        as_rust_string(&rt, key),
        as_rust_string(&rt, value)
      ));
    }
    assert_eq!(out.join("&"), "b=2&a=1&a=0");

    call_method(&mut rt, params, "sort", &[]);
    let sorted = call_method(&mut rt, params, "toString", &[]);
    assert_eq!(as_rust_string(&rt, sorted), "a=1&a=0&b=2");
  }

  #[test]
  fn urlsearchparams_iterators_are_iterable() {
    let mut rt = VmJsRuntime::new();
    let global = rt.alloc_object_value().unwrap();
    install_url_bindings(&mut rt, global).unwrap();

    let params = new_url_search_params(&mut rt, global, Some("a=1&b=2"));
    let iter_key = rt.symbol_iterator().unwrap();

    for method_name in ["entries", "keys", "values"] {
      let iter = call_method(&mut rt, params, method_name, &[]);
      let iter_method = rt.get(iter, iter_key).unwrap();
      assert!(
        rt.is_callable(iter_method),
        "expected URLSearchParams.{method_name}() iterator to have a callable [Symbol.iterator]"
      );
      let returned = call(&mut rt, iter_method, iter, &[]);
      assert_eq!(
        returned, iter,
        "expected URLSearchParams.{method_name}() iterator [Symbol.iterator]() to return itself"
      );
    }
  }

  #[test]
  fn urlsearchparams_iterator_rejects_fractional_internal_state() {
    let mut rt = VmJsRuntime::new();
    let global = rt.alloc_object_value().unwrap();
    install_url_bindings(&mut rt, global).unwrap();

    let params = new_url_search_params(&mut rt, global, Some("a=1&b=2"));

    fn check_throw(rt: &mut VmJsRuntime, iter: Value, prop: &str, value: f64, expected_substr: &str) {
      let iter_root = rt.heap_mut().add_root(iter).unwrap();

      // Mutate the internal iterator state stored on plain properties. This iterator implementation
      // exists because `vm-js` does not have interpreter-backed array iterators yet, so we must be
      // robust against scripts that tamper with these properties.
      let prop_key = key(rt, prop);
      let key_root = match prop_key {
        PropertyKey::String(s) => Some(rt.heap_mut().add_root(Value::String(s)).unwrap()),
        PropertyKey::Symbol(s) => Some(rt.heap_mut().add_root(Value::Symbol(s)).unwrap()),
      };
      rt.define_data_property(iter, prop_key, Value::Number(value), true)
        .unwrap();
      if let Some(id) = key_root {
        rt.heap_mut().remove_root(id);
      }

      let next = get(rt, iter, "next");
      let err = rt.call_function(next, iter, &[]).unwrap_err();
      let msg = match err {
        VmError::Throw(value) | VmError::ThrowWithStack { value, .. } => {
          let value_root = rt.heap_mut().add_root(value).unwrap();
          let s = rt.to_string(value).unwrap();
          rt.heap_mut().remove_root(value_root);
          rt.string_to_utf8_lossy(s).unwrap()
        }
        other => panic!("expected throw, got {other:?}"),
      };
      assert!(
        msg.contains(expected_substr),
        "expected {expected_substr:?} in error message, got {msg:?}"
      );

      rt.heap_mut().remove_root(iter_root);
    }

    let iter = call_method(&mut rt, params, "entries", &[]);
    check_throw(
      &mut rt,
      iter,
      "__fastrender_iter_index",
      0.5,
      "Iterator: invalid index",
    );

    let iter = call_method(&mut rt, params, "entries", &[]);
    check_throw(
      &mut rt,
      iter,
      "__fastrender_iter_len",
      1.5,
      "Iterator: invalid length",
    );
  }

  #[test]
  fn url_instance_initialization_survives_gc_pressure() {
    // Force a GC cycle before essentially every heap allocation to ensure that instance
    // initialization doesn't rely on Rust locals being traced.
    let mut rt = VmJsRuntime::with_limits(HeapLimits::new(1024 * 1024, 0));

    let global = rt.alloc_object_value().unwrap();
    install_url_bindings(&mut rt, global).unwrap();

    // Root values used across further allocations.
    let global_root = rt.heap_mut().add_root(global).unwrap();

    let url = new_url(&mut rt, global, "https://example.com/?x=1#hash", None);
    let url_root = rt.heap_mut().add_root(url).unwrap();

    let href = get(&mut rt, url, "href");
    assert_eq!(as_rust_string(&rt, href), "https://example.com/?x=1#hash");

    let json = call_method(&mut rt, url, "toJSON", &[]);
    assert_eq!(as_rust_string(&rt, json), "https://example.com/?x=1#hash");

    let stringified = call_method(&mut rt, url, "toString", &[]);
    assert_eq!(as_rust_string(&rt, stringified), "https://example.com/?x=1#hash");

    let search_params = get(&mut rt, url, "searchParams");
    let x = str_val(&mut rt, "x");
    // Root arguments across intermediate allocations (e.g. property key creation) so they survive
    // the forced-GC regime.
    let x_root = rt.heap_mut().add_root(x).unwrap();
    let x_value = call_method(&mut rt, search_params, "get", &[x]);
    rt.heap_mut().remove_root(x_root);
    assert_eq!(as_rust_string(&rt, x_value), "1");

    let a = str_val(&mut rt, "a");
    let a_root = rt.heap_mut().add_root(a).unwrap();
    let b = str_val(&mut rt, "b");
    let b_root = rt.heap_mut().add_root(b).unwrap();
    call_method(&mut rt, search_params, "append", &[a, b]);
    rt.heap_mut().remove_root(b_root);
    rt.heap_mut().remove_root(a_root);
    let href = get(&mut rt, url, "href");
    assert_eq!(as_rust_string(&rt, href), "https://example.com/?x=1&a=b#hash");

    rt.heap_mut().remove_root(url_root);
    rt.heap_mut().remove_root(global_root);
  }

  #[test]
  fn url_constructor_enforces_max_input_bytes_while_decoding_utf16() {
    let mut rt = VmJsRuntime::new();
    let global = rt.alloc_object_value().unwrap();
    let mut limits = UrlLimits::default();
    limits.max_input_bytes = 5;
    install_url_bindings_with_limits(&mut rt, global, limits).unwrap();

    // Root global so later allocations (property keys, etc) cannot collect it.
    let global_root = rt.heap_mut().add_root(global).unwrap();

    let url_ctor = get(&mut rt, global, "URL");
    let input = str_val(&mut rt, "ééé"); // 3 UTF-16 code units but 6 UTF-8 bytes.
    let input_root = rt.heap_mut().add_root(input).unwrap();
    let err = rt
      .call_function(url_ctor, Value::Undefined, &[input])
      .expect_err("expected URL() to throw");
    rt.heap_mut().remove_root(input_root);

    let Some(thrown) = err.thrown_value() else {
      panic!("expected thrown TypeError, got {err:?}");
    };
    let thrown_root = rt.heap_mut().add_root(thrown).unwrap();

    let message = get(&mut rt, thrown, "message");
    assert!(
      as_rust_string(&rt, message).contains("URL constructor input exceeded max bytes"),
      "unexpected error message: {}",
      as_rust_string(&rt, message)
    );

    rt.heap_mut().remove_root(thrown_root);
    rt.heap_mut().remove_root(global_root);
  }
}
