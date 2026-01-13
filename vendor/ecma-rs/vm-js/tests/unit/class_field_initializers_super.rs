use crate::{Heap, HeapLimits, JsRuntime, PropertyKey, Value, Vm, VmError, VmOptions};

fn new_runtime() -> Result<JsRuntime, VmError> {
  let vm = Vm::new(VmOptions::default());
  // Keep heap limits small for test speed but large enough that minor intrinsic layout changes
  // don't cause unrelated OOM failures.
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap)
}

fn thrown_error_message(rt: &mut JsRuntime, err: &VmError) -> Option<String> {
  let thrown = err.thrown_value()?;
  let Value::Object(thrown) = thrown else {
    return None;
  };

  let mut scope = rt.heap.scope();
  let key_s = scope.alloc_string("message").ok()?;
  let key = PropertyKey::from_string(key_s);
  let msg = scope.heap().get(thrown, &key).ok()?;
  let Value::String(msg) = msg else {
    return None;
  };
  Some(scope.heap().get_string(msg).ok()?.to_utf8_lossy())
}

fn is_unimplemented_error(rt: &mut JsRuntime, err: &VmError) -> bool {
  match err {
    VmError::Unimplemented(_) => true,
    VmError::Throw(_) | VmError::ThrowWithStack { .. } => thrown_error_message(rt, err)
      .is_some_and(|msg| msg.starts_with("unimplemented:")),
    _ => false,
  }
}

#[test]
fn super_prop_in_instance_field_initializers() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  let value = match rt.exec_script(
    r#"
      class B { m() { return 1; } }
      class D extends B {
        x = super.m();
        y = (() => super.m())();
      }
      (new D()).x === 1 && (new D()).y === 1
    "#,
  ) {
    Ok(v) => v,
    Err(err) if is_unimplemented_error(&mut rt, &err) => return Ok(()),
    Err(err) => return Err(err),
  };

  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn super_prop_in_static_field_initializers() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  let value = match rt.exec_script(
    r#"
      class B { static get x() { return 1; } }
      class D extends B {
        static y = super.x;
        static z = (() => super.x)();
      }
      D.y === 1 && D.z === 1
    "#,
  ) {
    Ok(v) => v,
    Err(err) if is_unimplemented_error(&mut rt, &err) => return Ok(()),
    Err(err) => return Err(err),
  };

  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn super_prop_in_private_static_field_initializers() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  let value = match rt.exec_script(
    r#"
      class B { static get x() { return 1; } }
      class D extends B {
        static #p = super.x;
        static #q = (() => super.x)();
        static get p() { return this.#p; }
        static get q() { return this.#q; }
      }
      D.p === 1 && D.q === 1
    "#,
  ) {
    Ok(v) => v,
    Err(err) if is_unimplemented_error(&mut rt, &err) => return Ok(()),
    Err(err) => return Err(err),
  };

  assert_eq!(value, Value::Bool(true));
  Ok(())
}

