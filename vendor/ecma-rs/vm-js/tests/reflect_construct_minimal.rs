use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn assert_value_is_utf8(rt: &JsRuntime, value: Value, expected: &str) {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  let actual = rt.heap().get_string(s).unwrap().to_utf8_lossy();
  assert_eq!(actual, expected);
}

#[test]
fn reflect_object_and_construct_exist_and_work_minimally() {
  let mut rt = new_runtime();

  let value = rt
    .exec_script(r#"typeof Reflect === "object" && typeof Reflect.construct === "function""#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));

  // `newTarget` must be a constructor; arrow functions are callable but not constructable.
  let value = rt
    .exec_script(r#"try { Reflect.construct(function(){}, [], (()=>{})); "no error"; } catch(e) { e.name }"#)
    .unwrap();
  assert_value_is_utf8(&rt, value, "TypeError");

  let value = rt
    .exec_script(r#"Reflect.construct(function(){ this.x=1; }, [], function(){}).x === 1"#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

