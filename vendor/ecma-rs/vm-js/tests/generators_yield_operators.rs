use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn new_runtime_with_frequent_gc() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Use a larger heap so we can force a GC at specific points in a test without risking OOM, while
  // keeping the GC threshold low enough that a single `churn()` call reliably triggers GC.
  //
  // Note: keep `gc_threshold` high enough that runtime initialization does not spend excessive time
  // collecting.
  let heap = Heap::new(HeapLimits::new(16 * 1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generators_yield_in_binary_addition_left() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() { return (yield 1) + 2; }
        const it = g();
        const r1 = it.next();
        const r2 = it.next(10);
        r1.value === 1 && r1.done === false &&
        r2.value === 12 && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_yield_in_binary_addition_right() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() { return 1 + (yield 2); }
        const it = g();
        const r1 = it.next();
        const r2 = it.next(10);
        r1.value === 2 && r1.done === false &&
        r2.value === 11 && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_yield_in_binary_multiplication_left() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() { return (yield 1) * 2; }
        const it = g();
        const r1 = it.next();
        const r2 = it.next(6);
        r1.value === 1 && r1.done === false &&
        r2.value === 12 && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_yield_in_binary_exponentiation_left() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() { return (yield 2) ** 3; }
        const it = g();
        const r1 = it.next();
        const r2 = it.next(4);
        r1.value === 2 && r1.done === false &&
        r2.value === 64 && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_yield_in_binary_strict_equality_both() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() { return (yield 1) === (yield 1); }
        const it = g();
        const r1 = it.next();
        // Ensure each yield consumes its own resume value.
        const r2 = it.next(7);
        const r3 = it.next(8);
        r1.value === 1 && r1.done === false &&
        r2.value === 1 && r2.done === false &&
        r3.value === false && r3.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_yield_in_assignment_rhs() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          let x = 0;
          const r = (x = (yield 1));
          return r === 10 && x === 10;
        }
        const it = g();
        const r1 = it.next();
        const r2 = it.next(10);
        r1.value === 1 && r1.done === false &&
        r2.value === true && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_yield_in_assignment_addition_rhs() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          let x = 5;
          const r = (x += (yield 1));
          return r === 12 && x === 12;
        }
        const it = g();
        const r1 = it.next();
        const r2 = it.next(7);
        r1.value === 1 && r1.done === false &&
        r2.value === true && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_yield_in_assignment_exponentiation_rhs() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          let x = 2;
          const r = (x **= (yield 1));
          return r === 8 && x === 8;
        }
        const it = g();
        const r1 = it.next();
        const r2 = it.next(3);
        r1.value === 1 && r1.done === false &&
        r2.value === true && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_yield_in_member_assignment_computed_key() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          const o = {};
          const r = (o[(yield "a")] = 1);
          return r === 1 && o.b === 1 && o.a === undefined;
        }
        const it = g();
        const r1 = it.next();
        const r2 = it.next("b");
        r1.value === "a" && r1.done === false &&
        r2.value === true && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_yield_in_member_addition_assignment_computed_key() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          const o = { b: 1 };
          const r = (o[(yield "a")] += 2);
          return r === 3 && o.b === 3 && o.a === undefined;
        }
        const it = g();
        const r1 = it.next();
        const r2 = it.next("b");
        r1.value === "a" && r1.done === false &&
        r2.value === true && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_yield_in_member_postfix_update_computed_key() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          const o = { b: 1 };
          const r = o[(yield "a")]++;
          return r === 1 && o.b === 2 && o.a === undefined;
        }
        const it = g();
        const r1 = it.next();
        const r2 = it.next("b");
        r1.value === "a" && r1.done === false &&
        r2.value === true && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_yield_in_member_prefix_update_computed_key() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          const o = { b: 1 };
          const r = ++o[(yield "a")];
          return r === 2 && o.b === 2 && o.a === undefined;
        }
        const it = g();
        const r1 = it.next();
        const r2 = it.next("b");
        r1.value === "a" && r1.done === false &&
        r2.value === true && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_yield_in_new_arg() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function C(x) { this.x = x; }
        function* g() {
          const o = new C(yield 1);
          return o.x === 42;
        }
        const it = g();
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
fn generators_yield_in_new_callee_and_arg_with_gc() {
  let mut rt = new_runtime_with_frequent_gc();
  rt
    .exec_script(
      r#"
        function churn() {
          // Allocate enough to force GC under the small `gc_threshold`.
          let junk = [];
          for (let i = 0; i < 200; i++) {
            junk.push(new Uint8Array(1024));
          }
          return junk.length;
        }

        function C(x) { this.x = x; }

        function* g() {
          const o = new (yield 1)(yield 2);
          return o.x;
        }
        var it = g();
        var r1 = it.next();
      "#,
    )
    .unwrap();

  rt.exec_script(r#"var r2 = it.next(C);"#).unwrap();
  let gc_before = rt.heap.gc_runs();
  rt.exec_script(r#"churn();"#).unwrap();
  let gc_after = rt.heap.gc_runs();
  assert!(
    gc_after > gc_before,
    "expected at least one GC cycle while generator is suspended in `new`"
  );

  let value = rt
    .exec_script(
      r#"
        var r3 = it.next(42);
        r1.value === 1 && r1.done === false &&
        r2.value === 2 && r2.done === false &&
        r3.value === 42 && r3.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_new_non_constructor_still_evaluates_args() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          var side = 0;
          function arg() { side++; return 123; }
          try {
            // Spec: argument expressions are evaluated before `IsConstructor` is checked.
            new (yield 1)(arg());
            return "no";
          } catch (e) {
            return side === 1 && (e instanceof TypeError);
          }
        }
        const it = g();
        const r1 = it.next();
        const r2 = it.next(0); // not a constructor
        r1.value === 1 && r1.done === false &&
        r2.value === true && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_yield_in_new_spread_arg() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function C(a, b, c, d) {
          this.a = a;
          this.b = b;
          this.c = c;
          this.d = d;
        }
        function* g() {
          // Spread is evaluated/expanded left-to-right, and the spread results must survive across
          // later yields.
          const obj = new C(0, ...(yield 1), (yield 2));
          return obj.a === 0 && obj.b === 1 && obj.c === 2 && obj.d === 3;
        }
        const it = g();
        const r1 = it.next();
        const r2 = it.next([1, 2]);
        const r3 = it.next(3);
        r1.value === 1 && r1.done === false &&
        r2.value === 2 && r2.done === false &&
        r3.value === true && r3.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_yield_in_delete_computed_key() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          const o = { b: 1 };
          const r = delete o[(yield "a")];
          return r === true && !("b" in o) && !("a" in o);
        }
        const it = g();
        const r1 = it.next();
        const r2 = it.next("b");
        r1.value === "a" && r1.done === false &&
        r2.value === true && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_yield_in_template_literals() {
  let mut rt = new_runtime_with_frequent_gc();
  let value = rt
    .exec_script(
      r#"
        function churn() {
          // Allocate enough to exceed the GC threshold and force a collection.
          //
          // Use a single large ArrayBuffer-backed TypedArray so this stays fast while still
          // deterministically triggering a GC due to the small `gc_threshold` configured above.
          const buf = new Uint8Array(2 * 1024 * 1024);
          return buf.length;
        }

        // `a${yield 1}b`
        function* tpl_simple() { return `a${yield 1}b`; }
        const it1 = tpl_simple();
        const a1 = it1.next();
        // Trigger GC while the generator is suspended with a `LitTemplateAfterSubstitution` frame
        // holding the already-appended prefix.
        churn();
        const a2 = it1.next(10);
        const ok1 = a1.value === 1 && a1.done === false && a2.value === "a10b" && a2.done === true;

        // Ensure substitution evaluation is left-to-right and each `yield` consumes its own resume value.
        function* tpl_multi() { return `x${yield 1}y${yield 2}z`; }
        const it2 = tpl_multi();
        const b1 = it2.next();
        churn();
        const b2 = it2.next("A");
        const b3 = it2.next("B");
        const ok2 =
          b1.value === 1 && b1.done === false &&
          b2.value === 2 && b2.done === false &&
          b3.value === "xAyBz" && b3.done === true;

        // Yield inside a larger substitution expression.
        function* tpl_nested() { return `a${1 + (yield 2)}b`; }
        const it3 = tpl_nested();
        const c1 = it3.next();
        const c2 = it3.next(10);
        const ok3 = c1.value === 2 && c1.done === false && c2.value === "a11b" && c2.done === true;

        // ToString is applied to substitution results (Symbol should throw).
        function* tpl_symbol() { return `${yield 1}`; }
        const it4 = tpl_symbol();
        it4.next();
        let ok4 = false;
        try {
          it4.next(Symbol("s"));
        } catch (e) {
          ok4 = e instanceof TypeError;
        }
        // After an uncaught error, the generator should be closed.
        const after_sym = it4.next();
        ok4 = ok4 && after_sym.value === void 0 && after_sym.done === true;

        // Force GC during `ToString` of a resumed yield value.
        function* tpl_tostring_gc() { return `a${yield 1}b`; }
        const it5 = tpl_tostring_gc();
        const d1 = it5.next();
        const d2 = it5.next({
          toString() {
            churn();
            return "X";
          }
        });
        const ok5 =
          d1.value === 1 && d1.done === false &&
          d2.value === "aXb" && d2.done === true;

        // `yield;` inside a substitution yields `undefined` regardless of `undefined` shadowing.
        function* tpl_yield_no_arg() {
          var undefined = 123;
          return `a${yield}b`;
        }
        const it6 = tpl_yield_no_arg();
        const e1 = it6.next();
        churn();
        const e2 = it6.next("X");
        const ok6 =
          e1.value === void 0 && e1.done === false &&
          e2.value === "aXb" && e2.done === true;

        // Template string parts use *cooked* values (escape sequences are interpreted).
        function* tpl_cooked() { return `a\n${yield 1}b`; }
        const it7 = tpl_cooked();
        const f1 = it7.next();
        churn();
        const f2 = it7.next("X");
        const ok7 =
          f1.value === 1 && f1.done === false &&
          f2.value === "a\nXb" && f2.done === true;

        // `yield*` inside a substitution may suspend multiple times while the already-appended
        // prefix and substitution index are preserved across resumption.
        function* inner() {
          const a = yield 1;
          const b = yield 2;
          return a + b;
        }
        function* tpl_yield_star() { return `a${yield* inner()}b`; }
        const it8 = tpl_yield_star();
        const g1 = it8.next();
        churn();
        const g2 = it8.next(10);
        churn();
        const g3 = it8.next(20);
        const ok8 =
          g1.value === 1 && g1.done === false &&
          g2.value === 2 && g2.done === false &&
          g3.value === "a30b" && g3.done === true;

        // `.throw` while suspended in a substitution should propagate through the template frame.
        function* tpl_throw() {
          try {
            return `a${yield 1}b`;
          } catch (e) {
            return e.message;
          }
        }
        const it9 = tpl_throw();
        const h1 = it9.next();
        churn();
        const h2 = it9.throw(new Error("boom"));
        const ok9 =
          h1.value === 1 && h1.done === false &&
          h2.value === "boom" && h2.done === true;

        // `.return` should bypass template concatenation and complete with the return value.
        function* tpl_return() { return `a${yield 1}b`; }
        const it10 = tpl_return();
        const i1 = it10.next();
        churn();
        const i2 = it10.return(99);
        const ok10 =
          i1.value === 1 && i1.done === false &&
          i2.value === 99 && i2.done === true;

        // Optional chaining short-circuit sentinel must not leak through the template frame.
        // If it did, ToString would observe a Symbol and throw.
        function* tpl_opt_chain() { return `a${(yield 1)?.x}b`; }
        const it11 = tpl_opt_chain();
        const j1 = it11.next();
        churn();
        const j2 = it11.next(null);
        const ok11 =
          j1.value === 1 && j1.done === false &&
          j2.value === "aundefinedb" && j2.done === true;

        // Nested template literal inside a substitution should preserve both inner + outer
        // accumulator state across suspension.
        function* tpl_nested_template() { return `a${`b${yield 1}c`}d`; }
        const it12 = tpl_nested_template();
        const k1 = it12.next();
        churn();
        const k2 = it12.next("X");
        const ok12 =
          k1.value === 1 && k1.done === false &&
          k2.value === "abXcd" && k2.done === true;

        // Nested template yield followed by another yield substitution in the outer template.
        function* tpl_nested_then_outer() { return `a${`b${yield 1}c`}d${yield 2}e`; }
        const it13 = tpl_nested_then_outer();
        const l1 = it13.next();
        churn();
        const l2 = it13.next("X");
        churn();
        const l3 = it13.next("Y");
        const ok13 =
          l1.value === 1 && l1.done === false &&
          l2.value === 2 && l2.done === false &&
          l3.value === "abXcdYe" && l3.done === true;

        // Re-entrancy: `ToString` is evaluated while the generator is executing. Attempting to
        // resume the same generator from within `toString()` must throw TypeError.
        function* tpl_reenter() { return `a${yield 1}b`; }
        const it14 = tpl_reenter();
        const m1 = it14.next();
        churn();
        const m2 = it14.next({
          toString() {
            let sawTypeError = false;
            try {
              it14.next(0);
            } catch (e) {
              sawTypeError = e instanceof TypeError;
            }
            if (!sawTypeError) throw new Error("expected TypeError");
            return "X";
          }
        });
        const ok14 =
          m1.value === 1 && m1.done === false &&
          m2.value === "aXb" && m2.done === true;

        // Multiple `yield` expressions inside a single substitution (comma operator).
        function* tpl_comma_yield() { return `a${(yield 1, yield 2)}b`; }
        const it15 = tpl_comma_yield();
        const n1 = it15.next();
        churn();
        const n2 = it15.next("X");
        churn();
        const n3 = it15.next("Y");
        const ok15 =
          n1.value === 1 && n1.done === false &&
          n2.value === 2 && n2.done === false &&
          n3.value === "aYb" && n3.done === true;

        ok1 && ok2 && ok3 && ok4 && ok5 && ok6 && ok7 && ok8 && ok9 && ok10 && ok11 && ok12 && ok13 && ok14 && ok15
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_yield_in_tagged_templates() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        // yield in tag expression + yield in substitutions
        function join(strings, ...values) { return values.join(","); }
        function* tag_and_subst() {
          return (yield join)`a${yield 1}b${yield 2}c`;
        }
        const it1 = tag_and_subst();
        const a1 = it1.next();
        const a2 = it1.next(join);
        const a3 = it1.next("X");
        const a4 = it1.next("Y");
        const ok1 =
          a1.value === join && a1.done === false &&
          a2.value === 1 && a2.done === false &&
          a3.value === 2 && a3.done === false &&
          a4.value === "X,Y" && a4.done === true;

        // yield in member base tag + correct this-binding
        const obj2 = {
          tag(strings, ...values) { return this === obj2 && values[0] === 42; }
        };
        function* member_base() {
          return (yield obj2).tag`x${yield 1}y`;
        }
        const it2 = member_base();
        const b1 = it2.next();
        const b2 = it2.next(obj2);
        const b3 = it2.next(42);
        const ok2 =
          b1.value === obj2 && b1.done === false &&
          b2.value === 1 && b2.done === false &&
          b3.value === true && b3.done === true;

        // yield in computed member key tag + correct this-binding
        const obj3 = {
          tag(strings, ...values) { return this === obj3 && values[0] === 7; }
        };
        function* computed_key() {
          return obj3[(yield "tag")]`x${yield 1}y`;
        }
        const it3 = computed_key();
        const c1 = it3.next();
        const c2 = it3.next("tag");
        const c3 = it3.next(7);
        const ok3 =
          c1.value === "tag" && c1.done === false &&
          c2.value === 1 && c2.done === false &&
          c3.value === true && c3.done === true;

        // Template object caching across generator invocations (same call site, cooked+raw identity).
        let cachedStrings;
        let cachedRaw;
        function capture(strings, ...values) {
          if (cachedStrings === undefined) {
            cachedStrings = strings;
            cachedRaw = strings.raw;
          }
          return strings === cachedStrings && strings.raw === cachedRaw;
        }
        function* cache_test() {
          return capture`hello${yield 1}world`;
        }
        const it4 = cache_test();
        const d1 = it4.next();
        const d2 = it4.next(0);
        const it5 = cache_test();
        const d3 = it5.next();
        const d4 = it5.next(0);
        const ok4 =
          d1.value === 1 && d1.done === false &&
          d2.value === true && d2.done === true &&
          d3.value === 1 && d3.done === false &&
          d4.value === true && d4.done === true;

        ok1 && ok2 && ok3 && ok4
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_yield_in_array_literals() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        // [(yield 1)]
        function* single_elem() { return [(yield 1)]; }
        const it1 = single_elem();
        const a1 = it1.next();
        const a2 = it1.next(10);
        const ok1 =
          a1.value === 1 && a1.done === false &&
          Array.isArray(a2.value) && a2.value.length === 1 && a2.value[0] === 10 && a2.done === true;

        // [...(yield [1,2]),] (yield inside spread + trailing comma)
        function* spread_elem() { return [...(yield [1, 2]),]; }
        const it2 = spread_elem();
        const b1 = it2.next();
        const b2 = it2.next([10, 20]);
        const ok2 =
          Array.isArray(b1.value) && b1.value.length === 2 && b1.value[0] === 1 && b1.value[1] === 2 && b1.done === false &&
          Array.isArray(b2.value) && b2.value.length === 2 && b2.value[0] === 10 && b2.value[1] === 20 && b2.done === true;

        // Elisions + multiple yields (including inside spread).
        function* holes_and_order() { return [,(yield 1), ...(yield [2]), (yield 3),]; }
        const it3 = holes_and_order();
        const c1 = it3.next();
        const c2 = it3.next(10);
        const c3 = it3.next([20]);
        const c4 = it3.next(30);
        const arr = c4.value;
        const ok3 =
          c1.value === 1 && c1.done === false &&
          Array.isArray(c2.value) && c2.value.length === 1 && c2.value[0] === 2 && c2.done === false &&
          c3.value === 3 && c3.done === false &&
          c4.done === true &&
          Array.isArray(arr) && arr.length === 4 &&
          (0 in arr) === false && arr[0] === undefined &&
          arr[1] === 10 && arr[2] === 20 && arr[3] === 30;

        ok1 && ok2 && ok3
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_yield_in_array_literals_gc_safety() {
  let mut rt = new_runtime_with_frequent_gc();
  rt
    .exec_script(
      r#"
        function churn() {
          // Allocate enough to exceed the GC threshold and force a collection.
          //
          // Use a single large ArrayBuffer-backed TypedArray so this stays fast while still
          // deterministically triggering a GC due to the small `gc_threshold` configured in the
          // test runtime.
          const buf = new Uint8Array(2 * 1024 * 1024);
          return buf.length;
        }

        function* g() { return [1, (yield 2), 3]; }
        var it = g();
        var r1 = it.next();
      "#,
    )
    .unwrap();

  let gc_before = rt.heap.gc_runs();
  rt.exec_script("churn();").unwrap();
  let gc_after = rt.heap.gc_runs();
  assert!(
    gc_after > gc_before,
    "expected at least one GC cycle while generator is suspended in array literal"
  );

  let value = rt
    .exec_script(
      r#"
        var r2 = it.next(10);
        r1.value === 2 && r1.done === false &&
        Array.isArray(r2.value) && r2.value.length === 3 &&
        r2.value[0] === 1 && r2.value[1] === 10 && r2.value[2] === 3 &&
        r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_yield_in_object_literals() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        // { [(yield "a")]: 1 }
        function* computed_key() {
          const o = { [(yield "a")]: 1 };
          return o.b === 1 && o.a === undefined;
        }
        const it1 = computed_key();
        const a1 = it1.next();
        const a2 = it1.next("b");
        const ok1 = a1.value === "a" && a1.done === false && a2.value === true && a2.done === true;

        // { a: (yield 1) }
        function* prop_value() {
          const o = { a: (yield 1) };
          return o.a === 42;
        }
        const it2 = prop_value();
        const b1 = it2.next();
        const b2 = it2.next(42);
        const ok2 = b1.value === 1 && b1.done === false && b2.value === true && b2.done === true;

        // { ...(yield "spread"), x: 1 }
        function* spread_prop() {
          const o = { ...(yield "spread"), x: 1 };
          return o.b === 2 && o.x === 1;
        }
        const it3 = spread_prop();
        const c1 = it3.next();
        const c2 = it3.next({ b: 2 });
        const ok3 = c1.value === "spread" && c1.done === false && c2.value === true && c2.done === true;

        // { [(yield)]() { ... } }
        // Use `yield` with no operand to exercise `yield;` semantics inside computed member names:
        // the yielded value is `undefined`, and the resumption value becomes the computed key.
        function* method_computed_key() {
          const o = { [(yield)]() { return 1; } };
          return typeof o.foo === "function" && o.foo() === 1;
        }
        const it4 = method_computed_key();
        const d1 = it4.next();
        const d2 = it4.next("foo");
        const ok4 = d1.value === undefined && d1.done === false && d2.value === true && d2.done === true;

        // { get [(yield)]() { ... } }
        function* getter_computed_key() {
          const o = { get [(yield)]() { return 7; } };
          return o.bar === 7;
        }
        const it5 = getter_computed_key();
        const e1 = it5.next();
        const e2 = it5.next("bar");
        const ok5 = e1.value === undefined && e1.done === false && e2.value === true && e2.done === true;

        // { set [(yield)](v) { ... } }
        function* setter_computed_key() {
          let captured = 0;
          const o = { set [(yield)](v) { captured = v; } };
          o.baz = 9;
          return captured === 9;
        }
        const it6 = setter_computed_key();
        const f1 = it6.next();
        const f2 = it6.next("baz");
        const ok6 = f1.value === undefined && f1.done === false && f2.value === true && f2.done === true;

        ok1 && ok2 && ok3 && ok4 && ok5 && ok6
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_yield_in_object_literals_gc_safety() {
  let mut rt = new_runtime_with_frequent_gc();
  rt
    .exec_script(
      r#"
        function churn() {
          // Allocate enough to exceed the GC threshold and force a collection.
          //
          // Use a single large ArrayBuffer-backed TypedArray so this stays fast while still
          // deterministically triggering a GC due to the small `gc_threshold` configured in the
          // test runtime.
          const buf = new Uint8Array(2 * 1024 * 1024);
          return buf.length;
        }

        function* g() {
          return { a: 0, ...(yield "spread"), [(yield "k")]: (yield "v"), b: 2 };
        }
        var it = g();
        var r1 = it.next();
      "#,
    )
    .unwrap();

  let gc_before = rt.heap.gc_runs();
  rt.exec_script("churn();").unwrap();
  let gc_after = rt.heap.gc_runs();
  assert!(
    gc_after > gc_before,
    "expected at least one GC cycle while generator is suspended in object literal (after spread yield)"
  );

  rt.exec_script(r#"var r2 = it.next({ x: 1 });"#).unwrap();
  let gc_before = rt.heap.gc_runs();
  rt.exec_script("churn();").unwrap();
  let gc_after = rt.heap.gc_runs();
  assert!(
    gc_after > gc_before,
    "expected at least one GC cycle while generator is suspended in object literal (after computed-key yield)"
  );

  rt.exec_script(r#"var r3 = it.next("prop");"#).unwrap();
  let gc_before = rt.heap.gc_runs();
  rt.exec_script("churn();").unwrap();
  let gc_after = rt.heap.gc_runs();
  assert!(
    gc_after > gc_before,
    "expected at least one GC cycle while generator is suspended in object literal (after value yield)"
  );

  let value = rt
    .exec_script(
      r#"
        var r4 = it.next(10);
        var o = r4.value;
        r1.value === "spread" && r1.done === false &&
        r2.value === "k" && r2.done === false &&
        r3.value === "v" && r3.done === false &&
        r4.done === true &&
        o.a === 0 && o.x === 1 && o.prop === 10 && o.b === 2
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_yield_in_object_literals_proto_setter() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        // Direct `__proto__` property is a special-case prototype setter.
        const proto = { marker: 1 };
        function* direct_proto() {
          const o = { __proto__: (yield "proto"), x: 1 };
          return Object.getPrototypeOf(o) === proto &&
            Object.getOwnPropertyDescriptor(o, "__proto__") === undefined &&
            o.x === 1;
        }
        const it1 = direct_proto();
        const a1 = it1.next();
        const a2 = it1.next(proto);
        const ok1 = a1.value === "proto" && a1.done === false && a2.value === true && a2.done === true;

        // Computed `["__proto__"]` is *not* a prototype setter; it should create a data property.
        function* computed_proto() {
          const o = { [(yield "__proto__")]: (yield 1) };
          return Object.getPrototypeOf(o) === Object.prototype &&
            Object.getOwnPropertyDescriptor(o, "__proto__").value === 7;
        }
        const it2 = computed_proto();
        const b1 = it2.next();
        const b2 = it2.next("__proto__");
        const b3 = it2.next(7);
        const ok2 =
          b1.value === "__proto__" && b1.done === false &&
          b2.value === 1 && b2.done === false &&
          b3.value === true && b3.done === true;

        ok1 && ok2
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_yield_in_object_literals_proto_setter_non_object_and_null() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* proto_null() {
          const o = { __proto__: (yield 1), x: 2 };
          return Object.getPrototypeOf(o) === null &&
            Object.getOwnPropertyDescriptor(o, "__proto__") === undefined &&
            o.x === 2;
        }
        const it1 = proto_null();
        const a1 = it1.next();
        const a2 = it1.next(null);
        const ok1 = a1.value === 1 && a1.done === false && a2.value === true && a2.done === true;

        function* proto_ignored() {
          const o = { __proto__: (yield 2), x: 3 };
          return Object.getPrototypeOf(o) === Object.prototype &&
            Object.getOwnPropertyDescriptor(o, "__proto__") === undefined &&
            o.x === 3;
        }
        const it2 = proto_ignored();
        const b1 = it2.next();
        const b2 = it2.next(123);
        const ok2 = b1.value === 2 && b1.done === false && b2.value === true && b2.done === true;

        ok1 && ok2
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_yield_in_object_literals_spread_does_not_trigger_proto_setter() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        const proto = { marker: 1 };
        const src = {};
        Object.defineProperty(src, "__proto__", {
          value: proto,
          enumerable: true,
          configurable: true,
          writable: true,
        });

        function* g() {
          const o = { ...(yield 1), x: 2 };
          const desc = Object.getOwnPropertyDescriptor(o, "__proto__");
          return Object.getPrototypeOf(o) === Object.prototype &&
            desc !== undefined &&
            desc.value === proto &&
            o.x === 2;
        }
        const it = g();
        const r1 = it.next();
        const r2 = it.next(src);
        r1.value === 1 && r1.done === false &&
          r2.value === true && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_yield_in_object_literals_proto_setter_and_proto_named_method() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        // A prototype setter `__proto__:` property should not affect a *method named* `__proto__`.
        // The method must be defined as an own property and remain callable after resumption.
        const proto = { marker: 1 };

        function* g() {
          const o = {
            __proto__: (yield "proto"),
            __proto__() { return 3; },
          };
          const desc = Object.getOwnPropertyDescriptor(o, "__proto__");
          return Object.getPrototypeOf(o) === proto &&
            desc !== undefined &&
            typeof desc.value === "function" &&
            o.__proto__() === 3;
        }

        const it = g();
        const r1 = it.next();
        const r2 = it.next(proto);
        r1.value === "proto" && r1.done === false &&
          r2.value === true && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_yield_in_object_literals_computed_method_symbol_key() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function* g() {
          const o = { [(yield Symbol.toPrimitive)]() { return 3; } };
          return o[Symbol.toPrimitive]() === 3;
        }
        const it = g();
        const r1 = it.next();
        const r2 = it.next(Symbol.toPrimitive);
        r1.value === Symbol.toPrimitive && r1.done === false &&
          r2.value === true && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generators_yield_in_object_literals_proto_setter_and_super() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        // Ensure an object literal that suspends while evaluating a `__proto__` setter value still
        // produces methods with a valid [[HomeObject]] for `super` resolution.
        const proto = { get x() { return this.y + 1; } };
        function* g() {
          const o = {
            __proto__: (yield "proto"),
            y: 41,
            m() { return super.x; },
          };
          return Object.getPrototypeOf(o) === proto && o.m() === 42;
        }
        const it = g();
        const r1 = it.next();
        const r2 = it.next(proto);
        r1.value === "proto" && r1.done === false &&
          r2.value === true && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
