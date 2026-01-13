use vm_js::{Budget, Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime_with_fuel(fuel: u64) -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Concat-spreading a large TypedArray allocates a large dense Array result. Use a larger heap
  // budget than most vm-js tests so failures reflect semantic/perf regressions rather than OOM.
  let heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap).unwrap();
  rt.vm.set_budget(Budget {
    fuel: Some(fuel),
    deadline: None,
    check_time_every: 1,
  });
  rt
}

#[test]
fn array_prototype_concat_large_typed_array_completes_under_budget() {
  // This is a focused regression test for `Array.prototype.concat` when spreading TypedArrays with
  // `@@isConcatSpreadable = true`. The operation must be close to linear and include periodic
  // `Vm::tick()` checks so the VM can enforce fuel/deadline budgets.
  let mut rt = new_runtime_with_fuel(5_000);

  let value = rt
    .exec_script(
      r#"
      const N = 50_000;
      const ta = new Uint32Array(N);
      ta[Symbol.isConcatSpreadable] = true;
      const out = [].concat(ta);
      out.length === N && out[0] === 0 && out[N - 1] === 0;
    "#,
    )
    .unwrap();

  assert_eq!(value, Value::Bool(true));
}

