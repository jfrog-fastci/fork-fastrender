use vm_js::{
  GcObject, Heap, HeapLimits, PropertyKey, Realm, RootId, Scope, Value, Vm, VmError, VmHost,
  VmHostHooks, VmOptions,
};

fn get_own_data_property(
  scope: &mut Scope<'_>,
  obj: GcObject,
  name: &str,
) -> Result<Option<Value>, VmError> {
  let key = PropertyKey::from_string(scope.alloc_string(name)?);
  scope.heap().object_get_own_data_property_value(obj, &key)
}

#[test]
fn weak_ref_deref_returns_undefined_after_gc() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut vm = Vm::new(VmOptions::default());
  let mut realm = Realm::new(&mut vm, &mut heap)?;
  let intr = *realm.intrinsics();

  let target;
  let weak_ref;
  let weak_ref_root: RootId;
  {
    let mut scope = heap.scope();
    target = scope.alloc_object()?;
    // Keep the target alive until after we observe the initial deref result.
    scope.push_root(Value::Object(target))?;

    weak_ref = match vm.construct_without_host(
      &mut scope,
      Value::Object(intr.weak_ref()),
      &[Value::Object(target)],
      Value::Object(intr.weak_ref()),
    )? {
      Value::Object(obj) => obj,
      _ => return Err(VmError::InvariantViolation("WeakRef constructor returned non-object")),
    };
    scope.push_root(Value::Object(weak_ref))?;

    let deref = get_own_data_property(&mut scope, intr.weak_ref_prototype(), "deref")?
      .ok_or(VmError::InvariantViolation("WeakRef.prototype.deref missing"))?;
    let out = vm.call_without_host(&mut scope, deref, Value::Object(weak_ref), &[])?;
    assert_eq!(out, Value::Object(target));

    // Keep the WeakRef alive after this scope drops, but allow `target` to be collected.
    weak_ref_root = scope.heap_mut().add_root(Value::Object(weak_ref))?;
  }

  heap.collect_garbage();
  assert!(!heap.is_valid_object(target));

  {
    let mut scope = heap.scope();
    scope.push_root(Value::Object(weak_ref))?;
    let deref = get_own_data_property(&mut scope, intr.weak_ref_prototype(), "deref")?
      .ok_or(VmError::InvariantViolation("WeakRef.prototype.deref missing"))?;
    let out = vm.call_without_host(&mut scope, deref, Value::Object(weak_ref), &[])?;
    assert_eq!(out, Value::Undefined);
  }

  heap.remove_root(weak_ref_root);
  realm.teardown(&mut heap);
  Ok(())
}

#[derive(Default)]
struct CleanupHost {
  held_values: Vec<Value>,
}

fn cleanup_callback_native(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let Some(host) = host.as_any_mut().downcast_mut::<CleanupHost>() else {
    return Err(VmError::InvariantViolation("unexpected host type"));
  };
  host.held_values.push(args.get(0).copied().unwrap_or(Value::Undefined));
  Ok(Value::Undefined)
}

#[test]
fn finalization_registry_enqueues_cleanup_job_and_calls_callback() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut vm = Vm::new(VmOptions::default());
  let mut realm = Realm::new(&mut vm, &mut heap)?;
  let intr = *realm.intrinsics();
  let mut host = CleanupHost::default();

  let registry;
  let registry_root: RootId;
  let target;
  {
    let mut scope = heap.scope();

    let cleanup_call_id = vm.register_native_call(cleanup_callback_native)?;
    let cleanup_name = scope.alloc_string("cleanup")?;
    scope.push_root(Value::String(cleanup_name))?;
    let cleanup_fn = scope.alloc_native_function(cleanup_call_id, None, cleanup_name, 1)?;
    scope.push_root(Value::Object(cleanup_fn))?;

    registry = match vm.construct(
      &mut host,
      &mut scope,
      Value::Object(intr.finalization_registry()),
      &[Value::Object(cleanup_fn)],
      Value::Object(intr.finalization_registry()),
    )? {
      Value::Object(obj) => obj,
      _ => {
        return Err(VmError::InvariantViolation(
          "FinalizationRegistry constructor returned non-object",
        ))
      }
    };
    scope.push_root(Value::Object(registry))?;

    target = scope.alloc_object()?;
    scope.push_root(Value::Object(target))?;

    let register =
      get_own_data_property(&mut scope, intr.finalization_registry_prototype(), "register")?
    .ok_or(VmError::InvariantViolation(
      "FinalizationRegistry.prototype.register missing",
    ))?;
    let held_value = Value::Number(123.0);
    let _ = vm.call(
      &mut host,
      &mut scope,
      register,
      Value::Object(registry),
      &[Value::Object(target), held_value],
    )?;

    registry_root = scope.heap_mut().add_root(Value::Object(registry))?;
  }

  heap.collect_garbage();
  assert!(!heap.is_valid_object(target));
  assert!(host.held_values.is_empty());

  vm.perform_microtask_checkpoint_with_host(&mut host, &mut heap)?;
  assert_eq!(host.held_values, vec![Value::Number(123.0)]);

  heap.remove_root(registry_root);
  realm.teardown(&mut heap);
  Ok(())
}
