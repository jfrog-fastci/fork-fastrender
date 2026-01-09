use crate::js::url::{Url, UrlLimits, UrlSearchParams};
use crate::js::webidl::{JsRuntime as _, WebIdlJsRuntime as _};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use vm_js::{GcObject, PropertyDescriptor, PropertyKey, PropertyKind, Value, VmError};

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

fn to_rust_string(rt: &mut webidl_js_runtime::VmJsRuntime, value: Value) -> Result<String, VmError> {
  let v = rt.to_string(value)?;
  let Value::String(s) = v else {
    return Err(type_error(rt, "ToString did not return a string"));
  };
  Ok(rt.heap().get_string(s)?.to_utf8_lossy())
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

fn expect_object(rt: &mut webidl_js_runtime::VmJsRuntime, this: Value, class_name: &str) -> Result<GcObject, VmError> {
  let Value::Object(obj) = this else {
    return Err(type_error(
      rt,
      &format!("{class_name}: illegal invocation (this is not an object)"),
    ));
  };
  Ok(obj)
}

fn init_url_instance(
  rt: &mut webidl_js_runtime::VmJsRuntime,
  state: Rc<RefCell<UrlBindingState>>,
  obj: Value,
) -> Result<(), VmError> {
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

  let href_set = rt.alloc_function_value({
    let state = state.clone();
    move |rt, this, args| {
      let obj = expect_object(rt, this, "URL")?;
      let value = args.get(0).copied().unwrap_or(Value::Undefined);
      let value = to_rust_string(rt, value)?;
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

  let search_set = rt.alloc_function_value({
    let state = state.clone();
    move |rt, this, args| {
      let obj = expect_object(rt, this, "URL")?;
      let value = args.get(0).copied().unwrap_or(Value::Undefined);
      let value = to_rust_string(rt, value)?;
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

  let hash_set = rt.alloc_function_value({
    let state = state.clone();
    move |rt, this, args| {
      let obj = expect_object(rt, this, "URL")?;
      let value = args.get(0).copied().unwrap_or(Value::Undefined);
      let value = to_rust_string(rt, value)?;
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
      init_urlsearchparams_instance(rt, state.clone(), params_obj, params)?;

      // Define the internal cache slot on the URL instance (non-enumerable, non-writable,
      // non-configurable).
      //
      // Note: allocate a fresh key here rather than reusing `slot_key` so we never hold a
      // non-rooted string handle across allocations that could trigger GC.
      let slot_key = rt.property_key_from_str("__fastrender_url_searchParams")?;
      let Value::Object(params_handle) = params_obj else {
        return Err(type_error(rt, "URLSearchParams: expected object"));
      };
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
      )?;
      Ok(params_obj)
    }
  })?;

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

  define_accessor(rt, obj, "href", href_get, href_set)?;
  define_accessor(rt, obj, "origin", origin_get, Value::Undefined)?;
  define_accessor(rt, obj, "protocol", protocol_get, Value::Undefined)?;
  define_accessor(rt, obj, "host", host_get, Value::Undefined)?;
  define_accessor(rt, obj, "hostname", hostname_get, Value::Undefined)?;
  define_accessor(rt, obj, "port", port_get, Value::Undefined)?;
  define_accessor(rt, obj, "pathname", pathname_get, Value::Undefined)?;
  define_accessor(rt, obj, "search", search_get, search_set)?;
  define_accessor(rt, obj, "hash", hash_get, hash_set)?;
  define_accessor(rt, obj, "searchParams", search_params_get, Value::Undefined)?;
  define_method(rt, obj, "toJSON", to_json)?;

  Ok(())
}

fn init_urlsearchparams_instance(
  rt: &mut webidl_js_runtime::VmJsRuntime,
  state: Rc<RefCell<UrlBindingState>>,
  obj: Value,
  params: UrlSearchParams,
) -> Result<(), VmError> {
  let Value::Object(obj_handle) = obj else {
    return Err(type_error(rt, "URLSearchParams: expected object"));
  };
  state.borrow_mut().search_params.insert(obj_handle, params);

  let append = rt.alloc_function_value({
    let state = state.clone();
    move |rt, this, args| {
      let obj = expect_object(rt, this, "URLSearchParams")?;
      let name = to_rust_string(rt, args.get(0).copied().unwrap_or(Value::Undefined))?;
      let value = to_rust_string(rt, args.get(1).copied().unwrap_or(Value::Undefined))?;
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

  let delete = rt.alloc_function_value({
    let state = state.clone();
    move |rt, this, args| {
      let obj = expect_object(rt, this, "URLSearchParams")?;
      let name = to_rust_string(rt, args.get(0).copied().unwrap_or(Value::Undefined))?;
      let value_arg = args.get(1).copied();
      let value = match value_arg {
        None | Some(Value::Undefined) => None,
        Some(v) => Some(to_rust_string(rt, v)?),
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

  let get = rt.alloc_function_value({
    let state = state.clone();
    move |rt, this, args| {
      let obj = expect_object(rt, this, "URLSearchParams")?;
      let name = to_rust_string(rt, args.get(0).copied().unwrap_or(Value::Undefined))?;
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

  let get_all = rt.alloc_function_value({
    let state = state.clone();
    move |rt, this, args| {
      let obj = expect_object(rt, this, "URLSearchParams")?;
      let name = to_rust_string(rt, args.get(0).copied().unwrap_or(Value::Undefined))?;
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
        let key = rt.property_key_from_u32(idx as u32)?;
        let value = rt.alloc_string_value(value)?;
        rt.define_data_property(arr, key, value, true)?;
      }
      Ok(arr)
    }
  })?;

  let has = rt.alloc_function_value({
    let state = state.clone();
    move |rt, this, args| {
      let obj = expect_object(rt, this, "URLSearchParams")?;
      let name = to_rust_string(rt, args.get(0).copied().unwrap_or(Value::Undefined))?;
      let value_arg = args.get(1).copied();
      let value = match value_arg {
        None | Some(Value::Undefined) => None,
        Some(v) => Some(to_rust_string(rt, v)?),
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

  let set = rt.alloc_function_value({
    let state = state.clone();
    move |rt, this, args| {
      let obj = expect_object(rt, this, "URLSearchParams")?;
      let name = to_rust_string(rt, args.get(0).copied().unwrap_or(Value::Undefined))?;
      let value = to_rust_string(rt, args.get(1).copied().unwrap_or(Value::Undefined))?;
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

  define_method(rt, obj, "append", append)?;
  define_method(rt, obj, "delete", delete)?;
  define_method(rt, obj, "get", get)?;
  define_method(rt, obj, "getAll", get_all)?;
  define_method(rt, obj, "has", has)?;
  define_method(rt, obj, "set", set)?;
  define_method(rt, obj, "toString", to_string)?;

  Ok(())
}

/// Install WHATWG-shaped `URL` and `URLSearchParams` constructors onto `global`.
///
/// The bindings are backed by the deterministic Rust primitives in [`crate::js::url`].
pub fn install_url_bindings(
  rt: &mut webidl_js_runtime::VmJsRuntime,
  global: Value,
) -> Result<(), VmError> {
  let state: Rc<RefCell<UrlBindingState>> = Rc::new(RefCell::new(UrlBindingState::default()));

  let url_ctor = rt.alloc_function_value({
    let state = state.clone();
    move |rt, _this, args| {
      let input = to_rust_string(rt, args.get(0).copied().unwrap_or(Value::Undefined))?;
      let base_value = args.get(1).copied();
      let base = match base_value {
        None | Some(Value::Undefined) => None,
        Some(v) => Some(to_rust_string(rt, v)?),
      };

      let limits = { state.borrow().limits.clone() };
      let url = Url::parse(&input, base.as_deref(), &limits)
        .map_err(|e| type_error(rt, &e.to_string()))?;
      let obj = rt.alloc_object_value()?;
      let Value::Object(obj_handle) = obj else {
        return Err(type_error(rt, "URL: expected object"));
      };

      state.borrow_mut().urls.insert(
        obj_handle,
        UrlInstance {
          url,
        },
      );

      init_url_instance(rt, state.clone(), obj)?;
      Ok(obj)
    }
  })?;

  let url_search_params_ctor = rt.alloc_function_value({
    let state = state.clone();
    move |rt, _this, args| {
      let init = args.get(0).copied();
      let limits = { state.borrow().limits.clone() };
      let params = match init {
        None | Some(Value::Undefined) => UrlSearchParams::new(&limits),
        Some(v) => {
          let init = to_rust_string(rt, v)?;
          UrlSearchParams::parse(&init, &limits)
            .map_err(|e| type_error(rt, &e.to_string()))?
        }
      };
      let obj = rt.alloc_object_value()?;
      init_urlsearchparams_instance(rt, state.clone(), obj, params)?;
      Ok(obj)
    }
  })?;

  let url_key = to_property_key(rt, "URL")?;
  rt.define_data_property(global, url_key, url_ctor, false)?;
  let usp_key = to_property_key(rt, "URLSearchParams")?;
  rt.define_data_property(
    global,
    usp_key,
    url_search_params_ctor,
    false,
  )?;

  Ok(())
}
