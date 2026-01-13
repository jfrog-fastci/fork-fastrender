use vm_js::{Heap, HeapLimits, JsRuntime, PromiseState, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn value_to_utf8(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap.get_string(s).unwrap().to_utf8_lossy()
}

#[test]
fn promise_resolve_thenable_immediate_calls_then_synchronously() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      globalThis.log = [];
      globalThis.thenable = {
        then(resolve, _reject) {
          log.push("then");
          resolve(7);
        }
      };
    "#,
  )?;

  let thenable = rt.exec_script("thenable")?;

  // Temporarily move the VM-owned microtask queue out so we can hold:
  // - `&mut Vm` (the job context)
  // - and `&mut dyn VmHostHooks` (the active microtask hooks)
  // simultaneously.
  let mut hooks = std::mem::take(rt.vm.microtask_queue_mut());
  let promise = {
    let mut scope = rt.heap.scope();
    let mut host_ctx = ();
    vm_js::promise_resolve_thenable_immediate_with_host_and_hooks(
      &mut rt.vm,
      &mut scope,
      &mut host_ctx,
      &mut hooks,
      thenable,
    )?
  };
  *rt.vm.microtask_queue_mut() = hooks;

  // The thenable's `then` method must be called synchronously during PromiseResolve/Await.
  let log = rt.exec_script("JSON.stringify(log)")?;
  assert_eq!(value_to_utf8(&rt, log), r#"["then"]"#);

  let Value::Object(promise_obj) = promise else {
    panic!("expected promise object, got {promise:?}");
  };
  assert_eq!(rt.heap.promise_state(promise_obj)?, PromiseState::Fulfilled);
  assert_eq!(rt.heap.promise_result(promise_obj)?, Some(Value::Number(7.0)));

  Ok(())
}

