use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use fastrender::resource::web_url::{WebUrl, WebUrlError, WebUrlLimits, WebUrlSearchParams};
use rquickjs::{Ctx, Function, Object};

#[derive(Default)]
struct UrlBindingsState {
  limits: WebUrlLimits,
  next_url_handle: u32,
  urls: HashMap<u32, WebUrl>,
  next_sp_handle: u32,
  search_params: HashMap<u32, WebUrlSearchParams>,
}

fn error_to_js(err: &WebUrlError) -> (&'static str, String) {
  match err {
    WebUrlError::LimitExceeded { .. } | WebUrlError::OutOfMemory => ("RangeError", err.to_string()),
    WebUrlError::InvalidUtf8 => ("TypeError", err.to_string()),
    WebUrlError::ParseError
    | WebUrlError::InvalidBase { .. }
    | WebUrlError::Parse { .. }
    | WebUrlError::SetterFailure { .. } => ("TypeError", "Invalid URL".to_string()),
  }
}

fn make_ok<'js>(ctx: Ctx<'js>) -> rquickjs::Result<Object<'js>> {
  let obj = Object::new(ctx)?;
  obj.set("ok", true)?;
  Ok(obj)
}

fn make_ok_value<'js, V>(ctx: Ctx<'js>, value: V) -> rquickjs::Result<Object<'js>>
where
  V: rquickjs::IntoJs<'js>,
{
  let obj = Object::new(ctx)?;
  obj.set("ok", true)?;
  obj.set("value", value)?;
  Ok(obj)
}

fn make_ok_handle<'js>(ctx: Ctx<'js>, handle: u32) -> rquickjs::Result<Object<'js>> {
  let obj = Object::new(ctx)?;
  obj.set("ok", true)?;
  obj.set("handle", handle)?;
  Ok(obj)
}

fn make_err<'js>(ctx: Ctx<'js>, name: &str, message: &str) -> rquickjs::Result<Object<'js>> {
  let obj = Object::new(ctx)?;
  obj.set("ok", false)?;
  obj.set("name", name)?;
  obj.set("message", message)?;
  Ok(obj)
}

fn make_err_from_core<'js>(ctx: Ctx<'js>, err: &WebUrlError) -> rquickjs::Result<Object<'js>> {
  let (name, message) = error_to_js(err);
  make_err(ctx, name, &message)
}

