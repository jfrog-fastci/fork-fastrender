use vm_js::{Heap, HeapLimits, PropertyKey, Realm, RootId, Value, Vm, VmError, VmOptions};

#[test]
fn array_of_proto_from_ctor_realm() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  // This test initializes two full realms (two copies of the intrinsic object graph) and uses the
  // Function constructor. Use a slightly larger heap than the 1MiB baseline used by many tests to
  // avoid spurious OOMs as builtin surface area grows (especially on debug/profile builds).
  let mut heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));

  // Create the "other" realm first so we can construct `C = new other.Function()` while that realm
  // is active.
  let mut other = Realm::new(&mut vm, &mut heap)?;
  let other_intr = *other.intrinsics();

  // Ensure realms are torn down even if we return early with `Err(..)` (e.g. OutOfMemory). The
  // `Realm` drop guard debug-asserts this invariant.
  let mut current: Option<Realm> = None;
  let mut c_obj: Option<vm_js::GcObject> = None;
  let mut c_root: Option<RootId> = None;

  let result = (|| -> Result<(), VmError> {
    // `C = new other.Function(); C.prototype = null;`
    let (created_obj, created_root): (vm_js::GcObject, RootId) = {
      let mut scope = heap.scope();

      let ctor = Value::Object(other_intr.function_constructor());
      let c = vm.construct_without_host(&mut scope, ctor, &[], ctor)?;
      let Value::Object(c_obj) = c else {
        return Err(VmError::InvariantViolation(
          "Function constructor should return an object",
        ));
      };

      // Root `c_obj` while allocating the `"prototype"` key.
      scope.push_root(Value::Object(c_obj))?;
      let prototype_key_s = scope.alloc_string("prototype")?;
      let prototype_key = PropertyKey::from_string(prototype_key_s);
      assert!(
        scope.ordinary_set(&mut vm, c_obj, prototype_key, Value::Null, Value::Object(c_obj))?
      );

      // Keep `c_obj` alive across subsequent realm initialization, which can trigger GC.
      let root = scope.heap_mut().add_root(Value::Object(c_obj))?;

      (c_obj, root)
    };

    c_obj = Some(created_obj);
    c_root = Some(created_root);

    // Now create the current realm (like the test262 harness calling `$262.createRealm()` and then
    // continuing execution in the original realm).
    current = Some(Realm::new(&mut vm, &mut heap)?);
    let current_intr = *current.as_ref().unwrap().intrinsics();
    let c_obj = c_obj.unwrap();

    {
      let mut scope = heap.scope();

      // Look up `Array.of` on the current realm's Array constructor.
      let array_ctor = current_intr.array_constructor();
      scope.push_root(Value::Object(array_ctor))?;
      let of_key_s = scope.alloc_string("of")?;
      scope.push_root(Value::String(of_key_s))?;
      let of_key = PropertyKey::from_string(of_key_s);
      let Some(Value::Object(of_fn)) = scope
        .heap()
        .object_get_own_data_property_value(array_ctor, &of_key)?
      else {
        return Err(VmError::InvariantViolation("missing Array.of intrinsic"));
      };

      // `Array.of.call(C, 1, 2, 3)`
      let args = [Value::Number(1.0), Value::Number(2.0), Value::Number(3.0)];
      let result = vm.call_without_host(&mut scope, Value::Object(of_fn), Value::Object(c_obj), &args)?;
      let Value::Object(result_obj) = result else {
        return Err(VmError::InvariantViolation("Array.of should return an object"));
      };

      // `Object.getPrototypeOf(result) === other.Object.prototype`
      assert_eq!(
        scope.heap().object_prototype(result_obj)?,
        Some(other_intr.object_prototype())
      );

      // Also verify Array.of defined elements and a length property on the constructed object.
      scope.push_root(Value::Object(result_obj))?;

      let length_key_s = scope.alloc_string("length")?;
      scope.push_root(Value::String(length_key_s))?;
      let length_key = PropertyKey::from_string(length_key_s);
      assert_eq!(
        scope
          .heap()
          .object_get_own_data_property_value(result_obj, &length_key)?,
        Some(Value::Number(3.0))
      );

      for (i, expected) in [(0, 1.0), (1, 2.0), (2, 3.0)] {
        let idx_key_s = scope.alloc_string(&i.to_string())?;
        scope.push_root(Value::String(idx_key_s))?;
        let idx_key = PropertyKey::from_string(idx_key_s);
        assert_eq!(
          scope
            .heap()
            .object_get_own_data_property_value(result_obj, &idx_key)?,
          Some(Value::Number(expected))
        );
      }
    }

    Ok(())
  })();

  if let Some(root) = c_root {
    heap.remove_root(root);
  }

  if let Some(mut current) = current {
    current.teardown(&mut heap);
  }
  other.teardown(&mut heap);

  // Avoid unused-variable warnings in case future refactors remove uses.
  let _ = c_obj;

  result
}
