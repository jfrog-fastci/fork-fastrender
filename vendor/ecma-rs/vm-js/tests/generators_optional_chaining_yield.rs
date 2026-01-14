use vm_js::{CompiledScript, Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn exec_compiled(rt: &mut JsRuntime, source: &str) -> Result<Value, VmError> {
  let script =
    CompiledScript::compile_script(rt.heap_mut(), "<generators_optional_chaining_yield>", source)?;
  assert!(
    !script.requires_ast_fallback,
    "script unexpectedly requires AST fallback"
  );
  rt.exec_compiled_script(script)
}

#[test]
fn generator_optional_chain_computed_member_propagates_short_circuit_and_skips_key() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        var side = 0;
        var r = (yield 0)?.x[(side++, "toString")];
        return r === undefined && side === 0;
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(null);
      r1.value === 0 && r1.done === false &&
      r2.value === true && r2.done === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_optional_chain_computed_member_propagates_and_skips_yield_in_key_after_base_short_circuit() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        var r = (yield 0)?.x[(yield "should-not-yield")];
        return r === undefined;
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(null);
      r1.value === 0 && r1.done === false &&
      r2.value === true && r2.done === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_optional_chain_call_computed_member_propagates_short_circuit_and_skips_key_and_args() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        var side = 0;
        var arg_side = 0;
        function arg() { arg_side++; return 0; }

        var r = (yield 0)?.x[(side++, "toString")](arg());
        return r === undefined && side === 0 && arg_side === 0;
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(null);
      r1.value === 0 && r1.done === false &&
      r2.value === true && r2.done === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_optional_chain_call_computed_member_propagates_and_skips_yield_in_key_and_args_after_base_short_circuit() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        var r = (yield 0)?.x[(yield "should-not-yield-key")](yield "should-not-yield-arg");
        return r === undefined;
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(null);
      r1.value === 0 && r1.done === false &&
      r2.value === true && r2.done === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_optional_chain_call_propagates_short_circuit_and_skips_args() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        var side = 0;
        function arg() { side++; return 0; }

        var r = (yield 0)?.(arg());
        return r === undefined && side === 0;
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(null);
      r1.value === 0 && r1.done === false &&
      r2.value === true && r2.done === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_optional_chain_call_propagates_and_skips_yield_in_arg_after_base_short_circuit() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        var r = (yield 0)?.(yield "should-not-yield-arg");
        return r === undefined;
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(null);
      r1.value === 0 && r1.done === false &&
      r2.value === true && r2.done === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_optional_chain_nullish_base_skips_yield_in_call_arg() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        var r = null?.(yield "should-not-yield-arg");
        return r === undefined;
      }
      var it = g();
      var r = it.next();
      r.value === true && r.done === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_optional_chain_member_call_propagates_and_skips_yield_in_arg_after_base_short_circuit() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        var r = (yield 0)?.x(yield "should-not-yield-arg");
        return r === undefined;
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(null);
      r1.value === 0 && r1.done === false &&
      r2.value === true && r2.done === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_optional_chain_computed_member_does_not_evaluate_key_when_base_is_nullish() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        var side = 0;
        var r = (yield 0)?.[(side++, "toString")];
        return r === undefined && side === 0;
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(null);
      r1.value === 0 && r1.done === false &&
      r2.value === true && r2.done === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_optional_chain_nullish_base_skips_yield_in_computed_key() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        var r = null?.[(yield "should-not-yield")];
        return r === undefined;
      }
      var it = g();
      var r = it.next();
      r.value === true && r.done === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_optional_chain_nullish_base_skips_yield_in_computed_key_and_args() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        var r = null?.x[(yield "should-not-yield-key")](yield "should-not-yield-arg");
        return r === undefined;
      }
      var it = g();
      var r = it.next();
      r.value === true && r.done === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_parenthesized_optional_chain_does_not_propagate_into_following_member_access() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        try {
          return ((yield null)?.x).y;
        } catch (e) {
          return e.name;
        }
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(null);
      r1.value === null && r1.done === false &&
      r2.value === "TypeError" && r2.done === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_parenthesized_optional_chain_does_not_propagate_into_following_computed_member_access_and_yield_in_key_runs() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        try {
          return ((yield null)?.x)[(yield "key")];
        } catch (e) {
          return e.name;
        }
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(null);
      var r3 = it.next("x");
      r1.value === null && r1.done === false &&
      r2.value === "key" && r2.done === false &&
      r3.value === "TypeError" && r3.done === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_parenthesized_optional_chain_does_not_propagate_into_following_call_and_yield_in_arg_runs() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        try {
          return ((yield null)?.x)(yield "arg");
        } catch (e) {
          return e.name;
        }
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(null);
      var r3 = it.next(0);
      r1.value === null && r1.done === false &&
      r2.value === "arg" && r2.done === false &&
      r3.value === "TypeError" && r3.done === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_parenthesized_optional_chain_followed_by_optional_call_skips_yield_in_arg() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      function* g() {
        var r = ((yield null)?.x)?.(yield "should-not-yield-arg");
        return r === undefined;
      }
      var it = g();
      var r1 = it.next();
      var r2 = it.next(null);
      r1.value === null && r1.done === false &&
      r2.value === true && r2.done === true
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_optional_chain_continuation_propagates_across_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() { const o = (yield 0); return o?.a.b; }

        const it1 = g();
        const r1 = it1.next();
        const r2 = it1.next(null);

        const it2 = g();
        it2.next();
        const r3 = it2.next({ a: { b: 1 } });

        r1.value === 0 && r1.done === false &&
        r2.value === undefined && r2.done === true &&
        r3.value === 1 && r3.done === true
      "#,
    )
    .unwrap();

  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_parentheses_break_optional_chain_propagation_across_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          const o = (yield 0);
          try {
            return (o?.a).b;
          } catch (e) {
            return e.name;
          }
        }

        const it1 = g();
        const r1 = it1.next();
        const r2 = it1.next(null);

        const it2 = g();
        it2.next();
        const r3 = it2.next({ a: { b: 1 } });

        r1.value === 0 && r1.done === false &&
        r2.value === "TypeError" && r2.done === true &&
        r3.value === 1 && r3.done === true
      "#,
    )
    .unwrap();

  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_optional_member_call_short_circuits_across_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() { const o = (yield 0); return o?.m().x; }

        const it1 = g();
        const r1 = it1.next();
        const r2 = it1.next(null);

        const it2 = g();
        it2.next();
        const r3 = it2.next({ m() { return { x: 2 }; } });

        r1.value === 0 && r1.done === false &&
        r2.value === undefined && r2.done === true &&
        r3.value === 2 && r3.done === true
      "#,
    )
    .unwrap();

  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_parenthesized_optional_member_callee_does_not_short_circuit_and_loses_this_binding() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          const o = (yield 0);
          try {
            return (o?.m)();
          } catch (e) {
            return e.name;
          }
        }

        const it1 = g();
        const r1 = it1.next();
        const r2 = it1.next(null);

        const it2 = g();
        it2.next();
        const r3 = it2.next({
          m: function () {
            'use strict';
            return this === undefined;
          }
        });

        r1.value === 0 && r1.done === false &&
        r2.value === "TypeError" && r2.done === true &&
        r3.value === true && r3.done === true
      "#,
    )
    .unwrap();

  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_parenthesized_call_after_computed_optional_chain_base_short_circuit_throws_and_skips_key_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          const o = (yield 0);
          try {
            // The optional chain (`o?.a[...]`) short-circuits when `o` is nullish. The computed key
            // expression must not run (including its `yield`), and the *call* must still happen
            // because it's outside the optional chain due to parentheses.
            return (o?.a[(yield "should-not-yield")])();
          } catch (e) {
            return e.name;
          }
        }

        const it = g();
        const r1 = it.next();
        const r2 = it.next(null);

        r1.value === 0 && r1.done === false &&
        r2.value === "TypeError" && r2.done === true
      "#,
    )
    .unwrap();

  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_optional_member_optional_call_short_circuits_and_skips_yield_in_arg() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          const o = (yield 0);
          const r = o?.m?.(yield "should-not-yield-arg");
          return r === undefined;
        }

        const it = g();
        const r1 = it.next();
        const r2 = it.next({}); // m is undefined => optional call short-circuits

        r1.value === 0 && r1.done === false &&
        r2.value === true && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_optional_member_optional_call_preserves_this_binding() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() { const o = (yield 0); return o?.m?.(); }

        const obj = {
          m: function () {
            'use strict';
            return this === obj;
          }
        };

        const it = g();
        const r1 = it.next();
        const r2 = it.next(obj);

        r1.value === 0 && r1.done === false &&
        r2.value === true && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_parenthesized_optional_member_then_optional_call_loses_this_binding() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() { const o = (yield 0); return (o?.m)?.(); }

        const obj = {
          m: function () {
            'use strict';
            return this === undefined;
          }
        };

        const it = g();
        const r1 = it.next();
        const r2 = it.next(obj);

        r1.value === 0 && r1.done === false &&
        r2.value === true && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_optional_chain_intermediate_short_circuit_skips_yield_in_computed_key() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          const o = (yield 0);
          const r = o?.a?.b[(yield "should-not-yield-key")];
          return r === undefined;
        }

        const it = g();
        const r1 = it.next();
        const r2 = it.next({ a: null });

        r1.value === 0 && r1.done === false &&
        r2.value === true && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_optional_chain_intermediate_short_circuit_skips_yield_in_call_arg() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          const o = (yield 0);
          const r = o?.a?.b(yield "should-not-yield-arg");
          return r === undefined;
        }

        const it = g();
        const r1 = it.next();
        const r2 = it.next({ a: null });

        r1.value === 0 && r1.done === false &&
        r2.value === true && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_optional_chain_call_evaluates_yield_in_arg_when_not_short_circuited() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          const f = (yield 0);
          const r = f?.(yield 1);
          return r;
        }

        function add1(x) { return x + 1; }

        const it = g();
        const r1 = it.next();
        const r2 = it.next(add1);
        const r3 = it.next(41);

        r1.value === 0 && r1.done === false &&
        r2.value === 1 && r2.done === false &&
        r3.value === 42 && r3.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_optional_chain_member_call_preserves_this_binding_across_yield_in_arg() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          const o = (yield 0);
          const r = o?.m(yield 1);
          return r;
        }

        const obj = {
          m: function (x) {
            'use strict';
            return this === obj && x === 123;
          }
        };

        const it = g();
        const r1 = it.next();
        const r2 = it.next(obj);
        const r3 = it.next(123);

        r1.value === 0 && r1.done === false &&
        r2.value === 1 && r2.done === false &&
        r3.value === true && r3.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_optional_chain_computed_member_call_preserves_this_binding_across_yield_in_key_and_arg() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          const o = (yield 0);
          const r = o?.[(yield "m")](yield 1);
          return r;
        }

        const obj = {
          m: function (x) {
            'use strict';
            return this === obj && x === 456;
          }
        };

        const it = g();
        const r1 = it.next();
        const r2 = it.next(obj);
        const r3 = it.next("m");
        const r4 = it.next(456);

        r1.value === 0 && r1.done === false &&
        r2.value === "m" && r2.done === false &&
        r3.value === 1 && r3.done === false &&
        r4.value === true && r4.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_optional_member_optional_call_preserves_this_binding_across_yield_in_arg() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          const o = (yield 0);
          const r = o?.m?.(yield 1);
          return r;
        }

        const obj = {
          m: function (x) {
            'use strict';
            return this === obj && x === 789;
          }
        };

        const it = g();
        const r1 = it.next();
        const r2 = it.next(obj);
        const r3 = it.next(789);

        r1.value === 0 && r1.done === false &&
        r2.value === 1 && r2.done === false &&
        r3.value === true && r3.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_optional_chain_computed_member_optional_call_short_circuits_and_skips_yield_in_arg() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          const o = (yield 0);
          const r = o?.[(yield "m")]?.(yield "should-not-yield-arg");
          return r === undefined;
        }

        const it = g();
        const r1 = it.next();
        const r2 = it.next({});
        const r3 = it.next("m");

        r1.value === 0 && r1.done === false &&
        r2.value === "m" && r2.done === false &&
        r3.value === true && r3.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_optional_chain_member_call_evaluates_yield_in_arg_before_throwing_type_error() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          const o = (yield 0);
          try {
            o?.m(yield 1);
            return "no";
          } catch (e) {
            return e.name;
          }
        }

        const it = g();
        const r1 = it.next();
        const r2 = it.next({});
        const r3 = it.next(0);

        r1.value === 0 && r1.done === false &&
        r2.value === 1 && r2.done === false &&
        r3.value === "TypeError" && r3.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_optional_member_optional_call_evaluates_yield_in_arg_before_throwing_type_error() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          const o = (yield 0);
          try {
            o?.m?.(yield 1);
            return "no";
          } catch (e) {
            return e.name;
          }
        }

        const it = g();
        const r1 = it.next();
        const r2 = it.next({ m: 0 });
        const r3 = it.next(0);

        r1.value === 0 && r1.done === false &&
        r2.value === 1 && r2.done === false &&
        r3.value === "TypeError" && r3.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_optional_chain_call_evaluates_yield_in_arg_before_throwing_type_error() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          const f = (yield 0);
          try {
            f?.(yield 1);
            return "no";
          } catch (e) {
            return e.name;
          }
        }

        const it = g();
        const r1 = it.next();
        const r2 = it.next(0);
        const r3 = it.next(0);

        r1.value === 0 && r1.done === false &&
        r2.value === 1 && r2.done === false &&
        r3.value === "TypeError" && r3.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_optional_chain_private_method_call_short_circuits_and_skips_yield_in_arg() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class C {
          #x = 1;
          #m(v) { return this.#x + v; }
          *g() {
            const o = (yield 0);
            const r = o?.#m(yield "should-not-yield");
            return r === undefined;
          }
        }

        const it = new C().g();
        const r1 = it.next();
        const r2 = it.next(null);

        r1.value === 0 && r1.done === false &&
        r2.value === true && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_optional_chain_private_method_call_evaluates_yield_in_arg_when_not_short_circuited() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class C {
          #x = 1;
          #m(v) { return this.#x + v; }
          *g() {
            const o = (yield 0);
            const r = o?.#m(yield 1);
            return r;
          }
        }

        const obj = new C();
        const it = obj.g();
        const r1 = it.next();
        const r2 = it.next(obj);
        const r3 = it.next(41);

        r1.value === 0 && r1.done === false &&
        r2.value === 1 && r2.done === false &&
        r3.value === 42 && r3.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_parenthesized_optional_chain_private_method_callee_does_not_short_circuit_and_yield_in_arg_runs() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class C {
          #m(_) { return 0; }
          *g() {
            const o = (yield 0);
            try {
              return (o?.#m)(yield 1);
            } catch (e) {
              return e.name;
            }
          }
        }

        const it = new C().g();
        const r1 = it.next();
        const r2 = it.next(null);
        const r3 = it.next(0);

        r1.value === 0 && r1.done === false &&
        r2.value === 1 && r2.done === false &&
        r3.value === "TypeError" && r3.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_super_property_optional_call_short_circuits_and_skips_yield_in_arg() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class Base {}
        class Derived extends Base {
          *g() { return super.m?.(yield "should-not-yield"); }
        }

        const it = new Derived().g();
        const r = it.next();

        r.value === undefined && r.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_super_property_optional_call_preserves_this_binding_across_yield_in_arg() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        class Base {
          m(x) {
            'use strict';
            return this === expected && x === 42;
          }
        }
        let expected;
        class Derived extends Base {
          constructor() { super(); expected = this; }
          *g() { return super.m?.(yield 1); }
        }

        const it = new Derived().g();
        const r1 = it.next();
        const r2 = it.next(42);

        r1.value === 1 && r1.done === false &&
        r2.value === true && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_optional_chaining_yield_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      let ok = true;

      function* g0() {
        const r = null?.(yield "should-not-yield");
        return r === undefined;
      }
      const r0 = g0().next();
      ok = ok && r0.value === true && r0.done === true;

      function* g1(o) { return o?.m?.(yield 0); }
      const obj1 = {
        m: function (x) {
          'use strict';
          return this === obj1 && x === 1;
        }
      };
      const it1 = g1(obj1);
      const r1_1 = it1.next();
      const r1_2 = it1.next(1);
      ok = ok && r1_1.value === 0 && r1_1.done === false &&
        r1_2.value === true && r1_2.done === true;

      function* g2(o) { return o?.[(yield "key")](yield 0); }
      const obj2 = {
        m: function (x) {
          'use strict';
          return this === obj2 && x === 2;
        }
      };
      const it2 = g2(obj2);
      const r2_1 = it2.next();
      const r2_2 = it2.next("m");
      const r2_3 = it2.next(2);
      ok = ok && r2_1.value === "key" && r2_1.done === false &&
        r2_2.value === 0 && r2_2.done === false &&
        r2_3.value === true && r2_3.done === true;

      ok
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}
