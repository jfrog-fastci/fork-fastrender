use vm_js::{CompiledScript, Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn assert_value_is_number(value: Value, expected: f64) {
  let Value::Number(n) = value else {
    panic!("expected number, got {value:?}");
  };
  assert_eq!(n, expected);
}

fn exec_compiled(rt: &mut JsRuntime, source: &str) -> Result<Value, VmError> {
  let script = CompiledScript::compile_script(rt.heap_mut(), "<super_get_super_base>", source)?;
  rt.exec_compiled_script(script)
}

#[test]
fn super_get_super_base_uses_internal_get_prototype_of_not_proto_property() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      let proto = { x: 1 };
      let obj = { __proto__: proto, m(){ return super.x; } };
      Object.defineProperty(obj, "__proto__", { get(){ throw "poison"; } });
      let out;
      try { out = obj.m(); } catch (e) { out = e; }
      out;
    "#,
  )?;

  assert_value_is_number(value, 1.0);
  Ok(())
}

#[test]
fn super_get_super_base_uses_internal_get_prototype_of_not_proto_property_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(
    &mut rt,
    r#"
      let proto = { x: 1 };
      let obj = { __proto__: proto, m(){ return super.x; } };
      Object.defineProperty(obj, "__proto__", { get(){ throw "poison"; } });
      let out;
      try { out = obj.m(); } catch (e) { out = e; }
      out;
    "#,
  )?;

  assert_value_is_number(value, 1.0);
  Ok(())
}

