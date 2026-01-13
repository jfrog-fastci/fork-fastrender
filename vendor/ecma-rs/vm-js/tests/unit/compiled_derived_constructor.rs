use crate::{CompiledScript, Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn exec_compiled(rt: &mut JsRuntime, source: &str) -> Result<Value, VmError> {
  let script = CompiledScript::compile_script(&mut rt.heap, "<compiled_derived_ctor>", source)?;
  rt.exec_compiled_script(script)
}

fn assert_value_is_utf8(rt: &JsRuntime, value: Value, expected: &str) {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  let actual = rt.heap.get_string(s).unwrap().to_utf8_lossy();
  assert_eq!(actual, expected);
}

#[test]
fn compiled_derived_constructor_super_initializes_this() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let value = exec_compiled(
    &mut rt,
    r#"
      class B {}
      class D extends B {
        constructor() {
          super();
          this.x = 1;
        }
      }
      new D().x === 1
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn compiled_derived_constructor_this_before_super_throws_reference_error() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let value = exec_compiled(
    &mut rt,
    r#"
      class B {}
      class D extends B {
        constructor() {
          this;
        }
      }
      try { new D(); "no"; } catch (e) { e.name; }
    "#,
  )?;
  assert_value_is_utf8(&rt, value, "ReferenceError");
  Ok(())
}

#[test]
fn compiled_derived_constructor_super_called_twice_throws_reference_error() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let value = exec_compiled(
    &mut rt,
    r#"
      class B {}
      class D extends B {
        constructor() {
          super();
          super();
        }
      }
      try { new D(); "no"; } catch (e) { e.name; }
    "#,
  )?;
  assert_value_is_utf8(&rt, value, "ReferenceError");
  Ok(())
}

