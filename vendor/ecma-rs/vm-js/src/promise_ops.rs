//! ECMAScript Promise abstract operations used by module loading algorithms.
//!
//! `vm-js` implements the `%Promise%` built-in primarily via [`crate::builtins`]. Some spec
//! algorithms (notably module loading and dynamic import continuations) need direct access to
//! Promise abstract operations such as:
//! - `NewPromiseCapability(%Promise%)`
//! - `PromiseResolve(%Promise%, value)`
//! - `PerformPromiseThen(promise, onFulfilled, onRejected, resultCapability)`
//!
//! This module exposes small, spec-shaped helpers that are convenient to call from engine code
//! without going through property lookups on the global `Promise` constructor.

use crate::{PromiseCapability, Scope, Value, Vm, VmError, VmHost, VmHostHooks};

/// `NewPromiseCapability(%Promise%)`.
pub fn new_promise_capability_with_host_and_hooks(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host_ctx: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
) -> Result<PromiseCapability, VmError> {
  let intr = vm.intrinsics().ok_or(VmError::Unimplemented(
    "NewPromiseCapability requires intrinsics (create a Realm first)",
  ))?;
  crate::builtins::new_promise_capability_with_host_and_hooks(
    vm,
    scope,
    host_ctx,
    hooks,
    Value::Object(intr.promise()),
  )
}

/// Convenience wrapper around [`new_promise_capability_with_host_and_hooks`] that passes a dummy
/// host context (`()`).
///
/// Promise construction and resolution can invoke user JS (thenables and `then` callbacks), so host
/// embeddings that need native handlers to observe real host state should prefer
/// [`new_promise_capability_with_host_and_hooks`].
pub fn new_promise_capability(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  hooks: &mut dyn VmHostHooks,
) -> Result<PromiseCapability, VmError> {
  let mut dummy_host = ();
  new_promise_capability_with_host_and_hooks(vm, scope, &mut dummy_host, hooks)
}

/// `PromiseResolve(%Promise%, value)`.
///
/// Returns a Promise object.
pub fn promise_resolve_with_host_and_hooks(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host_ctx: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  value: Value,
) -> Result<Value, VmError> {
  let intr = vm.intrinsics().ok_or(VmError::Unimplemented(
    "PromiseResolve requires intrinsics (create a Realm first)",
  ))?;

  // PromiseResolve(%Promise%, x) must observe `x.constructor` when `x` is a Promise object.
  //
  // Spec: https://tc39.es/ecma262/#sec-promise-resolve
  let promise_obj = crate::builtins::promise_resolve_abstract(
    vm,
    scope,
    host_ctx,
    hooks,
    Value::Object(intr.promise()),
    value,
  )?;
  Ok(Value::Object(promise_obj))
}

/// Convenience wrapper around [`promise_resolve_with_host_and_hooks`] that passes a dummy host
/// context (`()`).
///
/// Promise resolution can invoke user JS (thenables), so host embeddings that need native handlers
/// to observe real host state should prefer [`promise_resolve_with_host_and_hooks`].
pub fn promise_resolve(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  hooks: &mut dyn VmHostHooks,
  value: Value,
) -> Result<Value, VmError> {
  let mut dummy_host = ();
  promise_resolve_with_host_and_hooks(vm, scope, &mut dummy_host, hooks, value)
}

/// `PerformPromiseThen(promise, onFulfilled, onRejected, resultCapability)`.
///
/// When `result_capability` is `None` (spec `undefined`), this attaches Promise reactions without
/// creating a derived Promise and returns `Ok(None)`.
///
/// When `result_capability` is `Some`, this uses the provided capability and returns the capability
/// promise.
pub fn perform_promise_then_with_result_capability_with_host_and_hooks(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host_ctx: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  promise: Value,
  on_fulfilled: Value,
  on_rejected: Value,
  result_capability: Option<PromiseCapability>,
) -> Result<Option<Value>, VmError> {
  // `PerformPromiseThen` does not currently need the host context, but accept it so embeddings can
  // thread it through spec-shaped helper APIs consistently.
  let _ = host_ctx;

  match result_capability {
    None => {
      crate::builtins::perform_promise_then_no_capability(vm, scope, hooks, promise, on_fulfilled, on_rejected)?;
      Ok(None)
    }
    Some(capability) => {
      let Value::Object(promise_obj) = promise else {
        return Err(VmError::TypeError("expected Promise object"));
      };
      if !scope.heap().is_promise_object(promise_obj) {
        return Err(VmError::TypeError("expected Promise object"));
      }

      let promise = crate::builtins::perform_promise_then_with_capability(
        vm,
        scope,
        hooks,
        promise_obj,
        on_fulfilled,
        on_rejected,
        capability,
      )?;
      Ok(Some(promise))
    }
  }
}

