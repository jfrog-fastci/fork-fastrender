use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime_with_frequent_gc() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Force GC during generator-mode destructuring so missing roots manifest as stale handles.
  //
  // Keep `max_bytes` large enough that runtime initialization and the test can complete without OOM.
  let heap = Heap::new(HeapLimits::new(16 * 1024 * 1024, 64 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_object_destructuring_roots_excluded_keys_across_gc() -> Result<(), VmError> {
  let mut rt = new_runtime_with_frequent_gc();

  // When a destructuring pattern contains `yield`, `vm-js` evaluates it via the generator
  // continuation binder. That binder tracks an `excluded` key list for object rest patterns.
  //
  // The excluded keys are newly allocated strings and are not necessarily reachable from the source
  // object (strings are not interned). If we allocate/GC while excluded keys are only held in Rust
  // locals, GC can collect them, leaving stale handles stored in the generator continuation.
  //
  // Repro: create an excluded key ("a"), then allocate + GC before yielding. After suspending,
  // trigger another GC while the generator is suspended so the continuation is traced.
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
        let x;
        ({ a: x = (churn(), (yield 1)) } = {});
        return x;
      }

      let it = g();
      let r1 = it.next();
      // Trigger GC while the generator is suspended (so its continuation frames are traced).
      churn();
      let r2 = it.next(42);
      r1.value === 1 && r1.done === false && r2.value === 42 && r2.done === true;
    "#,
  )?;

  assert_eq!(value, Value::Bool(true));
  Ok(())
}

