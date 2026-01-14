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

        ok1 && ok2 && ok3 && ok4 && ok5 && ok6
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
