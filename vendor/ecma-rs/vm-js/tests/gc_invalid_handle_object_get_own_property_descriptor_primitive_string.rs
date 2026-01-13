use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime_with_frequent_gc() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Force frequent GC so missing roots manifest as stale handles.
  let heap = Heap::new(HeapLimits::new(16 * 1024 * 1024, 64 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn object_get_own_property_descriptor_on_primitive_string_does_not_use_stale_handles() -> Result<(), VmError> {
  let mut rt = new_runtime_with_frequent_gc();

  // Derived from test262:
  // `built-ins/Object/getOwnPropertyDescriptor/primitive-string.js`.
  let value = rt.exec_script(
    r#"
      (() => {
        // Heap pressure so GC runs during property descriptor construction.
        var junk = new Uint8Array(200000);

        var d0 = Object.getOwnPropertyDescriptor('', '0');
        if (d0 !== undefined) return false;

        var indexDesc = Object.getOwnPropertyDescriptor('foo', '0');
        if (indexDesc.value !== 'f') return false;
        if (indexDesc.writable !== false) return false;
        if (indexDesc.enumerable !== true) return false;
        if (indexDesc.configurable !== false) return false;

        var lengthDesc = Object.getOwnPropertyDescriptor('foo', 'length');
        if (lengthDesc.value !== 3) return false;
        if (lengthDesc.writable !== false) return false;
        if (lengthDesc.enumerable !== false) return false;
        if (lengthDesc.configurable !== false) return false;

        return true;
      })()
    "#,
  )?;

  assert_eq!(value, Value::Bool(true));
  Ok(())
}

