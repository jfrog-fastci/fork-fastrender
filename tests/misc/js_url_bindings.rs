use fastrender::js::{
  install_url_bindings, install_url_bindings_with_limits, webidl::VmJsRuntime, UrlLimits,
  WindowRealm, WindowRealmConfig,
};
use vm_js::{PropertyKey, Value, VmError};
use webidl_js_runtime::runtime::JsPropertyKind;
use webidl_js_runtime::JsRuntime as _;

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
  // Root receiver/value so intermediate allocations (e.g. property key creation) cannot GC them.
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

#[test]
fn url_constructor_getters_and_setters() {
  let mut rt = VmJsRuntime::new();
  let global = rt.alloc_object_value().unwrap();
  install_url_bindings(&mut rt, global).unwrap();

  let url_ctor = get(&mut rt, global, "URL");
  let arg = str_val(&mut rt, "https://example.com/path?x=1#y");
  let url = call(&mut rt, url_ctor, Value::Undefined, &[arg]);

  let href = get(&mut rt, url, "href");
  assert_eq!(as_rust_string(&rt, href), "https://example.com/path?x=1#y");
  let origin = get(&mut rt, url, "origin");
  assert_eq!(as_rust_string(&rt, origin), "https://example.com");
  let protocol = get(&mut rt, url, "protocol");
  assert_eq!(as_rust_string(&rt, protocol), "https:");
  let host = get(&mut rt, url, "host");
  assert_eq!(as_rust_string(&rt, host), "example.com");
  let hostname = get(&mut rt, url, "hostname");
  assert_eq!(as_rust_string(&rt, hostname), "example.com");
  let port = get(&mut rt, url, "port");
  assert_eq!(as_rust_string(&rt, port), "");
  let pathname = get(&mut rt, url, "pathname");
  assert_eq!(as_rust_string(&rt, pathname), "/path");
  let search = get(&mut rt, url, "search");
  assert_eq!(as_rust_string(&rt, search), "?x=1");
  let hash = get(&mut rt, url, "hash");
  assert_eq!(as_rust_string(&rt, hash), "#y");

  let new_search = str_val(&mut rt, "?q=a+b");
  set_accessor(&mut rt, url, "search", new_search);
  let search = get(&mut rt, url, "search");
  assert_eq!(as_rust_string(&rt, search), "?q=a+b");
}

#[test]
fn url_searchparams_is_live() {
  let mut rt = VmJsRuntime::new();
  let global = rt.alloc_object_value().unwrap();
  install_url_bindings(&mut rt, global).unwrap();

  let url_ctor = get(&mut rt, global, "URL");
  let arg = str_val(&mut rt, "https://example.com/?a=b%20~");
  let url = call(&mut rt, url_ctor, Value::Undefined, &[arg]);

  let search = get(&mut rt, url, "search");
  assert_eq!(as_rust_string(&rt, search), "?a=b%20~");

  let params = get(&mut rt, url, "searchParams");

  let a = str_val(&mut rt, "a");
  let got = call_method(&mut rt, params, "get", &[a]);
  assert_eq!(as_rust_string(&rt, got), "b ~");

  let s = call_method(&mut rt, params, "toString", &[]);
  assert_eq!(as_rust_string(&rt, s), "a=b+%7E");
  let search = get(&mut rt, url, "search");
  assert_eq!(as_rust_string(&rt, search), "?a=b%20~");

  let c = str_val(&mut rt, "c");
  let d = str_val(&mut rt, "d");
  call_method(&mut rt, params, "append", &[c, d]);

  let href = get(&mut rt, url, "href");
  assert_eq!(as_rust_string(&rt, href), "https://example.com/?a=b+%7E&c=d");
  let search = get(&mut rt, url, "search");
  assert_eq!(as_rust_string(&rt, search), "?a=b+%7E&c=d");
  let s = call_method(&mut rt, params, "toString", &[]);
  assert_eq!(as_rust_string(&rt, s), "a=b+%7E&c=d");
}

#[test]
fn boundedness_throws_type_error() {
  let mut rt = VmJsRuntime::new();
  let global = rt.alloc_object_value().unwrap();

  let mut limits = UrlLimits::default();
  limits.max_input_bytes = 8;
  limits.max_query_pairs = 4;
  limits.max_total_query_bytes = 16;
  install_url_bindings_with_limits(&mut rt, global, limits).unwrap();

  let url_ctor = get(&mut rt, global, "URL");
  let arg = str_val(&mut rt, "https://example.com/");
  let err = rt
    .call_function(url_ctor, Value::Undefined, &[arg])
    .unwrap_err();

  let Some(thrown) = err.thrown_value() else {
    panic!("expected Throw, got {err:?}");
  };

  let name = get(&mut rt, thrown, "name");
  assert_eq!(as_rust_string(&rt, name), "TypeError");

  let message = get(&mut rt, thrown, "message");
  assert!(
    as_rust_string(&rt, message).contains("URL constructor input exceeded max bytes"),
    "unexpected error message: {}",
    as_rust_string(&rt, message)
  );
}

