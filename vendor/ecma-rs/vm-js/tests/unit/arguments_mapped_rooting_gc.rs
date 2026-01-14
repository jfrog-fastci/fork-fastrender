use crate::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime_with_tiny_gc() -> Result<JsRuntime, VmError> {
  // Keep the heap small enough that allocations in function prologues reliably trigger GC, but
  // large enough for full realm + intrinsics initialization.
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1));
  JsRuntime::new(vm, heap)
}

#[test]
fn mapped_arguments_instantiation_roots_transient_strings_across_gc() -> Result<(), VmError> {
  let mut rt = new_runtime_with_tiny_gc()?;

  let gc_before = rt.heap.gc_runs();

  // Sloppy-mode functions with a simple parameter list create a "mapped" `arguments` object whose
  // indexed properties are accessors that alias the corresponding parameter bindings.
  //
  // Instantiating that mapped arguments object allocates temporary strings for:
  // - array index property keys (`"0"`, `"1"`, ...)
  // - empty getter/setter names, and
  // - parameter name slots.
  //
  // Each allocation can trigger GC under tight heap limits, so these transient handles must be
  // rooted across subsequent allocations during the prologue.
  let value = rt.exec_script(
    r#"
      (function(a, b, c) {
        return arguments[0] + arguments[1] + arguments[2];
      })(1, 2, 3)
    "#,
  )?;

  assert_eq!(value, Value::Number(6.0));
  assert!(
    rt.heap.gc_runs() > gc_before,
    "expected mapped arguments instantiation to trigger GC under tiny heap limits"
  );

  Ok(())
}

