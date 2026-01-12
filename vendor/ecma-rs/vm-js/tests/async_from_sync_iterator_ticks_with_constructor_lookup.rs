use vm_js::iterator;
use vm_js::{Heap, HeapLimits, JsRuntime, PropertyDescriptor, PropertyKey, PropertyKind, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // This test exercises async iteration + Promise job queuing; use a slightly larger heap than the
  // 1MiB default used by many unit tests to avoid spurious `VmError::OutOfMemory` failures.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn value_to_string(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap.get_string(s).unwrap().to_utf8_lossy()
}

fn data_desc(value: Value) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: true,
    configurable: true,
    kind: PropertyKind::Data {
      value,
      writable: true,
    },
  }
}

// Port of test262:
// `test/language/statements/for-await-of/ticks-with-sync-iter-resolved-promise-and-constructor-lookup.js`
//
// The AsyncFromSyncIterator wrapper must:
// - Create one PromiseCapability up-front
// - Await the sync iterator result's `value` via `PromiseResolve(%Promise%, value)`
// - Wire reactions via `PerformPromiseThen(valueWrapper, ..., resultCapability)` without calling
//   `Promise.prototype.then` / SpeciesConstructor (which would do extra `constructor` lookups).
#[test]
fn for_await_of_sync_iter_resolved_promise_does_not_do_extra_constructor_lookups() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let result: Result<(), VmError> = (|| {
    // --- Setup JS state ---
    rt.exec_script(
      r#"
        var log = [];

        // Establish a baseline microtask so the test can observe ordering.
        Promise.resolve().then(function () { log.push("t"); });

        var p = Promise.resolve("x");
        Object.defineProperty(p, "constructor", {
          get() {
            log.push("c");
            return Promise;
          }
        });

        // A sync iterable that yields `p` twice:
        // - once with done=false
        // - once with done=true (the AsyncFromSyncIterator continuation still awaits the value)
        var syncIterable = {};
        syncIterable[Symbol.iterator] = function () {
          var step = 0;
          return {
            next() {
              step++;
              if (step === 1) return { value: p, done: false };
              if (step === 2) return { value: p, done: true };
              return { value: undefined, done: true };
            },
          };
        };
      "#,
    )?;

    // Create an AsyncFromSyncIterator wrapper object (engine-internal) via the iterator abstract op,
    // then expose it to JS so `for await..of` uses the wrapper's `.next` implementation.
    //
    // This avoids the interpreter's `AsyncIteratorRecord::Sync` fast-path, which awaits sync values in
    // the loop itself instead of exercising `%AsyncFromSyncIteratorPrototype%.next`.
    let sync_iterable = rt.exec_script("syncIterable")?;
    let mut host_ctx = ();

    // Temporarily move the VM-owned microtask queue out so we can hold `&mut Vm` and `&mut dyn
    // VmHostHooks` simultaneously when calling the iterator abstract op.
    //
    // IMPORTANT: always restore the queue back into the VM before returning, even on error, so we
    // don't drop `Job`s with persistent roots still registered (vm-js debug-asserts on that).
    let mut hooks = std::mem::take(rt.vm.microtask_queue_mut());
    let wrapper_result: Result<Value, VmError> = (|| {
      let async_iter = {
        let mut scope = rt.heap.scope();
        iterator::get_async_iterator(&mut rt.vm, &mut host_ctx, &mut hooks, &mut scope, sync_iterable)?
      };
      let wrapper = async_iter.iterator;

      {
        let global_object = rt.realm().global_object();
        let mut scope = rt.heap.scope();
        scope.push_root(wrapper)?;
        let it_key = PropertyKey::from_string(scope.alloc_string("it")?);
        scope.define_property(global_object, it_key, data_desc(wrapper))?;
      }

      Ok(wrapper)
    })();

    // Drain any Promise jobs that were enqueued into the VM-owned queue while it was moved out (as
    // a safety net), then restore `hooks` back into the VM.
    while let Some((realm, job)) = rt.vm.microtask_queue_mut().pop_front() {
      hooks.enqueue_promise_job(job, realm);
    }
    *rt.vm.microtask_queue_mut() = hooks;

    let _wrapper = wrapper_result?;

    // Make the wrapper an async iterable (`it[Symbol.asyncIterator]()` returns itself).
    rt.exec_script(
      r#"
        it[Symbol.asyncIterator] = function () { return this; };
      "#,
    )?;

    // --- Run `for await..of` over the wrapper ---
    let value = rt.exec_script(
      r#"
        (async function () {
          for await (var v of it) {
            log.push(v);
          }
          log.push("a");
        })();
        log.join("")
      "#,
    )?;

    // The `PromiseResolve(%Promise%, p)` constructor lookup happens synchronously inside
    // `AsyncFromSyncIteratorContinuation`, before any microtasks run.
    assert_eq!(value_to_string(&rt, value), "c");

    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let value = rt.exec_script("log.join(\"\")")?;
    assert_eq!(value_to_string(&rt, value), "ctxca");
    Ok(())
  })();

  // Avoid failing the test due to the `Job` root-leak debug assertion if *any* step above returns
  // an error and exits early without a chance to drain the queue.
  rt.teardown_microtasks();
  result
}