pub fn install_url_bindings<'js>(ctx: Ctx<'js>, globals: &Object<'js>) -> rquickjs::Result<()> {
  let state = Rc::new(RefCell::new(UrlBindingsState::default()));

  // --- URL creation + inspection ---
  globals.set(
    "__fr_url_create",
    Function::new(ctx.clone(), {
      let state = state.clone();
      move |ctx: Ctx<'js>, input: String, base: Option<String>| -> rquickjs::Result<Object<'js>> {
        let mut state = state.borrow_mut();
        match WebUrl::parse_without_diagnostics(&input, base.as_deref(), &state.limits) {
          Ok(url) => {
            let handle = state.next_url_handle;
            state.next_url_handle = state.next_url_handle.wrapping_add(1);
            state.urls.insert(handle, url);
            make_ok_handle(ctx, handle)
          }
          Err(err) => make_err_from_core(ctx, &err),
        }
      }
    })?,
  )?;

  globals.set(
    "__fr_url_can_parse",
    Function::new(ctx.clone(), {
      let state = state.clone();
      move |input: String, base: Option<String>| -> bool {
        let limits = &state.borrow().limits;
        WebUrl::can_parse(&input, base.as_deref(), limits)
      }
    })?,
  )?;

  // URL getters.
  globals.set(
    "__fr_url_get_origin",
    Function::new(ctx.clone(), {
      let state = state.clone();
      move |handle: u32| -> String {
        let state = state.borrow();
        let Some(url) = state.urls.get(&handle) else {
          return String::new();
        };
        url.origin()
      }
    })?,
  )?;

  globals.set(
    "__fr_url_get_href",
    Function::new(ctx.clone(), {
      let state = state.clone();
      move |handle: u32| -> String {
        let state = state.borrow();
        let Some(url) = state.urls.get(&handle) else {
          return String::new();
        };
        url.href().unwrap_or_default()
      }
    })?,
  )?;

  macro_rules! url_getter_no_limits {
    ($name:literal, $method:ident) => {{
      globals.set(
        $name,
        Function::new(ctx.clone(), {
          let state = state.clone();
          move |handle: u32| -> String {
            let state = state.borrow();
            let Some(url) = state.urls.get(&handle) else {
              return String::new();
            };
            url.$method().unwrap_or_default()
          }
        })?,
      )?;
    }};
  }

  url_getter_no_limits!("__fr_url_get_protocol", protocol);
  url_getter_no_limits!("__fr_url_get_username", username);
  url_getter_no_limits!("__fr_url_get_password", password);
  url_getter_no_limits!("__fr_url_get_host", host);
  url_getter_no_limits!("__fr_url_get_hostname", hostname);
  url_getter_no_limits!("__fr_url_get_port", port);
  url_getter_no_limits!("__fr_url_get_pathname", pathname);
  url_getter_no_limits!("__fr_url_get_search", search);
  url_getter_no_limits!("__fr_url_get_hash", hash);

  // URL setters.
  macro_rules! url_setter {
    ($name:literal, $method:ident, $ignore_setter_failure:expr) => {{
      globals.set(
        $name,
        Function::new(ctx.clone(), {
          let state = state.clone();
          move |ctx: Ctx<'js>, handle: u32, value: String| -> rquickjs::Result<Object<'js>> {
            let state = state.borrow();
            let Some(url) = state.urls.get(&handle) else {
              return make_err(ctx, "TypeError", "Invalid URL handle");
            };
            match url.$method(&value) {
              Ok(()) => make_ok(ctx),
              Err(WebUrlError::SetterFailure { .. }) if $ignore_setter_failure => make_ok(ctx),
              Err(err) => make_err_from_core(ctx, &err),
            }
          }
        })?,
      )?;
    }};
  }

  url_setter!("__fr_url_set_href", set_href, false);
  url_setter!("__fr_url_set_protocol", set_protocol, true);
  url_setter!("__fr_url_set_username", set_username, true);
  url_setter!("__fr_url_set_password", set_password, true);
  url_setter!("__fr_url_set_host", set_host, true);
  url_setter!("__fr_url_set_hostname", set_hostname, true);
  url_setter!("__fr_url_set_port", set_port, true);
  url_setter!("__fr_url_set_pathname", set_pathname, true);
  url_setter!("__fr_url_set_search", set_search, true);
  url_setter!("__fr_url_set_hash", set_hash, true);

  // --- URLSearchParams (standalone) ---
  globals.set(
    "__fr_sp_create_from_string",
    Function::new(ctx.clone(), {
      let state = state.clone();
      move |ctx: Ctx<'js>, input: String| -> rquickjs::Result<Object<'js>> {
        let mut state = state.borrow_mut();
        match WebUrlSearchParams::parse(&input, &state.limits) {
          Ok(params) => {
            let handle = state.next_sp_handle;
            state.next_sp_handle = state.next_sp_handle.wrapping_add(1);
            state.search_params.insert(handle, params);
            make_ok_handle(ctx, handle)
          }
          Err(err) => make_err_from_core(ctx, &err),
        }
      }
    })?,
  )?;

  globals.set(
    "__fr_sp_create_from_pairs",
    Function::new(ctx.clone(), {
      let state = state.clone();
      move |ctx: Ctx<'js>, pairs: Vec<Vec<String>>| -> rquickjs::Result<Object<'js>> {
        let mut state = state.borrow_mut();
        // Avoid cloning `pair` entries if we can; `append` will clone into its own storage.
        // This preserves order and duplicates per the URL Standard.
        let params = WebUrlSearchParams::new(&state.limits);
        for pair in pairs {
          if pair.len() != 2 {
            return make_err(ctx, "TypeError", "Invalid URLSearchParams init sequence");
          }
          if let Err(err) = params.append(&pair[0], &pair[1]) {
            return make_err_from_core(ctx, &err);
          }
        }
        let handle = state.next_sp_handle;
        state.next_sp_handle = state.next_sp_handle.wrapping_add(1);
        state.search_params.insert(handle, params);
        make_ok_handle(ctx, handle)
      }
    })?,
  )?;

  globals.set(
    "__fr_sp_append",
    Function::new(ctx.clone(), {
      let state = state.clone();
      move |ctx: Ctx<'js>, handle: u32, name: String, value: String| -> rquickjs::Result<Object<'js>> {
        let state = state.borrow();
        let Some(params) = state.search_params.get(&handle) else {
          return make_err(ctx, "TypeError", "Invalid URLSearchParams handle");
        };
        match params.append(&name, &value) {
          Ok(()) => make_ok(ctx),
          Err(err) => make_err_from_core(ctx, &err),
        }
      }
    })?,
  )?;

  globals.set(
    "__fr_sp_delete",
    Function::new(ctx.clone(), {
      let state = state.clone();
      move |handle: u32, name: String, value: Option<String>| {
        let state = state.borrow();
        let Some(params) = state.search_params.get(&handle) else {
          return;
        };
        let _ = params.delete(&name, value.as_deref());
      }
    })?,
  )?;

  globals.set(
    "__fr_sp_get",
    Function::new(ctx.clone(), {
      let state = state.clone();
      move |handle: u32, name: String| -> Option<String> {
        let state = state.borrow();
        state
          .search_params
          .get(&handle)
          .and_then(|params| params.get(&name).ok().flatten())
      }
    })?,
  )?;

  globals.set(
    "__fr_sp_get_all",
    Function::new(ctx.clone(), {
      let state = state.clone();
      move |handle: u32, name: String| -> Vec<String> {
        let state = state.borrow();
        let Some(params) = state.search_params.get(&handle) else {
          return Vec::new();
        };
        params.get_all(&name).unwrap_or_default()
      }
    })?,
  )?;

  globals.set(
    "__fr_sp_has",
    Function::new(ctx.clone(), {
      let state = state.clone();
      move |handle: u32, name: String, value: Option<String>| -> bool {
        let state = state.borrow();
        let Some(params) = state.search_params.get(&handle) else {
          return false;
        };
        params.has(&name, value.as_deref()).unwrap_or(false)
      }
    })?,
  )?;

  globals.set(
    "__fr_sp_set",
    Function::new(ctx.clone(), {
      let state = state.clone();
      move |ctx: Ctx<'js>, handle: u32, name: String, value: String| -> rquickjs::Result<Object<'js>> {
        let state = state.borrow();
        let Some(params) = state.search_params.get(&handle) else {
          return make_err(ctx, "TypeError", "Invalid URLSearchParams handle");
        };
        match params.set(&name, &value) {
          Ok(()) => make_ok(ctx),
          Err(err) => make_err_from_core(ctx, &err),
        }
      }
    })?,
  )?;

  globals.set(
    "__fr_sp_sort",
    Function::new(ctx.clone(), {
      let state = state.clone();
      move |handle: u32| {
        let state = state.borrow();
        let Some(params) = state.search_params.get(&handle) else {
          return;
        };
        let _ = params.sort();
      }
    })?,
  )?;

  globals.set(
    "__fr_sp_size",
    Function::new(ctx.clone(), {
      let state = state.clone();
      move |handle: u32| -> usize {
        let state = state.borrow();
        state
          .search_params
          .get(&handle)
          .and_then(|p| p.len().ok())
          .unwrap_or(0)
      }
    })?,
  )?;

  globals.set(
    "__fr_sp_to_string",
    Function::new(ctx.clone(), {
      let state = state.clone();
      move |ctx: Ctx<'js>, handle: u32| -> rquickjs::Result<Object<'js>> {
        let state = state.borrow();
        let Some(params) = state.search_params.get(&handle) else {
          return make_ok_value(ctx, String::new());
        };
        match params.serialize() {
          Ok(s) => make_ok_value(ctx, s),
          Err(err) => make_err_from_core(ctx, &err),
        }
      }
    })?,
  )?;

  globals.set(
    "__fr_sp_pairs",
    Function::new(ctx.clone(), {
      let state = state.clone();
      move |handle: u32| -> Vec<Vec<String>> {
        let state = state.borrow();
        let Some(params) = state.search_params.get(&handle) else {
          return Vec::new();
        };
        params
          .pairs()
          .unwrap_or_default()
          .into_iter()
          .map(|(n, v)| vec![n, v])
          .collect()
      }
    })?,
  )?;

  // --- URLSearchParams (associated with a URL) ---
  globals.set(
    "__fr_sp_assoc_append",
    Function::new(ctx.clone(), {
      let state = state.clone();
      move |ctx: Ctx<'js>, url_handle: u32, name: String, value: String| -> rquickjs::Result<Object<'js>> {
        let state = state.borrow();
        let Some(url) = state.urls.get(&url_handle) else {
          return make_err(ctx, "TypeError", "Invalid URL handle");
        };
        let params = url.search_params();
        match params.append(&name, &value) {
          Ok(()) => make_ok(ctx),
          Err(err) => make_err_from_core(ctx, &err),
        }
      }
    })?,
  )?;

  globals.set(
    "__fr_sp_assoc_delete",
    Function::new(ctx.clone(), {
      let state = state.clone();
      move |ctx: Ctx<'js>, url_handle: u32, name: String, value: Option<String>| -> rquickjs::Result<Object<'js>> {
        let state = state.borrow();
        let Some(url) = state.urls.get(&url_handle) else {
          return make_err(ctx, "TypeError", "Invalid URL handle");
        };
        let params = url.search_params();
        match params.delete(&name, value.as_deref()) {
          Ok(()) => make_ok(ctx),
          Err(err) => make_err_from_core(ctx, &err),
        }
      }
    })?,
  )?;

  globals.set(
    "__fr_sp_assoc_get",
    Function::new(ctx.clone(), {
      let state = state.clone();
      move |url_handle: u32, name: String| -> Option<String> {
        let state = state.borrow();
        let url = state.urls.get(&url_handle)?;
        let params = url.search_params();
        params.get(&name).ok().flatten()
      }
    })?,
  )?;

  globals.set(
    "__fr_sp_assoc_get_all",
    Function::new(ctx.clone(), {
      let state = state.clone();
      move |url_handle: u32, name: String| -> Vec<String> {
        let state = state.borrow();
        let Some(url) = state.urls.get(&url_handle) else {
          return Vec::new();
        };
        let params = url.search_params();
        params.get_all(&name).unwrap_or_default()
      }
    })?,
  )?;

  globals.set(
    "__fr_sp_assoc_has",
    Function::new(ctx.clone(), {
      let state = state.clone();
      move |url_handle: u32, name: String, value: Option<String>| -> bool {
        let state = state.borrow();
        let Some(url) = state.urls.get(&url_handle) else {
          return false;
        };
        let params = url.search_params();
        params.has(&name, value.as_deref()).unwrap_or(false)
      }
    })?,
  )?;

  globals.set(
    "__fr_sp_assoc_set",
    Function::new(ctx.clone(), {
      let state = state.clone();
      move |ctx: Ctx<'js>, url_handle: u32, name: String, value: String| -> rquickjs::Result<Object<'js>> {
        let state = state.borrow();
        let Some(url) = state.urls.get(&url_handle) else {
          return make_err(ctx, "TypeError", "Invalid URL handle");
        };
        let params = url.search_params();
        match params.set(&name, &value) {
          Ok(()) => make_ok(ctx),
          Err(err) => make_err_from_core(ctx, &err),
        }
      }
    })?,
  )?;

  globals.set(
    "__fr_sp_assoc_sort",
    Function::new(ctx.clone(), {
      let state = state.clone();
      move |ctx: Ctx<'js>, url_handle: u32| -> rquickjs::Result<Object<'js>> {
        let state = state.borrow();
        let Some(url) = state.urls.get(&url_handle) else {
          return make_err(ctx, "TypeError", "Invalid URL handle");
        };
        let params = url.search_params();
        match params.sort() {
          Ok(()) => make_ok(ctx),
          Err(err) => make_err_from_core(ctx, &err),
        }
      }
    })?,
  )?;

  globals.set(
    "__fr_sp_assoc_size",
    Function::new(ctx.clone(), {
      let state = state.clone();
      move |url_handle: u32| -> usize {
        let state = state.borrow();
        let Some(url) = state.urls.get(&url_handle) else {
          return 0;
        };
        url.search_params().len().unwrap_or(0)
      }
    })?,
  )?;

  globals.set(
    "__fr_sp_assoc_to_string",
    Function::new(ctx.clone(), {
      let state = state.clone();
      move |ctx: Ctx<'js>, url_handle: u32| -> rquickjs::Result<Object<'js>> {
        let state = state.borrow();
        let Some(url) = state.urls.get(&url_handle) else {
          return make_ok_value(ctx, String::new());
        };
        let params = url.search_params();
        match params.serialize() {
          Ok(s) => make_ok_value(ctx, s),
          Err(err) => make_err_from_core(ctx, &err),
        }
      }
    })?,
  )?;

  globals.set(
    "__fr_sp_assoc_pairs",
    Function::new(ctx.clone(), {
      let state = state.clone();
      move |url_handle: u32| -> Vec<Vec<String>> {
        let state = state.borrow();
        let Some(url) = state.urls.get(&url_handle) else {
          return Vec::new();
        };
        url
          .search_params()
          .pairs()
          .unwrap_or_default()
          .into_iter()
          .map(|(n, v)| vec![n, v])
          .collect()
      }
    })?,
  )?;

  // JS glue: define URL/URLSearchParams classes.
  ctx.eval::<(), _>(URL_BINDINGS)?;

  Ok(())
}

