use crate::heap::{
  AsyncGeneratorFunc, AsyncGeneratorRequest, AsyncGeneratorRequestKind, AsyncGeneratorState, Trace,
  Tracer,
};
use crate::iterator;
use crate::property::PropertyKey;
use crate::{GcObject, Job, JobKind, PromiseCapability, RootId, Scope, Value, Vm, VmError, VmHost, VmHostHooks};

#[derive(Debug)]
pub(crate) enum YieldStarStep {
  /// Await the provided promise and resume the yield* state machine with the promise settlement.
  Await(Value),
  /// Yield the delegate iterator *result object* to the async generator consumer.
  ///
  /// vm-js intentionally yields the iterator result object directly (preserving any extra
  /// properties and avoiding eager access to `.value`) rather than extracting the `.value`
  /// property as in the spec.
  Yield(Value),
  /// Delegation completed (`done: true`) and the outer generator should resume with this value as
  /// the result of the `yield*` expression.
  Complete(Value),
  /// The outer generator should complete with a return completion whose value is this value.
  Return(Value),
}

#[derive(Debug, Clone, Copy)]
enum DelegateOpKind {
  Next,
  Throw,
  Return,
}

#[derive(Debug, Clone, Copy)]
enum AfterCloseKind {
  Throw(Value),
  Return(Value),
}

#[derive(Debug, Clone, Copy)]
enum YieldStarPending {
  /// Waiting for the next async generator request after yielding a value.
  WaitingForRequest,
  /// Waiting for the promise returned by a delegate iterator method call to settle.
  AwaitingDelegate(DelegateOpKind),
  /// Waiting for `AsyncIteratorClose` to settle (used when the delegate lacks `throw`/`return`).
  AwaitingClose {
    after: AfterCloseKind,
    suppress_close_errors: bool,
  },
}

/// Async generator `yield*` delegation state.
///
/// This struct models the `yield*` inner loop for `async function*`:
/// - acquires an async iterator via `GetAsyncIterator`
/// - forwards `next`/`throw`/`return` requests to the delegate iterator methods
/// - awaits the promise returned by those methods to obtain the iterator result object
/// - yields the iterator result object directly when `done` is `false`
/// - resumes the outer generator when `done` is `true`
///
/// Note: this is a low-level helper intended to be driven by an async generator runtime. It does
/// not manage promise job wiring or the outer generator's request queue.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AsyncYieldStar {
  iterator_record: iterator::AsyncIteratorRecord,
  /// Whether the outer generator is in the "returning" state (a `.return()` request was forwarded
  /// and the delegate returned `done: false`).
  returning: bool,
  pending: YieldStarPending,
}

impl Trace for AsyncYieldStar {
  fn trace(&self, tracer: &mut Tracer<'_>) {
    tracer.trace_value(self.iterator_record.iterator);
    tracer.trace_value(self.iterator_record.next_method);
    match self.pending {
      YieldStarPending::AwaitingClose { after, .. } => match after {
        AfterCloseKind::Throw(v) | AfterCloseKind::Return(v) => tracer.trace_value(v),
      },
      _ => {}
    }
  }
}

fn coerce_throw(vm: &Vm, scope: &mut Scope<'_>, err: VmError) -> VmError {
  crate::vm::coerce_error_to_throw(vm, scope, err)
}

fn property_key(scope: &mut Scope<'_>, name: &str) -> Result<PropertyKey, VmError> {
  let s = scope.alloc_string(name)?;
  scope.push_root(Value::String(s))?;
  Ok(PropertyKey::from_string(s))
}

fn await_promise_no_species(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  value: Value,
) -> Result<Value, VmError> {
  // Delegate to the shared `await` Promise resolution helper so async-generator `yield*` uses the
  // same "do not invoke Promise species" semantics as the AST/HIR async evaluators.
  crate::promise_ops::promise_resolve_for_await_with_host_and_hooks(vm, scope, host, hooks, value)
}

fn call_method_1arg_await(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: Value,
  this: Value,
  arg0: Value,
) -> Result<Value, VmError> {
  // Root inputs across the call + PromiseResolve, which can allocate/GC and invoke user code.
  let mut scope = scope.reborrow();
  scope.push_roots(&[callee, this, arg0])?;

  let result = vm.call_with_host_and_hooks(host, &mut scope, hooks, callee, this, &[arg0])?;
  scope.push_root(result)?;
  await_promise_no_species(vm, &mut scope, host, hooks, result)
}

