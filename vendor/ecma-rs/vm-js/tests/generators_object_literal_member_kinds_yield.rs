use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_object_literal_method_member_can_resume_after_yield_in_value() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          function* g(){
            var o = {
              a: yield 1,
              m() { return 42; }
            };
            return o.a === "x" && o.m() === 42;
          }
          var it = g();
          var r1 = it.next();
          if (r1.done !== false || r1.value !== 1) return false;
          var r2 = it.next("x");
          return r2.done === true && r2.value === true;
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_object_literal_getter_member_can_resume_after_yield_in_value() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          function* g(){
            var o = {
              a: yield 1,
              get x() { return 7; }
            };
            return o.a === "x" && o.x === 7;
          }
          var it = g();
          var r1 = it.next();
          if (r1.done !== false || r1.value !== 1) return false;
          var r2 = it.next("x");
          return r2.done === true && r2.value === true;
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn generator_object_literal_setter_member_can_resume_after_yield_in_value() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          function* g(){
            var seen = 0;
            var o = {
              a: yield 1,
              set x(v) { seen = v; }
            };
            o.x = 5;
            return seen;
          }
          var it = g();
          var r1 = it.next();
          if (r1.done !== false || r1.value !== 1) return false;
          var r2 = it.next("x");
          return r2.done === true && r2.value === 5;
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