const URL_BINDINGS: &str = r#"
(function () {
  var g = typeof globalThis !== "undefined" ? globalThis : this;
  if (typeof g.URL === "function" && typeof g.URLSearchParams === "function") return;

  function throwFromResult(res) {
    var name = res && res.name ? res.name : "Error";
    var msg = res && res.message ? res.message : "";
    var Ctor = g[name] || Error;
    throw new Ctor(msg);
  }

  function assertOk(res) {
    if (!res || res.ok !== true) throwFromResult(res);
    return res;
  }

   class URLSearchParams {
     constructor(init) {
      // Internal: `URL` creates associated params via `_fromUrlHandle`.
      this._fr_assoc = false;
      this._fr_url_handle = 0;
      this._fr_params_handle = 0;

      if (init === undefined) {
        var created = assertOk(__fr_sp_create_from_string(""));
        this._fr_params_handle = created.handle;
        return;
      }

      if (typeof init === "string") {
        // Per spec: strip a single leading `?` from string init.
        if (init.length > 0 && init[0] === "?") init = init.slice(1);
        var created = __fr_sp_create_from_string(init);
        if (!created.ok) throwFromResult(created);
        this._fr_params_handle = created.handle;
        return;
      }

      var pairs = [];
      var it = init != null && init[Symbol.iterator];
      if (typeof it === "function") {
        for (var entry of init) {
          if (entry == null) throw new TypeError("Invalid URLSearchParams init sequence");
          var innerIt = entry[Symbol.iterator];
          if (typeof innerIt !== "function") throw new TypeError("Invalid URLSearchParams init sequence");
          var inner = [];
          for (var item of entry) {
            inner.push(String(item));
            if (inner.length > 2) break;
          }
          if (inner.length !== 2) throw new TypeError("Invalid URLSearchParams init sequence");
          pairs.push([inner[0], inner[1]]);
        }
      } else {
        for (var key of Object.keys(Object(init))) {
          pairs.push([String(key), String(init[key])]);
        }
      }

      var created = __fr_sp_create_from_pairs(pairs);
      if (!created.ok) throwFromResult(created);
      this._fr_params_handle = created.handle;
    }

    static _fromUrlHandle(handle) {
      var obj = Object.create(URLSearchParams.prototype);
      obj._fr_assoc = true;
      obj._fr_url_handle = handle;
      obj._fr_params_handle = 0;
      return obj;
    }

    append(name, value) {
      name = String(name);
      value = String(value);
      var res = this._fr_assoc
        ? __fr_sp_assoc_append(this._fr_url_handle, name, value)
        : __fr_sp_append(this._fr_params_handle, name, value);
      if (res && res.ok === false) throwFromResult(res);
    }

    delete(name, value) {
      name = String(name);
      if (value === undefined) {
        if (this._fr_assoc) {
          var res = __fr_sp_assoc_delete(this._fr_url_handle, name, undefined);
          if (res && res.ok === false) throwFromResult(res);
        } else {
          __fr_sp_delete(this._fr_params_handle, name, undefined);
        }
        return;
      }

      value = String(value);
      if (this._fr_assoc) {
        var res = __fr_sp_assoc_delete(this._fr_url_handle, name, value);
        if (res && res.ok === false) throwFromResult(res);
      } else {
        __fr_sp_delete(this._fr_params_handle, name, value);
      }
    }

    get(name) {
      name = String(name);
      var v = this._fr_assoc ? __fr_sp_assoc_get(this._fr_url_handle, name) : __fr_sp_get(this._fr_params_handle, name);
      if (v === undefined || v === null) return null;
      // Ensure we return a primitive string (not a boxed `String` object).
      return String(v);
    }

    getAll(name) {
      name = String(name);
      return this._fr_assoc ? __fr_sp_assoc_get_all(this._fr_url_handle, name) : __fr_sp_get_all(this._fr_params_handle, name);
    }

    has(name, value) {
      name = String(name);
      if (value === undefined) {
        return this._fr_assoc ? __fr_sp_assoc_has(this._fr_url_handle, name, undefined) : __fr_sp_has(this._fr_params_handle, name, undefined);
      }
      value = String(value);
      return this._fr_assoc ? __fr_sp_assoc_has(this._fr_url_handle, name, value) : __fr_sp_has(this._fr_params_handle, name, value);
    }

    set(name, value) {
      name = String(name);
      value = String(value);
      var res = this._fr_assoc
        ? __fr_sp_assoc_set(this._fr_url_handle, name, value)
        : __fr_sp_set(this._fr_params_handle, name, value);
      if (res && res.ok === false) throwFromResult(res);
    }

    sort() {
      if (this._fr_assoc) {
        var res = __fr_sp_assoc_sort(this._fr_url_handle);
        if (res && res.ok === false) throwFromResult(res);
      } else {
        __fr_sp_sort(this._fr_params_handle);
      }
    }

    get size() {
      return this._fr_assoc ? __fr_sp_assoc_size(this._fr_url_handle) : __fr_sp_size(this._fr_params_handle);
    }

    toString() {
      var res = this._fr_assoc ? __fr_sp_assoc_to_string(this._fr_url_handle) : __fr_sp_to_string(this._fr_params_handle);
      if (res && res.ok === false) throwFromResult(res);
      return res && res.ok ? res.value : "";
    }

    entries() {
      var pairs = this._fr_assoc ? __fr_sp_assoc_pairs(this._fr_url_handle) : __fr_sp_pairs(this._fr_params_handle);
      return pairs[Symbol.iterator]();
    }

    [Symbol.iterator]() {
      return this.entries();
    }
  }

   class URL {
    constructor(url, base) {
      var res = __fr_url_create(String(url), base === undefined ? undefined : String(base));
      if (!res.ok) throwFromResult(res);
      this._fr_url_handle = res.handle;
      Object.defineProperty(this, "searchParams", {
        value: URLSearchParams._fromUrlHandle(res.handle),
        enumerable: true,
      });
    }

    static parse(url, base) {
      var res = __fr_url_create(String(url), base === undefined ? undefined : String(base));
      if (!res.ok) return null;
      var obj = Object.create(URL.prototype);
      obj._fr_url_handle = res.handle;
      Object.defineProperty(obj, "searchParams", {
        value: URLSearchParams._fromUrlHandle(res.handle),
        enumerable: true,
      });
      return obj;
    }

    static canParse(url, base) {
      return __fr_url_can_parse(String(url), base === undefined ? undefined : String(base));
    }

    get href() { return __fr_url_get_href(this._fr_url_handle); }
    set href(v) { var r = __fr_url_set_href(this._fr_url_handle, String(v)); if (!r.ok) throwFromResult(r); }

    get origin() { return __fr_url_get_origin(this._fr_url_handle); }

    get protocol() { return __fr_url_get_protocol(this._fr_url_handle); }
    set protocol(v) { var r = __fr_url_set_protocol(this._fr_url_handle, String(v)); if (!r.ok) throwFromResult(r); }

    get username() { return __fr_url_get_username(this._fr_url_handle); }
    set username(v) { var r = __fr_url_set_username(this._fr_url_handle, String(v)); if (!r.ok) throwFromResult(r); }

    get password() { return __fr_url_get_password(this._fr_url_handle); }
    set password(v) { var r = __fr_url_set_password(this._fr_url_handle, String(v)); if (!r.ok) throwFromResult(r); }

    get host() { return __fr_url_get_host(this._fr_url_handle); }
    set host(v) { var r = __fr_url_set_host(this._fr_url_handle, String(v)); if (!r.ok) throwFromResult(r); }

    get hostname() { return __fr_url_get_hostname(this._fr_url_handle); }
    set hostname(v) { var r = __fr_url_set_hostname(this._fr_url_handle, String(v)); if (!r.ok) throwFromResult(r); }

    get port() { return __fr_url_get_port(this._fr_url_handle); }
    set port(v) { var r = __fr_url_set_port(this._fr_url_handle, String(v)); if (!r.ok) throwFromResult(r); }

    get pathname() { return __fr_url_get_pathname(this._fr_url_handle); }
    set pathname(v) { var r = __fr_url_set_pathname(this._fr_url_handle, String(v)); if (!r.ok) throwFromResult(r); }

    get search() { return __fr_url_get_search(this._fr_url_handle); }
    set search(v) { var r = __fr_url_set_search(this._fr_url_handle, String(v)); if (!r.ok) throwFromResult(r); }

     get hash() { return __fr_url_get_hash(this._fr_url_handle); }
     set hash(v) { var r = __fr_url_set_hash(this._fr_url_handle, String(v)); if (!r.ok) throwFromResult(r); }

     toString() { return this.href; }
     toJSON() { return this.href; }
   }

  g.URLSearchParams = URLSearchParams;
  g.URL = URL;
})();
"#;

