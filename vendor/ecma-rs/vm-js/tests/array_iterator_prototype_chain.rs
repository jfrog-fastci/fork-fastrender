use vm_js::{
  Heap, HeapLimits, JsRuntime, PropertyKey, PropertyKind, Value, Vm, VmError, VmOptions,
};

fn new_runtime() -> Result<JsRuntime, VmError> {
  let vm = Vm::new(VmOptions::default());
  // This test executes a non-trivial inline script to observe iterator prototype chains via
  // ordinary JS operations (`Object.getPrototypeOf`, `Object.prototype.toString`, etc.). `vm-js`
  // retains `SourceText` + compiled code per `exec_script` call, so we give it a little more
  // headroom than the 1MiB heaps used by many unit tests to avoid spurious OOM failures.
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap)
}

fn get_array_index(
  scope: &mut vm_js::Scope<'_>,
  arr: vm_js::GcObject,
  index: usize,
) -> Result<Value, VmError> {
  // Root `arr` and the key string so `get_property` can allocate freely.
  scope.push_root(Value::Object(arr))?;

  let key_s = scope.alloc_string(&index.to_string())?;
  scope.push_root(Value::String(key_s))?;
  let key = PropertyKey::from_string(key_s);

  let desc = scope
    .heap()
    .get_property(arr, &key)?
    .ok_or(VmError::InvariantViolation("missing array element"))?;
  match desc.kind {
    PropertyKind::Data { value, .. } => Ok(value),
    _ => Err(VmError::InvariantViolation("array element is not a data property")),
  }
}

#[test]
fn array_iterator_prototype_chain_includes_iterator_prototype() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let intr = *rt.realm().intrinsics();
  let wks = *rt.realm().well_known_symbols();

  // Use a single script to materialize the objects needed for the assertions below. Each
  // `exec_script` call stores `SourceText` + compiled code in the VM, which can exhaust small test
  // heaps when many scripts are executed.
  let init = rt.exec_script(
    "(()=>{const it1=[][Symbol.iterator]();const it2=[][Symbol.iterator]();const v=[].values();const IteratorProto=Object.getPrototypeOf(Object.getPrototypeOf(it1));function* g(){};const GeneratorProto=Object.getPrototypeOf(g.prototype);const okGen=Object.getPrototypeOf(GeneratorProto)===IteratorProto;return [it1,it2,v,Object.prototype.toString.call(it1)===\"[object Array Iterator]\",okGen];})()",
  )?;
  let Value::Object(init) = init else {
    return Err(VmError::InvariantViolation(
      "expected init array from exec_script",
    ));
  };

  let mut scope = rt.heap.scope();
  scope.push_root(Value::Object(init))?;

  let Value::Object(it1) = get_array_index(&mut scope, init, 0)? else {
    return Err(VmError::InvariantViolation(
      "returned iterator instance is not an object",
    ));
  };
  let Value::Object(it2) = get_array_index(&mut scope, init, 1)? else {
    return Err(VmError::InvariantViolation(
      "returned iterator instance is not an object",
    ));
  };
  let Value::Object(values_it) = get_array_index(&mut scope, init, 2)? else {
    return Err(VmError::InvariantViolation(
      "returned values() iterator is not an object",
    ));
  };
  assert_eq!(get_array_index(&mut scope, init, 3)?, Value::Bool(true));
  assert_eq!(get_array_index(&mut scope, init, 4)?, Value::Bool(true));

  scope.push_root(Value::Object(it1))?;
  scope.push_root(Value::Object(it2))?;
  scope.push_root(Value::Object(values_it))?;

  let array_iter_proto = scope
    .heap()
    .object_prototype(it1)?
    .ok_or(VmError::InvariantViolation("Array iterator missing prototype"))?;
  let iterator_proto = scope
    .heap()
    .object_prototype(array_iter_proto)?
    .ok_or(VmError::InvariantViolation(
      "%ArrayIteratorPrototype% missing prototype",
    ))?;

  scope.push_root(Value::Object(array_iter_proto))?;
  scope.push_root(Value::Object(iterator_proto))?;

  // Two separately created Array iterator instances should share the same prototype object.
  assert_eq!(scope.heap().object_prototype(it2)?, Some(array_iter_proto));

  // `Array.prototype.values` should produce the same iterator shape as `%Array.prototype%[@@iterator]`.
  assert_eq!(scope.heap().object_prototype(values_it)?, Some(array_iter_proto));

  assert_eq!(iterator_proto, intr.iterator_prototype());
  assert_eq!(
    scope.heap().object_prototype(iterator_proto)?,
    Some(intr.object_prototype())
  );

  // `%ArrayIteratorPrototype%.[[Prototype]]` is `%IteratorPrototype%`.
  assert_eq!(
    scope.heap().object_prototype(array_iter_proto)?,
    Some(iterator_proto)
  );

  // Array iterator instances should have [[Prototype]] = %ArrayIteratorPrototype%.
  assert_eq!(
    scope.heap().object_prototype(it1)?,
    Some(array_iter_proto),
    "Array iterator instances should have [[Prototype]] = %ArrayIteratorPrototype%",
  );

  // Per spec, `%ArrayIteratorPrototype%` should inherit `@@iterator` from `%IteratorPrototype%`
  // (i.e. it should *not* define its own `Symbol.iterator` property).
  let iterator_key = PropertyKey::from_symbol(wks.iterator);
  assert!(
    scope
      .heap()
      .object_get_own_property(array_iter_proto, &iterator_key)?
      .is_none()
  );
  assert!(
    scope.heap().get_property(array_iter_proto, &iterator_key)?.is_some(),
    "%ArrayIteratorPrototype% should inherit @@iterator from %IteratorPrototype%",
  );

  // The iterator instance should inherit `.next` from `%ArrayIteratorPrototype%`.
  let next_key = PropertyKey::from_string(scope.alloc_string("next")?);
  let next_desc_on_it = scope
    .heap()
    .get_property(it1, &next_key)?
    .ok_or(VmError::InvariantViolation("Array iterator missing next"))?;
  match next_desc_on_it.kind {
    PropertyKind::Data { value, .. } => {
      assert!(scope.heap().is_callable(value)?);
    }
    _ => {
      return Err(VmError::InvariantViolation(
        "Array iterator next is not a data property",
      ));
    }
  }
  assert!(scope
    .heap()
    .object_get_own_property(it1, &next_key)?
    .is_none());
  let next_desc = scope
    .heap()
    .object_get_own_property(array_iter_proto, &next_key)?
    .ok_or(VmError::InvariantViolation(
      "ArrayIteratorPrototype missing next",
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
        "ArrayIteratorPrototype next is not a data property",
      ));
    }
  }

  // `%ArrayIteratorPrototype%[@@toStringTag]` should be `"Array Iterator"`.
  let tag_key = PropertyKey::from_symbol(wks.to_string_tag);
  let tag_desc = scope
    .heap()
    .object_get_own_property(array_iter_proto, &tag_key)?
    .ok_or(VmError::InvariantViolation(
      "ArrayIteratorPrototype missing Symbol.toStringTag",
    ))?;
  assert!(!tag_desc.enumerable);
  assert!(tag_desc.configurable);
  match tag_desc.kind {
    PropertyKind::Data {
      value: Value::String(s),
      writable,
    } => {
      assert!(!writable);
      assert_eq!(scope.heap().get_string(s)?.to_utf8_lossy(), "Array Iterator");
    }
    _ => {
      return Err(VmError::InvariantViolation(
        "ArrayIteratorPrototype Symbol.toStringTag is not a string data property",
      ));
    }
  }

  Ok(())
}
