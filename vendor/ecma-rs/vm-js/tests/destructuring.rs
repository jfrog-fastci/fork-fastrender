use vm_js::{
  GcObject, Heap, HeapLimits, JsRuntime, PropertyDescriptor, PropertyKey, PropertyKind, RootId,
  Scope, Value, Vm, VmError, VmOptions,
};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Destructuring tests allocate a variety of iterator scaffolding (functions/objects/symbol keys).
  // As vm-js grows, a 1MiB heap can be too tight and lead to spurious `VmError::OutOfMemory`
  // failures unrelated to destructuring semantics.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
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
fn destructuring_assignment_infers_anonymous_function_name_for_member_target() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          var o = {};
          ({ a: o.m } = { a: (0, function () {}) });
          return o.m.name === "m";
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn destructuring_assignment_infers_anonymous_function_name_for_computed_member_target() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          var o = {};
          ({ a: o["k"] } = { a: (0, function () {}) });
          return o.k.name === "k";
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn destructuring_assignment_to_private_member_assigns_private_field() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          class C {
            #x = 0;
            m() {
              ({ a: this.#x } = { a: 7 });
              return this.#x;
            }
          }
          return new C().m() === 7;
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn destructuring_assignment_rest_to_private_member_assigns_object() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          class C {
            #o = null;
            m() {
              ({ ...this.#o } = { a: 1, b: 2 });
              return this.#o.a === 1 && this.#o.b === 2;
            }
          }
          return new C().m();
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn array_destructuring_assignment_to_private_member_assigns_private_field() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          class C {
            #x = 0;
            m() {
              [this.#x] = [9];
              return this.#x;
            }
          }
          return new C().m() === 9;
        })()
      "#,
    )
    .unwrap();
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
fn object_destructuring_rest_uses_proxy_own_keys_trap() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var log = [];
      var target = { a: 1, b: 2 };
      var handler = {
        ownKeys: function (t) {
          log.push("ownKeys");
          return ["a", "b"];
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
        var { a, ...r } = p;
        return a === 1 && r.b === 2 && r.a === undefined && log.join(",").includes("ownKeys");
      })()
    "#,
  )?;
  assert_eq!(ok, Value::Bool(true));
  Ok(())
}

#[test]
fn proxy_own_keys_trap_keys_are_rooted_across_gc() -> Result<(), VmError> {
  let mut rt = new_runtime();
  rt.exec_script(
    r#"
      var target = {};
      var handler = {
        ownKeys: function (_t) {
          // Return freshly-allocated keys that are not stored anywhere else.
          return [Symbol(), Symbol(), Symbol()];
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

  let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
  let mut scope = heap.scope();
  scope.push_root(Value::Object(target))?;
  scope.push_root(Value::Object(handler))?;
  let proxy = scope.alloc_proxy(Some(target), Some(handler))?;

  let mut host = ();
  let mut hooks = std::mem::take(vm.microtask_queue_mut());
  let keys = scope.object_own_property_keys_with_host_and_hooks(vm, &mut host, &mut hooks, proxy)?;
  *vm.microtask_queue_mut() = hooks;

  // Without rooting, these symbol keys would be collected and become invalid handles.
  scope.heap_mut().collect_garbage();
  for key in keys {
    let PropertyKey::Symbol(sym) = key else {
      panic!("expected Symbol key, got {key:?}");
    };
    scope.heap().get_symbol_id(sym)?;
  }

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
fn array_destructuring_elision_does_not_access_iterator_value() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var valueGets = 0;
      var returnCalls = 0;
      var iter = {};
      iter[Symbol.iterator] = function () {
        var i = 0;
        return {
          next() {
            i++;
            const v = i;
            return {
              get value() { valueGets++; return v; },
              done: false,
            };
          },
          return() { returnCalls++; return {}; },
        };
      };
      var [,x] = iter;
      valueGets === 1 && x === 2 && returnCalls === 1
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn array_destructuring_does_not_close_iterator_when_iterator_value_throws() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var returnCalls = 0;
      var err = "";
      var iter = {};
      iter[Symbol.iterator] = function () {
        return {
          next() {
            return {
              get value() { throw "boom"; },
              done: false,
            };
          },
          return() { returnCalls++; return {}; },
        };
      };
      try { var [x] = iter; } catch (e) { err = e; }
      err === "boom" && returnCalls === 0
      "#,
    )
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
fn object_destructuring_assignment_identifier_target_evaluation_order() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var log = [];

      var source = {
        get p() {
          log.push("get");
          return 0;
        },
      };

      var env = new Proxy({}, {
        has(_t, pk) {
          log.push("binding::" + pk);
        },
      });

      // Spec: identifier target reference (ResolveBinding/HasBinding) is evaluated before GetV.
      with (env) {
        ({p: x} = source);
      }

      log.join(",")
      "#,
    )
    .unwrap();
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  let log = rt.heap().get_string(s).unwrap().to_utf8_lossy();
  assert_eq!(log, "binding::source,binding::x,get");
}

#[test]
fn object_destructuring_assignment_rest_identifier_target_evaluation_order() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var log = [];
      var vals = {
        get a() {
          log.push("get");
          return 1;
        },
      };

      var env = new Proxy({}, {
        has(_t, pk) {
          // Avoid `"" + Symbol()` which throws.
          log.push("binding::" + String(pk));
        },
      });

      var rest;
      with (env) {
        ({...rest} = vals);
      }

      log.join(",")
      "#,
    )
    .unwrap();
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  let log = rt.heap().get_string(s).unwrap().to_utf8_lossy();
  assert_eq!(log, "binding::vals,binding::rest,get");
}

