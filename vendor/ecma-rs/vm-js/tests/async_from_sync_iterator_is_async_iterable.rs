use vm_js::iterator::get_async_iterator;
use vm_js::{
  Heap, HeapLimits, JsRuntime, MicrotaskQueue, PropertyDescriptor, PropertyKey, PropertyKind, Value,
  Vm, VmError, VmOptions,
};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // `for await...of` exercises async iteration + Promise/job queuing. With ongoing vm-js builtin
  // growth, a 1MiB heap can be too tight and cause spurious `VmError::OutOfMemory` failures that
  // are not relevant to the semantics being tested here.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn value_to_string(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap.get_string(s).unwrap().to_utf8_lossy()
}

#[test]
fn async_from_sync_iterator_is_async_iterable() -> Result<(), VmError> {
  let mut rt = new_runtime();

  // Use a standalone host hooks queue for the Rust-level `get_async_iterator` call. This should
  // not enqueue Promise jobs for built-in sync iterables like Arrays, but we still tear it down to
  // avoid leaking persistent roots if that changes.
  let mut host = ();
  let mut hooks = MicrotaskQueue::new();

  // Build a sync iterable (Array) and create an AsyncFromSyncIterator wrapper via
  // `iterator::get_async_iterator`. Expose it to JS as `globalThis.it`.
  {
    let (vm, realm, heap) = rt.vm_realm_and_heap_mut();
    let intr = *realm.intrinsics();
    let global = realm.global_object();

    let mut scope = heap.scope();

    let arr = scope.alloc_array(0)?;
    scope.push_root(Value::Object(arr))?;
    scope
      .heap_mut()
      .object_set_prototype(arr, Some(intr.array_prototype()))?;

    let record = get_async_iterator(vm, &mut host, &mut hooks, &mut scope, Value::Object(arr))?;
    let it = record.iterator;
    scope.push_root(it)?;

    let key = PropertyKey::from_string(scope.alloc_string("it")?);
    scope.define_property(
      global,
      key,
      PropertyDescriptor {
        enumerable: true,
        configurable: true,
        kind: PropertyKind::Data {
          value: it,
          writable: true,
        },
      },
    )?;
  }

  // Ensure the hooks queue does not leak job roots on drop.
  hooks.cancel_all(&mut rt);

  // The wrapper must inherit `@@asyncIterator` from `%AsyncIteratorPrototype%`.
  let v = rt.exec_script(
    r#"typeof it[Symbol.asyncIterator] === "function" && it[Symbol.asyncIterator]() === it"#,
  )?;
  assert_eq!(v, Value::Bool(true));

  // The wrapper should be usable directly in a `for await...of` loop.
  let v = rt.exec_script(
    r#"
      var out = "";
      (async function () {
        try {
          for await (const _ of it) { break; }
          out = "ok";
        } catch (e) {
          out = "err:" + e;
        }
      })();
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, v), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let v = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, v), "ok");

  Ok(())
}

