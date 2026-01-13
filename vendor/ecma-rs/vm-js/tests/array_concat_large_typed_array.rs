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
      if (!(out.length === N && out[0] === 0 && out[N - 1] === 0)) {
        false;
      } else {
        // TypedArray with an overridden `"length"` property. Concat must respect the observable
        // `length` (creating holes past the real typed-array length) without devolving into an
        // O(len) loop for attacker-controlled fake lengths.
        const M = 200_000;
        const ta2 = new Uint8Array(1);
        ta2[0] = 7;
        Object.defineProperty(ta2, "length", { value: M });
        ta2[Symbol.isConcatSpreadable] = true;
        const out2 = [].concat(ta2);
        out2.length === M && out2[0] === 7 && out2[M - 1] === undefined && !((M - 1) in out2);
      }
     "#,
    )
    .unwrap();

  assert_eq!(value, Value::Bool(true));
}
