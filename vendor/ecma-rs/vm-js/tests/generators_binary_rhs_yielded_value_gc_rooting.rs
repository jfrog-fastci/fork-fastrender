use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime_gc() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Trigger GC on virtually every allocation so any missing temporary roots while suspending a
  // generator will be caught deterministically.
  let heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 0));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_binary_rhs_yielded_object_survives_gc_while_rooting_left_operand() {
  let mut rt = new_runtime_gc();

  // RHS yields an object. While suspending, the generator evaluator needs to capture the LHS for
  // later resumption, which pushes roots and can trigger GC. The yielded object must not be
  // collected during that GC cycle.
  rt
    .exec_script(
      r#"
      function* g() { return 0 + (yield {}); }
      var it = g();
    "#,
    )
    .unwrap();

  // Ensure `root_stack` capacity is minimized so the upcoming generator suspension needs to grow it,
  // which in turn triggers GC due to the aggressive heap limits above.
  rt.heap_mut().collect_garbage();

  let gc_before = rt.heap.gc_runs();
  let value = rt
    .exec_script(
      r#"
      var r1 = it.next();
      r1.done === false &&
      typeof r1.value === "object" &&
      r1.value !== null
    "#,
    )
    .unwrap();
  let gc_after = rt.heap.gc_runs();

  assert_eq!(value, Value::Bool(true));
  assert!(
    gc_after > gc_before,
    "expected generator suspension to trigger at least one GC cycle"
  );
}

