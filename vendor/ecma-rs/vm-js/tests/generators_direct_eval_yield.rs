use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_direct_eval_with_yield_argument_sees_lexical_bindings() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var ok = false;
        try {
          function* g(){ let x = 1; return eval(yield 0); }
          var it = g();
          it.next();
          var r = it.next("x");
          ok = r.done === true && r.value === 1;
        } catch (e) { ok = false; }
        ok
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_direct_eval_with_yield_argument_assigns_to_local_var() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var ok = false;
        try {
          function* g(){ var x = 1; eval(yield 0); return x; }
          var it = g();
          it.next();
          var r = it.next("x = 2");
          ok = r.done === true && r.value === 2;
        } catch (e) { ok = false; }
        ok
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_parenthesized_eval_with_yield_argument_is_indirect() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var ok = false;
        try {
          // `var` creates a global binding visible to indirect eval.
          var x = 2;
          function* g(){ let x = 1; return (eval)(yield 0); }
          var it = g();
          it.next();
          var r = it.next("x");
          // Parenthesized eval is *indirect* and must not see the generator's lexical `x`.
          ok = r.done === true && r.value === 2;
        } catch (e) { ok = false; }
        ok
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_optional_chain_eval_with_yield_argument_is_indirect() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var ok = false;
        try {
          var x = 2;
          function* g(){ let x = 1; return eval?.(yield 0); }
          var it = g();
          it.next();
          var r = it.next("x");
          // Optional-call eval is never a direct eval.
          ok = r.done === true && r.value === 2;
        } catch (e) { ok = false; }
        ok
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_shadowed_eval_is_not_direct_even_with_yield_argument() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var ok = false;
        try {
          function* g(){
            // Shadow `%eval%` with a local binding; syntactic `eval(...)` must not be treated as
            // direct eval unless the callee is the intrinsic `%eval%` object.
            let eval = function(_) { return 123; };
            return eval(yield 0);
          }
          var it = g();
          it.next();
          var r = it.next("x");
          ok = r.done === true && r.value === 123;
        } catch (e) { ok = false; }
        ok
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_direct_eval_with_yield_in_later_argument_is_direct() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var ok = false;
        try {
          // `var` creates a global binding visible to indirect eval.
          var x = 2;
          function* g(){ let x = 1; return eval("x", yield 0); }
          var it = g();
          var r1 = it.next();
          var r2 = it.next("ignored");
          // The `yield` occurs while evaluating the *second* argument, so the continuation must
          // still treat this as a direct eval.
          ok = r1.done === false && r1.value === 0 && r2.done === true && r2.value === 1;
        } catch (e) { ok = false; }
        ok
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_direct_eval_with_yield_spread_argument_is_direct() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var ok = false;
        try {
          var x = 2;
          // `yield` suspends while evaluating a spread argument; the resumed iterable provides the
          // argument list for `eval`.
          function* g(){ let x = 1; return eval(...(yield 0)); }
          var it = g();
          var r1 = it.next();
          var r2 = it.next(["x"]);
          ok = r1.done === false && r1.value === 0 && r2.done === true && r2.value === 1;
        } catch (e) { ok = false; }
        ok
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_direct_eval_with_yield_argument_inherits_strictness() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var ok = false;
        try {
          function* g(){
            "use strict";
            try { return eval(yield 0); }
            catch (e) { return e.name; }
          }
          var it = g();
          it.next();
          var r = it.next("with ({x:1}) { x }");
          ok = r.done === true && r.value === "SyntaxError";
        } catch (e) { ok = false; }
        ok
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_strict_direct_eval_does_not_leak_var_declarations_across_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var ok = false;
        try {
          function* g(){
            "use strict";
            eval(yield 0);
            return typeof x;
          }
          var it = g();
          it.next();
          var r = it.next("var x = 1");
          ok = r.done === true && r.value === "undefined";
        } catch (e) { ok = false; }
        ok
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_direct_eval_var_decl_conflicts_with_outer_let_across_yield() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var ok = false;
        try {
          function* g(){
            let x = 1;
            try { eval(yield 0); return "no error"; }
            catch (e) { return e.name; }
          }
          var it = g();
          it.next();
          var r = it.next("var x = 2");
          ok = r.done === true && r.value === "SyntaxError";
        } catch (e) { ok = false; }
        ok
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
