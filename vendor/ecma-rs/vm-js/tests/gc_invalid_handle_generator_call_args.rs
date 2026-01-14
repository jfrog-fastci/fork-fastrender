use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime_with_frequent_gc() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Force GC during generator continuation argument evaluation so missing roots manifest as stale
  // handles.
  //
  // Keep `max_bytes` large enough that runtime initialization and the test can complete without OOM.
  let heap = Heap::new(HeapLimits::new(16 * 1024 * 1024, 64 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_call_roots_callee_and_this_across_arg_gc() -> Result<(), VmError> {
  let mut rt = new_runtime_with_frequent_gc();

  // The call expression contains `yield`, so it is evaluated by the generator continuation
  // evaluator rather than the synchronous evaluator. That evaluator must root the callee + `this`
  // value across argument evaluation, since argument evaluation can allocate and trigger GC even
  // when it does not itself suspend.
  //
  // Repro shape (in JS):
  //   ({ m() { ... } }).m(<allocates>, (yield ...))
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
        return ({ m: function(a, b) { return a + b; } }).m(churn(), (yield 1));
      }

      let it = g();
      let r1 = it.next();
      let r2 = it.next(2);
      r1.value === 1 && r1.done === false && r2.value === 202 && r2.done === true;
    "#,
  )?;

  assert_eq!(value, Value::Bool(true));
  Ok(())
}

