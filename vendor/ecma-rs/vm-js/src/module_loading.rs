//! ECMAScript module loading host hooks and record types.
//!
//! This module defines a **spec-shaped**, evaluator-independent API surface for integrating the VM
//! with a host environment's module loader (e.g. HTML's event loop + network fetch).
//!
//! ## Spec references
//!
//! - [`EvaluateImportCall`](https://tc39.es/ecma262/#sec-evaluate-import-call)
//! - [`ContinueDynamicImport`](https://tc39.es/ecma262/#sec-continuedynamicimport)
//! - [`HostLoadImportedModule`](https://tc39.es/ecma262/#sec-hostloadimportedmodule)
//! - [`FinishLoadingImportedModule`](https://tc39.es/ecma262/#sec-finishloadingimportedmodule)
//! - [`ModuleRequestsEqual`](https://tc39.es/ecma262/#sec-modulerequestsequal)
//!
//! This layer is responsible for the *host-driven* parts of module loading:
//!
//! - Starting static graph loading (`LoadRequestedModules`),
//! - Completing host loads (`FinishLoadingImportedModule`),
//! - And the dynamic `import()` state machine (`EvaluateImportCall` / `ContinueDynamicImport`).
//!
//! Module record algorithms (linking, evaluation, top-level await execution) live in
//! [`crate::ModuleGraph`]. The host is still responsible for fetching/parsing source text modules in
//! [`crate::VmHostHooks::host_load_imported_module`].
//!
//! For an end-to-end embedder guide, see [`crate::docs::modules`].
//!
//! See also:
//! - [`crate::VmHostHooks::host_load_imported_module`]
//! - [`Vm::finish_loading_imported_module`]

use crate::module_graph::ModuleGraph;
use crate::module_record::ModuleStatus;
use crate::module_record::PromiseCapabilityRoots;
use crate::property::PropertyKey;
use crate::promise::PromiseCapability;
use crate::{
  GcObject, GcString, ImportAttribute, JsString, LoadedModuleRequest, ModuleId, ModuleRequest,
  RealmId, Scope, ScriptId, ScriptOrModule, Value, Vm, VmError, VmHost, VmHostHooks,
};
use std::any::Any;
use std::alloc::{alloc, Layout};
use std::cell::{Cell, RefCell};
use std::fmt;
use std::mem;
use std::ptr;
use std::rc::Rc;
use std::sync::Arc;

fn rc_try_new_vm<T>(value: T) -> Result<Rc<T>, VmError> {
  #[repr(C)]
  struct RcBox<T> {
    // Match the standard library's `RcBox` layout: strong and weak counts followed by `value`.
    //
    // We initialise both counts to 1, matching `Rc::new`'s semantics:
    // - one strong reference (the returned `Rc`)
    // - one implicit weak reference (keeps the allocation alive until all `Weak`s are dropped)
    strong: Cell<usize>,
    weak: Cell<usize>,
    value: T,
  }

  let layout = Layout::new::<RcBox<T>>();
  // SAFETY: We allocate enough space for the `RcBox<T>` header + `T`, initialise it, and then
  // construct an `Rc<T>` using a pointer to the `value` field (as required by `Rc::from_raw`).
  unsafe {
    let raw = alloc(layout) as *mut RcBox<T>;
    if raw.is_null() {
      return Err(VmError::OutOfMemory);
    }
    ptr::addr_of_mut!((*raw).strong).write(Cell::new(1));
    ptr::addr_of_mut!((*raw).weak).write(Cell::new(1));
    ptr::addr_of_mut!((*raw).value).write(value);
    let value_ptr = ptr::addr_of!((*raw).value);
    Ok(Rc::from_raw(value_ptr))
  }
}

/// The *identity* of the `referrer` passed to `HostLoadImportedModule`/`FinishLoadingImportedModule`.
///
/// Per ECMA-262, the referrer is a union of:
/// - Script Record
/// - Cyclic Module Record
/// - Realm Record
///
/// This enum is intentionally **identity-only**: it can be stored across asynchronous boundaries
/// without holding `&` references into the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModuleReferrer {
  Script(ScriptId),
  Module(ModuleId),
  Realm(RealmId),
}

/// Minimal access to an ECMA-262 `[[LoadedModules]]` list.
///
/// In the specification, Script Records, Cyclic Module Records, and Realm Records each have a
/// `[[LoadedModules]]` internal slot used by `FinishLoadingImportedModule` to memoize the result of
/// loading a `(specifier, attributes)` module request.
///
/// This trait exists so `FinishLoadingImportedModule` can be implemented in a reusable, spec-shaped
/// way without committing to concrete Script/Module/Realm record representations yet.
pub trait LoadedModulesOwner {
  fn loaded_modules(&self) -> &[LoadedModuleRequest<ModuleId>];
  fn loaded_modules_mut(&mut self) -> &mut Vec<LoadedModuleRequest<ModuleId>>;
}

impl LoadedModulesOwner for Vec<LoadedModuleRequest<ModuleId>> {
  #[inline]
  fn loaded_modules(&self) -> &[LoadedModuleRequest<ModuleId>] {
    self.as_slice()
  }

  #[inline]
  fn loaded_modules_mut(&mut self) -> &mut Vec<LoadedModuleRequest<ModuleId>> {
    self
  }
}

/// Host-defined data passed through `HostLoadImportedModule`.
///
/// In ECMA-262, `_hostDefined_` is typed as "anything" and is carried through spec algorithms.
///
/// This is an opaque record to the VM; the embedding chooses what to store.
#[derive(Clone, Default)]
pub struct HostDefined(Option<Arc<dyn Any + Send + Sync>>);

impl HostDefined {
  /// Wrap host-defined data.
  pub fn new<T: Any + Send + Sync>(data: T) -> Self {
    Self(Some(Arc::new(data)))
  }

  /// Attempts to downcast the payload by reference.
  pub fn downcast_ref<T: Any>(&self) -> Option<&T> {
    self.0.as_ref()?.downcast_ref::<T>()
  }
}

impl fmt::Debug for HostDefined {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match &self.0 {
      Some(v) => f
        .debug_struct("HostDefined")
        .field("type_id", &v.type_id())
        .finish(),
      None => f.debug_struct("HostDefined").field("value", &"undefined").finish(),
    }
  }
}

#[derive(Debug)]
struct GraphLoadingStateInner {
  promise_capability: PromiseCapability,
  promise_roots: Option<PromiseCapabilityRoots>,
  dynamic_import: Option<DynamicImportContinuation>,
  is_loading: bool,
  pending_modules_count: usize,
  visited: Vec<ModuleId>,
  host_defined: HostDefined,
}

impl Drop for GraphLoadingStateInner {
  fn drop(&mut self) {
    // Avoid panicking from a destructor while unwinding (that would abort).
    if std::thread::panicking() {
      return;
    }
    debug_assert!(
      self.promise_roots.is_none() && self.dynamic_import.is_none(),
      "GraphLoadingState dropped with leaked persistent roots; ensure the graph-loading promise is settled"
    );
  }
}

/// Opaque token representing the spec's `GraphLoadingState` record.
///
/// This is an engine-owned continuation state used by *static module loading* and passed through
/// the host's `HostLoadImportedModule` hook in the `_payload_` position.
///
/// The host MUST treat this value as opaque and pass it back unchanged in
/// `FinishLoadingImportedModule`.
#[derive(Clone)]
pub struct GraphLoadingState(Rc<RefCell<GraphLoadingStateInner>>);

impl fmt::Debug for GraphLoadingState {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    // Treat as opaque to hosts.
    let _ = &self.0;
    f.write_str("GraphLoadingState(..)")
  }
}

impl GraphLoadingState {
  fn new(
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    host_ctx: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    host_defined: HostDefined,
  ) -> Result<(Self, Value), VmError> {
    // Create a nested scope so any temporary stack roots created while constructing the promise
    // capability (and while registering persistent roots) are popped before we return.
    //
    // `GraphLoadingState` itself keeps the capability values alive via persistent roots.
    let (cap, promise_roots) = {
      let mut root_scope = scope.reborrow();
      let cap = crate::promise_ops::new_promise_capability_with_host_and_hooks(
        vm,
        &mut root_scope,
        host_ctx,
        hooks,
      )?;
      let promise_roots = PromiseCapabilityRoots::new(&mut root_scope, cap)?;
      Ok((cap, promise_roots))
    }?;

    let state = match rc_try_new_vm(RefCell::new(GraphLoadingStateInner {
      promise_capability: cap,
      promise_roots: None,
      dynamic_import: None,
      is_loading: true,
      pending_modules_count: 1,
      visited: Vec::new(),
      host_defined,
    })) {
      Ok(rc) => GraphLoadingState(rc),
      Err(err) => {
        promise_roots.teardown(scope.heap_mut());
        return Err(err);
      }
    };
    state.0.borrow_mut().promise_roots = Some(promise_roots);

    Ok((state, cap.promise))
  }

  fn set_dynamic_import(&self, state: DynamicImportState, module: ModuleId) -> Result<(), VmError> {
    let mut inner = self.0.borrow_mut();
    if inner.dynamic_import.is_some() {
      return Err(VmError::InvariantViolation(
        "GraphLoadingState already has a dynamic import continuation",
      ));
    }
    inner.dynamic_import = Some(DynamicImportContinuation { state, module });
    Ok(())
  }

  fn is_loading(&self) -> bool {
    self.0.borrow().is_loading
  }

  fn set_is_loading(&self, value: bool) {
    self.0.borrow_mut().is_loading = value;
  }

  fn host_defined(&self) -> HostDefined {
    self.0.borrow().host_defined.clone()
  }

  fn visited_contains(&self, module: ModuleId) -> bool {
    self.0.borrow().visited.contains(&module)
  }

  fn push_visited(&self, module: ModuleId) -> Result<(), VmError> {
    let mut state = self.0.borrow_mut();
    state.visited.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
    state.visited.push(module);
    Ok(())
  }

  fn inc_pending(&self, delta: usize) -> Result<(), VmError> {
    let mut state = self.0.borrow_mut();
    state.pending_modules_count = state
      .pending_modules_count
      .checked_add(delta)
      .ok_or(VmError::LimitExceeded(
        "module graph loader pending module count overflow",
      ))?;
    Ok(())
  }

  fn dec_pending(&self) -> usize {
    let mut state = self.0.borrow_mut();
    debug_assert!(state.pending_modules_count > 0, "pendingModulesCount underflow");
    state.pending_modules_count = state.pending_modules_count.saturating_sub(1);
    state.pending_modules_count
  }

