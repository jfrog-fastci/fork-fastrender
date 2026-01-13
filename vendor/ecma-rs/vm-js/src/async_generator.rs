use crate::heap::{AsyncGeneratorRequestKind, Trace, Tracer};
use crate::iterator;
use crate::property::PropertyKey;
use crate::{Scope, Value, Vm, VmError, VmHost, VmHostHooks};

#[derive(Debug)]
pub(crate) enum YieldStarStep {
  /// Await the provided promise and resume the yield* state machine with the promise settlement.
  Await(Value),
  /// Yield the provided iterator result object to the async generator consumer.
  ///
  /// This is intentionally the *delegate iterator result object* (not its `.value`), matching the
  /// sync-generator `yield*` behavior in vm-js and ensuring we do not eagerly access or unwrap the
  /// `value` property when `done` is false.
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

        // `done: false`: yield the iterator result object directly (do not eagerly access `.value`).
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
