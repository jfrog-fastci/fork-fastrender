use fastrender::js::{WindowRealm, WindowRealmConfig};
use vm_js::{Job, PropertyKey, RealmId, Value, VmError, VmHostHooks};

#[derive(Default)]
struct NoopHostHooks;

impl VmHostHooks for NoopHostHooks {
  fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}
}

fn exec_script(realm: &mut WindowRealm, source: &str) -> std::result::Result<Value, VmError> {
  let mut host_ctx = ();
  let mut hooks = NoopHostHooks::default();
  realm.exec_script_with_host_and_hooks(&mut host_ctx, &mut hooks, source)
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

  let href = exec_script(
    &mut realm,
    r#"new URL("https://example.com/path?x=1#y").href"#,
  )
  .unwrap_or_else(|err| {
    panic!(
      "exec_script failed:\n{}",
      vm_js_error_debug(&mut realm, err)
    )
  });
  assert_eq!(
    as_vm_js_heap_string(realm.heap(), href),
    "https://example.com/path?x=1#y"
  );

  // Base resolution with a URL object exercises object-to-string coercion fallback logic.
  let resolved = exec_script(
    &mut realm,
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

  let origin = exec_script(
    &mut realm,
    r#"new URL("https://example.com/path?x=1#y").origin"#,
  )
  .unwrap();
  assert_eq!(
    as_vm_js_heap_string(realm.heap(), origin),
    "https://example.com"
  );

  // Opaque origins serialize as the string "null" (WHATWG URL).
  let opaque_origin = exec_script(&mut realm, r#"new URL("data:text/plain,hi").origin"#).unwrap();
  assert_eq!(as_vm_js_heap_string(realm.heap(), opaque_origin), "null");
}

#[test]
fn window_realm_exec_script_url_constructors_require_new() {
  let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/")).unwrap();

  let typeof_url = exec_script(&mut realm, r#"typeof URL === "function""#).unwrap();
  assert_eq!(typeof_url, Value::Bool(true));

  let url_proto = exec_script(
    &mut realm,
    r#"URL.prototype !== null && typeof URL.prototype === "object""#,
  )
  .unwrap();
  assert_eq!(url_proto, Value::Bool(true));

  let url_call_throws = exec_script(
    &mut realm,
    r#"
        (function () {
          try {
            URL("https://example.com");
            return false;
          } catch (e) {
            return e instanceof TypeError && e.message === "Illegal constructor";
          }
        })()
      "#,
  )
  .unwrap();
  assert_eq!(url_call_throws, Value::Bool(true));

  let url_new_works = exec_script(
    &mut realm,
    r#"
        (function () {
          const u = new URL("https://example.com");
          return typeof u === "object" && u !== null;
        })()
      "#,
  )
  .unwrap();
  assert_eq!(url_new_works, Value::Bool(true));

  let typeof_sp = exec_script(&mut realm, r#"typeof URLSearchParams === "function""#).unwrap();
  assert_eq!(typeof_sp, Value::Bool(true));

  let sp_proto = exec_script(
    &mut realm,
    r#"URLSearchParams.prototype !== null && typeof URLSearchParams.prototype === "object""#,
  )
  .unwrap();
  assert_eq!(sp_proto, Value::Bool(true));

  let sp_call_throws = exec_script(
    &mut realm,
    r#"
        (function () {
          try {
            URLSearchParams("a=b");
            return false;
          } catch (e) {
            return e instanceof TypeError && e.message === "Illegal constructor";
          }
        })()
      "#,
  )
  .unwrap();
  assert_eq!(sp_call_throws, Value::Bool(true));

  let sp_new_works = exec_script(
    &mut realm,
    r#"
        (function () {
          const p = new URLSearchParams("a=b");
          return typeof p === "object" && p !== null;
        })()
      "#,
  )
  .unwrap();
  assert_eq!(sp_new_works, Value::Bool(true));
}

#[test]
fn window_realm_exec_script_url_searchparams_is_live_and_cached() {
  let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/")).unwrap();

  let href = exec_script(
    &mut realm,
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

  let cached = exec_script(
    &mut realm,
    r#"globalThis.u.searchParams === globalThis.u.searchParams"#,
  )
  .unwrap();
  assert_eq!(cached, Value::Bool(true));

  let params = exec_script(&mut realm, r#"globalThis.u.searchParams.toString()"#).unwrap();
  assert_eq!(as_vm_js_heap_string(realm.heap(), params), "a=b+%7E&c=d");
}

#[test]
fn window_realm_exec_script_url_origin_for_opaque_and_blob_schemes() {
  let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/")).unwrap();

  let origin = exec_script(&mut realm, r#"new URL("file:///tmp/x").origin"#).unwrap();
  assert_eq!(as_vm_js_heap_string(realm.heap(), origin), "null");

  let origin = exec_script(&mut realm, r#"new URL("data:text/plain,hello").origin"#).unwrap();
  assert_eq!(as_vm_js_heap_string(realm.heap(), origin), "null");

  let origin = exec_script(
    &mut realm,
    r#"new URL("blob:https://example.com/uuid").origin"#,
  )
  .unwrap();
  assert_eq!(
    as_vm_js_heap_string(realm.heap(), origin),
    "https://example.com"
  );

  let origin = exec_script(&mut realm, r#"new URL("blob:file:///tmp/x").origin"#).unwrap();
  assert_eq!(as_vm_js_heap_string(realm.heap(), origin), "null");
}