  fn resolve_promise(
    &self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    modules: &mut ModuleGraph,
    host_ctx: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
  ) -> Result<(), VmError> {
    let (cap, roots, dynamic_import) = {
      let mut state = self.0.borrow_mut();
      (
        state.promise_capability,
        state.promise_roots.take(),
        state.dynamic_import.take(),
      )
    };

    // Settlement is best-effort: if roots are already dropped, treat it as a no-op.
    let Some(roots) = roots else {
      if let Some(dynamic_import) = dynamic_import {
        dynamic_import
          .state
          .teardown_roots(scope.heap_mut());
      }
      return Ok(());
    };

    // Ensure we always release the persistent roots even if calling the resolve function fails
    // (e.g. termination due to budgets/interrupts).
    let result = (|| {
      let mut call_scope = scope.reborrow();
      call_scope.push_root(cap.resolve)?;
      let _ = vm.call_with_host_and_hooks(
        host_ctx,
        &mut call_scope,
        hooks,
        cap.resolve,
        Value::Undefined,
        &[Value::Undefined],
      )?;
      Ok(())
    })();

    roots.teardown(scope.heap_mut());

    if let Err(err) = result {
      if let Some(dynamic_import) = dynamic_import {
        dynamic_import
          .state
          .teardown_roots(scope.heap_mut());
      }
      return Err(err);
    }

    if let Some(dynamic_import) = dynamic_import {
      let global_object = dynamic_import.state.global_object();
      let realm_id = dynamic_import.state.realm_id();
      let module = dynamic_import.module;

      // Store the dynamic import promise capability so it survives until the module evaluation
      // promise settles (which can be asynchronous due to top-level await).
      let continuation_id = match modules.insert_pending_dynamic_import_evaluation(vm, dynamic_import.state.clone(), module) {
        Ok(id) => id,
        Err(err) => {
          dynamic_import.state.teardown_roots(scope.heap_mut());
          return Err(err);
        }
      };

      let eval_promise = match modules.evaluate_with_scope(
        vm,
        scope,
        global_object,
        realm_id,
        module,
        host_ctx,
        hooks,
      ) {
        Ok(promise) => promise,
        Err(err) => {
          let Some((state, _module)) = modules.take_pending_dynamic_import_evaluation(vm, continuation_id) else {
            // Continuation was already cleaned up; treat as a no-op.
            return Ok(());
          };
          if let Some(reason) = err.thrown_value() {
            state.reject(vm, scope, host_ctx, hooks, reason)?;
            return Ok(());
          }
          state.teardown_roots(scope.heap_mut());
          return Err(err);
        }
      };

      // Attach Promise reactions to the module evaluation promise.
      //
      // Important: this must be done even when `eval_promise` is already fulfilled/rejected, since
      // `ContinueDynamicImport` uses `PerformPromiseThen` and therefore settles the import() promise
      // via a microtask.
      let attach_result = (|| -> Result<(), VmError> {
        let intr = vm.intrinsics().ok_or(VmError::Unimplemented(
          "dynamic import requires intrinsics (create a Realm first)",
        ))?;
 
        // `PerformPromiseThen(evaluatePromise, onFulfilled, onRejected)`.
        let on_fulfilled_call = vm.dynamic_import_eval_on_fulfilled_call_id()?;
        let on_rejected_call = vm.dynamic_import_eval_on_rejected_call_id()?;
 
        let mut eval_scope = scope.reborrow();
        eval_scope.push_root(eval_promise)?;
        let Value::Object(_) = eval_promise else {
          return Err(VmError::InvariantViolation(
            "module evaluation did not return a promise object",
          ));
        };
 
        let on_fulfilled_name = eval_scope.alloc_string("dynamicImportEvalOnFulfilled")?;
        eval_scope.push_root(Value::String(on_fulfilled_name))?;
        let on_rejected_name = eval_scope.alloc_string("dynamicImportEvalOnRejected")?;
        eval_scope.push_root(Value::String(on_rejected_name))?;
 
        let slots = [Value::Number(continuation_id as f64)];
 
        let on_fulfilled = eval_scope.alloc_native_function_with_slots(
          on_fulfilled_call,
          None,
          on_fulfilled_name,
          1,
          &slots,
        )?;
        eval_scope
          .heap_mut()
          .object_set_prototype(on_fulfilled, Some(intr.function_prototype()))?;
        eval_scope.push_root(Value::Object(on_fulfilled))?;
 
        let on_rejected = eval_scope.alloc_native_function_with_slots(
          on_rejected_call,
          None,
          on_rejected_name,
          1,
          &slots,
        )?;
        eval_scope
          .heap_mut()
          .object_set_prototype(on_rejected, Some(intr.function_prototype()))?;
        eval_scope.push_root(Value::Object(on_rejected))?;
 
        crate::promise_ops::perform_promise_then_with_result_capability_with_host_and_hooks(
          vm,
          &mut eval_scope,
          host_ctx,
          hooks,
          eval_promise,
          Value::Object(on_fulfilled),
          Value::Object(on_rejected),
          None,
        )?;
        Ok(())
      })();

      if let Err(err) = attach_result {
        // Clean up continuation state before propagating. (The import() promise will not be settled
        // if we fail before installing the Promise reactions, so ensure we don't leak its
        // capability roots.)
        if let Some((state, _module)) = modules.take_pending_dynamic_import_evaluation(vm, continuation_id) {
          state.teardown_roots(scope.heap_mut());
        }
        return Err(err);
      }
    }

    Ok(())
  }

  fn reject_promise(
    &self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    host_ctx: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    err: VmError,
  ) -> Result<(), VmError> {
    let (cap, roots, dynamic_import) = {
      let mut state = self.0.borrow_mut();
      (
        state.promise_capability,
        state.promise_roots.take(),
        state.dynamic_import.take(),
      )
    };

    let Some(roots) = roots else {
      if let Some(dynamic_import) = dynamic_import {
        dynamic_import
          .state
          .teardown_roots(scope.heap_mut());
      }
      return Ok(());
    };

    let reason = err.thrown_value().unwrap_or(Value::Undefined);

    // Ensure we always release the persistent roots even if calling the reject function fails.
      let result = (|| {
        let mut call_scope = scope.reborrow();
        call_scope.push_roots(&[cap.reject, reason])?;
        let _ = vm.call_with_host_and_hooks(
          host_ctx,
          &mut call_scope,
          hooks,
        cap.reject,
        Value::Undefined,
        &[reason],
      )?;
      Ok(())
    })();

    roots.teardown(scope.heap_mut());

    if let Err(err) = result {
      if let Some(dynamic_import) = dynamic_import {
        dynamic_import
          .state
          .teardown_roots(scope.heap_mut());
      }
      return Err(err);
    }

    if let Some(dynamic_import) = dynamic_import {
      dynamic_import
        .state
        .reject(vm, scope, host_ctx, hooks, reason)?;
    }

    Ok(())
  }

  #[allow(dead_code)]
  fn teardown_roots(&self, heap: &mut crate::Heap) {
    let (roots, dynamic_import) = {
      let mut inner = self.0.borrow_mut();
      (inner.promise_roots.take(), inner.dynamic_import.take())
    };
    if let Some(roots) = roots {
      roots.teardown(heap);
    }
    if let Some(dynamic_import) = dynamic_import {
      dynamic_import.state.teardown_roots(heap);
    }
  }
}

fn dynamic_import_continuation_id_from_native_slot(
  scope: &Scope<'_>,
  callee: GcObject,
) -> Result<u32, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  let raw = match slots.get(0).copied().unwrap_or(Value::Undefined) {
    Value::Number(n) if n.is_finite() && n >= 0.0 && n <= u32::MAX as f64 => n as u32,
    _ => {
      return Err(VmError::InvariantViolation(
        "dynamic import eval callback missing continuation id slot",
      ))
    }
  };
  Ok(raw)
}

pub(crate) fn dynamic_import_eval_on_fulfilled(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host_ctx: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let id = dynamic_import_continuation_id_from_native_slot(scope, callee)?;
  let Some(ptr) = vm.module_graph_ptr() else {
    return Ok(Value::Undefined);
  };
  let graph = unsafe { &mut *ptr };
  let Some((state, module)) = graph.take_pending_dynamic_import_evaluation(vm, id) else {
    // Continuation already cleaned up.
    return Ok(Value::Undefined);
  };

  // Ensure Promise reaction jobs observe budgets/interrupt state even for tiny callbacks.
  if let Err(err) = vm.tick() {
    // The continuation has already been removed from `pending_dynamic_import_evaluations`, so if we
    // abort before settling the dynamic import promise we must manually release its capability
    // roots.
    state.teardown_roots(scope.heap_mut());
    return Err(err);
  }

  let ns = match graph.get_module_namespace(module, vm, scope) {
    Ok(ns) => ns,
    Err(err) => {
      // Reject the dynamic import promise if we cannot compute the module namespace.
      //
      // Most failures here should be internal invariants (the module should be linked/evaluated),
      // but rejecting is preferable to leaving the import() promise pending forever.
      let reason = if let Some(thrown) = err.thrown_value() {
        thrown
      } else {
        let intr = vm.intrinsics().ok_or(VmError::Unimplemented(
          "dynamic import continuation requires intrinsics (create a Realm first)",
        ))?;
        // Avoid formatting `err` into a host `String` here: allocator OOM while formatting would
        // abort the process. This path exists mainly for internal invariant failures, so a stable
        // generic message is sufficient.
        crate::new_error(scope, intr.error_prototype(), "Error", "dynamic import failed")?
      };
      state.reject(vm, scope, host_ctx, hooks, reason)?;
      return Ok(Value::Undefined);
    }
  };

  state.resolve(vm, scope, host_ctx, hooks, Value::Object(ns))?;
  Ok(Value::Undefined)
}

pub(crate) fn dynamic_import_eval_on_rejected(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host_ctx: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let id = dynamic_import_continuation_id_from_native_slot(scope, callee)?;
  let Some(ptr) = vm.module_graph_ptr() else {
    return Ok(Value::Undefined);
  };
  let graph = unsafe { &mut *ptr };
  let Some((state, _module)) = graph.take_pending_dynamic_import_evaluation(vm, id) else {
    return Ok(Value::Undefined);
  };

  if let Err(err) = vm.tick() {
    state.teardown_roots(scope.heap_mut());
    return Err(err);
  }

  let reason = args.get(0).copied().unwrap_or(Value::Undefined);
  state.reject(vm, scope, host_ctx, hooks, reason)?;
  Ok(Value::Undefined)
}

#[derive(Debug)]
struct DynamicImportContinuation {
  state: DynamicImportState,
  module: ModuleId,
}

#[derive(Debug)]
struct DynamicImportStateInner {
  promise_capability: PromiseCapability,
  promise_roots: Option<PromiseCapabilityRoots>,
  realm_id: RealmId,
  global_object: GcObject,
}

/// Promise capability payload used by dynamic `import()` (`EvaluateImportCall` / `ContinueDynamicImport`).
///
/// This is stored in [`ModuleLoadPayload`] and therefore may live across asynchronous host
/// boundaries. It roots the promise + resolving functions in the heap so they remain valid even if
/// the host stores the payload in non-traced memory.
#[derive(Clone)]
pub(crate) struct DynamicImportState(Rc<RefCell<DynamicImportStateInner>>);

impl fmt::Debug for DynamicImportState {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    // Treat as opaque to hosts.
    let _ = &self.0;
    f.write_str("DynamicImportState(..)")
  }
}

impl DynamicImportState {
  fn new(
    scope: &mut Scope<'_>,
    cap: PromiseCapability,
    realm_id: RealmId,
    global_object: GcObject,
  ) -> Result<Self, VmError> {
    // Use a nested scope so temporary stack roots created while registering persistent roots are
    // popped before we return.
    let roots = {
      let mut root_scope = scope.reborrow();
      PromiseCapabilityRoots::new(&mut root_scope, cap)?
    };

    let state = match rc_try_new_vm(RefCell::new(DynamicImportStateInner {
      promise_capability: cap,
      promise_roots: None,
      realm_id,
      global_object,
    })) {
      Ok(rc) => DynamicImportState(rc),
      Err(err) => {
        roots.teardown(scope.heap_mut());
        return Err(err);
      }
    };
    state.0.borrow_mut().promise_roots = Some(roots);
    Ok(state)
  }

  fn realm_id(&self) -> RealmId {
    self.0.borrow().realm_id
  }

  fn global_object(&self) -> GcObject {
    self.0.borrow().global_object
  }

  fn resolve(
    &self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    host_ctx: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    value: Value,
  ) -> Result<(), VmError> {
    let (cap, roots) = {
      let mut inner = self.0.borrow_mut();
      (inner.promise_capability, inner.promise_roots.take())
    };
    let Some(roots) = roots else {
      return Ok(());
    };

    let result = (|| {
      let mut call_scope = scope.reborrow();
      call_scope.push_roots(&[cap.resolve, value])?;
      let _ = vm.call_with_host_and_hooks(
        host_ctx,
        &mut call_scope,
        hooks,
        cap.resolve,
        Value::Undefined,
        &[value],
      )?;
      Ok(())
    })();

    roots.teardown(scope.heap_mut());
    result
  }

