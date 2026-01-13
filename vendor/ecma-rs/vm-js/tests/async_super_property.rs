use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // These tests use Promises/async-await; give them a slightly larger heap to avoid spurious OOMs.
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn value_to_string(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap.get_string(s).unwrap().to_utf8_lossy()
}

#[test]
fn async_instance_method_can_call_super_with_await_in_args() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = "";

      class A { f(x) { return x + 1; } }
      class B extends A {
        async g() {
          return super.f(await Promise.resolve(1));
        }
      }

      new B().g().then(v => out = String(v));
    "#,
  )?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "2");
  Ok(())
}

#[test]
fn async_class_eval_can_suspend_in_computed_key_and_static_method_can_call_super_with_await_in_args(
) -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = "";

      class A { static f(x) { return x + 1; } }

      async function run() {
        class B extends A {
          static async g() {
            // Force async-call evaluation of `super.f(...)` by placing an `await` in the argument
            // list.
            return super.f(await Promise.resolve(1));
          }
          // Suspend class evaluation while computing a class element name, then resume and ensure
          // `g` still has a valid `[[HomeObject]]` for `super` property access.
          static [await Promise.resolve("dummy")]() {}
        }
        return await B.g();
      }

      run().then(v => out = String(v));
    "#,
  )?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "2");
  Ok(())
}