fn get_method(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  receiver: Value,
  name: &str,
) -> Result<Option<Value>, VmError> {
  let key = property_key(scope, name)?;
  crate::spec_ops::get_method_with_host_and_hooks(vm, scope, host, hooks, receiver, key)
}

impl AsyncYieldStar {
  #[inline]
  pub(crate) fn iterator_record(&self) -> iterator::AsyncIteratorRecord {
    self.iterator_record
  }

  #[inline]
  pub(crate) fn iterator(&self) -> Value {
    self.iterator_record.iterator
  }

  #[inline]
  pub(crate) fn next_method(&self) -> Value {
    self.iterator_record.next_method
  }

  #[inline]
  pub(crate) fn is_waiting_for_request(&self) -> bool {
    matches!(self.pending, YieldStarPending::WaitingForRequest)
  }

  pub(crate) fn start(
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    iterable: Value,
  ) -> Result<(Self, YieldStarStep), VmError> {
    let mut scope = scope.reborrow();
    scope.push_root(iterable)?;

    let iterator_record = iterator::get_async_iterator(vm, host, hooks, &mut scope, iterable)
      .map_err(|err| coerce_throw(vm, &mut scope, err))?;

    // Root the delegate iterator and its `next` method across the initial `next` call.
    scope.push_roots(&[iterator_record.iterator, iterator_record.next_method])?;

    // The initial `.next()` argument is always `undefined` (the first async-generator `.next(x)`
    // argument is ignored).
    let awaited = call_method_1arg_await(
      vm,
      &mut scope,
      host,
      hooks,
      iterator_record.next_method,
      iterator_record.iterator,
      Value::Undefined,
    )
    .map_err(|err| coerce_throw(vm, &mut scope, err))?;

    Ok((
      Self {
        iterator_record,
        returning: false,
        pending: YieldStarPending::AwaitingDelegate(DelegateOpKind::Next),
      },
      YieldStarStep::Await(awaited),
    ))
  }

  pub(crate) fn resume_await(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    settle: Result<Value, Value>,
  ) -> Result<YieldStarStep, VmError> {
    let pending = self.pending;
    match pending {
      YieldStarPending::AwaitingDelegate(op) => {
        self.pending = YieldStarPending::WaitingForRequest;

        let iter_result = match settle {
          Ok(v) => v,
          Err(reason) => return Err(VmError::Throw(reason)),
        };

        if !matches!(iter_result, Value::Object(_)) {
          let err = VmError::TypeError("AsyncGenerator yield*: iterator result is not an object");
          return Err(coerce_throw(vm, scope, err));
        }

        let done = iterator::iterator_complete(vm, host, hooks, scope, iter_result)
          .map_err(|err| coerce_throw(vm, scope, err))?;

        if done {
          let value = iterator::iterator_value(vm, host, hooks, scope, iter_result)
            .map_err(|err| coerce_throw(vm, scope, err))?;

          let returning = match op {
            DelegateOpKind::Return => true,
            DelegateOpKind::Next | DelegateOpKind::Throw => self.returning,
          };

          return Ok(if returning {
            YieldStarStep::Return(value)
          } else {
            YieldStarStep::Complete(value)
          });
        }

        // `done: false`: yield the iterator result object to the consumer, deferring access to its
        // `.value` property until user code consumes it.
        if matches!(op, DelegateOpKind::Return) {
          self.returning = true;
        }
        Ok(YieldStarStep::Yield(iter_result))
      }

      YieldStarPending::AwaitingClose {
        after,
        suppress_close_errors,
      } => {
        self.pending = YieldStarPending::WaitingForRequest;

        match settle {
          Ok(_) => {}
          Err(reason) => {
            if !suppress_close_errors {
              return Err(VmError::Throw(reason));
            }
          }
        }

        match after {
          AfterCloseKind::Throw(reason) => Err(VmError::Throw(reason)),
          AfterCloseKind::Return(v) => Ok(YieldStarStep::Return(v)),
        }
      }

      YieldStarPending::WaitingForRequest => Err(VmError::InvariantViolation(
        "AsyncYieldStar resumed from await while not awaiting",
      )),
    }
  }

