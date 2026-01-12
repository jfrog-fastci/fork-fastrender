use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn array_prototype_at_basic_indices_and_holes() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      const a = [1, 2, , 4];
      a.at(0) === 1 &&
      a.at(1) === 2 &&
      a.at(2) === undefined &&
      a.at(-1) === 4 &&
      a.at(-4) === 1 &&
      a.at(99) === undefined &&
      a.at(-99) === undefined
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn array_prototype_at_coerces_index_once_and_truncates_toward_zero() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      const a = [0, 1, 2];
      let calls = 0;
      const idx = { valueOf() { calls++; return 1.9; } };
      a.at(idx) === 1 && calls === 1
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn array_prototype_at_has_expected_property_shape() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      const desc = Object.getOwnPropertyDescriptor(Array.prototype, "at");
      desc &&
      desc.writable === true &&
      desc.enumerable === false &&
      desc.configurable === true &&
      typeof desc.value === "function" &&
      desc.value.name === "at" &&
      desc.value.length === 1
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

