use vm_js::{Heap, HeapLimits, PropertyDescriptor, PropertyKey, PropertyKind, Value, VmError};
use std::mem;

#[test]
fn generator_objects_are_ordinary_objects_and_gc_traces_internal_slots() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

  let gen;
  let this_obj;
  let arg_obj;
  let env;
  let prop_obj;
  let key;
  {
    let mut scope = heap.scope();

    this_obj = scope.alloc_object()?;
    arg_obj = scope.alloc_object()?;
    env = scope.env_create(None)?;
    prop_obj = scope.alloc_object()?;
    key = scope.alloc_string("x")?;

    // `this_obj`, `arg_obj`, and `env` are only kept alive by Generator internal slots (not by
    // stack roots in this scope).
    gen = scope.alloc_generator_with_prototype(
      None,
      Value::Object(this_obj),
      &[Value::Object(arg_obj)],
      Some(env),
    )?;

    // Keep the generator live across collection.
    scope.push_root(Value::Object(gen))?;

    scope.define_property(
      gen,
      PropertyKey::from_string(key),
      PropertyDescriptor {
        enumerable: true,
        configurable: true,
        kind: PropertyKind::Data {
          value: Value::Object(prop_obj),
          writable: true,
        },
      },
    )?;

    assert_eq!(
      scope.heap().get(gen, &PropertyKey::from_string(key))?,
      Value::Object(prop_obj)
    );

    // Exercise object property deletion plumbing for Generator objects.
    assert!(scope
      .heap_mut()
      .ordinary_delete(gen, PropertyKey::from_string(key))?);
    assert_eq!(
      scope.heap().get(gen, &PropertyKey::from_string(key))?,
      Value::Undefined
    );

    // `prop_obj` is now unreachable (the property was deleted), but other internal-slot references
    // should keep `this_obj`/`arg_obj`/`env` live.
    scope.heap_mut().collect_garbage();
    assert!(scope.heap().is_valid_object(gen));
    assert!(scope.heap().is_valid_object(this_obj));
    assert!(scope.heap().is_valid_object(arg_obj));
    assert!(scope.heap().is_valid_env(env));
    assert!(!scope.heap().is_valid_object(prop_obj));

    // Exercise generator internal slot mutation APIs that must keep heap byte accounting in sync.
    // `cont_obj` is only kept alive via `gen.[[Continuation]]` once set (it's not a stack root).
    let cont_obj = scope.alloc_object()?;
    let used_before_cont_set = scope.heap().used_bytes();
    scope.heap_mut().generator_set_continuation(
      gen,
      Some(vec![Value::Object(cont_obj)].into_boxed_slice()),
    )?;
    let used_after_cont_set = scope.heap().used_bytes();
    assert_eq!(
      used_after_cont_set - used_before_cont_set,
      mem::size_of::<Value>()
    );

    scope.heap_mut().collect_garbage();
    assert!(scope.heap().is_valid_object(cont_obj));

    let used_before_cont_unset = scope.heap().used_bytes();
    scope.heap_mut().generator_set_continuation(gen, None)?;
    let used_after_cont_unset = scope.heap().used_bytes();
    assert_eq!(
      used_before_cont_unset - used_after_cont_unset,
      mem::size_of::<Value>()
    );
    assert_eq!(used_after_cont_unset, used_before_cont_set);

    // `cont_obj` is now unreachable (continuation cleared), so it should be collected.
    scope.heap_mut().collect_garbage();
    assert!(!scope.heap().is_valid_object(cont_obj));

    // `arg_obj` is only kept alive by `gen.[[Args]]`. Clear it and ensure the arg object can be
    // collected, and heap byte accounting shrinks accordingly.
    let used_before_args_clear = scope.heap().used_bytes();
    scope.heap_mut().generator_set_args(gen, None)?;
    let used_after_args_clear = scope.heap().used_bytes();
    assert_eq!(
      used_before_args_clear - used_after_args_clear,
      mem::size_of::<Value>()
    );

    scope.heap_mut().collect_garbage();
    assert!(!scope.heap().is_valid_object(arg_obj));
    assert!(scope.heap().is_valid_object(this_obj));
    assert!(scope.heap().is_valid_env(env));
  }

  // Stack roots were removed when the scope was dropped.
  heap.collect_garbage();
  assert!(!heap.is_valid_object(gen));
  assert!(!heap.is_valid_object(this_obj));
  assert!(!heap.is_valid_object(arg_obj));
  assert!(!heap.is_valid_env(env));
  assert!(!heap.is_valid_object(prop_obj));
  assert!(matches!(heap.get(gen, &PropertyKey::from_string(key)), Err(VmError::InvalidHandle { .. })));
  Ok(())
}
