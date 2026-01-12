use vm_js::{Budget, Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime_with_fuel(fuel: u64) -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap).unwrap();
  rt.vm.set_budget(Budget {
    fuel: Some(fuel),
    deadline: None,
    check_time_every: 1,
  });
  rt
}

#[test]
fn array_prototype_concat_throws_type_error_when_result_length_exceeds_max_safe_integer() {
  // Guard this test with a small fuel budget so regressions fail fast (termination) rather than
  // hanging in an O(len) concat loop.
  let mut rt = new_runtime_with_fuel(1_000);

  let value = rt
    .exec_script(
      r#"
      var ok1 = false;
      var spreadableLengthOutOfRange = {};
      spreadableLengthOutOfRange.length = Number.MAX_SAFE_INTEGER;
      spreadableLengthOutOfRange[Symbol.isConcatSpreadable] = true;
      try {
        [1].concat(spreadableLengthOutOfRange);
      } catch (e) {
        ok1 = e.name === "TypeError";
      }

      var ok2 = false;
      var proxyForArrayWithLengthOutOfRange = new Proxy([], {
        get: function(_target, key) {
          if (key === "length") {
            return Number.MAX_SAFE_INTEGER;
          }
        },
      });
      try {
        [].concat(1, proxyForArrayWithLengthOutOfRange);
      } catch (e) {
        ok2 = e.name === "TypeError";
      }

      ok1 && ok2;
    "#,
    )
    .unwrap();

  assert_eq!(value, Value::Bool(true));
}
