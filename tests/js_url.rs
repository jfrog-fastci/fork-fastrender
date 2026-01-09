use fastrender::js::{
  install_url_bindings,
  webidl::{JsRuntime as _, VmJsRuntime},
};
use vm_js::{PropertyKey, Value};
use webidl_js_runtime::runtime::JsPropertyKind;

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
  let key = key(rt, name);
  let desc = rt
    .get_own_property(obj, key)
    .unwrap()
    .unwrap_or_else(|| panic!("missing own property {name}"));
  let JsPropertyKind::Accessor { set, .. } = desc.kind else {
    panic!("{name} is not an accessor property");
  };
  call(rt, set, obj, &[value]);
}

fn new_url(rt: &mut VmJsRuntime, global: Value, input: &str, base: Option<&str>) -> Value {
  let url_ctor = get(rt, global, "URL");
  let mut args = vec![str_val(rt, input)];
  if let Some(base) = base {
    args.push(str_val(rt, base));
  }
  call(rt, url_ctor, Value::Undefined, &args)
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