#[cfg(test)]
mod tests {
  use super::install_url_bindings;
  use rquickjs::{Context, Runtime};

  fn eval<'js, T: rquickjs::FromJs<'js>>(ctx: rquickjs::Ctx<'js>, src: &str) -> T {
    ctx
      .eval::<T, _>(src)
      .unwrap_or_else(|e| panic!("eval failed: {e:#?} (src={src})"))
  }

  #[test]
  fn resolves_relative_url_with_base() {
    let rt = Runtime::new().unwrap();
    let ctx = Context::full(&rt).unwrap();
    ctx.with(|ctx| {
      let globals = ctx.globals();
      install_url_bindings(ctx.clone(), &globals).unwrap();
      let ok: bool = eval(
        ctx.clone(),
        "new URL('foo','https://example.com/base/').href === 'https://example.com/base/foo'",
      );
      assert!(ok);
    });
  }

  #[test]
  fn searchparams_is_live_and_normalizes_on_mutation() {
    let rt = Runtime::new().unwrap();
    let ctx = Context::full(&rt).unwrap();
    ctx.with(|ctx| {
      let globals = ctx.globals();
      install_url_bindings(ctx.clone(), &globals).unwrap();

      let ok: bool = eval(
        ctx.clone(),
        r#"
        (() => {
          const u = new URL('https://example.com/?a=b%20~');
          if (u.search !== '?a=b%20~') return false;
          if (u.searchParams.get('a') !== 'b ~') return false;
          if (String(u.searchParams) !== 'a=b+%7E') return false;
          u.searchParams.append('c', 'd');
          if (u.search !== '?a=b+%7E&c=d') return false;
          return true;
        })()
      "#,
      );
      assert!(ok);
    });
  }

