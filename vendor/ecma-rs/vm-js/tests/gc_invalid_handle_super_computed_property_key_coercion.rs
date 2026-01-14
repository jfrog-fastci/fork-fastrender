use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime_with_frequent_gc() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Keep `max_bytes` high enough that runtime initialization and the test can complete without OOM,
  // while using a small `gc_threshold` so we can reliably trigger a GC from inside `ToPropertyKey`.
  let heap = Heap::new(HeapLimits::new(16 * 1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn super_computed_property_roots_super_base_across_key_coercion_gc() -> Result<(), VmError> {
  let mut rt = new_runtime_with_frequent_gc();

  // Repro:
  // - `super[key]` computes `GetSuperBase()` before `ToPropertyKey(key)`.
  // - `key.toString()` mutates the home object's prototype, so the old super base becomes
  //   unreachable from any object.
  // - `key.toString()` allocates enough to trigger GC while key coercion is in progress.
  //
  // The resolved super base must be rooted across `ToPropertyKey` so it cannot be collected during
  // that GC cycle.
  let value = rt.exec_script(
    r#"
      'use strict';

      function churn() {
        // Allocate enough external memory that (given the configured `gc_threshold`) this reliably
        // forces at least one GC cycle.
        return (new Uint8Array(2 * 1024 * 1024)).length;
      }

      let proto2 = { p: "bad" };

      let obj = {
        m() {
          return super[key];
        }
      };

      // Don't keep a reference to the old prototype; after the prototype mutation in `toString`,
      // it should be collectible except for the rooted super base.
      Object.setPrototypeOf(obj, { p: "ok" });

      let key = {
        toString() {
          Object.setPrototypeOf(obj, proto2);
          churn(); // allocate/GC while key coercion is running
          return "p";
        }
      };

      obj.m() === "ok" && Object.getPrototypeOf(obj) === proto2;
    "#,
  )?;

  assert_eq!(value, Value::Bool(true));
  Ok(())
}