  fn reject(
    &self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    host_ctx: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    reason: Value,
  ) -> Result<(), VmError> {
    let (cap, roots) = {
      let mut inner = self.0.borrow_mut();
      (inner.promise_capability, inner.promise_roots.take())
    };
    let Some(roots) = roots else {
      return Ok(());
    };

    let result = (|| {
      let mut call_scope = scope.reborrow();
      call_scope.push_roots(&[cap.reject, reason])?;
      let _ = vm.call_with_host_and_hooks(
        host_ctx,
        &mut call_scope,
        hooks,
        cap.reject,
        Value::Undefined,
        &[reason],
      )?;
      Ok(())
    })();

    roots.teardown(scope.heap_mut());
    result
  }

  pub(crate) fn teardown_roots(&self, heap: &mut crate::Heap) {
    let roots = self.0.borrow_mut().promise_roots.take();
    let Some(roots) = roots else {
      return;
    };
    roots.teardown(heap);
  }
}

/// Opaque token passed through `HostLoadImportedModule` into `FinishLoadingImportedModule`.
///
/// In the ECMAScript spec, `_payload_` is either:
/// - a `GraphLoadingState` Record (module graph loading continuation), or
/// - a `PromiseCapability` Record (`import()` continuation).
///
/// The host MUST treat this value as opaque and pass it back unchanged.
///
/// ## Opaqueness (compile-time)
///
/// Hosts can store and clone this value, but cannot inspect or destructure it:
///
/// ```compile_fail
/// use vm_js::ModuleLoadPayload;
///
/// fn inspect(payload: ModuleLoadPayload) {
///   let ModuleLoadPayload(_) = payload;
/// }
/// ```
#[derive(Clone)]
pub struct ModuleLoadPayload(ModuleLoadPayloadInner);

#[derive(Clone)]
#[allow(dead_code)]
enum ModuleLoadPayloadInner {
  GraphLoadingState(GraphLoadingState),
  PromiseCapability(DynamicImportState),
}

impl fmt::Debug for ModuleLoadPayload {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    // Treat the inner discriminant as opaque: hosts should not be able to inspect it via `Debug`.
    let _ = &self.0;
    f.write_str("ModuleLoadPayload(..)")
  }
}

impl ModuleLoadPayload {
  #[inline]
  #[allow(dead_code)]
  pub(crate) fn graph_loading_state(state: GraphLoadingState) -> Self {
    Self(ModuleLoadPayloadInner::GraphLoadingState(state))
  }

  #[inline]
  #[allow(dead_code)]
  pub(crate) fn promise_capability(state: DynamicImportState) -> Self {
    Self(ModuleLoadPayloadInner::PromiseCapability(state))
  }

  #[allow(dead_code)]
  pub(crate) fn kind(&self) -> ModuleLoadPayloadKind {
    match &self.0 {
      ModuleLoadPayloadInner::GraphLoadingState(_) => ModuleLoadPayloadKind::GraphLoadingState,
      ModuleLoadPayloadInner::PromiseCapability(_) => ModuleLoadPayloadKind::PromiseCapability,
    }
  }