/// Convenience wrapper around [`perform_promise_then_with_result_capability_with_host_and_hooks`]
/// that passes a dummy host context (`()`).
pub fn perform_promise_then_with_result_capability(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  hooks: &mut dyn VmHostHooks,
  promise: Value,
  on_fulfilled: Value,
  on_rejected: Value,
  result_capability: Option<PromiseCapability>,
) -> Result<Option<Value>, VmError> {
  let mut dummy_host = ();
  perform_promise_then_with_result_capability_with_host_and_hooks(
    vm,
    scope,
    &mut dummy_host,
    hooks,
    promise,
    on_fulfilled,
    on_rejected,
    result_capability,
  )
}

/// `PerformPromiseThen(promise, onFulfilled, onRejected)`.
///
/// Returns the derived Promise (the value returned by `promise.then(...)`).
pub fn perform_promise_then_with_host_and_hooks(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host_ctx: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  promise: Value,
  on_fulfilled: Option<Value>,
  on_rejected: Option<Value>,
) -> Result<Value, VmError> {
  // `PerformPromiseThen` currently does not need the host context, but accept it so embeddings can
  // thread it through spec-shaped helper APIs consistently.
  let _ = host_ctx;
  crate::builtins::perform_promise_then(
    vm,
    scope,
    hooks,
    promise,
    on_fulfilled.unwrap_or(Value::Undefined),
    on_rejected.unwrap_or(Value::Undefined),
  )
}

/// `PerformPromiseThen(promise, onFulfilled, onRejected, resultCapability = undefined)`.
///
/// This is used by async/await and module top-level await: it attaches Promise reactions without
/// creating a derived promise (and therefore must not trigger Promise species side effects).
pub fn perform_promise_then_no_capability_with_host_and_hooks(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host_ctx: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  promise: Value,
  on_fulfilled: Value,
  on_rejected: Value,
) -> Result<(), VmError> {
  let _ = perform_promise_then_with_result_capability_with_host_and_hooks(
    vm,
    scope,
    host_ctx,
    hooks,
    promise,
    on_fulfilled,
    on_rejected,
    None,
  )?;
  Ok(())
}

/// `PerformPromiseThen(promise, onFulfilled, onRejected, resultCapability)`.
///
/// This is used by spec algorithms that must attach reactions to `promise` while wiring the result
/// into an **explicit PromiseCapability** record (and therefore must not create a derived promise
/// or trigger Promise species/constructor side effects).
///
/// Returns `capability.promise` (the passed-in capability's promise).
pub fn perform_promise_then_with_capability_with_host_and_hooks(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host_ctx: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  promise: Value,
  on_fulfilled: Value,
  on_rejected: Value,
  capability: PromiseCapability,
) -> Result<Value, VmError> {
  // `PerformPromiseThen` does not currently need the host context, but accept it so embeddings can
  // thread it through spec-shaped helper APIs consistently.
  let _ = host_ctx;

  let Value::Object(promise_obj) = promise else {
    return Err(VmError::TypeError("expected Promise object"));
  };
  if !scope.heap().is_promise_object(promise_obj) {
    return Err(VmError::TypeError("expected Promise object"));
  }

  crate::builtins::perform_promise_then_with_capability(
    vm,
    scope,
    hooks,
    promise_obj,
    on_fulfilled,
    on_rejected,
    capability,
  )
}

