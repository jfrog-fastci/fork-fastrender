use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime_with_frequent_gc() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Force GC during generator-mode tagged template evaluation so missing roots manifest as stale
  // handles.
  //
  // Keep `max_bytes` large enough that runtime initialization and the test can complete without OOM.
  let heap = Heap::new(HeapLimits::new(16 * 1024 * 1024, 64 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_tagged_template_roots_this_and_partial_args_across_gc() -> Result<(), VmError> {
  let mut rt = new_runtime_with_frequent_gc();

  // The tagged template contains `yield`, so it is evaluated by the generator continuation
  // evaluator. That evaluator must root:
  // - the callee + `this` value (which can be ephemeral), and
  // - the template object + partially built substitution list
  // across evaluation of later substitutions, since later substitutions can allocate/GC before
  // reaching the next yield boundary.
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
        // Both the tag base object and the first substitution value are ephemeral objects that are
        // only referenced by Rust locals until the tag call completes.
        //
        // Force GC during evaluation of the *second* substitution, before reaching `yield`, so we
        // exercise rooting of the partially-built substitution list across allocation.
        return ({ add: 2, tag: function(strings, obj, v) { return obj.x + v + this.add; } })
          .tag`${({ x: 40 })}${(churn(), (yield 1))}`;
      }

      let it = g();
      let r1 = it.next();
      // Trigger GC while the generator is suspended (so its continuation frames are traced).
      churn();
      let r2 = it.next(0);

      r1.value === 1 && r1.done === false &&
      r2.value === 42 && r2.done === true;
    "#,
  )?;

  assert_eq!(value, Value::Bool(true));
  Ok(())
}