  #[test]
  fn urlsearchparams_constructor_unions() {
    let rt = Runtime::new().unwrap();
    let ctx = Context::full(&rt).unwrap();
    ctx.with(|ctx| {
      let globals = ctx.globals();
      install_url_bindings(ctx.clone(), &globals).unwrap();

      let ok: bool = eval(
         ctx.clone(),
         r#"
         (() => {
          if (new URLSearchParams('?a=1').toString() !== 'a=1') return false;
          if (new URLSearchParams('a=1&a=2').toString() !== 'a=1&a=2') return false;
          if (new URLSearchParams({a:'1', b:'2'}).toString() !== 'a=1&b=2') return false;
          if (new URLSearchParams([['a','1'],['a','2'],['b','3']]).toString() !== 'a=1&a=2&b=3') return false;
          if (new URLSearchParams(new Map([['a','1'],['b','2']])).toString() !== 'a=1&b=2') return false;
          let threw = false;
          try {
            new URLSearchParams([['a','1','extra']]);
          } catch (e) {
            threw = e instanceof TypeError;
          }
          if (!threw) return false;
          return true;
         })()
      "#,
       );
      assert!(ok);
    });
  }

  #[test]
  fn urlsearchparams_encoding_spaces_and_plus() {
    let rt = Runtime::new().unwrap();
    let ctx = Context::full(&rt).unwrap();
    ctx.with(|ctx| {
      let globals = ctx.globals();
      install_url_bindings(ctx.clone(), &globals).unwrap();

      let ok: bool = eval(
        ctx.clone(),
        "const p = new URLSearchParams('a=1+2&b=3%2B4'); p.set('a','x y'); p.append('c','1+2'); p.toString() === 'a=x+y&b=3%2B4&c=1%2B2'",
      );
      assert!(ok);
    });
  }

