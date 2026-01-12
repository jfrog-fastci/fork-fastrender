use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> Result<JsRuntime, VmError> {
  let vm = Vm::new(VmOptions::default());
  // Proxy invariants tests exercise host-aware internal method dispatch. Keep the heap modest but
  // leave room for the intrinsic graph and any Promise/microtask allocations triggered by
  // incidental engine work.
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap)
}

#[test]
fn proxy_get_prototype_of_trap_must_match_non_extensible_target() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  let v = rt.exec_script(
    r#"
      let proto = {};
      let target = Object.create(proto);
      Object.preventExtensions(target);

      let other = {};
      let p = new Proxy(target, { getPrototypeOf() { return other; } });

      try {
        Object.getPrototypeOf(p);
        false;
      } catch (e) {
        e instanceof TypeError;
      }
    "#,
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn proxy_get_prototype_of_trap_can_return_target_proto_for_non_extensible_target() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  let v = rt.exec_script(
    r#"
      let proto = {};
      let target = Object.create(proto);
      Object.preventExtensions(target);

      let p = new Proxy(target, { getPrototypeOf() { return proto; } });
      Object.getPrototypeOf(p) === proto;
    "#,
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn proxy_get_prototype_of_trap_null_matches_non_extensible_null_proto_target() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  let v = rt.exec_script(
    r#"
      let target = Object.create(null);
      Object.preventExtensions(target);

      let p = new Proxy(target, { getPrototypeOf() { return null; } });
      Object.getPrototypeOf(p) === null;
    "#,
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn proxy_get_prototype_of_trap_result_is_unconstrained_for_extensible_target() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  let v = rt.exec_script(
    r#"
      let other = {};
      let target = {};
      let p = new Proxy(target, { getPrototypeOf() { return other; } });
      Object.getPrototypeOf(p) === other;
    "#,
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