/// Convenience wrapper around [`perform_promise_then_with_host_and_hooks`] that passes a dummy host
/// context (`()`).
///
/// Promise reactions can invoke user JS, so host embeddings that need native handlers to observe
/// real host state should prefer [`perform_promise_then_with_host_and_hooks`].
pub fn perform_promise_then(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  hooks: &mut dyn VmHostHooks,
  promise: Value,
  on_fulfilled: Option<Value>,
  on_rejected: Option<Value>,
) -> Result<Value, VmError> {
  let mut dummy_host = ();
  perform_promise_then_with_host_and_hooks(
    vm,
    scope,
    &mut dummy_host,
    hooks,
    promise,
    on_fulfilled,
    on_rejected,
  )
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::property::{PropertyDescriptor, PropertyKey, PropertyKind};
  use crate::{GcObject, Heap, HeapLimits, MicrotaskQueue, Realm, RootId, VmJobContext, VmOptions};

  fn throw_on_constructor_get(
    _vm: &mut Vm,
    _scope: &mut Scope<'_>,
    _host: &mut dyn VmHost,
    _hooks: &mut dyn VmHostHooks,
    _callee: GcObject,
    _this: Value,
    _args: &[Value],
  ) -> Result<Value, VmError> {
    Err(VmError::Throw(Value::Number(1.0)))
  }

  fn noop(
    _vm: &mut Vm,
    _scope: &mut Scope<'_>,
    _host: &mut dyn VmHost,
    _hooks: &mut dyn VmHostHooks,
    _callee: GcObject,
    _this: Value,
    _args: &[Value],
  ) -> Result<Value, VmError> {
    Ok(Value::Undefined)
  }

  #[test]
  fn perform_promise_then_does_not_observe_constructor() -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;

    let mut host_ctx = ();
    let mut hooks = MicrotaskQueue::new();

    let result: Result<(), VmError> = (|| {
      let intr = realm.intrinsics();
      let mut scope = heap.scope();

      let promise = promise_resolve_with_host_and_hooks(
        &mut vm,
        &mut scope,
        &mut host_ctx,
        &mut hooks,
        Value::Number(0.0),
      )?;
      scope.push_root(promise)?;

      // Install a throwing `constructor` getter on `%Promise.prototype%`. `PerformPromiseThen`
      // (both with and without `resultCapability`) must not consult `promise.constructor`.
      let getter_id = vm.register_native_call(throw_on_constructor_get)?;
      let getter_name = scope.alloc_string("")?;
      let getter_fn = scope.alloc_native_function(getter_id, None, getter_name, 0)?;
      scope.push_root(Value::Object(getter_fn))?;
      scope
        .heap_mut()
        .object_set_prototype(getter_fn, Some(intr.function_prototype()))?;

      let ctor_key_s = scope.alloc_string("constructor")?;
      scope.push_root(Value::String(ctor_key_s))?;
      scope.define_property(
        intr.promise_prototype(),
        PropertyKey::from_string(ctor_key_s),
        PropertyDescriptor {
          enumerable: false,
          configurable: true,
          kind: PropertyKind::Accessor {
            get: Value::Object(getter_fn),
            set: Value::Undefined,
          },
        },
      )?;

      let noop_id = vm.register_native_call(noop)?;
      let handler_name = scope.alloc_string("")?;
      let on_fulfilled = scope.alloc_native_function(noop_id, None, handler_name, 0)?;
      scope.push_root(Value::Object(on_fulfilled))?;
      scope
        .heap_mut()
        .object_set_prototype(on_fulfilled, Some(intr.function_prototype()))?;
      let handler_name = scope.alloc_string("")?;
      let on_rejected = scope.alloc_native_function(noop_id, None, handler_name, 0)?;
      scope.push_root(Value::Object(on_rejected))?;
      scope
        .heap_mut()
        .object_set_prototype(on_rejected, Some(intr.function_prototype()))?;

      let res = perform_promise_then_with_result_capability_with_host_and_hooks(
        &mut vm,
        &mut scope,
        &mut host_ctx,
        &mut hooks,
        promise,
        Value::Object(on_fulfilled),
        Value::Object(on_rejected),
        None,
      )?;
      assert!(res.is_none());

      let cap = new_promise_capability_with_host_and_hooks(&mut vm, &mut scope, &mut host_ctx, &mut hooks)?;
      let res = perform_promise_then_with_result_capability_with_host_and_hooks(
        &mut vm,
        &mut scope,
        &mut host_ctx,
        &mut hooks,
        promise,
        Value::Object(on_fulfilled),
        Value::Object(on_rejected),
        Some(cap),
      )?;
      assert_eq!(res, Some(cap.promise));

      Ok(())
    })();

    // `PerformPromiseThen` may enqueue Promise jobs (e.g. when the input promise is already
    // fulfilled). Tear down any queued jobs so their persistent roots are cleaned up before the
    // heap is dropped.
    struct TeardownCtx<'a> {
      heap: &'a mut Heap,
    }

    impl VmJobContext for TeardownCtx<'_> {
      fn call(
        &mut self,
        _hooks: &mut dyn VmHostHooks,
        _callee: Value,
        _this: Value,
        _args: &[Value],
      ) -> Result<Value, VmError> {
        Err(VmError::Unimplemented("TeardownCtx::call"))
      }

      fn construct(
        &mut self,
        _hooks: &mut dyn VmHostHooks,
        _callee: Value,
        _args: &[Value],
        _new_target: Value,
      ) -> Result<Value, VmError> {
        Err(VmError::Unimplemented("TeardownCtx::construct"))
      }

      fn add_root(&mut self, value: Value) -> Result<RootId, VmError> {
        self.heap.add_root(value)
      }

      fn remove_root(&mut self, id: RootId) {
        self.heap.remove_root(id)
      }
    }

    let mut teardown_ctx = TeardownCtx { heap: &mut heap };
    hooks.teardown(&mut teardown_ctx);

    realm.teardown(&mut heap);
    result
  }
}