  pub(crate) fn resume_request(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    request: AsyncGeneratorRequestKind,
  ) -> Result<YieldStarStep, VmError> {
    if !matches!(self.pending, YieldStarPending::WaitingForRequest) {
      return Err(VmError::InvariantViolation(
        "AsyncYieldStar received a generator request while awaiting",
      ));
    }

    match request {
      AsyncGeneratorRequestKind::Next(v) => {
        // Always call `next` with exactly one argument (even when `v` is `undefined`).
        let awaited = call_method_1arg_await(
          vm,
          scope,
          host,
          hooks,
          self.iterator_record.next_method,
          self.iterator_record.iterator,
          v,
        )
        .map_err(|err| coerce_throw(vm, scope, err))?;
        self.pending = YieldStarPending::AwaitingDelegate(DelegateOpKind::Next);
        Ok(YieldStarStep::Await(awaited))
      }

      AsyncGeneratorRequestKind::Throw(reason) => {
        let mut scope = scope.reborrow();
        // Root the thrown reason across GetMethod/calls, which can allocate and trigger GC.
        scope.push_root(reason)?;
        scope.push_root(self.iterator_record.iterator)?;

        let throw_method = get_method(
          vm,
          &mut scope,
          host,
          hooks,
          self.iterator_record.iterator,
          "throw",
        )
        .map_err(|err| coerce_throw(vm, &mut scope, err))?;

        if let Some(throw_method) = throw_method {
          scope.push_root(throw_method)?;
          let awaited = call_method_1arg_await(
            vm,
            &mut scope,
            host,
            hooks,
            throw_method,
            self.iterator_record.iterator,
            reason,
          )
          .map_err(|err| coerce_throw(vm, &mut scope, err))?;

          self.pending = YieldStarPending::AwaitingDelegate(DelegateOpKind::Throw);
          return Ok(YieldStarStep::Await(awaited));
        }

        // No `throw` method: close the iterator, then throw the original reason into the generator.
        let close_res =
          iterator::async_iterator_close(vm, host, hooks, &mut scope, &self.iterator_record)
            .map_err(|err| coerce_throw(vm, &mut scope, err));
        match close_res {
          Ok(promise) => {
            let awaited = await_promise_no_species(vm, &mut scope, host, hooks, promise)
              .map_err(|err| coerce_throw(vm, &mut scope, err))?;
            self.pending = YieldStarPending::AwaitingClose {
              after: AfterCloseKind::Throw(reason),
              suppress_close_errors: true,
            };
            Ok(YieldStarStep::Await(awaited))
          }
          Err(close_err) => {
            if close_err.is_throw_completion() {
              return Err(VmError::Throw(reason));
            }
            Err(close_err)
          }
        }
      }

      AsyncGeneratorRequestKind::Return(v) => {
        let mut scope = scope.reborrow();
        scope.push_root(v)?;
        scope.push_root(self.iterator_record.iterator)?;

        let return_method = get_method(
          vm,
          &mut scope,
          host,
          hooks,
          self.iterator_record.iterator,
          "return",
        )
        .map_err(|err| coerce_throw(vm, &mut scope, err))?;

        if let Some(return_method) = return_method {
          scope.push_root(return_method)?;
          let awaited = call_method_1arg_await(
            vm,
            &mut scope,
            host,
            hooks,
            return_method,
            self.iterator_record.iterator,
            v,
          )
          .map_err(|err| coerce_throw(vm, &mut scope, err))?;

          self.pending = YieldStarPending::AwaitingDelegate(DelegateOpKind::Return);
          return Ok(YieldStarStep::Await(awaited));
        }

        // No `return` method: close the iterator, then complete with the outer return value.
        let close_promise =
          iterator::async_iterator_close(vm, host, hooks, &mut scope, &self.iterator_record)
            .map_err(|err| coerce_throw(vm, &mut scope, err))?;

        let awaited = await_promise_no_species(vm, &mut scope, host, hooks, close_promise)
          .map_err(|err| coerce_throw(vm, &mut scope, err))?;

        self.pending = YieldStarPending::AwaitingClose {
          after: AfterCloseKind::Return(v),
          suppress_close_errors: false,
        };
        Ok(YieldStarStep::Await(awaited))
      }
    }
  }
}