#[test]
fn object_destructuring_assignment_rest_property_reference_evaluation_order() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var log = [];

      function source() {
        log.push("source");
        return {
          get a() {
            log.push("get");
            return 1;
          },
        };
      }

      function target() {
        log.push("target");
        return {
          set q(v) {
            log.push("set");
          },
        };
      }

      function targetKey() {
        log.push("target-key");
        return {
          toString: function() {
            log.push("target-key-tostring");
            return "q";
          },
        };
      }

      ({...target()[targetKey()]} = source());

      log.join(",")
      "#,
    )
    .unwrap();
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  let log = rt.heap().get_string(s).unwrap().to_utf8_lossy();
  assert_eq!(log, "source,target,target-key,get,target-key-tostring,set");
}

#[test]
fn object_destructuring_assignment_property_reference_evaluation_order() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var log = [];

      function source() {
        log.push("source");
        return {
          get p() {
            log.push("get");
            return 0;
          },
        };
      }

      function target() {
        log.push("target");
        return {
          set q(v) {
            log.push("set");
          },
        };
      }

      function sourceKey() {
        log.push("source-key");
        return {
          toString: function() {
            log.push("source-key-tostring");
            return "p";
          },
        };
      }

      function targetKey() {
        log.push("target-key");
        return {
          toString: function() {
            log.push("target-key-tostring");
            return "q";
          },
        };
      }

      ({[sourceKey()]: target()[targetKey()]} = source());

      log.join(",") ===
        "source,source-key,source-key-tostring,target,target-key,get,target-key-tostring,set"
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn object_destructuring_assignment_property_reference_evaluation_order_with_bindings() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var log = [];

      var targetKey = {
        toString: () => {
          log.push("targetKey");
          return "q";
        }
      };

      var sourceKey = {
        toString: () => {
          log.push("sourceKey");
          return "p";
        }
      };

      var source = {
        get p() {
          log.push("get source");
          return undefined;
        }
      };

      var target = {
        set q(v) {
          log.push("set target");
        },
      };

      var env = new Proxy({}, {
        has(t, pk) {
          log.push("binding::" + pk);
        }
      });

      var defaultValue = 0;

      with (env) {
        ({
          [sourceKey]: target[targetKey] = defaultValue
        } = source);
      }

      log.join(",") ===
        "binding::source,binding::sourceKey,sourceKey,binding::target,binding::targetKey,get source,binding::defaultValue,targetKey,set target"
      "#,
    )
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
fn array_destructuring_early_completion_closes_iterator() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var done = 0;
      var iter = {};
      iter[Symbol["iterator"]] = function () {
        return {
          next: function () { return { value: 1, done: false }; },
          "return": function () { done++; return {}; }
        };
      };
      var [x] = iter;
      done;
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Number(1.0));
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
      var target = {};
      function boom() { throw new Error("boom"); }
      try {
        ([target[boom()]] = iterable);
      } catch (e) {}
      returnCount === 1 && nextCount === 0
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn array_destructuring_assignment_rest_lhs_abrupt_closes_iterator() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var nextCount = 0;
      var returnCount = 0;
      var iterable = {};
      iterable[Symbol.iterator] = function() {
        return {
          next() { nextCount++; return { done: false }; },
          return() { returnCount++; return {}; },
        };
      };
      function throwlhs() { throw "in lhs"; }
      try {
        0, [...{}[throwlhs()]] = iterable;
      } catch (e) {}
      returnCount === 1 && nextCount === 0
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn array_destructuring_assignment_property_reference_evaluation_order() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var log = [];

      function source() {
        log.push("source");
        var iterator = {
          next: function() {
            log.push("iterator-step");
            return {
              get done() {
                log.push("iterator-done");
                return true;
              },
              get value() {
                // This getter should not be called when `done` is true.
                log.push("iterator-value");
                return 0;
              },
            };
          },
        };
        var src = {};
        src[Symbol.iterator] = function() {
          log.push("iterator");
          return iterator;
        };
        return src;
      }

      function target() {
        log.push("target");
        return {
          set q(v) {
            log.push("set");
          },
        };
      }

      function targetKey() {
        log.push("target-key");
        return {
          toString: function() {
            log.push("target-key-tostring");
            return "q";
          },
        };
      }

      // Spec (test262): `DestructuringAssignmentTarget` evaluation happens before `IteratorStep`,
      // but computed-key conversion (`ToPropertyKey`, via `toString`) is delayed until `PutValue`.
      ([target()[targetKey()]] = source());

      log.join(",") ===
        "source,iterator,target,target-key,iterator-step,iterator-done,target-key-tostring,set"
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn array_destructuring_assignment_rest_property_reference_evaluation_order() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var log = [];

      function source() {
        log.push("source");
        var iterator = {
          next: function() {
            log.push("iterator-step");
            return {
              get done() {
                log.push("iterator-done");
                return true;
              },
              get value() {
                // This getter should not be called when `done` is true.
                log.push("iterator-value");
                return 0;
              },
            };
          },
        };
        var src = {};
        src[Symbol.iterator] = function() {
          log.push("iterator");
          return iterator;
        };
        return src;
      }

      function target() {
        log.push("target");
        return {
          set q(v) {
            log.push("set");
          },
        };
      }

      function targetKey() {
        log.push("target-key");
        return {
          toString: function() {
            log.push("target-key-tostring");
            return "q";
          },
        };
      }

      // Like normal element targets, the computed key's `ToPropertyKey` conversion is delayed
      // until the final `PutValue` after the rest array is built.
      ([...target()[targetKey()]] = source());

      log.join(",")
      "#,
    )
    .unwrap();
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  let log = rt.heap().get_string(s).unwrap().to_utf8_lossy();
  assert_eq!(
    log,
    "source,iterator,target,target-key,iterator-step,iterator-done,target-key-tostring,set"
  );
}

