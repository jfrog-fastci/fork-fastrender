use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime_with_frequent_gc() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Force GC during try/finally evaluation so missing roots manifest as stale handles.
  //
  // Keep `max_bytes` large enough that runtime initialization and the test can complete without OOM.
  let heap = Heap::new(HeapLimits::new(16 * 1024 * 1024, 64 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_try_finally_roots_pending_completion_across_gc() -> Result<(), VmError> {
  let mut rt = new_runtime_with_frequent_gc();

  // The try statement contains `yield` in the finally block, so it is evaluated by the generator
  // continuation evaluator. That evaluator must root the pending completion value (here: the return
  // object) while evaluating the finally block, since the finally block can allocate/GC before the
  // first `yield` is reached.
  let value = rt.exec_script(
    r#"
      'use strict';

      function churn() {
        // Allocate enough to force GC under the small `gc_threshold`.
        let junk = [];
        for (let i = 0; i < 200; i++) {
          junk.push(new Uint8Array(1024));
        }
        return junk.length;
      }

      function* g() {
        try {
          return { x: 40, y: 2 };
        } finally {
          // Run allocations before the `yield` boundary so GC can happen while the pending
          // completion value is only held in Rust locals.
          churn();
          yield 1;
        }
      }

      let it = g();
      let r1 = it.next();
      let r2 = it.next();
      r1.value === 1 && r1.done === false && r2.value.x === 40 && r2.value.y === 2 && r2.done === true;
    "#,
  )?;

  assert_eq!(value, Value::Bool(true));
  Ok(())
}