fn pat_has_default_value(pat: &parse_js::ast::expr::pat::Pat) -> bool {
  use parse_js::ast::expr::pat::Pat;
  match pat {
    Pat::Arr(arr) => {
      for elem in arr.stx.elements.iter().flatten() {
        if elem.default_value.is_some() || pat_has_default_value(&elem.target.stx) {
          return true;
        }
      }
      if let Some(rest) = &arr.stx.rest {
        if pat_has_default_value(&rest.stx) {
          return true;
        }
      }
      false
    }
    Pat::Obj(obj) => {
      for prop in &obj.stx.properties {
        if prop.stx.default_value.is_some() || pat_has_default_value(&prop.stx.target.stx) {
          return true;
        }
      }
      if let Some(rest) = &obj.stx.rest {
        if pat_has_default_value(&rest.stx) {
          return true;
        }
      }
      false
    }
    Pat::Id(_) => false,
    Pat::AssignTarget(_) => false,
  }
}

fn hir_pat_has_default_value(body: &hir_js::Body, pat_id: hir_js::PatId) -> Result<bool, VmError> {
  let idx = usize::try_from(pat_id.0).map_err(|_| VmError::OutOfMemory)?;
  let pat = body.pats.get(idx).ok_or(VmError::InvariantViolation(
    "hir pattern id missing from body",
  ))?;
  match &pat.kind {
    hir_js::PatKind::Ident(_) => Ok(false),
    hir_js::PatKind::Assign { .. } => Ok(true),
    hir_js::PatKind::AssignTarget(_) => Ok(false),
    hir_js::PatKind::Rest(inner) => hir_pat_has_default_value(body, **inner),
    hir_js::PatKind::Array(arr) => {
      for elem in arr.elements.iter().flatten() {
        if elem.default_value.is_some() || hir_pat_has_default_value(body, elem.pat)? {
          return Ok(true);
        }
      }
      if let Some(rest) = arr.rest {
        if hir_pat_has_default_value(body, rest)? {
          return Ok(true);
        }
      }
      Ok(false)
    }
    hir_js::PatKind::Object(obj) => {
      for prop in &obj.props {
        if prop.default_value.is_some() || hir_pat_has_default_value(body, prop.value)? {
          return Ok(true);
        }
      }
      if let Some(rest) = obj.rest {
        if hir_pat_has_default_value(body, rest)? {
          return Ok(true);
        }
      }
      Ok(false)
    }
  }
}

fn async_generator_needs_deferred_start(
  scope: &Scope<'_>,
  generator: GcObject,
) -> Result<Option<GcObject>, VmError> {
  let Some(cont) = scope.heap().async_generator_continuation(generator)? else {
    return Ok(None);
  };

  // Defer start when parameter binding would evaluate default initializers. This matches the
  // observable test expectation that default parameter expressions are evaluated only once a
  // microtask checkpoint runs.
  match &cont.func {
    AsyncGeneratorFunc::Ast(func) => {
      for param in &func.stx.parameters {
        if param.stx.default_value.is_some() || pat_has_default_value(&param.stx.pattern.stx.pat.stx) {
          return Ok(Some(cont.env.global_object()));
        }
      }
    }
    AsyncGeneratorFunc::Hir(func_ref) => {
      let body = func_ref
        .script
        .hir
        .body(func_ref.body)
        .ok_or(VmError::InvariantViolation("compiled function body not found"))?;
      let Some(func_meta) = body.function.as_ref() else {
        return Err(VmError::InvariantViolation("function body missing metadata"));
      };
      for param in &func_meta.params {
        if param.default.is_some() || hir_pat_has_default_value(body, param.pat)? {
          return Ok(Some(cont.env.global_object()));
        }
      }
    }
  }

  Ok(None)
}