  /// Removes any persistent GC roots held by this payload.
  ///
  /// ## When to use this
  ///
  /// This method exists for **teardown/cancellation only**: embeddings can call it when a pending
  /// `HostLoadImportedModule` operation is abandoned mid-flight (navigation cancelled, event loop
  /// shut down, budgets exhausted, etc.) and the embedder will **not** drive the
  /// `FinishLoadingImportedModule` state machine to completion.
  ///
  /// Calling this ensures that dropping the payload does not leak persistent roots and (in debug
  /// builds) does not trip leaked-root assertions.
  ///
  /// ## Idempotent / best-effort
  ///
  /// This is best-effort and **idempotent**: calling it multiple times (or after the payload has
  /// already been completed via `FinishLoadingImportedModule`) is a no-op.
  ///
  /// ## Normal module loading
  ///
  /// For real module loads, hosts should still call [`Vm::finish_loading_imported_module`] exactly
  /// once per `host_load_imported_module` invocation. `teardown_roots` does **not** settle any
  /// promises or complete module loading; it only releases the payload's persistent roots.
  pub fn teardown_roots(&self, heap: &mut crate::Heap) {
    match &self.0 {
      ModuleLoadPayloadInner::GraphLoadingState(state) => state.teardown_roots(heap),
      ModuleLoadPayloadInner::PromiseCapability(state) => state.teardown_roots(heap),
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum ModuleLoadPayloadKind {
  GraphLoadingState,
  PromiseCapability,
}

/// The completion record passed to `FinishLoadingImportedModule` continuations.
///
/// In the spec this is either:
/// - a normal completion containing a Module Record, or
/// - a throw completion.
///
/// At this scaffolding layer modules are represented as opaque [`ModuleId`] tokens; errors are
/// represented by [`VmError`].
pub type ModuleCompletion = Result<ModuleId, VmError>;

fn try_clone_string(value: &str) -> Result<String, VmError> {
  let mut out = String::new();
  out.try_reserve_exact(value.len()).map_err(|_| VmError::OutOfMemory)?;
  out.push_str(value);
  Ok(out)
}

fn try_clone_js_string(vm: &mut Vm, value: &JsString) -> Result<JsString, VmError> {
  vm.tick()?;
  let units = value.as_code_units();

  // Clone in chunks so extremely large specifiers still observe VM fuel/deadline budgets.
  const TICK_EVERY_CODE_UNITS: usize = 1024;
  let mut buf: Vec<u16> = Vec::new();
  buf
    .try_reserve_exact(units.len())
    .map_err(|_| VmError::OutOfMemory)?;

  let mut start = 0usize;
  while start < units.len() {
    let end = units
      .len()
      .min(start.saturating_add(TICK_EVERY_CODE_UNITS));
    buf.extend_from_slice(&units[start..end]);
    start = end;
    if start < units.len() {
      vm.tick()?;
    }
  }

  JsString::from_u16_vec(buf)
}

fn try_clone_import_attribute(value: &ImportAttribute) -> Result<ImportAttribute, VmError> {
  Ok(ImportAttribute {
    key: try_clone_string(&value.key)?,
    value: try_clone_string(&value.value)?,
  })
}

fn try_clone_module_request(vm: &mut Vm, value: &ModuleRequest) -> Result<ModuleRequest, VmError> {
  vm.tick()?;
  let mut attributes: Vec<ImportAttribute> = Vec::new();
  attributes
    .try_reserve_exact(value.attributes.len())
    .map_err(|_| VmError::OutOfMemory)?;
  const ATTR_TICK_EVERY: usize = 32;
  for (i, attr) in value.attributes.iter().enumerate() {
    if i % ATTR_TICK_EVERY == 0 && i != 0 {
      vm.tick()?;
    }
    attributes.push(try_clone_import_attribute(attr)?);
  }
  Ok(ModuleRequest::new(
    try_clone_js_string(vm, &value.specifier)?,
    attributes,
  ))
}

fn try_clone_module_requests(vm: &mut Vm, values: &[ModuleRequest]) -> Result<Vec<ModuleRequest>, VmError> {
  vm.tick()?;
  let mut out: Vec<ModuleRequest> = Vec::new();
  out
    .try_reserve_exact(values.len())
    .map_err(|_| VmError::OutOfMemory)?;
  const REQUEST_TICK_EVERY: usize = 32;
  for (i, v) in values.iter().enumerate() {
    if i % REQUEST_TICK_EVERY == 0 && i != 0 {
      vm.tick()?;
    }
    out.push(try_clone_module_request(vm, v)?);
  }
  Ok(out)
}

/// Implements ECMA-262 `LoadRequestedModules(hostDefined?)` for cyclic modules.
///
/// This starts the module graph loading state machine and returns a Promise that is fulfilled once
/// all modules in the static import graph have been loaded.
///
/// For an end-to-end embedder guide, see [`crate::docs::modules`].
///
/// ## ⚠️ Dummy `VmHost` context
///
/// This hook-only API passes a **dummy host context** (`()`) to any native call/construct handlers
/// invoked while loading/evaluating modules.
///
/// Embeddings that need native handlers to observe real host state should prefer
/// [`load_requested_modules_with_host_and_hooks`].
pub fn load_requested_modules(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  modules: &mut ModuleGraph,
  host: &mut dyn VmHostHooks,
  module: ModuleId,
  host_defined: HostDefined,
) -> Result<Value, VmError> {
  let mut dummy_host = ();
  load_requested_modules_with_host_and_hooks(vm, scope, modules, &mut dummy_host, host, module, host_defined)
}

/// Host-context aware variant of [`load_requested_modules`].
pub fn load_requested_modules_with_host_and_hooks(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  modules: &mut ModuleGraph,
  host_ctx: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  module: ModuleId,
  host_defined: HostDefined,
) -> Result<Value, VmError> {
  vm.tick()?;
  let (state, promise) = GraphLoadingState::new(vm, scope, host_ctx, hooks, host_defined)?;
  if let Err(err) = inner_module_loading_with_host_and_hooks(vm, scope, modules, host_ctx, hooks, &state, module) {
    // `GraphLoadingState` owns persistent roots for the promise capability. If we abort the
    // algorithm with an abrupt completion (OOM, termination, etc), ensure those roots are released
    // before returning the error to the host.
    state.set_is_loading(false);
    let _ = state.reject_promise(vm, scope, host_ctx, hooks, err.clone());
    return Err(err);
  }
  Ok(promise)
}

/// Implements ECMA-262 `InnerModuleLoading(state, module)`.
///
/// ## ⚠️ Dummy `VmHost` context
///
/// This hook-only API passes a **dummy host context** (`()`) to any native call/construct handlers
/// invoked while loading modules.
///
/// Embeddings that need native handlers to observe real host state should prefer
/// [`inner_module_loading_with_host_and_hooks`].
pub fn inner_module_loading(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  modules: &mut ModuleGraph,
  host: &mut dyn VmHostHooks,
  state: &GraphLoadingState,
  module: ModuleId,
) -> Result<(), VmError> {
  let mut dummy_host = ();
  inner_module_loading_with_host_and_hooks(vm, scope, modules, &mut dummy_host, host, state, module)
}

/// Host-context aware variant of [`inner_module_loading`].
pub fn inner_module_loading_with_host_and_hooks(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  modules: &mut ModuleGraph,
  host_ctx: &mut dyn VmHost,
  host: &mut dyn VmHostHooks,
  state: &GraphLoadingState,
  module: ModuleId,
) -> Result<(), VmError> {
  vm.tick()?;
  let Some(record) = modules.get_module(module) else {
    state.set_is_loading(false);
    state.reject_promise(vm, scope, host_ctx, host, VmError::invalid_handle())?;
    return Ok(());
  };

  let should_traverse = record.status == ModuleStatus::New && !state.visited_contains(module);
  let requested_modules = if should_traverse {
    try_clone_module_requests(vm, &record.requested_modules)?
  } else {
    Vec::new()
  };

  if should_traverse {
    state.push_visited(module)?;
    state.inc_pending(requested_modules.len())?;

    for request in requested_modules {
      vm.tick()?;
      // `AllImportAttributesSupported`.
      let supported = host.host_get_supported_import_attributes();
      let unsupported_key = first_unsupported_import_attribute_key(vm, supported, &request.attributes)?;
      if let Some(unsupported_key) = unsupported_key {
        // Per ECMA-262, unsupported import attributes are a thrown SyntaxError.
        if let Some(intrinsics) = vm.intrinsics() {
          let message = crate::fallible_format::try_format_identifier_error(
            "Unsupported import attribute: ",
            unsupported_key,
          )?;

          let err_value = crate::new_error(
            scope,
            intrinsics.syntax_error_prototype(),
            "SyntaxError",
            &message,
          )?;

          continue_module_loading_with_host_and_hooks(
            vm,
            scope,
            modules,
            host_ctx,
            host,
            ModuleLoadPayload::graph_loading_state(state.clone()),
            Err(VmError::Throw(err_value)),
          )?;
        } else {
          continue_module_loading_with_host_and_hooks(
            vm,
            scope,
            modules,
            host_ctx,
            host,
            ModuleLoadPayload::graph_loading_state(state.clone()),
            Err(VmError::Unimplemented(
              "AllImportAttributesSupported requires Vm intrinsics (create a Realm first)",
            )),
          )?;
        }
      } else if let Some(loaded_module) = modules.get_imported_module(module, &request) {
        inner_module_loading_with_host_and_hooks(vm, scope, modules, host_ctx, host, state, loaded_module)?;
      } else {
        host.host_load_imported_module(
          vm,
          scope,
          modules,
          ModuleReferrer::Module(module),
          request,
          state.host_defined(),
          ModuleLoadPayload::graph_loading_state(state.clone()),
        )?;
      }

      if !state.is_loading() {
        return Ok(());
      }
    }
  }

  let pending_left = state.dec_pending();
  if pending_left != 0 {
    return Ok(());
  }

  state.set_is_loading(false);
  {
    let visited = state.0.borrow();
    const STATUS_UPDATE_TICK_INTERVAL: usize = 64;
    for (i, &visited_id) in visited.visited.iter().enumerate() {
      if i % STATUS_UPDATE_TICK_INTERVAL == 0 {
        vm.tick()?;
      }
      if let Some(module) = modules.get_module_mut(visited_id) {
        if module.status == ModuleStatus::New {
          module.status = ModuleStatus::Unlinked;
        }
      }
    }
  }
  state.resolve_promise(vm, scope, modules, host_ctx, host)?;
  Ok(())
}

/// Implements ECMA-262 `FinishLoadingImportedModule(...)`.
///
/// Hosts must call this exactly once for each [`crate::VmHostHooks::host_load_imported_module`]
/// invocation, either synchronously (re-entrantly) or asynchronously later.
///
/// ## ⚠️ Dummy `VmHost` context
///
/// This hook-only API passes a **dummy host context** (`()`) to any native call/construct handlers
/// invoked while continuing module loading.
///
/// Embeddings that need native handlers to observe real host state should prefer
/// [`finish_loading_imported_module_with_host_and_hooks`].
pub fn finish_loading_imported_module(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  modules: &mut ModuleGraph,
  host: &mut dyn VmHostHooks,
  referrer: ModuleReferrer,
  module_request: ModuleRequest,
  payload: ModuleLoadPayload,
  result: ModuleCompletion,
) -> Result<(), VmError> {
  let mut dummy_host = ();
  finish_loading_imported_module_with_host_and_hooks(
    vm,
    scope,
    modules,
    &mut dummy_host,
    host,
    referrer,
    module_request,
    payload,
    result,
  )
}

/// Host-context aware variant of [`finish_loading_imported_module`].
pub fn finish_loading_imported_module_with_host_and_hooks(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  modules: &mut ModuleGraph,
  host_ctx: &mut dyn VmHost,
  host: &mut dyn VmHostHooks,
  referrer: ModuleReferrer,
  module_request: ModuleRequest,
  payload: ModuleLoadPayload,
  result: ModuleCompletion,
) -> Result<(), VmError> {
  if let Err(err) = vm.tick() {
    // `payload` may hold persistent roots (graph loading promise capability / dynamic import
    // promise capability). If we abort before handing the payload off to its continuation, ensure
    // those roots are not leaked.
    payload.teardown_roots(scope.heap_mut());
    return Err(err);
  }

  // 1. `FinishLoadingImportedModule` caching invariant:
  //    If a `(referrer, moduleRequest)` pair resolves normally more than once, it must resolve to
  //    the same Module Record each time.
  let result = match (|| -> Result<ModuleCompletion, VmError> {
    Ok(match result {
      Ok(loaded) => {
        match referrer {
          ModuleReferrer::Module(referrer) => {
            if let Some(referrer_module) = modules.get_module_mut(referrer) {
              if let Some(existing) = referrer_module
                .loaded_modules
                .iter()
                .find(|record| record.request.spec_equal(&module_request))
              {
                if existing.module != loaded {
                  Err(VmError::InvariantViolation(
                    "FinishLoadingImportedModule invariant violation: module request resolved to different modules",
                  ))
                } else {
                  Ok(loaded)
                }
              } else {
                referrer_module
                  .loaded_modules
                  .try_reserve(1)
                  .map_err(|_| VmError::OutOfMemory)?;
                referrer_module
                  .loaded_modules
                  .push(LoadedModuleRequest::new(module_request, loaded));
                // `[[LoadedModules]]` edges affect SCC membership and therefore module evaluation order;
                // invalidate cached SCC structure when new edges are added during host-driven loading.
                modules.mark_scc_dirty();
                Ok(loaded)
              }
            } else {
              Ok(loaded)
            }
          }
          ModuleReferrer::Script(script) => {
            let loaded_modules = modules.script_loaded_modules_mut(script)?;
            if let Some(existing) = loaded_modules
              .iter()
              .find(|record| record.request.spec_equal(&module_request))
            {
              if existing.module != loaded {
                Err(VmError::InvariantViolation(
                  "FinishLoadingImportedModule invariant violation: module request resolved to different modules",
                ))
              } else {
                Ok(loaded)
              }
            } else {
              loaded_modules
                .try_reserve(1)
                .map_err(|_| VmError::OutOfMemory)?;
              loaded_modules.push(LoadedModuleRequest::new(module_request, loaded));
              Ok(loaded)
            }
          }
          ModuleReferrer::Realm(realm) => {
            let loaded_modules = modules.realm_loaded_modules_mut(realm)?;
            if let Some(existing) = loaded_modules
              .iter()
              .find(|record| record.request.spec_equal(&module_request))
            {
              if existing.module != loaded {
                Err(VmError::InvariantViolation(
                  "FinishLoadingImportedModule invariant violation: module request resolved to different modules",
                ))
              } else {
                Ok(loaded)
              }
            } else {
              loaded_modules
                .try_reserve(1)
                .map_err(|_| VmError::OutOfMemory)?;
              loaded_modules.push(LoadedModuleRequest::new(module_request, loaded));
              Ok(loaded)
            }
          }
        }
      }
      Err(e) => Err(e),
    })
  })() {
    Ok(result) => result,
    Err(err) => {
      // We failed before the payload was handed off to the module loading continuation. Avoid
      // leaking any persistent roots held by the payload.
      payload.teardown_roots(scope.heap_mut());
      return Err(err);
    }
  };

  match payload.0 {
    ModuleLoadPayloadInner::GraphLoadingState(state) => continue_module_loading_with_host_and_hooks(
      vm,
      scope,
      modules,
      host_ctx,
      host,
      ModuleLoadPayload::graph_loading_state(state),
      result,
    ),
    ModuleLoadPayloadInner::PromiseCapability(state) => continue_dynamic_import_with_host_and_hooks(
      vm,
      scope,
      modules,
      host_ctx,
      host,
      ModuleLoadPayload::promise_capability(state),
      result,
    ),
  }
}

impl Vm {
  /// Completes a pending `HostLoadImportedModule` operation.
  ///
  /// This is the entry point host environments should call once they have finished fetching and
  /// parsing a module (or have failed to do so). It performs `FinishLoadingImportedModule` and then
  /// dispatches to the appropriate continuation based on `payload`:
  /// - `ContinueModuleLoading` for static module graph loading, or
  /// - `ContinueDynamicImport` for dynamic `import()`.
  #[inline]
  pub fn finish_loading_imported_module(
    &mut self,
    scope: &mut Scope<'_>,
    modules: &mut ModuleGraph,
    host: &mut dyn VmHostHooks,
    referrer: ModuleReferrer,
    module_request: ModuleRequest,
    payload: ModuleLoadPayload,
    result: ModuleCompletion,
  ) -> Result<(), VmError> {
    finish_loading_imported_module(
      self,
      scope,
      modules,
      host,
      referrer,
      module_request,
      payload,
      result,
    )
  }

  /// Host-context aware variant of [`Vm::finish_loading_imported_module`].
  #[inline]
  pub fn finish_loading_imported_module_with_host_and_hooks(
    &mut self,
    host_ctx: &mut dyn VmHost,
    scope: &mut Scope<'_>,
    modules: &mut ModuleGraph,
    hooks: &mut dyn VmHostHooks,
    referrer: ModuleReferrer,
    module_request: ModuleRequest,
    payload: ModuleLoadPayload,
    result: ModuleCompletion,
  ) -> Result<(), VmError> {
    finish_loading_imported_module_with_host_and_hooks(
      self,
      scope,
      modules,
      host_ctx,
      hooks,
      referrer,
      module_request,
      payload,
      result,
    )
  }
}

/// Implements ECMA-262 `ContinueModuleLoading(state, moduleCompletion)`.
///
/// ## ⚠️ Dummy `VmHost` context
///
/// This hook-only API passes a **dummy host context** (`()`) to any native call/construct handlers
/// invoked while continuing module loading.
///
/// Embeddings that need native handlers to observe real host state should prefer
/// [`continue_module_loading_with_host_and_hooks`].
pub fn continue_module_loading(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  modules: &mut ModuleGraph,
  host: &mut dyn VmHostHooks,
  payload: ModuleLoadPayload,
  result: ModuleCompletion,
) -> Result<(), VmError> {
  let mut dummy_host = ();
  continue_module_loading_with_host_and_hooks(vm, scope, modules, &mut dummy_host, host, payload, result)
}

/// Host-context aware variant of [`continue_module_loading`].
pub fn continue_module_loading_with_host_and_hooks(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  modules: &mut ModuleGraph,
  host_ctx: &mut dyn VmHost,
  host: &mut dyn VmHostHooks,
  payload: ModuleLoadPayload,
  result: ModuleCompletion,
) -> Result<(), VmError> {
  if let Err(err) = vm.tick() {
    payload.teardown_roots(scope.heap_mut());
    return Err(err);
  }
  let state = match payload.0 {
    ModuleLoadPayloadInner::GraphLoadingState(state) => state,
    ModuleLoadPayloadInner::PromiseCapability(state) => {
      // Called with the wrong payload kind; avoid leaking its persistent roots.
      state.teardown_roots(scope.heap_mut());
      return Err(VmError::InvariantViolation(
        "ContinueModuleLoading called with non-GraphLoadingState payload",
      ));
    }
  };

  if !state.is_loading() {
    return Ok(());
  }

  match result {
    Ok(module) => {
      if let Err(err) = inner_module_loading_with_host_and_hooks(vm, scope, modules, host_ctx, host, &state, module) {
        // Ensure promise roots are released even on abrupt completion.
        state.set_is_loading(false);
        let _ = state.reject_promise(vm, scope, host_ctx, host, err.clone());
        return Err(err);
      }
      Ok(())
    }
    Err(err) => {
      state.set_is_loading(false);
      state.reject_promise(vm, scope, host_ctx, host, err)
    }
  }
}

/// Errors produced while validating dynamic import options / import attributes.
#[derive(Debug, Clone)]
pub enum ImportCallError {
  /// A spec-mandated TypeError rejection.
  TypeError(ImportCallTypeError),
  /// An abrupt error (e.g. OOM / invalid handle) encountered while inspecting objects.
  Vm(VmError),
}

/// The specific TypeError reason produced by `EvaluateImportCall` option/attribute validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportCallTypeError {
  OptionsNotObject,
  AttributesNotObject,
  AttributeValueNotString,
  UnsupportedImportAttribute { key: String },
}

const MAX_IMPORT_ATTRIBUTE_STRING_CODE_UNITS: usize = 1024;

fn clone_heap_string_to_string(heap: &crate::Heap, s: GcString) -> Result<String, VmError> {
  let js = heap.get_string(s)?;
  if js.len_code_units() > MAX_IMPORT_ATTRIBUTE_STRING_CODE_UNITS {
    return Err(VmError::LimitExceeded(
      "import attribute keys/values are limited to 1024 UTF-16 code units",
    ));
  }

  // `String::from_utf16_lossy` is infallible and aborts the process on allocator OOM.
  // Convert into a pre-reserved buffer so we can surface OOM as `VmError::OutOfMemory` instead.
  let mut out = String::new();

  let units = js.as_code_units();
  let max_utf8_len = units
    .len()
    // Maximum UTF-8 bytes per UTF-16 code unit is 3:
    // - non-BMP characters are 4 bytes but take *two* code units,
    // - invalid surrogate halves become U+FFFD (3 bytes).
    .checked_mul(3)
    .ok_or(VmError::LimitExceeded(
      "import attribute keys/values are too large to convert to UTF-8",
    ))?;
  out
    .try_reserve_exact(max_utf8_len)
    .map_err(|_| VmError::OutOfMemory)?;

  for r in std::char::decode_utf16(units.iter().copied()) {
    match r {
      Ok(ch) => out.push(ch),
      Err(_) => out.push('\u{FFFD}'),
    }
  }

  Ok(out)
}

fn clone_heap_string_to_js_string_unbounded_with_ticks(
  vm: &mut Vm,
  heap: &crate::Heap,
  s: GcString,
) -> Result<JsString, VmError> {
  let js = heap.get_string(s)?;
  let units = js.as_code_units();

  // Dynamic import specifiers can be arbitrarily large. Copying the UTF-16 code units must still be
  // budgeted so hostile inputs cannot perform unbounded work within a single tick interval.
  const TICK_EVERY_CODE_UNITS: usize = 1024;

  let mut buf: Vec<u16> = Vec::new();
  buf
    .try_reserve_exact(units.len())
    .map_err(|_| VmError::OutOfMemory)?;

  let mut start = 0usize;
  while start < units.len() {
    let end = units
      .len()
      .min(start.saturating_add(TICK_EVERY_CODE_UNITS));
    buf.extend_from_slice(&units[start..end]);
    start = end;
    if start < units.len() {
      vm.tick()?;
    }
  }

  JsString::from_u16_vec(buf)
}

fn make_key_string(scope: &mut Scope<'_>, s: &str) -> Result<GcString, VmError> {
  // Root the key string for the duration of the algorithm so it can't be collected if a later
  // allocation triggers GC.
  let key = scope.alloc_string(s)?;
  scope.push_root(Value::String(key))?;
  Ok(key)
}

/// Compare strings by lexicographic order of UTF-16 code units.
///
/// ECMA-262 module loading algorithms (e.g. `EvaluateImportCall`) define ordering of import
/// attribute keys in terms of UTF-16 code units, not Rust's default UTF-8 byte ordering.
fn cmp_utf16_code_units(a: &str, b: &str) -> std::cmp::Ordering {
  use std::cmp::Ordering;

  let mut a_units = a.encode_utf16();
  let mut b_units = b.encode_utf16();
  loop {
    match (a_units.next(), b_units.next()) {
      (Some(a_u), Some(b_u)) => match a_u.cmp(&b_u) {
        Ordering::Equal => {}
        non_eq => return non_eq,
      },
      (None, Some(_)) => return Ordering::Less,
      (Some(_), None) => return Ordering::Greater,
      (None, None) => return Ordering::Equal,
    }
  }
}

/// Extract and validate import attributes from the `options` argument of a dynamic `import()` call.
///
/// This implements the import-attributes portion of `EvaluateImportCall`:
/// <https://tc39.es/ecma262/#sec-evaluate-import-call>
///
/// ## ⚠️ Dummy `VmHost` context
///
/// This hook-only API can invoke user JS (for example getters on `options.with` and enumerating the
/// attributes object), but will pass a **dummy host context** (`()`) to any native call/construct
/// handlers reached through those invocations.
///
/// Embeddings that need native handlers to observe real host state should prefer
/// [`import_attributes_from_options_with_host_and_hooks`].
pub fn import_attributes_from_options(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  options: Value,
  supported_keys: &[&str],
) -> Result<Vec<ImportAttribute>, ImportCallError> {
  // Backwards-compatible wrapper that uses a dummy host context and the VM-owned microtask queue
  // as hooks.
  let mut dummy_host = ();
  let mut hooks = mem::take(vm.microtask_queue_mut());
  let result = import_attributes_from_options_with_host_and_hooks(
    vm,
    scope,
    &mut dummy_host,
    &mut hooks,
    options,
    supported_keys,
  );
  *vm.microtask_queue_mut() = hooks;
  result
}

/// Host-context aware variant of [`import_attributes_from_options`].
pub fn import_attributes_from_options_with_host_and_hooks(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host_ctx: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  options: Value,
  supported_keys: &[&str],
) -> Result<Vec<ImportAttribute>, ImportCallError> {
  vm.tick().map_err(ImportCallError::Vm)?;
  if matches!(options, Value::Undefined) {
    return Ok(Vec::new());
  }

  let Value::Object(options_obj) = options else {
    return Err(ImportCallError::TypeError(ImportCallTypeError::OptionsNotObject));
  };

  // Root the options object across allocations/GC while inspecting it.
  let mut scope = scope.reborrow();
  scope
    .push_root(Value::Object(options_obj))
    .map_err(ImportCallError::Vm)?;

  let with_key =
    PropertyKey::from_string(make_key_string(&mut scope, "with").map_err(ImportCallError::Vm)?);
  let attributes_obj = scope
    .object_get_with_host_and_hooks(vm, host_ctx, hooks, options_obj, with_key, Value::Object(options_obj))
    .map_err(ImportCallError::Vm)?;

  if matches!(attributes_obj, Value::Undefined) {
    return Ok(Vec::new());
  }

  let Value::Object(attributes_obj) = attributes_obj else {
    return Err(ImportCallError::TypeError(
      ImportCallTypeError::AttributesNotObject,
    ));
  };

  // Root the attributes object so property enumeration/getters cannot collect it.
  scope
    .push_root(Value::Object(attributes_obj))
    .map_err(ImportCallError::Vm)?;

  let own_keys = scope
    .object_own_property_keys_with_host_and_hooks(vm, host_ctx, hooks, attributes_obj)
    .map_err(ImportCallError::Vm)?;

  let mut attributes = Vec::<ImportAttribute>::new();
  attributes
    .try_reserve_exact(own_keys.len())
    .map_err(|_| ImportCallError::Vm(VmError::OutOfMemory))?;

  for key in own_keys {
    vm.tick().map_err(ImportCallError::Vm)?;
    let PropertyKey::String(key_string) = key else {
      continue;
    };

    let Some(desc) = scope
      .object_get_own_property_with_host_and_hooks(vm, host_ctx, hooks, attributes_obj, key)
      .map_err(ImportCallError::Vm)?
    else {
      continue;
    };

    if !desc.enumerable {
      continue;
    }

    let value = scope
      .object_get_with_host_and_hooks(vm, host_ctx, hooks, attributes_obj, key, Value::Object(attributes_obj))
      .map_err(ImportCallError::Vm)?;

    let Value::String(value_string) = value else {
      return Err(ImportCallError::TypeError(
        ImportCallTypeError::AttributeValueNotString,
      ));
    };

    let key = clone_heap_string_to_string(scope.heap(), key_string).map_err(ImportCallError::Vm)?;
    let value =
      clone_heap_string_to_string(scope.heap(), value_string).map_err(ImportCallError::Vm)?;

    attributes.push(ImportAttribute { key, value });
  }

  // `AllImportAttributesSupported`.
  for attribute in &attributes {
    vm.tick().map_err(ImportCallError::Vm)?;
    if !supported_keys
      .iter()
      .any(|supported| *supported == attribute.key.as_str())
    {
      return Err(ImportCallError::TypeError(
        ImportCallTypeError::UnsupportedImportAttribute {
          key: attribute.key.clone(),
        },
      ));
    }
  }

  // Sort by key (and value for determinism) by UTF-16 code unit order.
  attributes.sort_by(|a, b| match cmp_utf16_code_units(&a.key, &b.key) {
    std::cmp::Ordering::Equal => cmp_utf16_code_units(&a.value, &b.value),
    non_eq => non_eq,
  });
  Ok(attributes)
}

/// Spec helper: `AllImportAttributesSupported(attributes)`.
pub fn all_import_attributes_supported(supported_keys: &[&str], attributes: &[ImportAttribute]) -> bool {
  attributes
    .iter()
    .all(|attr| supported_keys.iter().any(|k| *k == attr.key.as_str()))
}

fn first_unsupported_import_attribute_key<'a>(
  vm: &mut Vm,
  supported_keys: &[&str],
  attributes: &'a [ImportAttribute],
) -> Result<Option<&'a str>, VmError> {
  // Attribute lists are user-controlled (bounded by source size / host objects), and the module
  // loading algorithms may scan them even when the graph contains a single request. Budget the scan
  // so a large list cannot do unbounded work within a single `vm.tick()` interval.
  const TICK_EVERY: usize = 32;
  for (i, attr) in attributes.iter().enumerate() {
    if i % TICK_EVERY == 0 && i != 0 {
      vm.tick()?;
    }
    if !supported_keys.iter().any(|k| *k == attr.key.as_str()) {
      return Ok(Some(attr.key.as_str()));
    }
  }
  Ok(None)
}