#[test]
fn array_destructuring_assignment_identifier_target_evaluation_order() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var log = [];
      var iterable = {};
      iterable[Symbol.iterator] = function() {
        log.push("iterator");
        return {
          next() {
            log.push("iterator-step");
            return {
              get done() {
                log.push("iterator-done");
                return true;
              },
              get value() {
                // This getter should not be called when `done` is true.
                log.push("iterator-value");
                return 0;
              },
            };
          },
        };
      };

      var env = new Proxy({}, {
        has(_t, pk) {
          log.push("binding::" + pk);
        },
      });

      // Spec: identifier target reference (ResolveBinding/HasBinding) is evaluated before IteratorStep.
      with (env) {
        ([x] = iterable);
      }

      log.join(",")
      "#,
    )
    .unwrap();
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  let log = rt.heap().get_string(s).unwrap().to_utf8_lossy();
  assert_eq!(
    log,
    "binding::iterable,iterator,binding::x,iterator-step,iterator-done"
  );
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

#[test]
fn array_destructuring_uses_proxy_get_trap_for_length_and_index() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let global = rt.realm().global_object();

  rt.exec_script(
    r#"
      var log = [];
      var target = ["x"];
      var handler = {
        get: function (t, k, r) {
          log.push(String(k));
          return t[k];
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
        var x;
        ([x] = p);
        return x === "x"
          && log.join(",").includes("length")
          && log.join(",").includes("0");
      })()
    "#,
  )?;
  assert_eq!(ok, Value::Bool(true));
  Ok(())
}

#[test]
fn array_destructuring_accepts_computed_symbol_iterator_method() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      (() => {
        // Regression test: object literal methods with computed keys (`[Symbol.iterator]`) are
        // reparsed lazily by slicing their source span. The span must include the leading `[` so the
        // method can be parsed/executed.
        //
        // Upstream test262 coverage: `staging/sm/destructuring/iterator-primitive.js`.
        // Keep the test small to avoid OOM in the unit-test heap limits.
        var obj = {
          [Symbol.iterator]() {
            return 1;
          },
        };
        try {
          var [] = obj;
          return false;
        } catch (e) {
          if (!(e instanceof TypeError)) return false;
        }
        try {
          [] = obj;
          return false;
        } catch (e) {
          if (!(e instanceof TypeError)) return false;
        }
        return true;
      })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
