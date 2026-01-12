use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn array_keys_spread() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"var a=[10,20]; var k=[...a.keys()]; k.length===2 && k[0]===0 && k[1]===1"#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn array_entries_spread() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"var a=[10,20]; var e=[...a.entries()]; e.length===2 && e[0][0]===0 && e[0][1]===10 && e[1][0]===1 && e[1][1]===20"#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn array_iterator_is_iterable() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"var a=[10,20]; var it=a.keys(); it[Symbol.iterator]() === it"#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