fn enqueue_async_generator_deferred_start_job(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  hooks: &mut dyn VmHostHooks,
  generator: GcObject,
  global_object: GcObject,
) -> Result<(), VmError> {
  let call_id = vm.async_generator_deferred_start_call_id()?;
  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
  let job_realm = vm.current_realm();
  let script_or_module_token = match vm.get_active_script_or_module() {
    Some(sm) => Some(vm.intern_script_or_module(sm)?),
    None => None,
  };

  let mut schedule_scope = scope.reborrow();
  schedule_scope.push_root(Value::Object(generator))?;

  let name = schedule_scope.alloc_string("")?;
  let slots = [Value::Object(generator)];
  let cb = schedule_scope.alloc_native_function_with_slots(call_id, None, name, 0, &slots)?;
  schedule_scope.push_root(Value::Object(cb))?;
  schedule_scope
    .heap_mut()
    .object_set_prototype(cb, Some(intr.function_prototype()))?;
  schedule_scope.heap_mut().set_function_realm(cb, global_object)?;
  if let Some(realm) = job_realm {
    schedule_scope.heap_mut().set_function_job_realm(cb, realm)?;
  }
  if let Some(token) = script_or_module_token {
    schedule_scope
      .heap_mut()
      .set_function_script_or_module_token(cb, Some(token))?;
  }

  let job = Job::new(JobKind::Promise, move |ctx, host| {
    ctx.call(host, Value::Object(cb), Value::Undefined, &[])?;
    Ok(())
  })?;

  // Root captured values until the job runs.
  let mut roots: Vec<RootId> = Vec::new();
  roots
    .try_reserve_exact(1)
    .map_err(|_| VmError::OutOfMemory)?;

  let root_res = schedule_scope.heap_mut().add_root(Value::Object(cb));
  let cb_root = match root_res {
    Ok(id) => id,
    Err(e) => return Err(e),
  };
  roots.push(cb_root);

  hooks.host_enqueue_promise_job(job.with_roots(roots), job_realm);
  Ok(())
}

/// `AsyncGeneratorEnqueue ( generator, completion, promiseCapability )`.
///
/// This is a spec-shaped helper that appends a request to `generator.[[AsyncGeneratorQueue]]`.
pub(crate) fn async_generator_enqueue(
  scope: &mut Scope<'_>,
  generator: GcObject,
  kind: AsyncGeneratorRequestKind,
  capability: PromiseCapability,
) -> Result<(), VmError> {
  // Root inputs across queue growth/GC.
  let mut scope = scope.reborrow();
  let mut roots = [Value::Undefined; 5];
  let mut root_count = 0usize;
  roots[root_count] = Value::Object(generator);
  root_count += 1;
  match kind {
    AsyncGeneratorRequestKind::Next(v)
    | AsyncGeneratorRequestKind::Return(v)
    | AsyncGeneratorRequestKind::Throw(v) => {
      roots[root_count] = v;
      root_count += 1;
    }
  }
  roots[root_count] = capability.promise;
  root_count += 1;
  roots[root_count] = capability.resolve;
  root_count += 1;
  roots[root_count] = capability.reject;
  root_count += 1;
  scope.push_roots(&roots[..root_count])?;

  scope.heap_mut().async_generator_request_queue_push(
    generator,
    AsyncGeneratorRequest { kind, capability },
  )
}

/// Native callback used by deferred async-generator start jobs (parameter default initializers).
///
/// Slots:
/// - slot 0: generator object
pub(crate) fn async_generator_deferred_start_call(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  let generator = match slots.get(0).copied().unwrap_or(Value::Undefined) {
    Value::Object(obj) => obj,
    _ => {
      return Err(VmError::InvariantViolation(
        "async generator deferred start callback missing generator slot",
      ))
    }
  };

  // This callback is scheduled as a Promise job to ensure async-generator parameter default
  // initializers are evaluated asynchronously (at the first `.next()`), matching observable
  // behaviour in tests and the spec's job-queued execution model.
  crate::exec::async_generator_resume_next(vm, scope, host, hooks, generator)?;
  Ok(Value::Undefined)
}

/// `AsyncGeneratorResumeNext ( generator )`.
pub(crate) fn async_generator_resume_next(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  generator: GcObject,
) -> Result<(), VmError> {
  // Defer starting `async function*` bodies that have default parameter initializers so their
  // evaluation is job-queued (and therefore observable only after a microtask checkpoint).
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(generator))?;

  let state = scope.heap().async_generator_state(generator)?;
  if state == AsyncGeneratorState::SuspendedStart {
    if let Some(req) = scope.heap().async_generator_request_queue_peek(generator)? {
      if matches!(req.kind, AsyncGeneratorRequestKind::Next(_)) {
        if let Some(global_object) = async_generator_needs_deferred_start(&scope, generator)? {
          enqueue_async_generator_deferred_start_job(vm, &mut scope, hooks, generator, global_object)?;
          return Ok(());
        }
      }
    }
  }

  crate::exec::async_generator_resume_next(vm, &mut scope, host, hooks, generator)
}
