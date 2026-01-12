//! ECMAScript Promise abstract operations used by module loading algorithms.
//!
//! `vm-js` implements the `%Promise%` built-in primarily via [`crate::builtins`]. Some spec
//! algorithms (notably module loading and dynamic import continuations) need direct access to
//! Promise abstract operations such as:
//! - `NewPromiseCapability(%Promise%)`
//! - `PromiseResolve(%Promise%, value)`
//! - `PerformPromiseThen(promise, onFulfilled, onRejected)`
//!
//! This module exposes small, spec-shaped helpers that are convenient to call from engine code
//! without going through property lookups on the global `Promise` constructor.

use crate::{PromiseCapability, PropertyKey, Scope, Value, Vm, VmError, VmHost, VmHostHooks};

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

  // `PromiseResolve(%Promise%, value)` (ECMA-262).
  //
  // Note: `PromiseResolve` can allocate and can invoke user code:
  // - `Get(value, "constructor")` can invoke accessors.
  // - Creating the capability allocates a Promise + resolving functions.
  // - Calling the resolve function can trigger thenable resolution.
  //
  // Root the input value (and `%Promise%`) up front so they cannot be collected during those
  // operations.
  let promise_ctor = Value::Object(intr.promise());
  let mut scope = scope.reborrow();
  scope.push_roots(&[value, promise_ctor])?;

  // 1. If IsPromise(value) is true:
  //    a. Let xConstructor be ? Get(value, "constructor").
  //    b. If SameValue(xConstructor, %Promise%) is true, return value.
  if let Value::Object(obj) = value {
    if scope.heap().is_promise_object(obj) {
      let ctor_key_s = scope.alloc_string("constructor")?;
      scope.push_root(Value::String(ctor_key_s))?;
      let ctor_key = PropertyKey::from_string(ctor_key_s);

      let x_ctor = scope.ordinary_get_with_host_and_hooks(
        vm,
        host_ctx,
        hooks,
        obj,
        ctor_key,
        Value::Object(obj),
      )?;
      if x_ctor.same_value(promise_ctor, scope.heap()) {
        return Ok(value);
      }
    }
  }

  let cap = new_promise_capability_with_host_and_hooks(vm, &mut scope, host_ctx, hooks)?;
  // Root the newly-created promise and resolve function across the `Call(resolve, ...)`, which can
  // allocate and run user code.
  scope.push_roots(&[cap.promise, cap.resolve])?;
  let _ = vm.call_with_host_and_hooks(
    host_ctx,
    &mut scope,
    hooks,
    cap.resolve,
    Value::Undefined,
    &[value],
  )?;
  Ok(cap.promise)
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
  // `PerformPromiseThen` does not currently need the host context, but accept it so embeddings can
  // thread it through spec-shaped helper APIs consistently.
  let _ = host_ctx;
  crate::builtins::perform_promise_then_no_capability(vm, scope, hooks, promise, on_fulfilled, on_rejected)
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
