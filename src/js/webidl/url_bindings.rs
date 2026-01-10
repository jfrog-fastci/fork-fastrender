use std::cell::RefCell;
use std::char::decode_utf16;
use std::collections::HashMap;
use std::rc::Rc;
use crate::js::url::{Url, UrlError, UrlLimits, UrlSearchParams};
use webidl_js_runtime::{JsRuntime as _, WebIdlJsRuntime as _};
use vm_js::{GcObject, PropertyDescriptor, PropertyKey, PropertyKind, RootId, Value, VmError};
use webidl_js_runtime::runtime::JsPropertyKind;

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
      &format!(
        "{context} exceeded max bytes (len_code_units={code_units_len}, limit={max_bytes})"
      ),
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
      if !index.is_finite() || index < 0.0 || index > u32::MAX as f64 {
        return Err(type_error(rt, "Iterator: invalid index"));
      }
      let idx_u32: u32 = index as u32;
      let idx_usize = idx_u32 as usize;

      let len_key = rt.property_key_from_str("__fastrender_iter_len")?;
      let len = rt.get(Value::Object(obj), len_key)?;
      let Value::Number(len) = len else {
        return Err(type_error(rt, "Iterator: invalid length"));
      };
      if !len.is_finite() || len < 0.0 || len > u32::MAX as f64 {
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

    Ok(iter)
  })();

  for id in roots {
    rt.heap_mut().remove_root(id);
  }
  result
}

fn expect_object(rt: &mut webidl_js_runtime::VmJsRuntime, this: Value, class_name: &str) -> Result<GcObject, VmError> {
  let Value::Object(obj) = this else {
    return Err(type_error(
      rt,
      &format!("{class_name}: illegal invocation (this is not an object)"),
    ));
  };
  Ok(obj)
}

fn array_like_length(rt: &mut webidl_js_runtime::VmJsRuntime, value: Value) -> Result<Option<u32>, VmError> {
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
        let href = url
          .href()
          .map_err(|e| type_error(rt, &e.to_string()))?;
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
        let protocol = url
          .protocol()
          .map_err(|e| type_error(rt, &e.to_string()))?;
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
        let username = url
          .username()
          .map_err(|e| type_error(rt, &e.to_string()))?;
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
        let password = url
          .password()
          .map_err(|e| type_error(rt, &e.to_string()))?;
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
        let hostname = url
          .hostname()
          .map_err(|e| type_error(rt, &e.to_string()))?;
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
        let pathname = url
          .pathname()
          .map_err(|e| type_error(rt, &e.to_string()))?;
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
        let value = params.get(&name).map_err(|e| type_error(rt, &e.to_string()))?;
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
          return Err(type_error(rt, "URLSearchParams.forEach callback is not a function"));
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
          rt.call_function(callback, this_arg, &[value_value, name_value, Value::Object(obj)])?;
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
              let href = base_url.href().map_err(|e| type_error(rt, &e.to_string()))?;
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

        state.borrow_mut().urls.insert(obj_handle, UrlInstance { url });

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
              let href = base_url.href().map_err(|e| type_error(rt, &e.to_string()))?;
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
          Some(v) => Some(to_rust_string_limited(rt, v, limits.max_input_bytes, "URL.parse base")?),
        };

        let url = match Url::parse_without_diagnostics(&input, base.as_deref(), &limits) {
          Ok(url) => url,
          Err(_) => return Ok(Value::Null),
        };

        let obj = rt.alloc_object_value()?;
        let Value::Object(obj_handle) = obj else {
          return Err(type_error(rt, "URL: expected object"));
        };

        state.borrow_mut().urls.insert(obj_handle, UrlInstance { url });
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
              let href = base_url.href().map_err(|e| type_error(rt, &e.to_string()))?;
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

        Ok(Value::Bool(Url::can_parse(&input, base.as_deref(), &limits)))
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
                      return Err(type_error(rt, "URLSearchParams init: expected a [name, value] pair"));
                    };

                    let length_key = rt.property_key_from_str("length")?;
                    let len = rt.get(Value::Object(pair_obj), length_key)?;
                    if len != Value::Number(2.0) {
                      return Err(type_error(rt, "URLSearchParams init: expected a [name, value] pair"));
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
                  return Err(type_error(rt, "URLSearchParams init: expected a [name, value] pair"));
                };

                let length_key = rt.property_key_from_str("length")?;
                let len = rt.get(Value::Object(pair_obj), length_key)?;
                if len != Value::Number(2.0) {
                  return Err(type_error(rt, "URLSearchParams init: expected a [name, value] pair"));
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
