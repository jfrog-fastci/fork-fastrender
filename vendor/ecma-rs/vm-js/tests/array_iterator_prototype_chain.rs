use vm_js::{
  Heap, HeapLimits, JsRuntime, PropertyKey, PropertyKind, Value, Vm, VmError, VmOptions,
};

fn new_runtime() -> Result<JsRuntime, VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap)
}

#[test]
fn array_iterator_prototype_chain_includes_iterator_prototype() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let intr = *rt.realm().intrinsics();
  let wks = *rt.realm().well_known_symbols();

  // Two separately created Array iterator instances should share the same prototype object.
  let same_array_iterator_proto = rt.exec_script(
    r#"
      (() => {
        const it1 = [][Symbol.iterator]();
        const it2 = [][Symbol.iterator]();
        return Object.getPrototypeOf(it1) === Object.getPrototypeOf(it2);
      })()
    "#,
  )?;
  assert_eq!(same_array_iterator_proto, Value::Bool(true));

  // `Array.prototype.values` should produce the same iterator shape as `%Array.prototype%[@@iterator]`.
  let same_values_proto =
    rt.exec_script("Object.getPrototypeOf([].values()) === Object.getPrototypeOf([][Symbol.iterator]())")?;
  assert_eq!(same_values_proto, Value::Bool(true));

  // `Object.prototype.toString` should observe the `@@toStringTag` on `%ArrayIteratorPrototype%`.
  let to_string_tag =
    rt.exec_script(r#"Object.prototype.toString.call([][Symbol.iterator]()) === "[object Array Iterator]""#)?;
  assert_eq!(to_string_tag, Value::Bool(true));

  // `Object.getOwnPropertyDescriptor(%ArrayIteratorPrototype%, @@toStringTag).value` should be
  // `"Array Iterator"` (matches test262-style assertions).
  let tag_desc_value = rt.exec_script(
    r#"
      (() => {
        const proto = Object.getPrototypeOf([][Symbol.iterator]());
        return Object.getOwnPropertyDescriptor(proto, Symbol.toStringTag).value === "Array Iterator";
      })()
    "#,
  )?;
  assert_eq!(tag_desc_value, Value::Bool(true));

  // `%ArrayIteratorPrototype%.next` should exist as an own data property and match the expected
  // descriptor shape.
  let next_desc_shape = rt.exec_script(
    r#"
      (() => {
        const proto = Object.getPrototypeOf([][Symbol.iterator]());
        const desc = Object.getOwnPropertyDescriptor(proto, "next");
        return (
          typeof desc.value === "function" &&
          desc.writable === true &&
          desc.enumerable === false &&
          desc.configurable === true
        );
      })()
    "#,
  )?;
  assert_eq!(next_desc_shape, Value::Bool(true));

  // Iterator instances must not have an own `next` property (it is inherited from the prototype).
  let iter_has_own_next = rt.exec_script(
    r#"
      (() => {
        const it = [][Symbol.iterator]();
        return Object.prototype.hasOwnProperty.call(it, "next");
      })()
    "#,
  )?;
  assert_eq!(iter_has_own_next, Value::Bool(false));

  // Per spec, `%ArrayIteratorPrototype%` should inherit `@@iterator` from `%IteratorPrototype%`
  // (i.e. it should *not* define its own `Symbol.iterator` property).
  let array_iter_proto_has_own_iterator = rt.exec_script(
    r#"
      (() => {
        const proto = Object.getPrototypeOf([][Symbol.iterator]());
        return Object.prototype.hasOwnProperty.call(proto, Symbol.iterator);
      })()
    "#,
  )?;
  assert_eq!(array_iter_proto_has_own_iterator, Value::Bool(false));

  // Array iterator instances must be iterable (calling `@@iterator` returns the iterator itself).
  let array_iter_is_iterable = rt.exec_script(
    r#"
      (() => {
        const it = [][Symbol.iterator]();
        return it[Symbol.iterator]() === it;
      })()
    "#,
  )?;
  assert_eq!(array_iter_is_iterable, Value::Bool(true));

  // Generator tests in test262 compute `%IteratorPrototype%` from an Array iterator and compare it
  // to `Object.getPrototypeOf(%GeneratorPrototype%)`.
  let generator_iterator_proto_match = rt.exec_script(
    r#"
      (() => {
        const IteratorProto = Object.getPrototypeOf(Object.getPrototypeOf([][Symbol.iterator]()));
        function* g() {}
        const GeneratorProto = Object.getPrototypeOf(g.prototype);
        return Object.getPrototypeOf(GeneratorProto) === IteratorProto;
      })()
    "#,
  )?;
  assert_eq!(generator_iterator_proto_match, Value::Bool(true));

  let it = rt.exec_script("[][Symbol.iterator]()")?;
  let array_iter_proto_v = rt.exec_script("Object.getPrototypeOf([][Symbol.iterator]())")?;
  let iterator_proto_v =
    rt.exec_script("Object.getPrototypeOf(Object.getPrototypeOf([][Symbol.iterator]()))")?;

  let Value::Object(it) = it else {
    return Err(VmError::InvariantViolation(
      "[][Symbol.iterator]() did not return an object",
    ));
  };
  let Value::Object(array_iter_proto) = array_iter_proto_v else {
    return Err(VmError::InvariantViolation(
      "Object.getPrototypeOf(ArrayIterator) did not return an object",
    ));
  };
  let Value::Object(iterator_proto) = iterator_proto_v else {
    return Err(VmError::InvariantViolation(
      "Object.getPrototypeOf(ArrayIteratorPrototype) did not return an object",
    ));
  };

  let mut scope = rt.heap.scope();
  scope.push_root(Value::Object(it))?;
  scope.push_root(Value::Object(array_iter_proto))?;
  scope.push_root(Value::Object(iterator_proto))?;

  assert_eq!(iterator_proto, intr.iterator_prototype());
  assert_eq!(
    scope.heap().object_prototype(iterator_proto)?,
    Some(intr.object_prototype())
  );
  assert_eq!(
    scope.heap().object_prototype(array_iter_proto)?,
    Some(iterator_proto)
  );
  assert_eq!(
    scope.heap().object_prototype(it)?,
    Some(array_iter_proto),
    "Array iterator instances should have [[Prototype]] = %ArrayIteratorPrototype%",
  );

  // The iterator instance should inherit `.next` from `%ArrayIteratorPrototype%`.
  let next_key = PropertyKey::from_string(scope.alloc_string("next")?);
  let next_desc_on_it = scope
    .heap()
    .get_property(it, &next_key)?
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
    .object_get_own_property(it, &next_key)?
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
