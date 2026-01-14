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

// Regression tests for async `super` property assignment/update when the derived constructor `this`
// binding is captured as a heap-owned `DerivedConstructorState` (e.g. by an async arrow function
// created before `super()`).

#[test]
fn async_super_property_assignment_in_derived_constructor_arrow() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = "";
      class B {}
      B.prototype.x = 1;
      class D extends B {
        constructor() {
          // Create the async arrow before `super()` so it captures the derived-constructor `this`
          // binding via `DerivedConstructorState`.
          const f = async () => {
            super.x = await Promise.resolve(2);
            return String(this.x);
          };
          super();
          f().then(v => out = v);
        }
      }
      new D();
    "#,
  )?;

  // No microtasks run yet.
  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "2");
  Ok(())
}

#[test]
fn async_super_computed_property_assignment_in_derived_constructor_arrow() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = "";
      class B {}
      B.prototype.x = 1;
      class D extends B {
        constructor() {
          const f = async () => {
            super[await Promise.resolve("x")] = await Promise.resolve(5);
            return String(this.x);
          };
          super();
          f().then(v => out = v);
        }
      }
      new D();
    "#,
  )?;

  // No microtasks run yet.
  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "5");
  Ok(())
}

#[test]
fn async_super_computed_property_update_in_derived_constructor_arrow() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = "";
      class B {}
      B.prototype.x = 1;
      class D extends B {
        constructor() {
          const f = async () => {
            const old = super[await Promise.resolve("x")]++;
            return old + "," + this.x;
          };
          super();
          f().then(v => out = String(v));
        }
      }
      new D();
    "#,
  )?;

  // No microtasks run yet.
  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "1,2");
  Ok(())
}