#[test]
fn urlsearchparams_pair_limit_is_enforced() {
  let mut rt = VmJsRuntime::new();
  let global = rt.alloc_object_value().unwrap();

  let mut limits = UrlLimits::default();
  limits.max_input_bytes = 1024;
  limits.max_query_pairs = 1;
  limits.max_total_query_bytes = 1024;
  install_url_bindings_with_limits(&mut rt, global, limits).unwrap();

  let url_ctor = get(&mut rt, global, "URL");
  let arg = str_val(&mut rt, "https://example.com/");
  let url = call(&mut rt, url_ctor, Value::Undefined, &[arg]);
  let params = get(&mut rt, url, "searchParams");

  let a = str_val(&mut rt, "a");
  let one = str_val(&mut rt, "1");
  call_method(&mut rt, params, "append", &[a, one]);

  let b = str_val(&mut rt, "b");
  let two = str_val(&mut rt, "2");
  let append = get(&mut rt, params, "append");
  let err = rt.call_function(append, params, &[b, two]).unwrap_err();

  let Some(thrown) = err.thrown_value() else {
    panic!("expected Throw");
  };
  let name = get(&mut rt, thrown, "name");
  assert_eq!(as_rust_string(&rt, name), "TypeError");
}

#[test]
fn urlsearchparams_to_string_enforces_output_limit() {
  let mut rt = VmJsRuntime::new();
  let global = rt.alloc_object_value().unwrap();

  let mut limits = UrlLimits::default();
  limits.max_input_bytes = 3; // "a=~" fits, but "a=%7E" does not.
  install_url_bindings_with_limits(&mut rt, global, limits).unwrap();

  let params_ctor = get(&mut rt, global, "URLSearchParams");
  let init = str_val(&mut rt, "a=~");
  let params = call(&mut rt, params_ctor, Value::Undefined, &[init]);

  let to_string = get(&mut rt, params, "toString");
  let err = rt.call_function(to_string, params, &[]).unwrap_err();
  let Some(thrown) = err.thrown_value() else {
    panic!("expected Throw");
  };
  let name = get(&mut rt, thrown, "name");
  assert_eq!(as_rust_string(&rt, name), "TypeError");
}

fn as_vm_js_heap_string(heap: &vm_js::Heap, v: Value) -> String {
  let Value::String(s) = v else {
    panic!("expected string, got {v:?}");
  };
  heap.get_string(s).unwrap().to_utf8_lossy()
}

fn vm_js_error_debug(realm: &mut WindowRealm, err: VmError) -> String {
  let VmError::Throw(thrown) = err else {
    return format!("{err:?}");
  };
  let Value::Object(obj) = thrown else {
    return format!("throw {thrown:?}");
  };

  let heap = realm.heap_mut();
  let mut scope = heap.scope();
  scope.push_root(thrown).unwrap();

  let name_key = PropertyKey::from_string(scope.alloc_string("name").unwrap());
  let message_key = PropertyKey::from_string(scope.alloc_string("message").unwrap());
  let stack_key = PropertyKey::from_string(scope.alloc_string("stack").unwrap());

  let name = scope
    .heap()
    .object_get_own_data_property_value(obj, &name_key)
    .ok()
    .flatten()
    .map(|v| as_vm_js_heap_string(scope.heap(), v))
    .unwrap_or_else(|| "<unknown>".to_string());
  let message = scope
    .heap()
    .object_get_own_data_property_value(obj, &message_key)
    .ok()
    .flatten()
    .map(|v| as_vm_js_heap_string(scope.heap(), v))
    .unwrap_or_else(|| "<unknown>".to_string());
  let stack = scope
    .heap()
    .object_get_own_data_property_value(obj, &stack_key)
    .ok()
    .flatten()
    .and_then(|v| {
      if matches!(v, Value::String(_)) {
        Some(as_vm_js_heap_string(scope.heap(), v))
      } else {
        None
      }
    })
    .unwrap_or_else(|| "<no stack>".to_string());

  format!("{name}: {message}\n{stack}")
}

#[test]
fn window_realm_exec_script_url_constructor_smoke() {
  let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/")).unwrap();

  let href = realm
    .exec_script(r#"new URL("https://example.com/path?x=1#y").href"#)
    .unwrap_or_else(|err| panic!("exec_script failed:\n{}", vm_js_error_debug(&mut realm, err)));
  assert_eq!(
    as_vm_js_heap_string(realm.heap(), href),
    "https://example.com/path?x=1#y"
  );

  // Base resolution with a URL object exercises object-to-string coercion fallback logic.
  let resolved = realm
    .exec_script(
      r#"
        var base = new URL("https://example.com/dir/");
        new URL("a", base).href
      "#,
    )
    .unwrap();
  assert_eq!(
    as_vm_js_heap_string(realm.heap(), resolved),
    "https://example.com/dir/a"
  );
}

#[test]
fn window_realm_exec_script_url_searchparams_is_live_and_cached() {
  let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/")).unwrap();

  let href = realm
    .exec_script(
      r#"
        globalThis.u = new URL("https://example.com/?a=b%20~");
        globalThis.u.searchParams.append("c", "d");
        globalThis.u.href
      "#,
    )
    .unwrap();
  assert_eq!(
    as_vm_js_heap_string(realm.heap(), href),
    "https://example.com/?a=b+%7E&c=d"
  );

  let cached = realm
    .exec_script(r#"globalThis.u.searchParams === globalThis.u.searchParams"#)
    .unwrap();
  assert_eq!(cached, Value::Bool(true));

  let params = realm
    .exec_script(r#"globalThis.u.searchParams.toString()"#)
    .unwrap();
  assert_eq!(as_vm_js_heap_string(realm.heap(), params), "a=b+%7E&c=d");
}