/// Spec-shaped dynamic import entry point (EvaluateImportCall).
///
/// For an end-to-end embedder guide, see [`crate::docs::modules`].
///
/// ## ⚠️ Dummy `VmHost` context
///
/// This hook-only API can invoke user JS (for example `ToString(specifier)` or getters on the
/// `options` argument), but will pass a **dummy host context** (`()`) to any native call/construct
/// handlers reached through those invocations.
///
/// Embeddings that need native handlers to observe real host state should prefer
/// [`start_dynamic_import_with_host_and_hooks`].
pub fn start_dynamic_import(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  modules: &mut ModuleGraph,
  host: &mut dyn VmHostHooks,
  global_object: GcObject,
  specifier: Value,
  options: Value,
) -> Result<Value, VmError> {
  let mut dummy_host = ();
  start_dynamic_import_with_host_and_hooks(
    vm,
    scope,
    modules,
    &mut dummy_host,
    host,
    global_object,
    specifier,
    options,
  )
}

/// Host-context aware variant of [`start_dynamic_import`].
pub fn start_dynamic_import_with_host_and_hooks(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  modules: &mut ModuleGraph,
  host_ctx: &mut dyn VmHost,
  host: &mut dyn VmHostHooks,
  global_object: GcObject,
  specifier: Value,
  options: Value,
) -> Result<Value, VmError> {
  vm.tick()?;

  // Root the arguments while evaluating options and calling host hooks: the algorithm may allocate
  // and trigger GC.
  let mut import_scope = scope.reborrow();
  import_scope.push_roots(&[specifier, options])?;

  // 1. Let promiseCapability be ? NewPromiseCapability(%Promise%).
  let (state, promise, realm_id) = {
    let mut root_scope = import_scope.reborrow();
    let cap = crate::promise_ops::new_promise_capability_with_host_and_hooks(
      vm,
      &mut root_scope,
      host_ctx,
      host,
    )?;
    let realm_id = vm.current_realm().ok_or(VmError::Unimplemented(
      "dynamic import requires an active Realm (push an ExecutionContext)",
    ))?;
    let state = DynamicImportState::new(&mut root_scope, cap, realm_id, global_object)?;
    Ok::<_, VmError>((state, cap.promise, realm_id))
  }?;

  // 2. Let specifierString be ? ToString(specifier).
  let specifier_string = match import_scope.to_string(vm, host_ctx, host, specifier) {
    Ok(s) => {
      // Root the resulting string while copying its UTF-16 code units into an owned `JsString`,
      // since this can allocate and trigger GC later (e.g. during import attribute validation).
      if let Err(err) = import_scope.push_root(Value::String(s)) {
        state.teardown_roots(import_scope.heap_mut());
        return Err(err);
      }
      match clone_heap_string_to_js_string_unbounded_with_ticks(vm, import_scope.heap(), s) {
        Ok(s) => s,
        Err(err) => {
          state.teardown_roots(import_scope.heap_mut());
          return Err(err);
        }
      }
    }
    Err(VmError::Throw(value) | VmError::ThrowWithStack { value, .. }) => {
      state.reject(vm, &mut import_scope, host_ctx, host, value)?;
      return Ok(promise);
    }
    Err(VmError::TypeError(message)) => {
      let Some(intr) = vm.intrinsics() else {
        state.teardown_roots(import_scope.heap_mut());
        return Err(VmError::Unimplemented(
          "dynamic import requires intrinsics (create a Realm first)",
        ));
      };
      let err_value = crate::new_error(
        &mut import_scope,
        intr.type_error_prototype(),
        "TypeError",
        message,
      )?;
      state.reject(vm, &mut import_scope, host_ctx, host, err_value)?;
      return Ok(promise);
    }
    Err(e) => {
      state.teardown_roots(import_scope.heap_mut());
      return Err(e);
    }
  };

  // 3. Extract import attributes from options (reject on validation errors).
  let supported = host.host_get_supported_import_attributes();
  let attributes = match import_attributes_from_options_with_host_and_hooks(
    vm,
    &mut import_scope,
    host_ctx,
    host,
    options,
    supported,
  ) {
    Ok(attrs) => attrs,
    Err(ImportCallError::TypeError(kind)) => {
      let Some(intr) = vm.intrinsics() else {
        state.teardown_roots(import_scope.heap_mut());
        return Err(VmError::Unimplemented(
          "dynamic import requires intrinsics (create a Realm first)",
        ));
      };

      let message = match kind {
        ImportCallTypeError::OptionsNotObject => "import() options must be an object",
        ImportCallTypeError::AttributesNotObject => "import() options.with must be an object",
        ImportCallTypeError::AttributeValueNotString => "import() attribute values must be strings",
        ImportCallTypeError::UnsupportedImportAttribute { key } => {
          return {
            let msg = crate::fallible_format::try_format_identifier_error(
              "Unsupported import attribute: ",
              &key,
            )?;
            let err_value = crate::new_error(
              &mut import_scope,
              intr.type_error_prototype(),
              "TypeError",
              &msg,
            )?;
            state.reject(vm, &mut import_scope, host_ctx, host, err_value)?;
            Ok(promise)
          };
        }
      };

      let err_value =
        crate::new_error(&mut import_scope, intr.type_error_prototype(), "TypeError", message)?;
      state.reject(vm, &mut import_scope, host_ctx, host, err_value)?;
      return Ok(promise);
    }
    Err(ImportCallError::Vm(err)) => {
      if let Some(reason) = err.thrown_value() {
        state.reject(vm, &mut import_scope, host_ctx, host, reason)?;
        return Ok(promise);
      }
      match err {
        VmError::TypeError(message) => {
          let Some(intr) = vm.intrinsics() else {
            state.teardown_roots(import_scope.heap_mut());
            return Err(VmError::Unimplemented(
              "dynamic import requires intrinsics (create a Realm first)",
            ));
          };
          let err_value = crate::new_error(
            &mut import_scope,
            intr.type_error_prototype(),
            "TypeError",
            message,
          )?;
          state.reject(vm, &mut import_scope, host_ctx, host, err_value)?;
          return Ok(promise);
        }
        other => {
          state.teardown_roots(import_scope.heap_mut());
          return Err(other);
        }
      }
    }
  };

  let module_request = ModuleRequest::new(specifier_string, attributes);

  // 4. Let referrer be GetActiveScriptOrModule(). If null, use the current Realm.
  let referrer = match vm.get_active_script_or_module() {
    Some(ScriptOrModule::Script(id)) => ModuleReferrer::Script(id),
    Some(ScriptOrModule::Module(id)) => ModuleReferrer::Module(id),
    None => ModuleReferrer::Realm(realm_id),
  };

  // 5. HostLoadImportedModule(referrer, moduleRequest, empty, promiseCapability)
  let payload = ModuleLoadPayload::promise_capability(state.clone());
  if let Err(err) = host.host_load_imported_module(
    vm,
    &mut import_scope,
    modules,
    referrer,
    module_request,
    HostDefined::default(),
    payload,
  ) {
    if let Some(reason) = err.thrown_value() {
      state.reject(vm, &mut import_scope, host_ctx, host, reason)?;
      return Ok(promise);
    }
    state.teardown_roots(import_scope.heap_mut());
    return Err(err);
  }

  // 6. Return promiseCapability.[[Promise]].
  Ok(promise)
}

