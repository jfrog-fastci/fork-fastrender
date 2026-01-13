use vm_js::{
  Heap, HeapLimits, JsRuntime, PropertyDescriptor, PropertyKey, PropertyKind, Value, Vm, VmError,
  VmOptions,
};

fn define_global_string(rt: &mut JsRuntime, name: &str, units: Vec<u16>) -> Result<(), VmError> {
  let s = {
    let mut scope = rt.heap_mut().scope();
    scope.alloc_string_from_u16_vec(units)?
  };

  let global = rt.realm().global_object();
  let mut scope = rt.heap_mut().scope();
  scope.push_roots(&[Value::Object(global), Value::String(s)])?;

  let key_s = scope.alloc_string(name)?;
  scope.push_root(Value::String(key_s))?;

  let desc = PropertyDescriptor {
    enumerable: true,
    configurable: true,
    kind: PropertyKind::Data {
      value: Value::String(s),
      writable: true,
    },
  };
  let key = PropertyKey::from_string(key_s);
  scope.define_property(global, key, desc)?;
  Ok(())
}

#[test]
fn regexp_exec_capture_substrings_respect_heap_limits() -> Result<(), VmError> {
  // Allocate a large string that fits within a small heap, then attempt to `exec` a regexp that
  // would need to allocate a similarly large match substring. This should fail with
  // `VmError::OutOfMemory` without attempting large intermediate off-heap allocations.
  let max_bytes = 4 * 1024 * 1024; // 4 MiB
  let heap = Heap::new(HeapLimits::new(max_bytes, max_bytes));
  let vm = Vm::new(VmOptions::default());
  let mut rt = JsRuntime::new(vm, heap)?;

  // Size the input so it fits, but leaves insufficient headroom for an additional capture string of
  // the same size. (Heap accounting includes runtime overhead, so derive from current usage.)
  let base = rt.heap().estimated_total_bytes();
  let available = max_bytes.saturating_sub(base);
  // Use >50% of the remaining headroom so duplicating the input into a match string exceeds the
  // heap limit.
  let input_payload_bytes = available.saturating_mul(3) / 5;
  let len_units = (input_payload_bytes / 2).max(1);

  let mut units: Vec<u16> = Vec::new();
  units
    .try_reserve_exact(len_units)
    .map_err(|_| VmError::OutOfMemory)?;
  units.resize(len_units, b'a' as u16);
  define_global_string(&mut rt, "S", units)?;

  let err = rt.exec_script("new RegExp('a+').exec(S);").unwrap_err();
  assert!(matches!(err, VmError::OutOfMemory));
  Ok(())
}

