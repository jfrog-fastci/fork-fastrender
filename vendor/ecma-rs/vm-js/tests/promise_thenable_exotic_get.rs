use vm_js::{
  GcObject, Heap, HeapLimits, Job, JobCallback, MicrotaskQueue, PropertyKey, RealmId, Scope, Value, Vm, VmError,
  VmHostHooks, VmJobContext, VmOptions,
};

#[derive(Debug, Default)]
struct ExoticThenableHooks {
  target: Option<GcObject>,
  then_func: Option<GcObject>,
  exotic_get_calls: usize,
  microtasks: MicrotaskQueue,
}

impl ExoticThenableHooks {
  fn set_thenable(&mut self, target: GcObject, then_func: GcObject) {
    self.target = Some(target);
    self.then_func = Some(then_func);
  }

  fn perform_microtask_checkpoint(&mut self, ctx: &mut dyn VmJobContext) -> Vec<VmError> {
    // Use `MicrotaskQueue` as a FIFO storage container, but run jobs with `self` as the active host
    // hooks so callbacks can observe `host_exotic_get` (and any other embedder hooks).
    if !self.microtasks.begin_checkpoint() {
      return Vec::new();
    }

    let mut errors = Vec::new();
    while let Some((_realm, job)) = self.microtasks.pop_front() {
      if let Err(err) = job.run(ctx, self) {
        let is_termination = matches!(err, VmError::Termination(_));
        errors.push(err);
        if is_termination {
          // Termination is a hard stop: discard remaining queued jobs (and any jobs enqueued by the
          // failing job) so we don't leak persistent roots.
          self.microtasks.teardown(ctx);
          break;
        }
      }
    }
    self.microtasks.end_checkpoint();
    errors
  }
}

impl VmHostHooks for ExoticThenableHooks {
  fn host_enqueue_promise_job(&mut self, job: Job, realm: Option<RealmId>) {
    self.microtasks.enqueue_promise_job(job, realm);
  }

  fn host_exotic_get(
    &mut self,
    scope: &mut Scope<'_>,
    obj: GcObject,
    key: PropertyKey,
    _receiver: Value,
  ) -> Result<Option<Value>, VmError> {
    let Some(target) = self.target else {
      return Ok(None);
    };
    if obj != target {
      return Ok(None);
    }

    let PropertyKey::String(key_str) = key else {
      return Ok(None);
    };
    if scope.heap().get_string(key_str)?.to_utf8_lossy() != "then" {
      return Ok(None);
    }

    self.exotic_get_calls += 1;
    Ok(Some(Value::Object(
      self
        .then_func
        .expect("ExoticThenableHooks::then_func should be initialized before use"),
    )))
  }

  fn host_call_job_callback(
    &mut self,
    ctx: &mut dyn VmJobContext,
    callback: &JobCallback,
    this_argument: Value,
    arguments: &[Value],
  ) -> Result<Value, VmError> {
    ctx.call(
      self,
      Value::Object(callback.callback_object()),
      this_argument,
      arguments,
    )
  }
}

fn get_global_data_property(rt_heap: &mut Heap, global: GcObject, name: &str) -> Result<Value, VmError> {
  let mut scope = rt_heap.scope();
  let key = PropertyKey::from_string(scope.alloc_string(name)?);
  Ok(scope
    .heap()
    .object_get_own_data_property_value(global, &key)?
    .unwrap_or(Value::Undefined))
}

#[test]
fn promise_thenable_assimilation_uses_host_exotic_get_for_then() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = vm_js::JsRuntime::new(vm, heap)?;

  let mut hooks = ExoticThenableHooks::default();

  // Create the thenable + then function in JS so they are ordinary ECMAScript objects.
  rt.exec_script_with_hooks(
    &mut hooks,
    r#"
      globalThis.t = {};
      globalThis.thenCalls = 0;
      globalThis.thenFunc = function(resolve, reject) {
        thenCalls++;
        resolve('ok');
      };
    "#,
  )?;

  // Capture the object/function handles for the host exotic getter.
  let global = rt.realm().global_object();
  let (t_obj, then_func_obj) = {
    let t = get_global_data_property(&mut rt.heap, global, "t")?;
    let then_func = get_global_data_property(&mut rt.heap, global, "thenFunc")?;
    let Value::Object(t_obj) = t else {
      return Err(VmError::Unimplemented("globalThis.t should be an object"));
    };
    let Value::Object(then_func_obj) = then_func else {
      return Err(VmError::Unimplemented("globalThis.thenFunc should be a function object"));
    };
    (t_obj, then_func_obj)
  };
  hooks.set_thenable(t_obj, then_func_obj);

  rt.exec_script_with_hooks(
    &mut hooks,
    r#"
      Promise.resolve(t).then(v => { globalThis.out = v; });
    "#,
  )?;

  // Before draining microtasks, the resolve-thenable job should be queued but not run.
  assert_eq!(hooks.exotic_get_calls, 1);
  assert_eq!(
    get_global_data_property(&mut rt.heap, global, "thenCalls")?,
    Value::Number(0.0)
  );
  assert_eq!(
    get_global_data_property(&mut rt.heap, global, "out")?,
    Value::Undefined
  );

  let errors = hooks.perform_microtask_checkpoint(&mut rt);
  assert!(errors.is_empty());

  assert_eq!(
    get_global_data_property(&mut rt.heap, global, "thenCalls")?,
    Value::Number(1.0)
  );

  let out = get_global_data_property(&mut rt.heap, global, "out")?;
  let Value::String(out_s) = out else {
    return Err(VmError::Unimplemented("globalThis.out should be a string"));
  };
  assert_eq!(rt.heap.get_string(out_s)?.to_utf8_lossy(), "ok");

  Ok(())
}

