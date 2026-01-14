use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime_with_frequent_gc() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Small `gc_threshold` so missing stack roots manifest as stale handles during GC validation.
  let heap = Heap::new(HeapLimits::new(16 * 1024 * 1024, 64 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_computed_member_assignment_roots_key_across_gc() -> Result<(), VmError> {
  let mut rt = new_runtime_with_frequent_gc();

  // Repro:
  // - The computed member key contains `yield`, so it resumes via `GenFrame::AssignComputedMemberAfterMember`.
  // - On resume, `ToPropertyKey` converts `12345` to a fresh string that is not reachable from any object.
  // - The RHS runs `churn()` (alloc/GC) *before* hitting the next `yield`, so the computed key must be
  //   rooted across RHS evaluation.
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
        let o = {};
        o[(yield 0)] = (churn(), (yield 1));
        return o;
      }

      let it = g();
      let r0 = it.next();
      churn(); // GC while suspended after key yield
      let r1 = it.next(12345);
      churn(); // GC while suspended at RHS yield, tracing the continuation
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