  #[test]
  fn limit_exceeded_throws_range_error() {
    let rt = Runtime::new().unwrap();
    let ctx = Context::full(&rt).unwrap();
    ctx.with(|ctx| {
      let globals = ctx.globals();
      install_url_bindings(ctx.clone(), &globals).unwrap();

      // Exceed the default `WebUrlLimits.max_input_bytes` (1 MiB).
      let threw: bool = eval(
        ctx.clone(),
        r#"
        const s = 'a'.repeat(1024 * 1024 + 1);
        let ok = false;
        try {
          new URLSearchParams(s);
        } catch (e) {
          ok = e instanceof RangeError;
        }
        ok
      "#,
      );
      assert!(threw);
    });
  }

  #[test]
  fn url_setters_ignore_invalid_values() {
    let rt = Runtime::new().unwrap();
    let ctx = Context::full(&rt).unwrap();
    ctx.with(|ctx| {
      let globals = ctx.globals();
      install_url_bindings(ctx.clone(), &globals).unwrap();

      // Per WHATWG URL, invalid setters should be a no-op (and must not throw).
      let ok: bool = eval(
        ctx.clone(),
        r#"
        (() => {
          const u = new URL('https://example.com/path?x=1#y');
          const beforeHref = u.href;

          let threw = false;
          try {
            u.protocol = 'ht!tp:'; // invalid scheme
            u.port = '99999'; // invalid port
          } catch (e) {
            threw = true;
          }

          if (threw) return false;
          if (u.href !== beforeHref) return false;
          if (u.protocol !== 'https:') return false;
          if (u.port !== '') return false;
          return true;
        })()
      "#,
      );
      assert!(ok);
    });
  }
}
