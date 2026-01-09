use fastrender::js::{install_url_bindings, install_url_bindings_with_limits, webidl::VmJsRuntime, UrlLimits};
use vm_js::{HeapLimits, PropertyKey, Value, VmError};
use webidl_js_runtime::{JsRuntime as _, WebIdlJsRuntime as _};
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
  assert_eq!(as_rust_string(&rt, href), "http://user:pass@example.org:8080/a/b");
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
  assert_eq!(
    as_rust_string(&rt, stringified),
    "https://example.com/?x=1#hash"
  );

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

  let VmError::Throw(thrown) = err else {
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
