use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime_with_frequent_gc() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Keep `max_bytes` high enough for runtime initialization while using a small `gc_threshold` so
  // missing generator roots surface as stale handles during GC.
  let heap = Heap::new(HeapLimits::new(16 * 1024 * 1024, 64 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_object_literal_roots_computed_key_across_gc() -> Result<(), VmError> {
  let mut rt = new_runtime_with_frequent_gc();

  // Repro:
  // - the computed key contains `yield`, so object literal evaluation resumes via
  //   `GenFrame::LitObjAfterComputedKey`.
  // - on resume, `ToPropertyKey` produces a fresh string (from a number) that is not reachable from
  //   any object.
  // - the property value expression allocates and triggers GC before reaching the next `yield`,
  //   so the key must be rooted even though it is held only in Rust locals until suspension.
  let value = rt.exec_script(
    r#"
      'use strict';

      function churn() {
        let junk = [];
        for (let i = 0; i < 200; i++) {
          junk.push(new Uint8Array(1024));
        }
        return junk.length;
      }

      function* g() {
        let obj = { [(yield 0)]: (churn(), (yield 1)) };
        return obj;
      }

      let it = g();
      let r0 = it.next();
      churn();
      let r1 = it.next(12345);
      churn();
      let r2 = it.next(42);

      r0.value === 0 && r0.done === false &&
      r1.value === 1 && r1.done === false &&
      r2.done === true &&
      r2.value['12345'] === 42;
    "#,
  )?;

  assert_eq!(value, Value::Bool(true));
  Ok(())
}

