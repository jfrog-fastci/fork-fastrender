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
//! The goal of this module is to provide the *host hook surface* and spec-shaped record types,
//! **not** to implement full module parsing/linking/evaluation.
//!
//! See also:
//! - [`crate::VmHostHooks::host_load_imported_module`]
//! - [`Vm::finish_loading_imported_module`]

use crate::module_graph::ModuleGraph;
use crate::module_record::ModuleStatus;
use crate::property::PropertyKey;
use crate::promise::PromiseCapability;
use crate::{
  GcString, ImportAttribute, LoadedModuleRequest, ModuleId, ModuleRequest, RealmId, RootId, Scope,
  ScriptId, Value, Vm, VmError,
};
use std::any::Any;
use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;
use std::sync::Arc;

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
struct PromiseCapabilityRoots {
  promise: RootId,
  resolve: RootId,
  reject: RootId,
}

#[derive(Debug)]
struct GraphLoadingStateInner {
  promise_capability: PromiseCapability,
  promise_roots: Option<PromiseCapabilityRoots>,
  is_loading: bool,
  pending_modules_count: usize,
  visited: Vec<ModuleId>,
  host_defined: HostDefined,
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
    host: &mut dyn crate::VmHostHooks,
    host_defined: HostDefined,
  ) -> Result<(Self, Value), VmError> {
    // Create a nested scope so any temporary stack roots created while constructing the promise
    // capability (and while registering persistent roots) are popped before we return.
    //
    // `GraphLoadingState` itself keeps the capability values alive via persistent roots.
    let (cap, promise_roots) = {
      let mut root_scope = scope.reborrow();
      let cap = crate::promise_ops::new_promise_capability(vm, &mut root_scope, host)?;

      // Root the capability values while creating persistent roots: `Heap::add_root` can trigger GC.
      let values = [cap.promise, cap.resolve, cap.reject];
      root_scope.push_roots(&values)?;

      let mut roots: Vec<RootId> = Vec::new();
      roots
        .try_reserve_exact(values.len())
        .map_err(|_| VmError::OutOfMemory)?;
      for &value in &values {
        match root_scope.heap_mut().add_root(value) {
          Ok(id) => roots.push(id),
          Err(e) => {
            for root in roots.drain(..) {
              root_scope.heap_mut().remove_root(root);
            }
            return Err(e);
          }
        }
      }

      let promise_roots = PromiseCapabilityRoots {
        promise: roots[0],
        resolve: roots[1],
        reject: roots[2],
      };
      Ok((cap, promise_roots))
    }?;

    Ok((
      Self(Rc::new(RefCell::new(GraphLoadingStateInner {
        promise_capability: cap,
        promise_roots: Some(promise_roots),
        is_loading: true,
        pending_modules_count: 1,
        visited: Vec::new(),
        host_defined,
      }))),
      cap.promise,
    ))
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
    host: &mut dyn crate::VmHostHooks,
  ) -> Result<(), VmError> {
    let (cap, roots) = {
      let mut state = self.0.borrow_mut();
      (state.promise_capability, state.promise_roots.take())
    };

    // Settlement is best-effort: if roots are already dropped, treat it as a no-op.
    let Some(roots) = roots else {
      return Ok(());
    };

    // Ensure we always release the persistent roots even if calling the resolve function fails
    // (e.g. termination due to budgets/interrupts).
    let result = (|| {
      let mut call_scope = scope.reborrow();
      call_scope.push_root(cap.resolve)?;
      let _ = vm.call_with_host(
        &mut call_scope,
        host,
        cap.resolve,
        Value::Undefined,
        &[Value::Undefined],
      )?;
      Ok(())
    })();
    scope.heap_mut().remove_root(roots.promise);
    scope.heap_mut().remove_root(roots.resolve);
    scope.heap_mut().remove_root(roots.reject);
    result
  }

  fn reject_promise(
    &self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    host: &mut dyn crate::VmHostHooks,
    err: VmError,
  ) -> Result<(), VmError> {
    let (cap, roots) = {
      let mut state = self.0.borrow_mut();
      (state.promise_capability, state.promise_roots.take())
    };

    let Some(roots) = roots else {
      return Ok(());
    };

    let reason = err.thrown_value().unwrap_or(Value::Undefined);

    let result = (|| {
      let mut call_scope = scope.reborrow();
      call_scope.push_root(cap.reject)?;
      call_scope.push_root(reason)?;
      let _ = vm.call_with_host(&mut call_scope, host, cap.reject, Value::Undefined, &[reason])?;
      Ok(())
    })();
    scope.heap_mut().remove_root(roots.promise);
    scope.heap_mut().remove_root(roots.resolve);
    scope.heap_mut().remove_root(roots.reject);
    result
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
  PromiseCapability(PromiseCapability),
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
  pub(crate) fn promise_capability(capability: PromiseCapability) -> Self {
    Self(ModuleLoadPayloadInner::PromiseCapability(capability))
  }
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

/// Implements ECMA-262 `LoadRequestedModules(hostDefined?)` for cyclic modules.
///
/// This starts the module graph loading state machine and returns a Promise that is fulfilled once
/// all modules in the static import graph have been loaded.
pub fn load_requested_modules(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  modules: &mut ModuleGraph,
  host: &mut dyn crate::VmHostHooks,
  module: ModuleId,
  host_defined: HostDefined,
) -> Result<Value, VmError> {
  let (state, promise) = GraphLoadingState::new(vm, scope, host, host_defined)?;
  if let Err(err) = inner_module_loading(vm, scope, modules, host, &state, module) {
    // `GraphLoadingState` owns persistent roots for the promise capability. If we abort the
    // algorithm with an abrupt completion (OOM, termination, etc), ensure those roots are released
    // before returning the error to the host.
    state.set_is_loading(false);
    let _ = state.reject_promise(vm, scope, host, err.clone());
    return Err(err);
  }
  Ok(promise)
}

/// Implements ECMA-262 `InnerModuleLoading(state, module)`.
pub fn inner_module_loading(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  modules: &mut ModuleGraph,
  host: &mut dyn crate::VmHostHooks,
  state: &GraphLoadingState,
  module: ModuleId,
) -> Result<(), VmError> {
  let Some(record) = modules.get_module(module) else {
    state.set_is_loading(false);
    state.reject_promise(vm, scope, host, VmError::InvalidHandle)?;
    return Ok(());
  };

  let should_traverse = record.status == ModuleStatus::New && !state.visited_contains(module);
  let requested_modules = if should_traverse {
    record.requested_modules.clone()
  } else {
    Vec::new()
  };

  if should_traverse {
    state.push_visited(module)?;
    state.inc_pending(requested_modules.len())?;

    for request in requested_modules {
      // `AllImportAttributesSupported`.
      let supported = host.host_get_supported_import_attributes();
      if !all_import_attributes_supported(supported, &request.attributes) {
        // Per ECMA-262, unsupported import attributes are a thrown SyntaxError.
        if let Some(intrinsics) = vm.intrinsics() {
          let unsupported_key = request
            .attributes
            .iter()
            .find(|attr| !supported.iter().any(|k| *k == attr.key.as_str()))
            .map(|attr| attr.key.as_str());

          let message = match unsupported_key {
            Some(key) => format!("Unsupported import attribute: {key}"),
            None => "Unsupported import attributes".to_string(),
          };

          let err_value = crate::new_error(
            scope,
            intrinsics.syntax_error_prototype(),
            "SyntaxError",
            &message,
          )?;

          continue_module_loading(
            vm,
            scope,
            modules,
            host,
            ModuleLoadPayload::graph_loading_state(state.clone()),
            Err(VmError::Throw(err_value)),
          )?;
        } else {
          continue_module_loading(
            vm,
            scope,
            modules,
            host,
            ModuleLoadPayload::graph_loading_state(state.clone()),
            Err(VmError::Unimplemented(
              "AllImportAttributesSupported requires Vm intrinsics (create a Realm first)",
            )),
          )?;
        }
      } else if let Some(loaded_module) = modules.get_imported_module(module, &request) {
        inner_module_loading(vm, scope, modules, host, state, loaded_module)?;
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
    for &visited_id in &visited.visited {
      if let Some(module) = modules.get_module_mut(visited_id) {
        if module.status == ModuleStatus::New {
          module.status = ModuleStatus::Unlinked;
        }
      }
    }
  }
  state.resolve_promise(vm, scope, host)?;
  Ok(())
}

/// Implements ECMA-262 `FinishLoadingImportedModule(...)`.
///
/// Hosts must call this exactly once for each [`crate::VmHostHooks::host_load_imported_module`]
/// invocation, either synchronously (re-entrantly) or asynchronously later.
pub fn finish_loading_imported_module(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  modules: &mut ModuleGraph,
  host: &mut dyn crate::VmHostHooks,
  referrer: ModuleReferrer,
  module_request: ModuleRequest,
  payload: ModuleLoadPayload,
  result: ModuleCompletion,
) -> Result<(), VmError> {
  // 1. `FinishLoadingImportedModule` caching invariant:
  //    If a `(referrer, moduleRequest)` pair resolves normally more than once, it must resolve to
  //    the same Module Record each time.
  let result = match result {
    Ok(loaded) => {
      if let ModuleReferrer::Module(referrer) = referrer {
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
            Ok(loaded)
          }
        } else {
          Ok(loaded)
        }
      } else {
        Ok(loaded)
      }
    }
    Err(e) => Err(e),
  };

  match payload.0 {
    ModuleLoadPayloadInner::GraphLoadingState(state) => continue_module_loading(
      vm,
      scope,
      modules,
      host,
      ModuleLoadPayload::graph_loading_state(state),
      result,
    ),
    ModuleLoadPayloadInner::PromiseCapability(capability) => {
      // Placeholder until dynamic import is implemented.
      continue_dynamic_import(capability, result)
    }
  }
}

impl Vm {
  /// Completes a pending `HostLoadImportedModule` operation.
  ///
  /// This is the entry point host environments should call once they have finished fetching and
  /// parsing a module (or have failed to do so). It performs `FinishLoadingImportedModule` and then
  /// dispatches to the appropriate continuation based on `payload`:
  /// - `ContinueModuleLoading` for static module graph loading, or
  /// - `ContinueDynamicImport` for `import()` (currently unimplemented).
  #[inline]
  pub fn finish_loading_imported_module(
    &mut self,
    scope: &mut Scope<'_>,
    modules: &mut ModuleGraph,
    host: &mut dyn crate::VmHostHooks,
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
}

/// Implements ECMA-262 `ContinueModuleLoading(state, moduleCompletion)`.
pub fn continue_module_loading(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  modules: &mut ModuleGraph,
  host: &mut dyn crate::VmHostHooks,
  payload: ModuleLoadPayload,
  result: ModuleCompletion,
) -> Result<(), VmError> {
  let ModuleLoadPayloadInner::GraphLoadingState(state) = payload.0 else {
    return Err(VmError::InvariantViolation(
      "ContinueModuleLoading called with non-GraphLoadingState payload",
    ));
  };

  if !state.is_loading() {
    return Ok(());
  }

  match result {
    Ok(module) => {
      if let Err(err) = inner_module_loading(vm, scope, modules, host, &state, module) {
        // Ensure promise roots are released even on abrupt completion.
        state.set_is_loading(false);
        let _ = state.reject_promise(vm, scope, host, err.clone());
        return Err(err);
      }
      Ok(())
    }
    Err(err) => {
      state.set_is_loading(false);
      state.reject_promise(vm, scope, host, err)
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

fn clone_heap_string_to_string(heap: &crate::Heap, s: GcString) -> Result<String, VmError> {
  Ok(heap.get_string(s)?.to_utf8_lossy())
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
pub fn import_attributes_from_options(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  options: Value,
  supported_keys: &[&str],
) -> Result<Vec<ImportAttribute>, ImportCallError> {
  if matches!(options, Value::Undefined) {
    return Ok(Vec::new());
  }

  let Value::Object(options_obj) = options else {
    return Err(ImportCallError::TypeError(ImportCallTypeError::OptionsNotObject));
  };

  let with_key =
    PropertyKey::from_string(make_key_string(scope, "with").map_err(ImportCallError::Vm)?);
  let attributes_obj = scope
    .ordinary_get(vm, options_obj, with_key, Value::Object(options_obj))
    .map_err(ImportCallError::Vm)?;

  if matches!(attributes_obj, Value::Undefined) {
    return Ok(Vec::new());
  }

  let Value::Object(attributes_obj) = attributes_obj else {
    return Err(ImportCallError::TypeError(
      ImportCallTypeError::AttributesNotObject,
    ));
  };

  let own_keys = scope
    .ordinary_own_property_keys(attributes_obj)
    .map_err(ImportCallError::Vm)?;

  let mut attributes = Vec::<ImportAttribute>::new();

  for key in own_keys {
    let PropertyKey::String(key_string) = key else {
      continue;
    };

    let Some(desc) = scope
      .heap()
      .object_get_own_property(attributes_obj, &key)
      .map_err(ImportCallError::Vm)?
    else {
      continue;
    };

    if !desc.enumerable {
      continue;
    }

    let value = scope
      .ordinary_get(vm, attributes_obj, key, Value::Object(attributes_obj))
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

/// Spec-shaped dynamic import entry point (EvaluateImportCall).
///
/// This function currently returns [`VmError::Unimplemented`] because `vm-js` does not yet provide
/// dynamic import (`import()`) module fetching/linking/evaluation.
#[allow(unused_variables)]
pub fn start_dynamic_import(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn crate::VmHostHooks,
  specifier: Value,
  options: Value,
) -> Result<Value, VmError> {
  Err(VmError::Unimplemented("dynamic import"))
}

/// Placeholder for the dynamic import continuation (`ContinueDynamicImport`).
pub fn continue_dynamic_import(
  _promise_capability: PromiseCapability,
  _module_completion: ModuleCompletion,
) -> Result<(), VmError> {
  Err(VmError::Unimplemented("ContinueDynamicImport"))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::property::PropertyDescriptor;
  use crate::property::PropertyKey as HeapPropertyKey;
  use crate::property::PropertyKind as HeapPropertyKind;
  use crate::Heap;
  use crate::HeapLimits;
  use crate::Job;
  use crate::Realm;
  use crate::RealmId;
  use crate::VmHostHooks;
  use crate::VmOptions;

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
  fn continue_dynamic_import_is_stub() {
    let cap = PromiseCapability {
      promise: Value::Undefined,
      resolve: Value::Undefined,
      reject: Value::Undefined,
    };
    let err = continue_dynamic_import(cap, Ok(ModuleId::from_raw(1))).unwrap_err();
    assert!(matches!(err, VmError::Unimplemented("ContinueDynamicImport")));
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
      let mut scope = heap.scope();
      let (state, _promise) =
        GraphLoadingState::new(&mut vm, &mut scope, &mut host, HostDefined::default()).unwrap();

      // Capture the persistent roots created by `GraphLoadingState::new` so we can observe whether
      // they are removed.
      let (promise_root, resolve_root, reject_root) = {
        let guard = state.0.borrow();
        let roots = guard
          .promise_roots
          .as_ref()
          .expect("GraphLoadingState should have promise roots after creation");
        (roots.promise, roots.resolve, roots.reject)
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
        .resolve_promise(&mut vm, &mut scope, &mut host)
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
      let mut scope = heap.scope();
      let (state, _promise) =
        GraphLoadingState::new(&mut vm, &mut scope, &mut host, HostDefined::default()).unwrap();

      let (promise_root, resolve_root, reject_root) = {
        let guard = state.0.borrow();
        let roots = guard
          .promise_roots
          .as_ref()
          .expect("GraphLoadingState should have promise roots after creation");
        (roots.promise, roots.resolve, roots.reject)
      };
      assert!(scope.heap().get_root(promise_root).is_some());
      assert!(scope.heap().get_root(resolve_root).is_some());
      assert!(scope.heap().get_root(reject_root).is_some());

      // Force `GraphLoadingState::inc_pending` to overflow.
      state.0.borrow_mut().pending_modules_count = usize::MAX;

      let mut modules = ModuleGraph::new();
      let module = modules.add_module(crate::module_record::SourceTextModuleRecord {
        requested_modules: vec![ModuleRequest::new("dep", Vec::new())],
        status: ModuleStatus::New,
        ..Default::default()
      });

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
      let mut scope = heap.scope();

      let roots_before = (
        scope.heap().root_stack.len(),
        scope.heap().env_root_stack.len(),
      );

      let (state, _promise) =
        GraphLoadingState::new(&mut vm, &mut scope, &mut host, HostDefined::default()).unwrap();

      assert_eq!(
        (scope.heap().root_stack.len(), scope.heap().env_root_stack.len()),
        roots_before
      );

      // Resolving the promise should not leak stack roots either.
      let roots_before_resolve = (
        scope.heap().root_stack.len(),
        scope.heap().env_root_stack.len(),
      );
      state.resolve_promise(&mut vm, &mut scope, &mut host).unwrap();
      assert_eq!(
        (scope.heap().root_stack.len(), scope.heap().env_root_stack.len()),
        roots_before_resolve
      );

      // Fresh state to exercise `reject_promise`.
      let (state2, _promise2) =
        GraphLoadingState::new(&mut vm, &mut scope, &mut host, HostDefined::default()).unwrap();
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
}
