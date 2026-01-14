use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Configure the heap so we can:
  // - drive the heap close to its max size (to trigger root-stack capacity shrinking), and
  // - trigger a GC during generator resumption when the root stack needs to grow.
  let heap = Heap::new(HeapLimits::new(32 * 1024 * 1024, 24 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_computed_member_assignment_keeps_base_alive_across_yield() -> Result<(), VmError> {
  let mut rt = new_runtime();

  // 1) Create and suspend the generator.
  let value = rt.exec_script(
    r#"
      function* g() {
        // Base object is not bound to any variable, so it is only kept alive by the generator
        // continuation frame created for the computed key expression.
        ({})[yield 0] = 1;
        return 2;
      }
      // Pass enough arguments so resuming the generator will need to grow the VM root stack.
      var args = [];
      for (var i = 0; i < 256; i++) args.push(0);
      globalThis.it = g(...args);
      var r1 = it.next();
      r1.value === 0 && r1.done === false
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  assert_eq!(rt.heap.stack_root_len(), 0, "exec_script should leave no stack roots");

  // 2) Make the heap appear "nearly full" so the outermost rooting scope will opportunistically
  // reset `root_stack` capacity when dropped (see `Scope::drop`).
  //
  // Keep this token alive until after resumption so root-stack growth triggers a GC.
  let limits = rt.heap.limits();
  let baseline = rt.heap.estimated_total_bytes();
  // Leave enough headroom for allocations during resumption (iterator result objects, root stack
  // growth, etc).
  let headroom = 2 * 1024 * 1024;
  let max_target = limits.max_bytes.saturating_sub(headroom);
  // Push the heap comfortably over both the "shrink root stack" threshold and the GC threshold.
  let min_target = (limits.max_bytes.saturating_mul(3) / 4).saturating_add(headroom);
  let target = min_target.min(max_target);
  let charge = target.saturating_sub(baseline);
  let _pressure = rt.heap.charge_external(charge.max(1))?;

  // 3) Force the root stack to grow beyond the shrink threshold, then drop the outermost scope.
  // With the heap near its limit, this should reset root-stack capacity back to 0.
  {
    assert_eq!(rt.heap.stack_root_len(), 0);
    let mut scope = rt.heap.scope();
    let roots = vec![Value::Number(0.0); 1024];
    scope.push_roots(&roots)?;
  }
  assert_eq!(rt.heap.stack_root_len(), 0);

  // 4) Resume the generator. `gen_root_values_for_continuation` must treat the base object stored
  // in the continuation frame as a GC root while growing the root stack; otherwise the GC triggered
  // by that growth can collect the base before the assignment completes.
  let gc_before = rt.heap.gc_runs();
  let value = rt.exec_script(
    r#"
      var r2 = it.next("x");
      r2.value === 2 && r2.done === true
    "#,
  )?;
  assert!(
    rt.heap.gc_runs() > gc_before,
    "expected generator resumption to trigger a GC while heap is over gc_threshold"
  );
  assert_eq!(value, Value::Bool(true));
  Ok(())
}
