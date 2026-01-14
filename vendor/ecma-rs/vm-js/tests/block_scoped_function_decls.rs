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

#[test]
fn sloppy_block_function_decl_is_hoisted_as_undefined_var() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function outer(cond) {
          if (cond) { function f() { return 1; } }
          return f;
        }

        // If the block is not executed, Annex B requires `f` to exist as a `var` binding with the
        // value `undefined` (not as an unbound identifier).
        outer(false) === undefined &&
          // If the block is executed, the var binding is updated to the block-scoped function.
          outer(true)() === 1
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn sloppy_global_block_function_decl_is_hoisted_as_undefined_global_var() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        // Annex B: even if the block is not executed, a global `var` binding is created.
        if (false) { function f() { return 1; } }
        f === undefined
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}
