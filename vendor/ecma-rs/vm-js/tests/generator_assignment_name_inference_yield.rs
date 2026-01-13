use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_assignment_sets_anonymous_function_name_for_binding() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      (function () {
        function* g() {
          var f;
          f = ((yield 1) && function () {});
          return f.name;
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next(true);
        return r1.done === false && r1.value === 1 && r2.done === true && r2.value === "f";
      })()
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_assignment_sets_anonymous_function_name_for_property() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      (function () {
        var captured;
        function* g() {
          var o = {};
          captured = o;
          (yield o).p = function () {};
          return captured.p.name;
        }
        var it = g();
        var r1 = it.next();
        var r2 = it.next(r1.value);
        return r2.done === true && r2.value === "p";
      })()
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

