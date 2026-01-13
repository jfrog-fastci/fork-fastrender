use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn sloppy_block_function_decl_captures_block_lexicals() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function outer(one) {
          var x = one + 1;
          let y = one + 2;
          const u = one + 4;
          {
            let z = one + 3;
            const v = one + 5;
            function f() {
              return one + x + y + z + u + v;
            }
            return f();
          }
        }
        outer(1) === 21
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn sloppy_block_function_decl_updates_outer_var_binding() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function outer() {
          { function f() { return 1; } }
          return typeof f;
        }
        outer() === "function"
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

