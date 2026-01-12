use vm_js::{Heap, HeapLimits, JsRuntime, RootId, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn object_destructuring_binds_properties() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"var {a,b} = {a:1,b:2}; a+b === 3"#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn object_destructuring_supports_renaming() {
  let mut rt = new_runtime();
  let value = rt.exec_script(r#"var {a:x} = {a:5}; x === 5"#).unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn object_destructuring_supports_defaults() {
  let mut rt = new_runtime();
  let value = rt.exec_script(r#"var {a=1} = {}; a === 1"#).unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn object_destructuring_supports_rest() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"var {a,...r} = {a:1,b:2}; r.b === 2 && r.a === undefined"#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn object_destructuring_rest_object_has_object_prototype() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"var {a,...r} = {a:1,b:2}; r instanceof Object && Object.getPrototypeOf(r) === Object.prototype"#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn array_destructuring_binds_elements_and_holes() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"var [x,,y] = [1,2,3]; x===1 && y===3"#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn array_destructuring_supports_defaults() {
  let mut rt = new_runtime();
  let value = rt.exec_script(r#"var [x=1] = []; x===1"#).unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn array_destructuring_supports_rest() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"var [x,...r] = [1,2,3]; r.length===2"#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn object_destructuring_assignment_binds_properties() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"var a; var b; ({a,b} = {a:1,b:2}); a+b === 3"#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn object_destructuring_assignment_can_assign_to_member() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"var o = {}; ({a:o.x} = {a:1}); o.x === 1"#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn object_destructuring_assignment_member_on_primitive_is_silent_in_sloppy_mode() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"var s = "abc"; ({a: s.x} = {a: 1}); true"#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn object_destructuring_assignment_member_on_primitive_throws_in_strict_mode() {
  let mut rt = new_runtime();
  let err = rt
    .exec_script(r#""use strict"; var s = "abc"; ({a: s.x} = {a: 1});"#)
    .unwrap_err();
  let thrown = err
    .thrown_value()
    .unwrap_or_else(|| panic!("expected thrown exception, got {err:?}"));

  // Root the thrown value across any subsequent allocations / script runs.
  let root: RootId = rt
    .heap_mut()
    .add_root(thrown)
    .expect("root thrown value");

  let Value::Object(thrown_obj) = thrown else {
    panic!("expected thrown value to be an object, got {thrown:?}");
  };

  let type_error_proto = rt
    .exec_script("globalThis.TypeError.prototype")
    .expect("evaluate TypeError.prototype");
  let Value::Object(type_error_proto) = type_error_proto else {
    panic!("expected TypeError.prototype to be an object");
  };

  let thrown_proto = rt
    .heap()
    .object_prototype(thrown_obj)
    .expect("get thrown prototype");
  assert_eq!(thrown_proto, Some(type_error_proto));

  rt.heap_mut().remove_root(root);
}

#[test]
fn array_destructuring_assignment_supports_holes_and_rest() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"var x; var y; var r; ([x,,y,...r] = [1,2,3,4,5]); x===1 && y===3 && r.length===2 && r[0]===4"#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn catch_param_supports_destructuring() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"try { throw {x:1}; } catch({x}) { x }"#)
    .unwrap();
  assert_eq!(value, Value::Number(1.0));
}

#[test]
fn object_destructuring_string_key_preserves_unpaired_surrogate() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"var o = {"\uD800": 1}; var {"\uD800": x} = o; x"#)
    .unwrap();
  assert_eq!(value, Value::Number(1.0));
}

#[test]
fn object_destructuring_uses_getv_receiver_for_accessors_on_primitives() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        Object.defineProperty(Number.prototype, "x", {
          configurable: true,
          get: function() {
            'use strict';
            return typeof this;
          },
        });
        var key = "x";
        var {x, [key]: y} = 1;
        x === "number" && y === "number"
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
