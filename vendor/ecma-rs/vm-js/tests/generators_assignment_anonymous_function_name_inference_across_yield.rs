use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_binding_assignment_sets_anonymous_function_name_across_yield() {
  let mut rt = new_runtime();
  let v = rt
    .exec_script(
      r#"
        function* g() {
          var f;
          f = (yield 0, function () {});
          return f.name;
        }

        var it = g();
        var r1 = it.next();
        var r2 = it.next();

        r1.value === 0 && r1.done === false &&
        r2.value === "f" && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(v, Value::Bool(true));
}

#[test]
fn generator_property_assignment_sets_anonymous_function_name_across_yield() {
  let mut rt = new_runtime();
  let v = rt
    .exec_script(
      r#"
        function* g() {
          var o = {};
          o.m = (yield 0, function () {});
          return o.m.name;
        }

        var it = g();
        var r1 = it.next();
        var r2 = it.next();

        r1.value === 0 && r1.done === false &&
        r2.value === "m" && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(v, Value::Bool(true));
}

#[test]
fn generator_binding_assignment_sets_anonymous_class_name_across_yield() {
  let mut rt = new_runtime();
  let v = rt
    .exec_script(
      r#"
        function* g() {
          var C;
          C = (yield 0, class {});
          return C.name;
        }

        var it = g();
        var r1 = it.next();
        var r2 = it.next();

        r1.value === 0 && r1.done === false &&
        r2.value === "C" && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(v, Value::Bool(true));
}

#[test]
fn generator_property_assignment_sets_anonymous_class_name_across_yield() {
  let mut rt = new_runtime();
  let v = rt
    .exec_script(
      r#"
        function* g() {
          var o = {};
          o.C = (yield 0, class {});
          return o.C.name;
        }

        var it = g();
        var r1 = it.next();
        var r2 = it.next();

        r1.value === 0 && r1.done === false &&
        r2.value === "C" && r2.done === true
      "#,
    )
    .unwrap();
  assert_eq!(v, Value::Bool(true));
}