/// Implements ECMA-262 `ContinueDynamicImport`.
///
/// ## ⚠️ Dummy `VmHost` context
///
/// This hook-only API passes a **dummy host context** (`()`) to any native call/construct handlers
/// invoked while continuing the dynamic import state machine.
///
/// Embeddings that need native handlers to observe real host state should prefer
/// [`continue_dynamic_import_with_host_and_hooks`].
pub fn continue_dynamic_import(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  modules: &mut ModuleGraph,
  host: &mut dyn VmHostHooks,
  payload: ModuleLoadPayload,
  module_completion: ModuleCompletion,
) -> Result<(), VmError> {
  let mut dummy_host = ();
  continue_dynamic_import_with_host_and_hooks(vm, scope, modules, &mut dummy_host, host, payload, module_completion)
}

/// Host-context aware variant of [`continue_dynamic_import`].
pub fn continue_dynamic_import_with_host_and_hooks(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  modules: &mut ModuleGraph,
  host_ctx: &mut dyn VmHost,
  host: &mut dyn VmHostHooks,
  payload: ModuleLoadPayload,
  module_completion: ModuleCompletion,
) -> Result<(), VmError> {
  let state = match payload.0 {
    ModuleLoadPayloadInner::PromiseCapability(state) => state,
    ModuleLoadPayloadInner::GraphLoadingState(state) => {
      // Called with the wrong payload kind; avoid leaking its persistent roots.
      state.teardown_roots(scope.heap_mut());
      return Err(VmError::InvariantViolation(
        "ContinueDynamicImport called with non-PromiseCapability payload",
      ));
    }
  };

  match module_completion {
    Err(err) => {
      if let Some(reason) = err.thrown_value() {
        state.reject(vm, scope, host_ctx, host, reason)?;
        return Ok(());
      }
      state.teardown_roots(scope.heap_mut());
      Err(err)
    }
    Ok(module) => {
      // Start `LoadRequestedModules` for the newly-loaded module. The dynamic import promise is
      // resolved once the graph-loading promise settles (via the `GraphLoadingState` continuation).
      let (graph_state, _promise) =
        match GraphLoadingState::new(vm, scope, host_ctx, host, HostDefined::default()) {
          Ok(v) => v,
          Err(err) => {
            // We failed before handing off to `GraphLoadingState`, so ensure the dynamic import
            // capability roots are not leaked.
            state.teardown_roots(scope.heap_mut());
            return Err(err);
          }
        };

      let state_for_teardown = state.clone();
      if let Err(err) = graph_state.set_dynamic_import(state, module) {
        graph_state.teardown_roots(scope.heap_mut());
        state_for_teardown.teardown_roots(scope.heap_mut());
        return Err(err);
      }
      if let Err(err) =
        inner_module_loading_with_host_and_hooks(vm, scope, modules, host_ctx, host, &graph_state, module)
      {
        graph_state.set_is_loading(false);
        let _ = graph_state.reject_promise(vm, scope, host_ctx, host, err.clone());
        return Err(err);
      }
      Ok(())
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::Budget;
  use crate::property::PropertyDescriptor;
  use crate::property::PropertyKey as HeapPropertyKey;
  use crate::property::PropertyKind as HeapPropertyKind;
  use crate::ExecutionContext;
  use crate::Heap;
  use crate::HeapLimits;
  use crate::MicrotaskQueue;
  use crate::TerminationReason;
  use crate::Job;
  use crate::Realm;
  use crate::RealmId;
  use crate::test_alloc::FailNextMatchingAllocGuard;
  use crate::VmHostHooks;
  use crate::VmOptions;

  #[repr(C)]
  struct TestRcBox<T> {
    strong: Cell<usize>,
    weak: Cell<usize>,
    value: T,
  }

  fn data_desc(value: Value, enumerable: bool) -> PropertyDescriptor {
    PropertyDescriptor {
      enumerable,
      configurable: true,
      kind: HeapPropertyKind::Data {
        value,
        writable: true,
      },
    }
  }

  #[test]
  fn import_attributes_from_options_validates_and_sorts() {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut scope = heap.scope();
    let mut vm = Vm::new(VmOptions::default());

    let options = scope.alloc_object().unwrap();
    let attributes = scope.alloc_object().unwrap();

    let k_with = scope.alloc_string("with").unwrap();
    let k_type = scope.alloc_string("type").unwrap();
    let v_json = scope.alloc_string("json").unwrap();
    let k_a = scope.alloc_string("a").unwrap();
    let v_b = scope.alloc_string("b").unwrap();
    let k_ignored = scope.alloc_string("ignored").unwrap();
    let v_x = scope.alloc_string("x").unwrap();

    scope
      .define_property(
        attributes,
        HeapPropertyKey::String(k_type),
        data_desc(Value::String(v_json), true),
      )
      .unwrap();
    scope
      .define_property(
        attributes,
        HeapPropertyKey::String(k_a),
        data_desc(Value::String(v_b), true),
      )
      .unwrap();
    scope
      .define_property(
        attributes,
        HeapPropertyKey::String(k_ignored),
        data_desc(Value::String(v_x), false),
      )
      .unwrap();

    scope
      .define_property(
        options,
        HeapPropertyKey::String(k_with),
        data_desc(Value::Object(attributes), true),
      )
      .unwrap();

    let supported = ["a", "type"];
    let attrs =
      import_attributes_from_options(&mut vm, &mut scope, Value::Object(options), &supported)
        .unwrap();

    let keys: Vec<&str> = attrs.iter().map(|a| a.key.as_str()).collect();
    assert_eq!(keys, vec!["a", "type"]);
  }

  #[test]
  fn import_attributes_from_options_rejects_invalid_types() {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut scope = heap.scope();
    let mut vm = Vm::new(VmOptions::default());

    let supported = ["type"];
    let err =
      import_attributes_from_options(&mut vm, &mut scope, Value::Number(1.0), &supported).unwrap_err();
    assert!(matches!(
      err,
      ImportCallError::TypeError(ImportCallTypeError::OptionsNotObject)
    ));

    let options = scope.alloc_object().unwrap();
    let k_with = scope.alloc_string("with").unwrap();
    scope
      .define_property(
        options,
        HeapPropertyKey::String(k_with),
        data_desc(Value::Number(1.0), true),
      )
      .unwrap();

    let err =
      import_attributes_from_options(&mut vm, &mut scope, Value::Object(options), &supported).unwrap_err();
    assert!(matches!(
      err,
      ImportCallError::TypeError(ImportCallTypeError::AttributesNotObject)
    ));

    let options2 = scope.alloc_object().unwrap();
    let attrs_obj = scope.alloc_object().unwrap();
    let k_with2 = scope.alloc_string("with").unwrap();
    let k_type = scope.alloc_string("type").unwrap();
    scope
      .define_property(
        attrs_obj,
        HeapPropertyKey::String(k_type),
        data_desc(Value::Number(1.0), true),
      )
      .unwrap();
    scope
      .define_property(
        options2,
        HeapPropertyKey::String(k_with2),
        data_desc(Value::Object(attrs_obj), true),
      )
      .unwrap();

    let err =
      import_attributes_from_options(&mut vm, &mut scope, Value::Object(options2), &supported).unwrap_err();
    assert!(matches!(
      err,
      ImportCallError::TypeError(ImportCallTypeError::AttributeValueNotString)
    ));
  }

  #[test]
  fn all_import_attributes_supported_scan_consumes_fuel() {
    // `InnerModuleLoading` checks `AllImportAttributesSupported` while traversing the static import
    // graph. Import attribute lists can be large and may not involve nested parsing/evaluation work,
    // so the scan must be budgeted.
    let mut vm = Vm::new(VmOptions {
      check_time_every: 1,
      ..VmOptions::default()
    });
    vm.set_budget(Budget {
      // With `TICK_EVERY=32`, we should trip fuel exhaustion quickly even though the scan itself is
      // just a tight loop.
      fuel: Some(1),
      deadline: None,
      check_time_every: 1,
    });

    let supported = ["type"];
    let mut attrs = Vec::<ImportAttribute>::new();
    for _ in 0..5000 {
      attrs.push(ImportAttribute::new("type", "json"));
    }

    let err = first_unsupported_import_attribute_key(&mut vm, &supported, &attrs).unwrap_err();
    match err {
      VmError::Termination(term) => assert_eq!(term.reason, TerminationReason::OutOfFuel),
      other => panic!("expected OutOfFuel termination, got {other:?}"),
    }
  }

  #[test]
  fn cloning_requested_module_lists_consumes_fuel() {
    // `InnerModuleLoading` clones `[[RequestedModules]]` out of module records so it can recurse
    // without holding borrows. That cloning work is user-controlled (source size) and must be
    // budgeted.
    let mut vm = Vm::new(VmOptions {
      check_time_every: 1,
      ..VmOptions::default()
    });
    vm.set_budget(Budget {
      fuel: Some(1),
      deadline: None,
      check_time_every: 1,
    });

    let spec = crate::JsString::from_str("A").unwrap();
    let mut requests = Vec::<ModuleRequest>::new();
    for _ in 0..5000 {
      requests.push(ModuleRequest::new(spec.clone(), Vec::new()));
    }

    let err = try_clone_module_requests(&mut vm, &requests).unwrap_err();
    match err {
      VmError::Termination(term) => assert_eq!(term.reason, TerminationReason::OutOfFuel),
      other => panic!("expected OutOfFuel termination, got {other:?}"),
    }
  }

  #[test]
  fn continue_dynamic_import_rejects_on_invalid_payload() {
    let payload = ModuleLoadPayload::graph_loading_state(GraphLoadingState(Rc::new(RefCell::new(
      GraphLoadingStateInner {
        promise_capability: PromiseCapability {
          promise: Value::Undefined,
          resolve: Value::Undefined,
          reject: Value::Undefined,
        },
        promise_roots: None,
        dynamic_import: None,
        is_loading: false,
        pending_modules_count: 0,
        visited: Vec::new(),
        host_defined: HostDefined::default(),
      },
    ))));

    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut scope = heap.scope();
    let mut vm = Vm::new(VmOptions::default());
    let mut modules = ModuleGraph::new();

    let mut host = MicrotaskQueue::new();
    let err = continue_dynamic_import(
      &mut vm,
      &mut scope,
      &mut modules,
      &mut host,
      payload,
      Ok(ModuleId::from_raw(1)),
    )
    .unwrap_err();
    assert!(matches!(err, VmError::InvariantViolation(_)));
  }

  #[test]
  fn finish_loading_imported_module_tears_down_graph_payload_roots_on_tick_termination() {
    // Regression test: if `FinishLoadingImportedModule` returns early due to `vm.tick()` (fuel
    // exhaustion / interrupts), any persistent roots held by the payload must be released.
    //
    // Without this, dropping the payload can trigger `GraphLoadingStateInner`'s debug-assert root
    // leak check.
    struct Host {
      captured: Option<(ModuleReferrer, ModuleRequest, ModuleLoadPayload)>,
    }
    impl VmHostHooks for Host {
      fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}

      fn host_load_imported_module(
        &mut self,
        _vm: &mut Vm,
        _scope: &mut Scope<'_>,
        _modules: &mut ModuleGraph,
        referrer: ModuleReferrer,
        module_request: ModuleRequest,
        _host_defined: HostDefined,
        payload: ModuleLoadPayload,
      ) -> Result<(), VmError> {
        assert!(self.captured.is_none(), "expected a single load request");
        self.captured = Some((referrer, module_request, payload));
        Ok(())
      }
    }

    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap).unwrap();

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      let mut host = Host { captured: None };
      let mut scope = heap.scope();
      let mut modules = ModuleGraph::new();

      let baseline_root = scope.heap_mut().add_root(Value::Undefined).unwrap();
      let baseline_idx = baseline_root.index();
      scope.heap_mut().remove_root(baseline_root);

      let root = modules
        .add_module(crate::module_record::SourceTextModuleRecord {
          requested_modules: vec![ModuleRequest::new(crate::JsString::from_str("dep").unwrap(), Vec::new())],
          status: ModuleStatus::New,
          ..Default::default()
        })
        .unwrap();

      let _promise =
        load_requested_modules(&mut vm, &mut scope, &mut modules, &mut host, root, HostDefined::default())
          .unwrap();

      let (referrer, module_request, payload) = host.captured.take().expect("expected payload");
      assert_eq!(payload.kind(), ModuleLoadPayloadKind::GraphLoadingState);

      vm.set_budget(Budget {
        fuel: Some(0),
        deadline: None,
        check_time_every: 1,
      });

      let err = finish_loading_imported_module(
        &mut vm,
        &mut scope,
        &mut modules,
        &mut host,
        referrer,
        module_request,
        payload,
        Ok(ModuleId::from_raw(1)),
      )
      .unwrap_err();
      assert!(matches!(err, VmError::Termination(_)));

      // Ensure the payload's capability roots were actually released.
      let after_root = scope.heap_mut().add_root(Value::Undefined).unwrap();
      let after_idx = after_root.index();
      scope.heap_mut().remove_root(after_root);
      assert!(
        (after_idx as u64) < (baseline_idx as u64 + 3),
        "expected root id reuse after FinishLoadingImportedModule termination"
      );
    }));

    realm.teardown(&mut heap);
    if let Err(panic) = result {
      std::panic::resume_unwind(panic);
    }
  }

  #[test]
  fn finish_loading_imported_module_tears_down_dynamic_import_payload_roots_on_tick_termination() {
    // Regression test: `FinishLoadingImportedModule` must tear down DynamicImportState roots if it
    // returns early due to `vm.tick()` termination.
    struct Host {
      captured: Option<(ModuleReferrer, ModuleRequest, ModuleLoadPayload)>,
    }
    impl VmHostHooks for Host {
      fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}

      fn host_load_imported_module(
        &mut self,
        _vm: &mut Vm,
        _scope: &mut Scope<'_>,
        _modules: &mut ModuleGraph,
        referrer: ModuleReferrer,
        module_request: ModuleRequest,
        _host_defined: HostDefined,
        payload: ModuleLoadPayload,
      ) -> Result<(), VmError> {
        assert!(self.captured.is_none(), "expected a single dynamic import load request");
        self.captured = Some((referrer, module_request, payload));
        Ok(())
      }
    }

    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap).unwrap();

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      let mut host = Host { captured: None };
      let mut host_ctx = ();
      let mut scope = heap.scope();
      let mut modules = ModuleGraph::new();

      let baseline_root = scope.heap_mut().add_root(Value::Undefined).unwrap();
      let baseline_idx = baseline_root.index();
      scope.heap_mut().remove_root(baseline_root);

      // Ensure dynamic import sees an active Realm.
      let mut ctx_guard = vm
        .execution_context_guard(ExecutionContext {
          realm: realm.id(),
          script_or_module: None,
        })
        .unwrap();

      let global_object = realm.global_object();
      let specifier = scope.alloc_string("dep").unwrap();

      let _promise = start_dynamic_import_with_host_and_hooks(
        &mut ctx_guard,
        &mut scope,
        &mut modules,
        &mut host_ctx,
        &mut host,
        global_object,
        Value::String(specifier),
        Value::Undefined,
      )
      .unwrap();

      let (referrer, module_request, payload) = host.captured.take().expect("expected payload");
      assert_eq!(payload.kind(), ModuleLoadPayloadKind::PromiseCapability);

      ctx_guard.set_budget(Budget {
        fuel: Some(0),
        deadline: None,
        check_time_every: 1,
      });

      let err = finish_loading_imported_module(
        &mut ctx_guard,
        &mut scope,
        &mut modules,
        &mut host,
        referrer,
        module_request,
        payload,
        Ok(ModuleId::from_raw(1)),
      )
      .unwrap_err();
      assert!(matches!(err, VmError::Termination(_)));

      // Ensure the payload's capability roots were actually released.
      let after_root = scope.heap_mut().add_root(Value::Undefined).unwrap();
      let after_idx = after_root.index();
      scope.heap_mut().remove_root(after_root);
      assert!(
        (after_idx as u64) < (baseline_idx as u64 + 3),
        "expected root id reuse after FinishLoadingImportedModule termination"
      );
    }));

    realm.teardown(&mut heap);
    if let Err(panic) = result {
      std::panic::resume_unwind(panic);
    }
  }

  #[test]
  fn graph_loading_state_releases_persistent_roots_even_if_settlement_fails() {
    // If calling the internal promise capability resolve/reject functions fails (for example due to
    // budgets/interrupts), we still must release the persistent roots held by the graph loading
    // state so hostile inputs cannot leak roots indefinitely.
    //
    // We can observe this by checking that the next `Heap::add_root` call reuses one of the freed
    // root ids (0..=2) rather than allocating a new slot (index 3).
    struct Host;
    impl VmHostHooks for Host {
      fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}
    }

    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap).unwrap();

    // Ensure we always call `Realm::teardown` even if the test panics, otherwise `Realm`'s `Drop`
    // will panic in debug builds.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      let mut host = Host;
      let mut host_ctx = ();
      let mut scope = heap.scope();
      let mut modules = ModuleGraph::new();
      let (state, _promise) =
        GraphLoadingState::new(&mut vm, &mut scope, &mut host_ctx, &mut host, HostDefined::default()).unwrap();

      // Capture the persistent roots created by `GraphLoadingState::new` so we can observe whether
      // they are removed.
      let (promise_root, resolve_root, reject_root) = {
        let guard = state.0.borrow();
        let roots = guard
          .promise_roots
          .as_ref()
          .expect("GraphLoadingState should have promise roots after creation");
        (roots.promise_root(), roots.resolve_root(), roots.reject_root())
      };
      assert!(scope.heap().get_root(promise_root).is_some());
      assert!(scope.heap().get_root(resolve_root).is_some());
      assert!(scope.heap().get_root(reject_root).is_some());

      // Force `Vm::call_with_host` to fail via the tick at call entry.
      vm.set_budget(crate::vm::Budget {
        fuel: Some(0),
        deadline: None,
        check_time_every: 1,
      });

      let err = state
        .resolve_promise(&mut vm, &mut scope, &mut modules, &mut host_ctx, &mut host)
        .unwrap_err();
      assert!(matches!(err, VmError::Termination(_)));

      // Even though settlement failed, the persistent roots must be released.
      assert!(scope.heap().get_root(promise_root).is_none());
      assert!(scope.heap().get_root(resolve_root).is_none());
      assert!(scope.heap().get_root(reject_root).is_none());
    }));

    realm.teardown(&mut heap);
    if let Err(panic) = result {
      std::panic::resume_unwind(panic);
    }
  }

  #[test]
  fn graph_loading_state_releases_persistent_roots_on_inner_module_loading_error() {
    // If `InnerModuleLoading` aborts with an abrupt completion, `GraphLoadingState` owns persistent
    // roots that must be released before returning the error to the host.
    //
    // This test forces `GraphLoadingState::inc_pending` to overflow, which causes
    // `inner_module_loading` to return `VmError::LimitExceeded` before the promise is settled.
    struct Host;
    impl VmHostHooks for Host {
      fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}
    }

    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap).unwrap();

    // Ensure we always call `Realm::teardown` even if the test panics, otherwise `Realm`'s `Drop`
    // will panic in debug builds.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      let mut host = Host;
      let mut host_ctx = ();
      let mut scope = heap.scope();
      let (state, _promise) =
        GraphLoadingState::new(&mut vm, &mut scope, &mut host_ctx, &mut host, HostDefined::default()).unwrap();

      let (promise_root, resolve_root, reject_root) = {
        let guard = state.0.borrow();
        let roots = guard
          .promise_roots
          .as_ref()
          .expect("GraphLoadingState should have promise roots after creation");
        (roots.promise_root(), roots.resolve_root(), roots.reject_root())
      };
      assert!(scope.heap().get_root(promise_root).is_some());
      assert!(scope.heap().get_root(resolve_root).is_some());
      assert!(scope.heap().get_root(reject_root).is_some());

      // Force `GraphLoadingState::inc_pending` to overflow.
      state.0.borrow_mut().pending_modules_count = usize::MAX;

      let mut modules = ModuleGraph::new();
      let module = modules.add_module(crate::module_record::SourceTextModuleRecord {
        requested_modules: vec![ModuleRequest::new(crate::JsString::from_str("dep").unwrap(), Vec::new())],
        status: ModuleStatus::New,
        ..Default::default()
      })
      .expect("add module");

      let err = continue_module_loading(
        &mut vm,
        &mut scope,
        &mut modules,
        &mut host,
        ModuleLoadPayload::graph_loading_state(state.clone()),
        Ok(module),
      )
      .unwrap_err();
      assert!(matches!(
        err,
        VmError::LimitExceeded("module graph loader pending module count overflow")
      ));

      // Even though module loading aborted, the graph loading promise roots must be released.
      assert!(scope.heap().get_root(promise_root).is_none());
      assert!(scope.heap().get_root(resolve_root).is_none());
      assert!(scope.heap().get_root(reject_root).is_none());
    }));

    realm.teardown(&mut heap);
    if let Err(panic) = result {
      std::panic::resume_unwind(panic);
    }
  }

  #[test]
  fn graph_loading_state_does_not_leak_stack_roots() {
    // `GraphLoadingState` must not leave temporary stack roots behind on the caller's `Scope`.
    // Persistent roots are tracked separately and are removed when the graph-loading promise is
    // settled.
    struct Host;
    impl VmHostHooks for Host {
      fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}
    }

    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap).unwrap();

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      let mut host = Host;
      let mut host_ctx = ();
      let mut scope = heap.scope();
      let mut modules = ModuleGraph::new();

      let roots_before = (
        scope.heap().root_stack.len(),
        scope.heap().env_root_stack.len(),
      );

      let (state, _promise) =
        GraphLoadingState::new(&mut vm, &mut scope, &mut host_ctx, &mut host, HostDefined::default()).unwrap();

      assert_eq!(
        (scope.heap().root_stack.len(), scope.heap().env_root_stack.len()),
        roots_before
      );

      // Resolving the promise should not leak stack roots either.
      let roots_before_resolve = (
        scope.heap().root_stack.len(),
        scope.heap().env_root_stack.len(),
      );
      state
        .resolve_promise(&mut vm, &mut scope, &mut modules, &mut host_ctx, &mut host)
        .unwrap();
      assert_eq!(
        (scope.heap().root_stack.len(), scope.heap().env_root_stack.len()),
        roots_before_resolve
      );

      // Fresh state to exercise `reject_promise`.
      let (state2, _promise2) =
        GraphLoadingState::new(&mut vm, &mut scope, &mut host_ctx, &mut host, HostDefined::default()).unwrap();
      assert_eq!(
        (scope.heap().root_stack.len(), scope.heap().env_root_stack.len()),
        roots_before
      );

      let roots_before_reject = (
        scope.heap().root_stack.len(),
        scope.heap().env_root_stack.len(),
      );
      state2
        .reject_promise(
          &mut vm,
          &mut scope,
          &mut host_ctx,
          &mut host,
          VmError::LimitExceeded("test"),
        )
        .unwrap();
      assert_eq!(
        (scope.heap().root_stack.len(), scope.heap().env_root_stack.len()),
        roots_before_reject
      );
    }));

    realm.teardown(&mut heap);
    if let Err(panic) = result {
      std::panic::resume_unwind(panic);
    }
  }

  #[test]
  fn module_load_payload_teardown_roots_allows_abandoned_module_loading() {
    // `ModuleLoadPayload` contains persistent roots (via `GraphLoadingState`) so that hosts can
    // keep the payload in non-traced memory across async boundaries.
    //
    // If a host abandons module loading mid-flight (navigation cancel, event loop teardown, etc.)
    // and drops the payload without completing the `FinishLoadingImportedModule` state machine, we
    // must not panic in debug builds due to leaked-root assertions.
    struct Host {
      payload: Option<ModuleLoadPayload>,
    }
    impl VmHostHooks for Host {
      fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}

      fn host_load_imported_module(
        &mut self,
        _vm: &mut Vm,
        _scope: &mut Scope<'_>,
        _modules: &mut ModuleGraph,
        _referrer: ModuleReferrer,
        _module_request: ModuleRequest,
        _host_defined: HostDefined,
        payload: ModuleLoadPayload,
      ) -> Result<(), VmError> {
        self.payload = Some(payload);
        Ok(())
      }
    }

    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap).unwrap();

    // Ensure we always call `Realm::teardown` even if the test panics, otherwise `Realm`'s `Drop`
    // will panic in debug builds.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      let mut host = Host { payload: None };
      let mut host_ctx = ();
      let mut scope = heap.scope();
      let mut modules = ModuleGraph::new();

      // Create a module with a pending static import so `LoadRequestedModules` enters the
      // host-driven loading path and passes a `ModuleLoadPayload` to `host_load_imported_module`.
      let root = modules
        .add_module(crate::module_record::SourceTextModuleRecord {
          requested_modules: vec![ModuleRequest::new("dep", Vec::new())],
          status: ModuleStatus::New,
          ..Default::default()
        })
        .expect("add module");

      let _promise = load_requested_modules_with_host_and_hooks(
        &mut vm,
        &mut scope,
        &mut modules,
        &mut host_ctx,
        &mut host,
        root,
        HostDefined::default(),
      )
      .unwrap();

      let payload = host
        .payload
        .take()
        .expect("expected host_load_imported_module to capture a payload");

      // This should be safe and idempotent.
      payload.teardown_roots(scope.heap_mut());
      payload.teardown_roots(scope.heap_mut());

      // Dropping after teardown should not trip debug assertions in `GraphLoadingStateInner::drop`.
      drop(payload);
    }));

    realm.teardown(&mut heap);
    if let Err(panic) = result {
      std::panic::resume_unwind(panic);
    }
  }

  #[test]
  fn graph_loading_state_rc_alloc_failure_surfaces_out_of_memory() {
    struct Host;
    impl VmHostHooks for Host {
      fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}
    }

    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap).unwrap();

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      let mut host = Host;
      let mut host_ctx = ();
      let mut scope = heap.scope();
      let mut modules = ModuleGraph::new();

      // Capture the next root slot index so we can detect leaked persistent roots from
      // `PromiseCapabilityRoots::new` even when `GraphLoadingState::new` fails before returning the
      // continuation state.
      let baseline_root = scope.heap_mut().add_root(Value::Undefined).unwrap();
      let baseline_idx = baseline_root.index();
      scope.heap_mut().remove_root(baseline_root);

      let layout = Layout::new::<TestRcBox<RefCell<GraphLoadingStateInner>>>();
      let _guard = FailNextMatchingAllocGuard::new(layout.size(), layout.align());

      let err = load_requested_modules_with_host_and_hooks(
        &mut vm,
        &mut scope,
        &mut modules,
        &mut host_ctx,
        &mut host,
        ModuleId::from_raw(1),
        HostDefined::default(),
      )
      .unwrap_err();

      assert!(matches!(err, VmError::OutOfMemory));

      // If the promise capability roots were released, the next root allocation should reuse one
      // of the freed indices (baseline..baseline+2) rather than growing the root table.
      let after_root = scope.heap_mut().add_root(Value::Undefined).unwrap();
      let after_idx = after_root.index();
      scope.heap_mut().remove_root(after_root);
      assert!(
        (after_idx as u64) < (baseline_idx as u64 + 3),
        "expected root id reuse after GraphLoadingState::new OOM"
      );
    }));

    realm.teardown(&mut heap);
    if let Err(panic) = result {
      std::panic::resume_unwind(panic);
    }
  }

  #[test]
  fn dynamic_import_state_rc_alloc_failure_surfaces_out_of_memory() {
    struct Host;
    impl VmHostHooks for Host {
      fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}
    }

    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap).unwrap();

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      let mut host = Host;
      let mut host_ctx = ();
      let mut scope = heap.scope();
      let mut modules = ModuleGraph::new();

      // Ensure dynamic import sees an active Realm.
      let mut ctx_guard = vm
        .execution_context_guard(ExecutionContext {
        realm: realm.id(),
        script_or_module: None,
      })
        .unwrap();

      let global_object = realm.global_object();
      let specifier = scope.alloc_string("dep").unwrap();

      let baseline_root = scope.heap_mut().add_root(Value::Undefined).unwrap();
      let baseline_idx = baseline_root.index();
      scope.heap_mut().remove_root(baseline_root);

      let layout = Layout::new::<TestRcBox<RefCell<DynamicImportStateInner>>>();
      let _guard = FailNextMatchingAllocGuard::new(layout.size(), layout.align());

      let err = start_dynamic_import_with_host_and_hooks(
        &mut ctx_guard,
        &mut scope,
        &mut modules,
        &mut host_ctx,
        &mut host,
        global_object,
        Value::String(specifier),
        Value::Undefined,
      )
      .unwrap_err();

      assert!(matches!(err, VmError::OutOfMemory));

      let after_root = scope.heap_mut().add_root(Value::Undefined).unwrap();
      let after_idx = after_root.index();
      scope.heap_mut().remove_root(after_root);
      assert!(
        (after_idx as u64) < (baseline_idx as u64 + 3),
        "expected root id reuse after DynamicImportState::new OOM"
      );
    }));

    // Realm teardown unregisters its persistent roots.
    realm.teardown(&mut heap);
    if let Err(panic) = result {
      std::panic::resume_unwind(panic);
    }
  }
}
