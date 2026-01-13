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

