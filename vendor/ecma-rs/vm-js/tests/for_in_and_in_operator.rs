use vm_js::{
  GcObject, Heap, HeapLimits, JsRuntime, Scope, Value, Vm, VmError, VmHost, VmHostHooks, VmOptions,
};

fn test_uncatchable_error(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(VmError::InvariantViolation("test invariant violation"))
}

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn assert_value_is_utf8(rt: &JsRuntime, value: Value, expected: &str) {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  let actual = rt.heap().get_string(s).unwrap().to_utf8_lossy();
  assert_eq!(actual, expected);
}

#[test]
fn for_in_over_own_enumerable_props() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"var o={a:1,b:2}; var s=''; for (var k in o) { s = s + k; } s"#)
    .unwrap();
  assert_value_is_utf8(&rt, value, "ab");
}

#[test]
fn for_in_includes_prototype_enumerable_props() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"var p={x:1}; var o=Object.create(p); o.y=2; var s=''; for (var k in o) { s = s + k; } s"#,
    )
    .unwrap();
  assert_value_is_utf8(&rt, value, "yx");
}

#[test]
fn for_in_null_does_not_throw() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"try { for (var k in null) {} "no"; } catch(e) { e.name }"#)
    .unwrap();
  assert_value_is_utf8(&rt, value, "no");
}

#[test]
fn for_in_undefined_does_not_throw() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"try { for (var k in undefined) {} "no"; } catch(e) { e.name }"#)
    .unwrap();
  assert_value_is_utf8(&rt, value, "no");
}

#[test]
fn for_in_skips_deleted_keys() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var o = {a:1,b:2,c:3};
      var s = "";
      for (var k in o) {
        s += k;
        if (k === "a") { delete o.b; }
      }
      s
      "#,
    )
    .unwrap();
  assert_value_is_utf8(&rt, value, "ac");
}

#[test]
fn for_in_deleted_key_allows_prototype_key() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var p = {b:1};
      var o = Object.create(p);
      o.a = 1;
      o.b = 2;
      o.c = 3;
      var s = "";
      for (var k in o) {
        s += k;
        if (k === "a") { delete o.b; }
      }
      s
      "#,
    )
    .unwrap();
  assert_value_is_utf8(&rt, value, "acb");
}

#[test]
fn for_in_non_enumerable_own_property_shadows_prototype() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var p = {x:1};
      var o = Object.create(p);
      Object.defineProperty(o, "x", { value: 2, enumerable: false });
      var s = "";
      for (var k in o) { s += k; }
      s
      "#,
    )
    .unwrap();
  assert_value_is_utf8(&rt, value, "");
}

#[test]
fn in_operator_walks_prototype_chain() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"var p={x:1}; var o=Object.create(p); o.y=2; ('x' in o) && !('z' in o)"#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn in_operator_treats_string_indices_as_properties() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"'0' in new String("abc") && !('3' in new String("abc"))"#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn for_in_restores_lexical_env_on_uncatchable_error() {
  let mut rt = new_runtime();

  rt.register_global_native_function("__test_uncatchable_error", test_uncatchable_error, 0)
    .unwrap();
  let err = rt
    // Trigger an uncatchable VM error inside the loop body so we can assert the loop restores its
    // per-iteration lexical environment before unwinding.
    .exec_script(r#"for (let k in {a:1}) { __test_uncatchable_error(); }"#)
    .unwrap_err();
  assert!(matches!(err, VmError::InvariantViolation(_)));

  // If the loop's per-iteration lexical environment is not restored when the body returns an
  // uncatchable error, the loop variable binding would leak into subsequent script executions.
  let value = rt
    .exec_script(r#"try { k; "leaked" } catch(e) { "ok" }"#)
    .unwrap();
  assert_value_is_utf8(&rt, value, "ok");
}

#[test]
fn for_in_over_typed_array_skips_prototype_numeric_keys() {
  let mut rt = new_runtime();

  // Prototype numeric index keys should be ignored when the typed array does not have a valid
  // integer index (consistent with the `in` operator and TypedArray `[[HasProperty]]` semantics).
  let value = rt
    .exec_script(
      r#"
      Uint8Array.prototype['0']=7;
      Uint8Array.prototype['-0']=7;
      Uint8Array.prototype['1.5']=7;
      Uint8Array.prototype['4294967295']=7;
      var s='';
      for (var k in new Uint8Array(0)) { s+=k; }
      delete Uint8Array.prototype['0'];
      delete Uint8Array.prototype['-0'];
      delete Uint8Array.prototype['1.5'];
      delete Uint8Array.prototype['4294967295'];
      s
      "#,
    )
    .unwrap();
  assert_value_is_utf8(&rt, value, "");

  // Non-numeric prototype keys are still enumerable.
  let value = rt
    .exec_script(
      r#"
      Uint8Array.prototype.foo=1;
      var s='';
      for (var k in new Uint8Array(0)) { s+=k; }
      delete Uint8Array.prototype.foo;
      s
      "#,
    )
    .unwrap();
  assert_value_is_utf8(&rt, value, "foo");
}

#[test]
fn for_in_over_detached_typed_array_skips_indices_but_keeps_non_numeric_prototype_keys() {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
    Uint8Array.prototype['0']=7;
    Uint8Array.prototype.foo=1;
    u = new Uint8Array(2);
    "#,
  )
  .unwrap();

  let buffer = match rt.exec_script("u.buffer").unwrap() {
    Value::Object(o) => o,
    other => panic!("expected ArrayBuffer object, got {other:?}"),
  };

  // Detach from Rust to simulate transfer/structured-clone behaviour.
  {
    let (_vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    heap.detach_array_buffer(buffer).unwrap();
  }

  let value = rt
    .exec_script(
      r#"
      var s='';
      for (var k in u) { s+=k; }
      delete Uint8Array.prototype['0'];
      delete Uint8Array.prototype.foo;
      s
      "#,
    )
    .unwrap();
  assert_value_is_utf8(&rt, value, "foo");
}

#[test]
fn in_operator_on_typed_array_skips_prototype_numeric_keys() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      Uint8Array.prototype['0']=7;
      Uint8Array.prototype['1.5']=7;
      Uint8Array.prototype.foo=1;

      var u = new Uint8Array(0);
      var ok = !('0' in u) && !('1.5' in u) && ('foo' in u);

      delete Uint8Array.prototype['0'];
      delete Uint8Array.prototype['1.5'];
      delete Uint8Array.prototype.foo;
      ok
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
