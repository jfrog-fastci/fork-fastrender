use vm_js::{
  GcObject, Heap, HeapLimits, JsRuntime, PropertyDescriptor, PropertyKey, PropertyKind, RootId,
  Scope, Value, Vm, VmError, VmOptions,
};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn global_var_desc(value: Value) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: true,
    configurable: true,
    kind: PropertyKind::Data {
      value,
      writable: true,
    },
  }
}

fn define_global(
  scope: &mut Scope<'_>,
  global: GcObject,
  name: &str,
  value: Value,
) -> Result<(), VmError> {
  scope.push_root(Value::Object(global))?;
  scope.push_root(value)?;
  let key_s = scope.alloc_string(name)?;
  scope.push_root(Value::String(key_s))?;
  let key = PropertyKey::from_string(key_s);
  scope.define_property(global, key, global_var_desc(value))
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
fn object_destructuring_uses_proxy_get_trap_for_property_binding() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var log = [];
      var target = {};
      var handler = {
        get: function (t, k, r) {
          log.push(String(k));
          if (k === "x") return 1;
        },
      };
    "#,
  )?;

  let target = match rt.exec_script("target")? {
    Value::Object(o) => o,
    other => panic!("expected target object, got {other:?}"),
  };
  let handler = match rt.exec_script("handler")? {
    Value::Object(o) => o,
    other => panic!("expected handler object, got {other:?}"),
  };

  let global = rt.realm().global_object();
  {
    let (_vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    scope.push_root(Value::Object(target))?;
    scope.push_root(Value::Object(handler))?;
    let proxy = scope.alloc_proxy(Some(target), Some(handler))?;
    define_global(&mut scope, global, "p", Value::Object(proxy))?;
  }

  let ok = rt.exec_script(
    r#"
      (() => {
        var { x } = p;
        return x === 1 && log.join(",").includes("x");
      })()
    "#,
  )?;
  assert_eq!(ok, Value::Bool(true));
  Ok(())
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
fn object_destructuring_accepts_number_primitives() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"var {toString:f} = 1; typeof f === "function""#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn array_destructuring_uses_iterator_protocol_for_strings() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"var [a,b,c] = "a\uD834\uDF06b"; a==="a" && b==="\uD834\uDF06" && c==="b""#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn object_destructuring_accepts_string_primitives() {
  let mut rt = new_runtime();
  let value = rt.exec_script(r#"var {length:l} = "ab"; l === 2"#).unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn object_destructuring_throws_for_null() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"try { var {x} = null; false } catch(e) { e instanceof TypeError }"#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn array_destructuring_non_iterable_array_like_throws() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"try { var [x] = {0:1,length:1}; false } catch(e) { e instanceof TypeError }"#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn array_destructuring_rest_produces_real_array() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"var [x,...r] = "ab"; Array.isArray(r) && r.length===1 && r[0]==="b""#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn array_destructuring_abrupt_completion_closes_iterator() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var closed = false;
      var iterable = {};
      iterable[Symbol.iterator] = function() {
        var i = 0;
        return {
          next: function() {
            i++;
            if (i === 1) return { value: "a", done: false };
            if (i === 2) return { value: undefined, done: false };
            return { value: "c", done: false };
          },
          return: function() { closed = true; return {}; }
        };
      };
      try {
        var [x, y = (function(){ throw 1; })()] = iterable;
      } catch (e) {}
      closed === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
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

#[test]
fn array_destructuring_calls_iterator_return_on_empty_pattern() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var nextCount = 0;
      var returnCount = 0;
      var iterable = {};
      iterable[Symbol.iterator] = function() {
        return {
          next() { nextCount++; return { done: true }; },
          return() { returnCount++; return {}; },
        };
      };
      var [] = iterable;
      returnCount === 1 && nextCount === 0
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn array_destructuring_calls_iterator_return_when_not_done() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var nextCount = 0;
      var returnCount = 0;
      var iterable = {};
      iterable[Symbol.iterator] = function() {
        var i = 0;
        return {
          next() { nextCount++; return { value: i++, done: false }; },
          return() { returnCount++; return {}; },
        };
      };
      var [a,b] = iterable;
      a === 0 && b === 1 && nextCount === 2 && returnCount === 1
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn array_destructuring_assignment_closes_iterator_on_lhs_abrupt() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var returnCount = 0;
      var iterable = {};
      iterable[Symbol.iterator] = function() {
        var i = 0;
        return {
          next() { return { value: i++, done: false }; },
          return() { returnCount++; return {}; },
        };
      };
      var target = {};
      function boom() { throw new Error("boom"); }
      try {
        ([target[boom()]] = iterable);
      } catch (e) {}
      returnCount === 1
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn array_destructuring_does_not_close_iterator_when_next_throws() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var nextCount = 0;
      var returnCount = 0;
      var iterable = {};
      iterable[Symbol.iterator] = function() {
        return {
          next() { nextCount++; throw new Error("boom"); },
          return() { returnCount++; return {}; },
        };
      };
      try { var [a] = iterable; } catch (e) {}
      nextCount === 1 && returnCount === 0
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn array_destructuring_iterator_return_must_return_object() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var iterable = {};
      iterable[Symbol.iterator] = function() {
        return {
          next() { return { done: false, value: 1 }; },
          return() { return 123; },
        };
      };
      try {
        var [] = iterable;
        false;
      } catch (e) {
        e instanceof TypeError
      }
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
