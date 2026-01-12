use vm_js::{
  Heap, HeapLimits, PropertyKey, Realm, RootId, Value, Vm, VmError, VmOptions, WeakGcObject,
};

#[test]
fn proxy_revocable_revoke_does_not_keep_proxy_alive_for_gc() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut vm = Vm::new(VmOptions::default());
  let mut realm = Realm::new(&mut vm, &mut heap)?;
  let intr = *realm.intrinsics();

  let weak_proxy: WeakGcObject;
  let revoke_root: RootId;

  {
    let mut scope = heap.scope();

    // Get the `Proxy.revocable` function from the `%Proxy%` constructor.
    let revocable_key = PropertyKey::from_string(scope.alloc_string("revocable")?);
    let revocable = scope
      .heap()
      .object_get_own_data_property_value(intr.proxy_constructor(), &revocable_key)?
      .expect("Proxy.revocable should exist on the Proxy constructor");
    let Value::Object(revocable) = revocable else {
      panic!("Proxy.revocable should be an object (function)");
    };

    // Call `Proxy.revocable(target, handler)` to create a proxy + revoke closure.
    let target = scope.alloc_object()?;
    let handler = scope.alloc_object()?;
    scope.push_roots(&[Value::Object(target), Value::Object(handler)])?;
    let out = vm.call_without_host(
      &mut scope,
      Value::Object(revocable),
      Value::Undefined,
      &[Value::Object(target), Value::Object(handler)],
    )?;
    let Value::Object(out) = out else {
      panic!("Proxy.revocable should return an object");
    };

    // Root the returned object while we allocate property keys.
    scope.push_root(Value::Object(out))?;

    let proxy_key = PropertyKey::from_string(scope.alloc_string("proxy")?);
    let proxy = scope
      .heap()
      .object_get_own_data_property_value(out, &proxy_key)?
      .expect("Proxy.revocable result should have a proxy property");
    let Value::Object(proxy) = proxy else {
      panic!("Proxy.revocable result proxy property should be an object");
    };
    weak_proxy = WeakGcObject::from(proxy);

    let revoke_key = PropertyKey::from_string(scope.alloc_string("revoke")?);
    let revoke = scope
      .heap()
      .object_get_own_data_property_value(out, &revoke_key)?
      .expect("Proxy.revocable result should have a revoke property");
    let Value::Object(revoke) = revoke else {
      panic!("Proxy.revocable result revoke property should be an object (function)");
    };

    // Keep only the revoke function alive across GC; drop all other strong refs after this scope.
    revoke_root = scope.heap_mut().add_root(Value::Object(revoke))?;

    // First revoke should revoke the proxy and clear the revoker's captured slot.
    let out = vm.call_without_host(&mut scope, Value::Object(revoke), Value::Undefined, &[])?;
    assert_eq!(out, Value::Undefined);
    assert_eq!(
      scope
        .heap()
        .get_function_native_slots(revoke)?
        .first()
        .copied(),
      Some(Value::Null),
      "revoke() should clear its captured proxy slot",
    );
  }

  // After all stack roots are dropped, the only remaining strong reference is the `revoke`
  // function. If `revoke()` cleared its captured slot, the proxy should be collectible.
  heap.collect_garbage();
  assert!(
    weak_proxy.upgrade(&heap).is_none(),
    "revoked proxy should be collectible after revoke()"
  );

  heap.remove_root(revoke_root);
  realm.teardown(&mut heap);
  Ok(())
}

