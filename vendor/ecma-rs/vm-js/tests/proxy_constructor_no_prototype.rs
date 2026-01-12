use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

#[test]
fn proxy_constructor_has_no_prototype_property() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  // ECMA-262: `%Proxy%` does not have an own `"prototype"` property (test262:
  // `built-ins/Proxy/proxy-no-prototype.js`).
  let ok = rt.exec_script(
    r#"
      typeof Proxy === "function" &&
        !Object.prototype.hasOwnProperty.call(Proxy, "prototype") &&
        typeof new Proxy({}, {}) === "object"
    "#,
  )?;
  assert_eq!(ok, Value::Bool(true));

  Ok(())
}

