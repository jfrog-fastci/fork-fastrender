use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn array_prototype_to_sorted_array_create_happens_before_element_get() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      let called = 0;
      const obj = { length: 4294967296 }; // 2**32
      Object.defineProperty(obj, "0", {
        get() { called += 1; return 1; }
      });

      let ok = false;
      try {
        Array.prototype.toSorted.call(obj);
      } catch (e) {
        ok = e && e.name === "RangeError";
      }

      ok && called === 0
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn array_prototype_to_spliced_throws_type_error_when_new_len_exceeds_max_safe_integer() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      let called = 0;
      const obj = { length: 9007199254740991 }; // 2**53 - 1
      Object.defineProperty(obj, "0", {
        get() { called += 1; return 1; }
      });

      let ok = false;
      try {
        // newLen = (2**53 - 1) + 1 - 0 => 2**53, which should throw TypeError.
        Array.prototype.toSpliced.call(obj, 0, 0, "x");
      } catch (e) {
        ok = e && e.name === "TypeError";
      }

      ok && called === 0
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

