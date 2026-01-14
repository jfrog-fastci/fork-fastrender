use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn for_of_destructuring_default_observes_tdz() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          try {
            for (let { x = x } of [{}]) {}
            return false;
          } catch (e) {
            return e && e.name === "ReferenceError";
          }
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn for_in_destructuring_default_observes_tdz() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        (() => {
          try {
            for (let { x = x } in { a: 1 }) {}
            return false;
          } catch (e) {
            return e && e.name === "ReferenceError";
          }
        })()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

