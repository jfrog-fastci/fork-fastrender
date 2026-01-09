use vm_js::{
  Heap, HeapLimits, JobCallback, PromiseHandler, PromiseReaction, PromiseReactionType, Value, VmError,
};

// This is a lightweight integration-smoke test for `vm-js`'s Promise heap object
// representation (internal slots + reaction lists).
//
// `vm-js` has its own unit tests, but keeping one or two invariants covered here helps catch
// accidental regressions when bumping the `engines/ecma-rs` submodule.

#[test]
fn promise_result_is_traced_by_gc_and_brand_check_works() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

  let referenced;
  {
    let mut scope = heap.scope();

    let promise = scope.alloc_promise()?;
    referenced = scope.alloc_object()?;

    assert!(scope.heap().is_promise_object(promise));
    assert!(!scope.heap().is_promise_object(referenced));

    scope
      .heap_mut()
      .promise_fulfill(promise, Value::Object(referenced))?;

    scope.push_root(Value::Object(promise))?;
    scope.heap_mut().collect_garbage();

    assert!(
      scope.heap().is_valid_object(referenced),
      "promise.[[PromiseResult]] should be traced"
    );
  }

  // Stack roots were removed when the scope was dropped, so the result object should now be
  // collectable.
  heap.collect_garbage();
  assert!(!heap.is_valid_object(referenced));
  Ok(())
}

#[test]
fn promise_reaction_lists_are_cleared_on_settlement() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

  let handler;
  {
    let mut scope = heap.scope();

    let promise = scope.alloc_promise()?;
    handler = scope.alloc_object()?;

    scope.promise_append_fulfill_reaction(
      promise,
      PromiseReaction {
        capability: None,
        reaction_type: PromiseReactionType::Fulfill,
        handler: Some(PromiseHandler::JobCallback(JobCallback::new(handler))),
      },
    )?;

    scope.push_root(Value::Object(promise))?;

    // While the promise is pending, its reaction lists keep handlers alive.
    scope.heap_mut().collect_garbage();
    assert!(scope.heap().is_valid_object(handler));

    // Settlement clears the reaction lists so they do not keep handlers alive unnecessarily.
    scope.heap_mut().promise_fulfill(promise, Value::Undefined)?;
    scope.heap_mut().collect_garbage();
    assert!(!scope.heap().is_valid_object(handler));
  }

  Ok(())
}
