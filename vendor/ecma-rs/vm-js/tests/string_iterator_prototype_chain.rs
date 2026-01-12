use vm_js::{
  Heap, HeapLimits, JsRuntime, PropertyKey, PropertyKind, Value, Vm, VmError, VmOptions,
};

fn new_runtime() -> Result<JsRuntime, VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap)
}

#[test]
fn string_iterator_prototype_chain_includes_iterator_prototype() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let intr = *rt.realm().intrinsics();
  let wks = *rt.realm().well_known_symbols();

  let it = rt.exec_script(r#""ab"[Symbol.iterator]()"#)?;
  let Value::Object(it) = it else {
    return Err(VmError::InvariantViolation(
      "\"ab\"[Symbol.iterator]() did not return an object",
    ));
  };

  let mut scope = rt.heap.scope();
  scope.push_root(Value::Object(it))?;

  let string_iterator_proto = scope
    .heap()
    .object_prototype(it)?
    .ok_or(VmError::InvariantViolation(
      "String iterator has no prototype",
    ))?;
  assert_ne!(string_iterator_proto, intr.object_prototype());

  let iterator_proto = scope
    .heap()
    .object_prototype(string_iterator_proto)?
    .ok_or(VmError::InvariantViolation(
      "StringIteratorPrototype has no prototype",
    ))?;
  assert_eq!(iterator_proto, intr.iterator_prototype());
  assert_eq!(
    scope.heap().object_prototype(iterator_proto)?,
    Some(intr.object_prototype())
  );

  // The iterator instance should inherit `.next` from `%StringIteratorPrototype%`.
  let next_key = PropertyKey::from_string(scope.alloc_string("next")?);
  assert!(scope.heap().object_get_own_property(it, &next_key)?.is_none());
  let next_desc = scope
    .heap()
    .object_get_own_property(string_iterator_proto, &next_key)?
    .ok_or(VmError::InvariantViolation(
      "StringIteratorPrototype missing next",
    ))?;
  assert!(!next_desc.enumerable);
  assert!(next_desc.configurable);
  match next_desc.kind {
    PropertyKind::Data { value, writable } => {
      assert!(writable);
      assert!(scope.heap().is_callable(value)?);
    }
    _ => {
      return Err(VmError::InvariantViolation(
        "StringIteratorPrototype next is not a data property",
      ));
    }
  }

  // `%StringIteratorPrototype%[@@toStringTag]` should be `"String Iterator"`.
  let tag_key = PropertyKey::from_symbol(wks.to_string_tag);
  let tag_desc = scope
    .heap()
    .object_get_own_property(string_iterator_proto, &tag_key)?
    .ok_or(VmError::InvariantViolation(
      "StringIteratorPrototype missing Symbol.toStringTag",
    ))?;
  assert!(!tag_desc.enumerable);
  assert!(tag_desc.configurable);
  match tag_desc.kind {
    PropertyKind::Data {
      value: Value::String(s),
      writable,
    } => {
      assert!(!writable);
      assert_eq!(scope.heap().get_string(s)?.to_utf8_lossy(), "String Iterator");
    }
    _ => {
      return Err(VmError::InvariantViolation(
        "StringIteratorPrototype Symbol.toStringTag is not a string data property",
      ));
    }
  }

  Ok(())
}

