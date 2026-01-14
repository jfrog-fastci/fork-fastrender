use crate::error::Termination;
use crate::error::TerminationReason;
use crate::error::VmError;
use crate::exec::RuntimeEnv;
use crate::hir_exec::HirAsyncContinuation;
use crate::execution_context::ExecutionContext;
use crate::execution_context::ModuleId;
use crate::execution_context::ScriptId;
use crate::execution_context::ScriptOrModule;
use crate::fallible_alloc::arc_try_new_vm;
use crate::function::{
  CallHandler, ConstructHandler, EcmaFunctionId, FunctionData, NativeConstructId, NativeFunctionId, ThisMode,
};
use crate::meta_properties::MetaPropertyContext;
use crate::interrupt::InterruptHandle;
use crate::interrupt::InterruptToken;
use crate::jobs::VmHost;
use crate::jobs::VmHostHooks;
use crate::jobs::VmJobContext;
use crate::microtasks::MicrotaskQueue;
use crate::module_graph::ModuleGraph;
use crate::source::SourceText;
use crate::source::StackFrame;
use crate::ExternalMemoryToken;
use crate::GcObject;
use crate::Heap;
use crate::Intrinsics;
use crate::PropertyDescriptor;
use crate::PropertyKey;
use crate::PropertyKind;
use crate::RealmId;
use crate::RootId;
use crate::Scope;
use crate::Value;
use diagnostics::FileId;
use parse_js::ast::class_or_object::{
  ClassOrObjGetter, ClassOrObjMethod, ClassOrObjSetter, ClassOrObjVal, ObjMember, ObjMemberType,
};
use parse_js::ast::expr::Expr as AstExpr;
use parse_js::ast::func::Func;
use parse_js::ast::node::Node;
use parse_js::ast::stmt::Stmt;
use parse_js::error::SyntaxErrorType;
use parse_js::{parse_with_options_cancellable_by_with_init, Dialect, ParseOptions, SourceType};
use std::any::Any;
use std::collections::{HashMap, HashSet, VecDeque};
use std::convert::TryFrom;
use std::num::NonZeroU32;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use std::{mem, ops};

const ARG_HANDLING_CHUNK_SIZE: usize = 256;
// Stack traces are best-effort: never allow attacker-controlled function names to force unbounded
// host allocations while capturing frames.
const MAX_STACK_FRAME_FUNCTION_NAME_BYTES: usize = 256;

#[derive(Debug)]
pub(crate) enum VmAsyncContinuation {
  Ast(crate::exec::AsyncContinuation),
  Hir(crate::hir_exec::HirAsyncContinuation),
}

#[derive(Debug, Clone)]
struct CallSite {
  source: Arc<str>,
  line: u32,
  col: u32,
}

impl CallSite {
  fn from_source_offset(source: &SourceText, offset: u32) -> Self {
    let (line, col) = source.line_col(offset);
    Self {
      source: source.name.clone(),
      line,
      col,
    }
  }
}

/// Converts internal `VmError` variants that represent spec `ThrowCompletion`s (TypeError, etc.)
/// into `VmError::Throw` by allocating a minimal `TypeError` instance.
///
/// This is primarily needed for non-evaluator call sites (e.g. Promise jobs/microtasks), which
/// treat only `VmError::Throw*` as catchable exceptions.
pub(crate) fn coerce_error_to_throw(vm: &Vm, scope: &mut Scope<'_>, err: VmError) -> VmError {
  let Some(intr) = vm.intrinsics() else {
    return err;
  };
  match err {
    VmError::Unimplemented(reason) => {
      let message =
        match crate::fallible_format::try_format_error_message("unimplemented: ", reason, "") {
          Ok(m) => m,
          Err(err) => return err,
        };
      let value = match crate::error_object::new_error(
        scope,
        intr.error_prototype(),
        "Error",
        message.as_str(),
      ) {
        Ok(value) => value,
        Err(err) => return err,
      };

      let stack = vm.capture_stack();
      crate::error_object::attach_stack_property_for_throw(scope, value, &stack);
      VmError::ThrowWithStack { value, stack }
    }
    VmError::TypeError(message) => crate::throw_type_error(scope, intr, message),
    VmError::RangeError(message) => crate::throw_range_error(scope, intr, message),
    VmError::NotCallable => crate::throw_type_error(scope, intr, "value is not callable"),
    VmError::NotConstructable => crate::throw_type_error(scope, intr, "value is not a constructor"),
    VmError::PrototypeCycle => crate::throw_type_error(scope, intr, "prototype cycle"),
    VmError::PrototypeChainTooDeep => {
      crate::throw_type_error(scope, intr, "prototype chain too deep")
    }
    VmError::InvalidPropertyDescriptorPatch => crate::throw_type_error(
      scope,
      intr,
      "invalid property descriptor patch: cannot mix data and accessor fields",
    ),
    other => other,
  }
}

/// Like [`coerce_error_to_throw`], but:
/// - always returns a [`VmError::ThrowWithStack`] for throw-completion errors when intrinsics are
///   available, and
/// - is best-effort under allocator/heap OOM when constructing the Error object (returns the
///   original error).
///
/// This is intended for **host-facing execution boundaries** (script entry points, job/microtask
/// execution) so embeddings never observe internal helper errors such as
/// [`VmError::TypeError`]/[`VmError::RangeError`]/[`VmError::NotCallable`].
pub(crate) fn coerce_error_to_throw_with_stack(
  vm: &Vm,
  scope: &mut Scope<'_>,
  err: VmError,
) -> VmError {
  let Some(intr) = vm.intrinsics() else {
    return err;
  };

  let stack = || vm.capture_stack();

  match err {
    VmError::ThrowWithStack { value, stack } => {
      crate::error_object::attach_stack_property_for_throw(scope, value, &stack);
      VmError::ThrowWithStack { value, stack }
    }
    VmError::Throw(value) => {
      let stack = stack();
      crate::error_object::attach_stack_property_for_throw(scope, value, &stack);
      VmError::ThrowWithStack { value, stack }
    }

    VmError::Unimplemented(reason) => {
      let original = VmError::Unimplemented(reason);
      let message = match crate::fallible_format::try_format_error_message("unimplemented: ", reason, "") {
        Ok(m) => m,
        Err(_) => return original,
      };
      match crate::error_object::new_error(scope, intr.error_prototype(), "Error", message.as_str()) {
        Ok(value) => {
          let stack = stack();
          crate::error_object::attach_stack_property_for_throw(scope, value, &stack);
          VmError::ThrowWithStack { value, stack }
        }
        Err(_) => original,
      }
    }

    VmError::TypeError(message) => {
      let original = VmError::TypeError(message);
      match crate::error_object::new_error(scope, intr.type_error_prototype(), "TypeError", message) {
        Ok(value) => {
          let stack = stack();
          crate::error_object::attach_stack_property_for_throw(scope, value, &stack);
          VmError::ThrowWithStack { value, stack }
        }
        Err(_) => original,
      }
    }
    VmError::RangeError(message) => {
      let original = VmError::RangeError(message);
      match crate::error_object::new_error(scope, intr.range_error_prototype(), "RangeError", message) {
        Ok(value) => {
          let stack = stack();
          crate::error_object::attach_stack_property_for_throw(scope, value, &stack);
          VmError::ThrowWithStack { value, stack }
        }
        Err(_) => original,
      }
    }
    VmError::NotCallable => {
      let original = VmError::NotCallable;
      let message = "value is not callable";
      match crate::error_object::new_error(scope, intr.type_error_prototype(), "TypeError", message) {
        Ok(value) => {
          let stack = stack();
          crate::error_object::attach_stack_property_for_throw(scope, value, &stack);
          VmError::ThrowWithStack { value, stack }
        }
        Err(_) => original,
      }
    }
    VmError::NotConstructable => {
      let original = VmError::NotConstructable;
      let message = "value is not a constructor";
      match crate::error_object::new_error(scope, intr.type_error_prototype(), "TypeError", message) {
        Ok(value) => {
          let stack = stack();
          crate::error_object::attach_stack_property_for_throw(scope, value, &stack);
          VmError::ThrowWithStack { value, stack }
        }
        Err(_) => original,
      }
    }
    VmError::PrototypeCycle => {
      let original = VmError::PrototypeCycle;
      let message = "prototype cycle";
      match crate::error_object::new_error(scope, intr.type_error_prototype(), "TypeError", message) {
        Ok(value) => {
          let stack = stack();
          crate::error_object::attach_stack_property_for_throw(scope, value, &stack);
          VmError::ThrowWithStack { value, stack }
        }
        Err(_) => original,
      }
    }
    VmError::PrototypeChainTooDeep => {
      let original = VmError::PrototypeChainTooDeep;
      let message = "prototype chain too deep";
      match crate::error_object::new_error(scope, intr.type_error_prototype(), "TypeError", message) {
        Ok(value) => {
          let stack = stack();
          crate::error_object::attach_stack_property_for_throw(scope, value, &stack);
          VmError::ThrowWithStack { value, stack }
        }
        Err(_) => original,
      }
    }
    VmError::InvalidPropertyDescriptorPatch => {
      let original = VmError::InvalidPropertyDescriptorPatch;
      let message = "invalid property descriptor patch: cannot mix data and accessor fields";
      match crate::error_object::new_error(scope, intr.type_error_prototype(), "TypeError", message) {
        Ok(value) => {
          let stack = stack();
          crate::error_object::attach_stack_property_for_throw(scope, value, &stack);
          VmError::ThrowWithStack { value, stack }
        }
        Err(_) => original,
      }
    }

    other => other,
  }
}
/// A native (host-implemented) function call handler.
pub type NativeCall = for<'a> fn(
  &mut Vm,
  &mut Scope<'a>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError>;

/// A native (host-implemented) function constructor handler.
pub type NativeConstruct = for<'a> fn(
  &mut Vm,
  &mut Scope<'a>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  args: &[Value],
  new_target: Value,
) -> Result<Value, VmError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum EcmaFunctionKind {
  /// A `function f() {}` declaration statement.
  Decl,
  /// A function or arrow function expression (`function() {}` / `() => {}`).
  Expr,
  /// A public class field initializer (`x = <expr>` / `static x = <expr>`).
  ///
  /// These are parsed by wrapping the field initializer expression in a synthetic class *method*
  /// body:
  /// `(class extends null { m() { return <expr>; } })`.
  ///
  /// This allows the VM to represent field initializers as ordinary callable functions which can be
  /// invoked during instance/static initialization with the correct `this` value.
  ClassFieldInitializer,
  /// An object literal method/getter/setter definition (e.g. `f() {}` / `get x() {}`).
  ///
  /// These are parsed by wrapping the snippet in an object literal expression: `({ <snippet> })`.
  ObjectMember,
  /// A class method/getter/setter/constructor definition (e.g. `m() {}` / `static m() {}` /
  /// `[expr]() {}`).
  ///
  /// These are parsed by wrapping the snippet in a class expression: `(class { <snippet> })`.
  ClassMember,
}

#[derive(Debug)]
pub(crate) struct EcmaFunctionCode {
  pub(crate) source: Arc<SourceText>,
  pub(crate) span_start: u32,
  pub(crate) span_end: u32,
  pub(crate) kind: EcmaFunctionKind,
  /// How many bytes of synthetic prefix were inserted when parsing the snippet.
  ///
  /// This is used to translate `Loc` offsets from the cached AST back into offsets in the original
  /// source text (e.g. function expressions are parsed by wrapping them in parentheses).
  pub(crate) prefix_len: u32,
  parsed: Option<Arc<Node<Func>>>,
  #[allow(dead_code)]
  parsed_memory: Option<ExternalMemoryToken>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct EcmaFunctionKey {
  source: *const (),
  span_start: u32,
  span_end: u32,
  kind: EcmaFunctionKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct TemplateRegistryKey {
  realm: RealmId,
  source: *const (),
  span_start: u32,
  span_end: u32,
}

#[derive(Debug)]
struct TemplateRegistryEntry {
  #[allow(dead_code)]
  source: Arc<SourceText>,
  root: RootId,
}

#[derive(Debug)]
struct RealmState {
  intrinsics: Intrinsics,
  global_var_names: HashSet<String>,
  math_random_state: u64,
}

/// Construction-time VM options.
#[derive(Debug, Clone)]
pub struct VmOptions {
  pub max_stack_depth: usize,
  pub default_fuel: Option<u64>,
  /// Default wall-clock execution budget applied when a VM budget is (re)initialized.
  ///
  /// Note: this is stored as a [`Duration`] and is converted into an [`Instant`] relative to the
  /// time the budget is initialized/reset (for example via [`Vm::reset_budget_to_default`]).
  pub default_deadline: Option<Duration>,
  pub check_time_every: u32,
  /// Seed for the VM's deterministic `Math.random()` PRNG.
  ///
  /// Embeddings that need non-deterministic randomness should provide a host hook (see
  /// [`crate::VmHostHooks::host_math_random_u64`]) or initialize this seed from an external RNG.
  pub math_random_seed: u64,
  /// Optional shared interrupt flag to observe for cooperative cancellation.
  ///
  /// If provided, the VM will use this flag for its interrupt token so hosts can cancel execution
  /// by setting the flag to `true`.
  pub interrupt_flag: Option<Arc<AtomicBool>>,
  /// Optional external interrupt flag observed for cooperative cancellation.
  ///
  /// This flag is observed **in addition to** [`VmOptions::interrupt_flag`], but is never cleared by
  /// [`Vm::reset_interrupt`] or the VM-owned [`InterruptHandle`]. This is intended for embedding-wide
  /// cancellation tokens (for example, a renderer/render-wide cancellation flag).
  pub external_interrupt_flag: Option<Arc<AtomicBool>>,
}

impl Default for VmOptions {
  fn default() -> Self {
    Self {
      max_stack_depth: 1024,
      default_fuel: None,
      default_deadline: None,
      check_time_every: 100,
      // A fixed default seed keeps `Math.random()` deterministic for tests and reproducible
      // embeddings. Hosts can override via `VmOptions` or `VmHostHooks::host_math_random_u64`.
      math_random_seed: 0x243F_6A88_85A3_08D3,
      interrupt_flag: None,
      external_interrupt_flag: None,
    }
  }
}

/// Per-run execution budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Budget {
  pub fuel: Option<u64>,
  pub deadline: Option<Instant>,
  pub check_time_every: u32,
}

impl Budget {
  pub fn unlimited(check_time_every: u32) -> Self {
    Self {
      fuel: None,
      deadline: None,
      check_time_every,
    }
  }
}

#[derive(Debug, Clone)]
pub(crate) struct BudgetState {
  budget: Budget,
  ticks: u64,
}

impl BudgetState {
  fn new(budget: Budget) -> Self {
    Self { budget, ticks: 0 }
  }
}

/// Opaque snapshot of a VM's budget state (budget + tick counter).
///
/// This is returned by [`Vm::swap_budget_state`] and can be passed back into
/// [`Vm::restore_budget_state`] to fully restore the previous budget, including its tick counter.
///
/// Host embeddings often need this because they cannot hold a mutable borrow of the [`Vm`] across
/// complex operations (for example, `JsRuntime::exec_script_*` borrows the runtime, including the
/// VM, mutably). [`Vm::push_budget`] provides an RAII guard for nested budgets, but it borrows the
/// VM for the lifetime of the guard; `swap_budget_state`/`restore_budget_state` provides the same
/// semantics without a borrow that spans the guest execution.
#[derive(Debug)]
pub struct BudgetStateToken {
  state: BudgetState,
}

/// VM execution state shell.
pub struct Vm {
  options: VmOptions,
  interrupt: InterruptToken,
  interrupt_handle: InterruptHandle,
  budget: BudgetState,
  /// Fallback deterministic `Math.random()` state used when no realm is active.
  ///
  /// In normal operation, `Math.random()` state is stored per-realm in [`Vm::realm_states`].
  unscoped_math_random_state: u64,
  /// Counter used to assign deterministic `[[AsyncEvaluationOrder]]` values during async module
  /// evaluation (top-level await).
  ///
  /// This is the engine's equivalent of ECMA-262's Agent Record `[[ModuleAsyncEvaluationCount]]`
  /// and is incremented by `IncrementModuleAsyncEvaluationCount`.
  module_async_evaluation_count: u64,
  /// Counter used to assign fresh [`ScriptId`] values to executed classic scripts.
  ///
  /// Each `JsRuntime::exec_*script*` entry point assigns a new ScriptId and installs it into the
  /// active execution context so dynamic `import()` can pass `ModuleReferrer::Script(..)` to host
  /// module loading hooks.
  next_script_id: u64,
  stack: Vec<StackFrame>,
  execution_context_stack: Vec<ExecutionContext>,
  /// Intern table for `ScriptOrModule` identities stored on function objects.
  ///
  /// Functions store `[[ScriptOrModule]]` as a compact token rather than an inline
  /// `Option<ScriptOrModule>` to avoid inflating the heap slot table size (which is charged against
  /// `HeapLimits`).
  script_or_module_table: Vec<ScriptOrModule>,
  native_calls: Vec<NativeCall>,
  native_constructs: Vec<NativeConstruct>,
  /// Optional host/embedding state associated with this VM.
  ///
  /// Native (host-implemented) call/construct handlers are expected to downcast this to their
  /// embedding-specific type via [`Vm::user_data`] / [`Vm::user_data_mut`].
  user_data: Option<Box<dyn Any>>,
  microtasks: MicrotaskQueue,
  /// Optional host hook override used by embedding entry points such as
  /// [`crate::JsRuntime::exec_script_source_with_host`].
  ///
  /// When set, [`Vm::call`] / [`Vm::construct`] will route through this host hook implementation
  /// instead of the VM-owned [`MicrotaskQueue`].
  ///
  /// ## Safety contract
  ///
  /// This raw pointer is written by:
  /// - [`Vm::with_host_hooks_override`]
  /// - [`Vm::push_active_host_hooks`] / [`Vm::pop_active_host_hooks`]
  /// - [`Vm::call_with_host`] / [`Vm::construct_with_host`] (for the duration of the call)
  ///
  /// Each of these APIs borrows the host hook implementation mutably for the duration in which the
  /// pointer may be dereferenced, and treats it as a reborrow of the original `&mut dyn
  /// VmHostHooks`.
  host_hooks_override: Option<*mut (dyn VmHostHooks + 'static)>,
  ecma_functions: Vec<EcmaFunctionCode>,
  ecma_function_cache: HashMap<EcmaFunctionKey, EcmaFunctionId>,
  template_registry: HashMap<TemplateRegistryKey, TemplateRegistryEntry>,
  async_resume_call: Option<NativeFunctionId>,
  exec_module_load_on_fulfilled_call: Option<NativeFunctionId>,
  exec_module_load_on_rejected_call: Option<NativeFunctionId>,
  module_tla_on_fulfilled_call: Option<NativeFunctionId>,
  module_tla_on_rejected_call: Option<NativeFunctionId>,
  dynamic_import_eval_on_fulfilled_call: Option<NativeFunctionId>,
  dynamic_import_eval_on_rejected_call: Option<NativeFunctionId>,
  module_namespace_getter_call: Option<NativeFunctionId>,
  async_from_sync_iterator_next_call: Option<NativeFunctionId>,
  async_from_sync_iterator_return_call: Option<NativeFunctionId>,
  async_from_sync_iterator_throw_call: Option<NativeFunctionId>,
  async_from_sync_iterator_unwrap_call: Option<NativeFunctionId>,
  async_from_sync_iterator_close_call: Option<NativeFunctionId>,
  async_iterator_close_on_fulfilled_call: Option<NativeFunctionId>,
  async_iterator_close_on_rejected_call: Option<NativeFunctionId>,
  next_async_continuation_id: u32,
  async_continuations: HashMap<u32, VmAsyncContinuation>,
  /// Optional pointer to an embedding-owned [`ModuleGraph`].
  ///
  /// This enables dynamic `import()` expressions evaluated from the AST interpreter (`exec.rs`) to
  /// access the module graph even when running inside nested ECMAScript function calls (which are
  /// invoked through `Vm::call` and do not thread an explicit `&mut ModuleGraph` parameter).
  ///
  /// ## Safety
  ///
  /// The embedding MUST ensure the pointed-to `ModuleGraph` outlives the VM (or clears this pointer
  /// before dropping the graph).
  module_graph: Option<*mut ModuleGraph>,
  /// Per-realm VM state (intrinsics, tracked global var names, deterministic PRNG state, ...).
  realm_states: HashMap<RealmId, RealmState>,
  /// Unscoped intrinsics used when the host installs intrinsics without associating them with a
  /// specific [`RealmId`] (see [`Vm::set_intrinsics`]).
  intrinsics: Option<Intrinsics>,
  /// Default realm used by host entry points that run without an active execution context.
  ///
  /// When multiple realms exist, most VM operations should consult [`Vm::current_realm`] instead.
  intrinsics_realm: Option<RealmId>,
  #[cfg(test)]
  native_calls_len_override: Option<usize>,
  #[cfg(test)]
  native_constructs_len_override: Option<usize>,
}

impl std::fmt::Debug for Vm {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    let mut ds = f.debug_struct("Vm");
    ds.field("options", &self.options);
    ds.field("interrupt", &self.interrupt);
    ds.field("interrupt_handle", &self.interrupt_handle);
    ds.field("budget", &self.budget);
    ds.field("stack", &self.stack);
    ds.field("execution_context_stack", &self.execution_context_stack);
    ds.field("native_calls", &self.native_calls.len());
    ds.field("native_constructs", &self.native_constructs.len());
    ds.field("user_data", &self.user_data.as_ref().map(|_| "<opaque>"));
    ds.field("microtasks", &self.microtasks);
    ds.field("host_hooks_override", &self.host_hooks_override.is_some());
    ds.field("ecma_functions", &self.ecma_functions.len());
    ds.field("ecma_function_cache", &self.ecma_function_cache.len());
    ds.field("template_registry", &self.template_registry.len());
    ds.field("async_continuations", &self.async_continuations.len());
    let hir_async_count = self
      .async_continuations
      .values()
      .filter(|c| matches!(c, VmAsyncContinuation::Hir(_)))
      .count();
    ds.field("hir_async_continuations", &hir_async_count);
    ds.field("module_graph", &self.module_graph.is_some());
    ds.field("realm_states", &self.realm_states.len());
    ds.field("intrinsics", &self.intrinsics);
    ds.field("intrinsics_realm", &self.intrinsics_realm);
    #[cfg(test)]
    {
      ds.field("native_calls_len_override", &self.native_calls_len_override);
      ds.field(
        "native_constructs_len_override",
        &self.native_constructs_len_override,
      );
    }
    ds.finish()
  }
}

/// RAII guard returned by [`Vm::push_budget`].
///
/// Dropping the guard restores the previous VM budget state, including its tick counter.
pub struct BudgetGuard<'a> {
  vm: &'a mut Vm,
  previous: Option<BudgetState>,
}

impl<'a> ops::Deref for BudgetGuard<'a> {
  type Target = Vm;

  fn deref(&self) -> &Self::Target {
    &*self.vm
  }
}

impl<'a> ops::DerefMut for BudgetGuard<'a> {
  fn deref_mut(&mut self) -> &mut Self::Target {
    &mut *self.vm
  }
}

impl Drop for BudgetGuard<'_> {
  fn drop(&mut self) {
    if let Some(previous) = self.previous.take() {
      self.vm.budget = previous;
    }
  }
}

/// RAII guard returned by [`Vm::push_active_host_hooks_guard`].
///
/// Dropping the guard restores the previous active host hook override (if any).
#[derive(Debug)]
#[must_use = "dropping the guard restores the previous host hooks override; bind it to keep hooks active"]
pub(crate) struct ActiveHostHooksGuard<'vm> {
  vm: &'vm mut Vm,
  previous: Option<*mut (dyn VmHostHooks + 'static)>,
}

impl ops::Deref for ActiveHostHooksGuard<'_> {
  type Target = Vm;

  fn deref(&self) -> &Self::Target {
    &*self.vm
  }
}

impl ops::DerefMut for ActiveHostHooksGuard<'_> {
  fn deref_mut(&mut self) -> &mut Self::Target {
    &mut *self.vm
  }
}

impl Drop for ActiveHostHooksGuard<'_> {
  fn drop(&mut self) {
    self.vm.pop_active_host_hooks(self.previous);
  }
}

/// RAII helper that pushes an [`ExecutionContext`] on creation and pops it on drop.
///
/// This is intended to prevent mismatched `push_execution_context` / `pop_execution_context`
/// sequences when running nested evaluator/host work.
#[derive(Debug)]
#[must_use = "dropping the guard pops the execution context; bind it to keep the context active"]
pub struct ExecutionContextGuard<'vm> {
  vm: &'vm mut Vm,
  ctx: ExecutionContext,
  expected_len: usize,
}

impl<'vm> ExecutionContextGuard<'vm> {
  fn new(vm: &'vm mut Vm, ctx: ExecutionContext) -> Result<Self, VmError> {
    vm.push_execution_context(ctx)?;
    let expected_len = vm.execution_context_stack.len();
    Ok(Self {
      vm,
      ctx,
      expected_len,
    })
  }
}

impl ops::Deref for ExecutionContextGuard<'_> {
  type Target = Vm;

  fn deref(&self) -> &Self::Target {
    &*self.vm
  }
}

impl ops::DerefMut for ExecutionContextGuard<'_> {
  fn deref_mut(&mut self) -> &mut Self::Target {
    &mut *self.vm
  }
}

impl Drop for ExecutionContextGuard<'_> {
  fn drop(&mut self) {
    debug_assert_eq!(
      self.vm.execution_context_stack.len(),
      self.expected_len,
      "ExecutionContextGuard dropped after stack length changed (did you manually pop?)"
    );
    let popped = self.vm.pop_execution_context();
    debug_assert_eq!(
      popped,
      Some(self.ctx),
      "ExecutionContextGuard popped a different execution context than it pushed"
    );
    debug_assert!(
      popped.is_some(),
      "ExecutionContextGuard dropped with empty execution context stack"
    );
  }
}

/// RAII helper that pushes a [`StackFrame`] on creation and pops it on drop.
///
/// This is intended to prevent mismatched `push_frame` / `pop_frame` sequences when running nested
/// evaluator/host work, and ensures VM stack frames are popped even when unwinding through `?`.
#[derive(Debug)]
pub struct VmFrameGuard<'vm> {
  vm: &'vm mut Vm,
  expected_len: usize,
}

impl<'vm> VmFrameGuard<'vm> {
  fn new(vm: &'vm mut Vm, frame: StackFrame) -> Result<Self, VmError> {
    vm.push_frame(frame)?;
    let expected_len = vm.stack.len();
    Ok(Self { vm, expected_len })
  }
}

impl ops::Deref for VmFrameGuard<'_> {
  type Target = Vm;

  fn deref(&self) -> &Self::Target {
    &*self.vm
  }
}

impl ops::DerefMut for VmFrameGuard<'_> {
  fn deref_mut(&mut self) -> &mut Self::Target {
    &mut *self.vm
  }
}

impl Drop for VmFrameGuard<'_> {
  fn drop(&mut self) {
    debug_assert_eq!(
      self.vm.stack.len(),
      self.expected_len,
      "VmFrameGuard dropped after stack length changed (did you manually pop?)"
    );
    debug_assert!(
      !self.vm.stack.is_empty(),
      "VmFrameGuard dropped with empty VM stack"
    );
    self.vm.pop_frame();
  }
}

impl Vm {
  fn erase_host_hooks_lifetime(host: &mut dyn VmHostHooks) -> *mut (dyn VmHostHooks + 'static) {
    let ptr: *mut (dyn VmHostHooks + '_) = host;
    // SAFETY: We only use this pointer while the embedder-provided `host` reference is alive.
    unsafe { mem::transmute::<*mut (dyn VmHostHooks + '_), *mut (dyn VmHostHooks + 'static)>(ptr) }
  }

  pub fn new(options: VmOptions) -> Self {
    let internal = match &options.interrupt_flag {
      Some(flag) => flag.clone(),
      None => Arc::new(AtomicBool::new(false)),
    };
    let external = options.external_interrupt_flag.clone();
    let (interrupt, interrupt_handle) =
      InterruptToken::from_internal_and_external_flags(internal, external);
    let check_time_every = options.check_time_every;
    let unscoped_math_random_state = options.math_random_seed;
    let mut vm = Self {
      options,
      interrupt,
      interrupt_handle,
      // Placeholder; immediately overwritten by `reset_budget_to_default`.
      budget: BudgetState::new(Budget::unlimited(check_time_every)),
      unscoped_math_random_state,
      module_async_evaluation_count: 0,
      next_script_id: 0,
      stack: Vec::new(),
      execution_context_stack: Vec::new(),
      script_or_module_table: Vec::new(),
      native_calls: Vec::new(),
      native_constructs: Vec::new(),
      user_data: None,
      microtasks: MicrotaskQueue::new(),
      host_hooks_override: None,
      ecma_functions: Vec::new(),
      ecma_function_cache: HashMap::new(),
      template_registry: HashMap::new(),
      async_resume_call: None,
      exec_module_load_on_fulfilled_call: None,
      exec_module_load_on_rejected_call: None,
      module_tla_on_fulfilled_call: None,
      module_tla_on_rejected_call: None,
      dynamic_import_eval_on_fulfilled_call: None,
      dynamic_import_eval_on_rejected_call: None,
      module_namespace_getter_call: None,
      async_from_sync_iterator_next_call: None,
      async_from_sync_iterator_return_call: None,
      async_from_sync_iterator_throw_call: None,
      async_from_sync_iterator_unwrap_call: None,
      async_from_sync_iterator_close_call: None,
      async_iterator_close_on_fulfilled_call: None,
      async_iterator_close_on_rejected_call: None,
      next_async_continuation_id: 0,
      async_continuations: HashMap::new(),
      module_graph: None,
      realm_states: HashMap::new(),
      intrinsics: None,
      intrinsics_realm: None,
      #[cfg(test)]
      native_calls_len_override: None,
      #[cfg(test)]
      native_constructs_len_override: None,
    };
    vm.reset_budget_to_default();
    vm
  }

  pub(crate) fn register_realm_state(
    &mut self,
    realm: RealmId,
    intrinsics: Intrinsics,
  ) -> Result<(), VmError> {
    // Ensure we can insert without panicking on allocator OOM.
    if !self.realm_states.contains_key(&realm) {
      self
        .realm_states
        .try_reserve(1)
        .map_err(|_| VmError::OutOfMemory)?;
    }

    // Realm initialization boundary for per-realm PRNG + `[[VarNames]]`.
    self.realm_states.insert(
      realm,
      RealmState {
        intrinsics,
        global_var_names: HashSet::new(),
        math_random_state: self.options.math_random_seed,
      },
    );

    // Treat the most recently-initialized realm as the default for host entry points that run
    // without an execution context.
    self.intrinsics_realm = Some(realm);
    Ok(())
  }

  /// Set the VM's **default realm** (used when no [`ExecutionContext`] is active) to `realm`.
  ///
  /// `vm-js` stores per-realm VM state (intrinsics, tracked global `var`/function names, deterministic
  /// `Math.random()` state, ...) in [`Vm::realm_states`]. Most runtime operations consult the realm
  /// on the active execution context stack (`Vm::current_realm`).
  ///
  /// Some host entry points run without an execution context (for example, module linking/loader
  /// plumbing). In those situations this API allows an embedder to set a default realm for
  /// realm-sensitive operations, and updates the heap's default `%Object.prototype%` accordingly.
  pub fn load_realm_state(
    &mut self,
    heap: &mut Heap,
    realm: RealmId,
  ) -> Result<Option<RealmId>, VmError> {
    let prev = self.intrinsics_realm;
    let state = self
      .realm_states
      .get(&realm)
      .ok_or(VmError::InvariantViolation("unknown realm id"))?;

    self.intrinsics_realm = Some(realm);
    heap.set_default_object_prototype(Some(state.intrinsics.object_prototype()));
    Ok(prev)
  }

  /// Restore a previous default realm returned by [`Vm::load_realm_state`].
  pub fn restore_realm_state(&mut self, heap: &mut Heap, prev: Option<RealmId>) -> Result<(), VmError> {
    match prev {
      Some(id) => {
        let _ = self.load_realm_state(heap, id)?;
      }
      None => {
        // No prior realm: clear realm-dependent VM state.
        self.intrinsics_realm = None;
        heap.set_default_object_prototype(None);
      }
    }
    Ok(())
  }

  /// Returns the intrinsics for `realm` if the VM has previously initialized or loaded that realm.
  ///
  /// This is primarily used by spec operations that need to consult a *different* realm's
  /// intrinsics without switching the VM's active realm state (e.g. `ArraySpeciesCreate`'s
  /// cross-realm `%Array%` check).
  pub(crate) fn intrinsics_for_realm(&self, realm: RealmId) -> Option<Intrinsics> {
    self.realm_states.get(&realm).map(|state| state.intrinsics)
  }

  /// Returns the realm id associated with the VM's currently loaded per-realm state, if any.
  ///
  /// This is distinct from [`Vm::current_realm`], which reflects the realm of the active execution
  /// context stack. Host code may call into the VM without an execution context (e.g. native tests);
  /// in that case, built-ins can treat this as the "current" realm for realm-sensitive operations.
  pub(crate) fn active_realm_state(&self) -> Option<RealmId> {
    self.current_realm().or(self.intrinsics_realm)
  }

  pub(crate) fn global_var_names_contains(&self, name: &str) -> bool {
    let Some(realm) = self.current_realm().or(self.intrinsics_realm) else {
      return false;
    };
    self
      .realm_states
      .get(&realm)
      .is_some_and(|state| state.global_var_names.contains(name))
  }

  pub(crate) fn global_var_names_insert_all<I>(&mut self, names: I) -> Result<(), VmError>
  where
    I: IntoIterator<Item = String>,
  {
    let realm = self
      .current_realm()
      .or(self.intrinsics_realm)
      .ok_or(VmError::InvariantViolation(
        "global var name tracking requires an active realm",
      ))?;
    let state = self
      .realm_states
      .get_mut(&realm)
      .ok_or(VmError::InvariantViolation("unknown realm id"))?;

    let iter = names.into_iter();
    // Best-effort capacity hint to avoid OOM aborts on large scripts.
    let (lower, _) = iter.size_hint();
    if lower != 0 {
      state
        .global_var_names
        .try_reserve(lower)
        .map_err(|_| VmError::OutOfMemory)?;
    }
    for name in iter {
      // Ensure each insert cannot abort on OOM.
      state
        .global_var_names
        .try_reserve(1)
        .map_err(|_| VmError::OutOfMemory)?;
      state.global_var_names.insert(name);
    }
    Ok(())
  }

  pub(crate) fn global_var_names_clear(&mut self) {
    if let Some(realm) = self.current_realm().or(self.intrinsics_realm) {
      if let Some(state) = self.realm_states.get_mut(&realm) {
        state.global_var_names.clear();
      }
    }
  }

  /// Returns the next pseudorandom `u64` output for `Math.random()` and advances the VM's internal
  /// PRNG state.
  ///
  /// This PRNG is **deterministic** and **not** cryptographically secure. Hosts that need
  /// non-deterministic randomness should either:
  /// - seed the VM using [`VmOptions::math_random_seed`], or
  /// - override randomness via [`crate::VmHostHooks::host_math_random_u64`].
  ///
  /// The algorithm is xorshift64* (Marsaglia), chosen for its tiny constant-time implementation.
  pub(crate) fn next_math_random_u64(&mut self) -> u64 {
    // Prefer the current realm if one is active; otherwise fall back to the default realm installed
    // by `Realm::new` / `Vm::load_realm_state`.
    if let Some(realm) = self.current_realm().or(self.intrinsics_realm) {
      if let Some(state) = self.realm_states.get_mut(&realm) {
        // xorshift64* requires a non-zero state. If a host explicitly seeded with 0, fall back to
        // the default constant so `Math.random()` still produces a useful sequence.
        if state.math_random_state == 0 {
          state.math_random_state = 0x243F_6A88_85A3_08D3;
        }
        let mut x = state.math_random_state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        state.math_random_state = x;
        return x.wrapping_mul(0x2545_F491_4F6C_DD1D);
      }
    }

    // xorshift64* requires a non-zero state. If a host explicitly seeded with 0, fall back to the
    // default constant so `Math.random()` still produces a useful sequence.
    if self.unscoped_math_random_state == 0 {
      self.unscoped_math_random_state = 0x243F_6A88_85A3_08D3;
    }
    let mut x = self.unscoped_math_random_state;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    self.unscoped_math_random_state = x;
    x.wrapping_mul(0x2545_F491_4F6C_DD1D)
  }

  /// Implements ECMA-262 `IncrementModuleAsyncEvaluationCount`.
  ///
  /// Returns the old counter value and increments it.
  pub(crate) fn increment_module_async_evaluation_count(&mut self) -> u64 {
    let old = self.module_async_evaluation_count;
    // Prevent debug-overflow panics; wrap is fine (overflow would be unobservable in practice).
    self.module_async_evaluation_count = self.module_async_evaluation_count.wrapping_add(1);
    old
  }

  /// Resets the async module evaluation counter.
  ///
  /// Spec note: `[[ModuleAsyncEvaluationCount]]` may be unobservably reset to 0 whenever there are
  /// no pending async modules.
  #[allow(dead_code)]
  pub(crate) fn reset_module_async_evaluation_count(&mut self) {
    self.module_async_evaluation_count = 0;
  }

  /// Returns the VM-owned microtask queue.
  ///
  /// Promise built-ins enqueue Promise jobs onto this queue.
  #[inline]
  pub fn microtask_queue(&self) -> &MicrotaskQueue {
    &self.microtasks
  }

  /// Borrows the VM-owned microtask queue mutably.
  #[inline]
  pub fn microtask_queue_mut(&mut self) -> &mut MicrotaskQueue {
    &mut self.microtasks
  }

  /// Discard all queued Promise jobs in the VM-owned microtask queue without running them.
  ///
  /// This unregisters any persistent roots held by queued jobs.
  ///
  /// In addition, this tears down any in-progress async continuations stored in the VM.
  ///
  /// Async continuations (from `async` functions and async module evaluation) are resumed exclusively
  /// via Promise jobs. If an embedding abandons/discards the microtask queue while intending to
  /// reuse the heap, leaving suspended continuations in the VM would leak their persistent roots.
  pub fn teardown_microtasks(&mut self, heap: &mut Heap) {
    #[cfg(debug_assertions)]
    let root_stack_len_at_entry = heap.root_stack.len();
    #[cfg(debug_assertions)]
    let env_root_stack_len_at_entry = heap.env_root_stack.len();

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

    // Discard queued jobs first so their persistent roots are unregistered even if we later hit an
    // error while tearing down async continuations.
    {
      let mut ctx = TeardownCtx { heap: &mut *heap };
      self.microtasks.teardown(&mut ctx);
    }

    if !self.async_continuations.is_empty() {
      // Tearing down async continuations is a best-effort cleanup path for embeddings that abort
      // execution and will not resume the event loop. This does *not* settle the associated
      // Promises; it only unregisters the continuation's persistent roots so the heap can be reused
      // safely.
      let continuations = mem::take(&mut self.async_continuations);
      let mut scope = heap.scope();
      for (_, cont) in continuations {
        match cont {
          VmAsyncContinuation::Ast(cont) => {
            crate::exec::async_teardown_continuation(&mut scope, cont)
          }
          VmAsyncContinuation::Hir(cont) => {
            crate::hir_exec::hir_async_teardown_continuation(&mut scope, cont)
          }
        }
      }
    }

    // Ensure this teardown path does not leave behind leaked continuation frames or stack roots.
    // This is debug-only to avoid surprising host behaviour (e.g. embeddings that keep explicit
    // stack roots across teardown).
    #[cfg(debug_assertions)]
    {
      debug_assert!(
        self.microtasks.is_empty(),
        "expected microtask queue to be empty after Vm::teardown_microtasks"
      );
      debug_assert!(
        self.async_continuations.is_empty(),
        "expected async continuations to be empty after Vm::teardown_microtasks"
      );
      debug_assert_eq!(
        heap.root_stack.len(),
        root_stack_len_at_entry,
        "Vm::teardown_microtasks leaked value stack roots"
      );
      debug_assert_eq!(
        heap.env_root_stack.len(),
        env_root_stack_len_at_entry,
        "Vm::teardown_microtasks leaked env stack roots"
      );
    }
  }

  /// Tears down VM-owned state for `realm`.
  ///
  /// This is intended for long-lived embeddings that create and tear down multiple realms while
  /// reusing a single [`Vm`] and [`Heap`]. In addition to a realm's own heap-level persistent roots
  /// (see [`crate::Realm::teardown`]), the VM may also own per-realm state such as:
  /// - cached intrinsic handles,
  /// - tracked global `var`/function names,
  /// - deterministic `Math.random()` PRNG state,
  /// - and cached template objects (tagged template literal `GetTemplateObject` cache).
  ///
  /// This method cleans up that VM-owned state for the provided [`RealmId`], including unregistering
  /// any persistent GC roots held by the VM (notably template objects).
  ///
  /// This method is **idempotent**: calling it multiple times for the same realm is safe.
  pub fn teardown_realm(&mut self, heap: &mut Heap, realm: RealmId) {
    // Remove cached template objects for this realm, unregistering their persistent roots so they
    // are eligible for collection.
    self.template_registry.retain(|key, entry| {
      if key.realm != realm {
        return true;
      }
      heap.remove_root(entry.root);
      false
    });

    self.realm_states.remove(&realm);
    if self.intrinsics_realm == Some(realm) {
      self.intrinsics_realm = None;
    }
  }

  /// Returns the number of in-progress async continuations currently stored in the VM.
  ///
  /// Async continuations are used to resume `async` function bodies and async module evaluation
  /// (top-level await) across microtasks.
  #[inline]
  pub fn async_continuation_count(&self) -> usize {
    self.async_continuations.len()
  }

  pub(crate) fn reserve_async_continuations(&mut self, additional: usize) -> Result<(), VmError> {
    self
      .async_continuations
      .try_reserve(additional)
      .map_err(|_| VmError::OutOfMemory)
  }

  pub(crate) fn async_resume_call_id(&mut self) -> Result<NativeFunctionId, VmError> {
    if let Some(id) = self.async_resume_call {
      return Ok(id);
    }
    let id = self.register_native_call(crate::exec::async_resume_call)?;
    self.async_resume_call = Some(id);
    Ok(id)
  }

  pub(crate) fn exec_module_load_on_fulfilled_call_id(
    &mut self,
  ) -> Result<NativeFunctionId, VmError> {
    if let Some(id) = self.exec_module_load_on_fulfilled_call {
      return Ok(id);
    }
    let id = self.register_native_call(crate::exec::exec_module_load_on_fulfilled)?;
    self.exec_module_load_on_fulfilled_call = Some(id);
    Ok(id)
  }

  pub(crate) fn exec_module_load_on_rejected_call_id(
    &mut self,
  ) -> Result<NativeFunctionId, VmError> {
    if let Some(id) = self.exec_module_load_on_rejected_call {
      return Ok(id);
    }
    let id = self.register_native_call(crate::exec::exec_module_load_on_rejected)?;
    self.exec_module_load_on_rejected_call = Some(id);
    Ok(id)
  }

  pub(crate) fn module_tla_on_fulfilled_call_id(&mut self) -> Result<NativeFunctionId, VmError> {
    if let Some(id) = self.module_tla_on_fulfilled_call {
      return Ok(id);
    }
    let id = self.register_native_call(crate::module_graph::module_tla_on_fulfilled)?;
    self.module_tla_on_fulfilled_call = Some(id);
    Ok(id)
  }

  pub(crate) fn module_tla_on_rejected_call_id(&mut self) -> Result<NativeFunctionId, VmError> {
    if let Some(id) = self.module_tla_on_rejected_call {
      return Ok(id);
    }
    let id = self.register_native_call(crate::module_graph::module_tla_on_rejected)?;
    self.module_tla_on_rejected_call = Some(id);
    Ok(id)
  }
  pub(crate) fn dynamic_import_eval_on_fulfilled_call_id(
    &mut self,
  ) -> Result<NativeFunctionId, VmError> {
    if let Some(id) = self.dynamic_import_eval_on_fulfilled_call {
      return Ok(id);
    }
    let id = self.register_native_call(crate::module_loading::dynamic_import_eval_on_fulfilled)?;
    self.dynamic_import_eval_on_fulfilled_call = Some(id);
    Ok(id)
  }

  pub(crate) fn dynamic_import_eval_on_rejected_call_id(
    &mut self,
  ) -> Result<NativeFunctionId, VmError> {
    if let Some(id) = self.dynamic_import_eval_on_rejected_call {
      return Ok(id);
    }
    let id = self.register_native_call(crate::module_loading::dynamic_import_eval_on_rejected)?;
    self.dynamic_import_eval_on_rejected_call = Some(id);
    Ok(id)
  }

  pub(crate) fn module_namespace_getter_call_id(&mut self) -> Result<NativeFunctionId, VmError> {
    if let Some(id) = self.module_namespace_getter_call {
      return Ok(id);
    }
    let id = self.register_native_call(crate::module_graph::module_namespace_getter)?;
    self.module_namespace_getter_call = Some(id);
    Ok(id)
  }

  pub(crate) fn async_from_sync_iterator_next_call_id(&mut self) -> Result<NativeFunctionId, VmError> {
    if let Some(id) = self.async_from_sync_iterator_next_call {
      return Ok(id);
    }
    let id = self.register_native_call(crate::iterator::async_from_sync_iterator_next_call)?;
    self.async_from_sync_iterator_next_call = Some(id);
    Ok(id)
  }

  pub(crate) fn async_from_sync_iterator_return_call_id(&mut self) -> Result<NativeFunctionId, VmError> {
    if let Some(id) = self.async_from_sync_iterator_return_call {
      return Ok(id);
    }
    let id = self.register_native_call(crate::iterator::async_from_sync_iterator_return_call)?;
    self.async_from_sync_iterator_return_call = Some(id);
    Ok(id)
  }

  pub(crate) fn async_from_sync_iterator_throw_call_id(&mut self) -> Result<NativeFunctionId, VmError> {
    if let Some(id) = self.async_from_sync_iterator_throw_call {
      return Ok(id);
    }
    let id = self.register_native_call(crate::iterator::async_from_sync_iterator_throw_call)?;
    self.async_from_sync_iterator_throw_call = Some(id);
    Ok(id)
  }

  pub(crate) fn async_from_sync_iterator_unwrap_call_id(
    &mut self,
  ) -> Result<NativeFunctionId, VmError> {
    if let Some(id) = self.async_from_sync_iterator_unwrap_call {
      return Ok(id);
    }
    let id = self.register_native_call(crate::iterator::async_from_sync_iterator_unwrap_call)?;
    self.async_from_sync_iterator_unwrap_call = Some(id);
    Ok(id)
  }

  pub(crate) fn async_from_sync_iterator_close_call_id(
    &mut self,
  ) -> Result<NativeFunctionId, VmError> {
    if let Some(id) = self.async_from_sync_iterator_close_call {
      return Ok(id);
    }
    let id = self.register_native_call(crate::iterator::async_from_sync_iterator_close_call)?;
    self.async_from_sync_iterator_close_call = Some(id);
    Ok(id)
  }

  pub(crate) fn async_iterator_close_on_fulfilled_call_id(&mut self) -> Result<NativeFunctionId, VmError> {
    if let Some(id) = self.async_iterator_close_on_fulfilled_call {
      return Ok(id);
    }
    let id = self.register_native_call(crate::iterator::async_iterator_close_on_fulfilled_call)?;
    self.async_iterator_close_on_fulfilled_call = Some(id);
    Ok(id)
  }

  pub(crate) fn async_iterator_close_on_rejected_call_id(&mut self) -> Result<NativeFunctionId, VmError> {
    if let Some(id) = self.async_iterator_close_on_rejected_call {
      return Ok(id);
    }
    let id = self.register_native_call(crate::iterator::async_iterator_close_on_rejected_call)?;
    self.async_iterator_close_on_rejected_call = Some(id);
    Ok(id)
  }

  #[cfg(test)]
  pub(crate) fn native_call_count(&self) -> usize {
    self.native_calls.len()
  }

  pub(crate) fn insert_async_continuation(
    &mut self,
    cont: VmAsyncContinuation,
  ) -> Result<u32, VmError> {
    self
      .async_continuations
      .try_reserve(1)
      .map_err(|_| VmError::OutOfMemory)?;

    let id = self.next_async_continuation_id;
    self.next_async_continuation_id = self.next_async_continuation_id.wrapping_add(1);
    self.async_continuations.insert(id, cont);
    Ok(id)
  }

  pub(crate) fn insert_async_continuation_reserved(&mut self, cont: VmAsyncContinuation) -> u32 {
    let id = self.next_async_continuation_id;
    self.next_async_continuation_id = self.next_async_continuation_id.wrapping_add(1);
    // Callers are expected to call `reserve_async_continuations` before inserting so this does not
    // allocate (and therefore cannot abort the process). See also
    // `insert_generator_continuation_reserved`.
    self.async_continuations.insert(id, cont);
    id
  }

  pub(crate) fn take_async_continuation(&mut self, id: u32) -> Option<VmAsyncContinuation> {
    self.async_continuations.remove(&id)
  }

  pub(crate) fn replace_async_continuation(
    &mut self,
    id: u32,
    cont: VmAsyncContinuation,
  ) -> Result<(), VmError> {
    self
      .async_continuations
      .try_reserve(1)
      .map_err(|_| VmError::OutOfMemory)?;
    self.async_continuations.insert(id, cont);
    Ok(())
  }

  pub(crate) fn reserve_hir_async_continuations(&mut self, additional: usize) -> Result<(), VmError> {
    // HIR and AST continuations share a single ID space and backing store.
    self.reserve_async_continuations(additional)
  }

  pub(crate) fn insert_hir_async_continuation_reserved(&mut self, cont: HirAsyncContinuation) -> u32 {
    self.insert_async_continuation_reserved(VmAsyncContinuation::Hir(cont))
  }

  pub(crate) fn take_hir_async_continuation(&mut self, id: u32) -> Option<HirAsyncContinuation> {
    match self.take_async_continuation(id) {
      Some(VmAsyncContinuation::Hir(cont)) => Some(cont),
      Some(other) => {
        // Mismatched continuation kind (e.g. wrong resume callback): preserve the entry.
        self.async_continuations.insert(id, other);
        None
      }
      None => None,
    }
  }

  pub(crate) fn replace_hir_async_continuation(
    &mut self,
    id: u32,
    cont: HirAsyncContinuation,
  ) -> Result<(), VmError> {
    self.replace_async_continuation(id, VmAsyncContinuation::Hir(cont))
  }

  /// Removes an async continuation from the VM and tears down all of its persistent roots.
  ///
  /// This is used by embeddings that abort in-progress async module evaluation (top-level await),
  /// ensuring we do not leak rooted async state when the host will not drive the event loop.
  pub(crate) fn abort_async_continuation(&mut self, heap: &mut Heap, id: u32) {
    let Some(cont) = self.take_async_continuation(id) else {
      return;
    };
    let mut scope = heap.scope();
    match cont {
      VmAsyncContinuation::Ast(cont) => crate::exec::async_teardown_continuation(&mut scope, cont),
      VmAsyncContinuation::Hir(cont) => crate::hir_exec::hir_async_teardown_continuation(&mut scope, cont),
    }
  }

  /// Temporarily override the host hook implementation used by [`Vm::call`] / [`Vm::construct`].
  ///
  /// This is primarily used by embedding entry points that need `vm-js` evaluation to enqueue
  /// Promise jobs onto a host-owned microtask queue (HTML `HostEnqueuePromiseJob`) instead of the
  /// VM-owned [`MicrotaskQueue`].
  pub fn with_host_hooks_override<R>(
    &mut self,
    host: &mut dyn VmHostHooks,
    f: impl FnOnce(&mut Vm) -> R,
  ) -> R {
    let mut guard = self.push_active_host_hooks_guard(host);
    f(&mut *guard)
  }

  /// Returns the currently-active host hook implementation pointer, if one is installed.
  ///
  /// `vm-js` stores the active hooks as a raw pointer so call/construct entry points can borrow-split
  /// `&mut Vm` and `&mut dyn VmHostHooks` (and so nested calls can reuse the same hooks).
  ///
  /// This accessor exists for embedder-side glue that needs to recover the active hooks (and any
  /// payload exposed via [`VmHostHooks::as_any_mut`]) from contexts that only have access to
  /// `&mut Vm` (for example, WebIDL host dispatch).
  ///
  /// # Safety
  ///
  /// The returned pointer is only valid during the dynamic extent of a VM entry point that
  /// installed a hooks override (e.g. [`Vm::call_with_host_and_hooks`], [`Vm::construct_with_host_and_hooks`],
  /// or [`Vm::with_host_hooks_override`]). Do not dereference it outside that extent.
  #[inline]
  pub fn active_host_hooks_ptr(&self) -> Option<*mut (dyn VmHostHooks + 'static)> {
    self.host_hooks_override
  }

  /// Returns the currently active host hook implementation, if one is installed.
  ///
  /// This is primarily useful for embeddings that store per-call dynamic context inside their
  /// [`VmHostHooks`] implementation. For example, FastRender stores an erased pointer to the active
  /// HTML-like event loop so `setTimeout` / `queueMicrotask` can schedule work without relying on a
  /// thread-local “current event loop”.
  ///
  /// The returned reference is only valid while the VM is executing under a host hooks override
  /// (for example inside [`Vm::call_with_host_and_hooks`] or [`Vm::with_host_hooks_override`]).
  #[inline]
  pub fn active_host_hooks_mut(&mut self) -> Option<&mut dyn VmHostHooks> {
    let Some(ptr) = self.host_hooks_override else {
      return None;
    };
    // SAFETY: `host_hooks_override` is only set while a host hooks implementation is mutably
    // borrowed by an embedder entry point, and is treated as a reborrow of that `&mut dyn
    // VmHostHooks`.
    Some(unsafe { &mut *ptr })
  }

  /// Performs a microtask checkpoint, draining the VM's microtask queue.
  ///
  /// This is a convenience wrapper that uses a dummy host context (`()`), preserving
  /// source-compatibility with earlier versions of `vm-js`.
  ///
  /// Embeddings that need queued Promise jobs to run with access to embedder state should prefer
  /// [`Vm::perform_microtask_checkpoint_with_host`].
  #[inline]
  pub fn perform_microtask_checkpoint(&mut self, heap: &mut Heap) -> Result<(), VmError> {
    let mut dummy_host = ();
    self.perform_microtask_checkpoint_with_host(&mut dummy_host, heap)
  }

  /// Performs a microtask checkpoint, draining the VM's microtask queue using an explicit embedder
  /// host context.
  ///
  /// Any calls/constructs performed by queued jobs (for example `Promise.then` callbacks) will be
  /// executed with the provided `host` and therefore can access embedder state in native call and
  /// construct handlers.
  ///
  /// ## Errors
  ///
  /// This method mirrors HTML microtask checkpoint semantics for ordinary job failures (exceptions,
  /// `TypeError`, etc): it captures the **first non-termination error** but continues draining the
  /// queue so queued job roots are cleaned up.
  ///
  /// [`VmError::Termination`] is different: it represents a non-catchable termination condition
  /// (fuel exhausted, deadline exceeded, interrupt). Termination is treated as a
  /// **hard stop**: once a job returns `Err(VmError::Termination(..))`, the checkpoint stops
  /// executing any further jobs, discards all remaining queued jobs to clean up persistent roots,
  /// and returns the termination error (even if earlier jobs already failed with a non-termination
  /// error).
  ///
  /// [`VmError::OutOfMemory`] is also treated as a hard stop: it represents a fatal resource
  /// condition. Continuing to run further jobs after an OOM is unlikely to succeed and risks
  /// hitting infallible allocation paths. The checkpoint therefore discards all remaining queued
  /// jobs (cleaning up their persistent roots) and returns `OutOfMemory`.
  pub fn perform_microtask_checkpoint_with_host(
    &mut self,
    host: &mut dyn VmHost,
    heap: &mut Heap,
  ) -> Result<(), VmError> {
    struct Ctx<'a> {
      vm: &'a mut Vm,
      host: &'a mut dyn VmHost,
      heap: &'a mut Heap,
    }

    impl VmJobContext for Ctx<'_> {
      fn call(
        &mut self,
        hooks: &mut dyn VmHostHooks,
        callee: Value,
        this: Value,
        args: &[Value],
      ) -> Result<Value, VmError> {
        let mut scope = self.heap.scope();
        self
          .vm
          .call_with_host_and_hooks(&mut *self.host, &mut scope, hooks, callee, this, args)
      }

      fn construct(
        &mut self,
        hooks: &mut dyn VmHostHooks,
        callee: Value,
        args: &[Value],
        new_target: Value,
      ) -> Result<Value, VmError> {
        let mut scope = self.heap.scope();
        self.vm.construct_with_host_and_hooks(
          &mut *self.host,
          &mut scope,
          hooks,
          callee,
          args,
          new_target,
        )
      }

      fn add_root(&mut self, value: Value) -> Result<RootId, VmError> {
        self.heap.add_root(value)
      }

      fn remove_root(&mut self, id: RootId) {
        self.heap.remove_root(id)
      }

      fn coerce_error_to_throw_with_stack(&mut self, err: VmError) -> VmError {
        let mut scope = self.heap.scope();
        coerce_error_to_throw_with_stack(&*self.vm, &mut scope, err)
      }
    }

    struct LocalHost {
      pending: VecDeque<(Option<RealmId>, crate::Job)>,
    }

    impl LocalHost {
      fn new() -> Self {
        Self {
          pending: VecDeque::new(),
        }
      }

      fn drain_into(
        &mut self,
        ctx: &mut dyn VmJobContext,
        queue: &mut MicrotaskQueue,
      ) -> Result<(), VmError> {
        while let Some((realm, job)) = self.pending.pop_front() {
          if let Err(err) = queue.host_enqueue_promise_job_fallible(ctx, job, realm) {
            // Best-effort cleanup: if we can't enqueue remaining jobs, discard them so we don't leak
            // persistent roots.
            while let Some((_realm, job)) = self.pending.pop_front() {
              job.discard(ctx);
            }
            return Err(err);
          }
        }
        Ok(())
      }
    }

    impl VmHostHooks for LocalHost {
      fn host_enqueue_promise_job(&mut self, job: crate::Job, realm: Option<RealmId>) {
        self.pending.push_back((realm, job));
      }

      fn host_enqueue_promise_job_fallible(
        &mut self,
        ctx: &mut dyn VmJobContext,
        job: crate::Job,
        realm: Option<RealmId>,
      ) -> Result<(), VmError> {
        // `VecDeque::push_back` aborts the process on allocator OOM; reserve fallibly so we can
        // surface a recoverable `VmError::OutOfMemory`.
        if self.pending.try_reserve(1).is_err() {
          // If we can't enqueue, discard the job so we don't leak any persistent roots it owns.
          job.discard(ctx);
          return Err(VmError::OutOfMemory);
        }
        // `try_reserve(1)` guarantees `push_back` won't grow/reallocate the buffer.
        self.pending.push_back((realm, job));
        Ok(())
      }

      fn host_call_job_callback(
        &mut self,
        ctx: &mut dyn VmJobContext,
        callback: &crate::JobCallback,
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

      fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
      }
    }

    if !self.microtasks.begin_checkpoint() {
      return Ok(());
    }

    // Keep running jobs until the queue becomes empty, capturing the first non-termination error
    // but continuing to drain so we don't leak job roots.
    //
    // Termination errors are treated as a hard stop: we stop running further jobs and discard the
    // remainder of the queue (including any jobs enqueued by the failing job).
    let mut first_err: Option<VmError> = None;
    let mut termination_err: Option<VmError> = None;

    loop {
      // FinalizationRegistry cleanup jobs are discovered during GC, but are enqueued outside of GC
      // to avoid allocating during collection.
      //
      // Ensure a microtask checkpoint will also run these cleanup jobs even when the Promise job
      // queue is currently empty.
      if self.microtasks.is_empty() {
        let mut fr_hooks = LocalHost::new();
        let enqueue_result = heap.enqueue_finalization_registry_cleanup_jobs(self, &mut fr_hooks);
        let drain_result = {
          struct DrainCtx<'a> {
            heap: &'a mut Heap,
          }
          impl VmJobContext for DrainCtx<'_> {
            fn call(
              &mut self,
              _hooks: &mut dyn VmHostHooks,
              _callee: Value,
              _this: Value,
              _args: &[Value],
            ) -> Result<Value, VmError> {
              Err(VmError::Unimplemented("DrainCtx::call"))
            }

            fn construct(
              &mut self,
              _hooks: &mut dyn VmHostHooks,
              _callee: Value,
              _args: &[Value],
              _new_target: Value,
            ) -> Result<Value, VmError> {
              Err(VmError::Unimplemented("DrainCtx::construct"))
            }

            fn add_root(&mut self, value: Value) -> Result<RootId, VmError> {
              self.heap.add_root(value)
            }

            fn remove_root(&mut self, id: RootId) {
              self.heap.remove_root(id)
            }
          }

          let mut ctx = DrainCtx { heap };
          fr_hooks.drain_into(&mut ctx, &mut self.microtasks)
        };

        if let Err(err) = drain_result {
          // Treat enqueue/transfer OOM as a hard stop: discard remaining jobs so we don't leak
          // persistent roots.
          termination_err = Some(err);
          break;
        }

        match enqueue_result {
          Ok(()) => {}
          Err(e @ (VmError::Termination(_) | VmError::OutOfMemory)) => {
            termination_err = Some(e);
            break;
          }
          Err(e) => {
            if first_err.is_none() {
              first_err = Some(e);
            }
          }
        }
        if self.microtasks.is_empty() {
          break;
        }
      }

      let Some((_realm, job)) = self.microtasks.pop_front() else {
        continue;
      };

      let job_result = {
        let mut ctx = Ctx {
          vm: self,
          host,
          heap,
        };
        let mut hooks = LocalHost::new();

        let job_result = job.run(&mut ctx, &mut hooks);

        // Some job types may schedule new Promise jobs via `VmHostHooks`; enqueue them into the VM's
        // microtask queue before proceeding (or before discarding the remaining queue on
        // termination).
        // Borrow-split `ctx` from `ctx.vm.microtasks` so we can pass both as mutable references.
        // `VmJobContext` methods may call back into `ctx.vm`, so borrowing the queue directly would
        // conflict with the `&mut ctx` borrow.
        let drain_result = {
          let mut microtasks = std::mem::take(&mut ctx.vm.microtasks);
          let drain_result = hooks.drain_into(&mut ctx, &mut microtasks);
          ctx.vm.microtasks = microtasks;
          drain_result
        };
        match drain_result {
          Ok(()) => job_result,
          Err(e) => Err(e),
        }
      };

      match job_result {
        Ok(()) => {}
        Err(e @ (VmError::Termination(_) | VmError::OutOfMemory)) => {
          termination_err = Some(e);
          break;
        }
        Err(e) => {
          if first_err.is_none() {
            first_err = Some(e);
          }
        }
      }
    }

    if termination_err.is_some() {
      // A termination condition was observed. Discard all remaining queued jobs so we don't run
      // arbitrary host-side closures after termination, and so we don't leak any persistent roots.
      //
      // Note: jobs enqueued by the failing job into `LocalHost.pending` are drained into
      // `self.microtasks` above before we reach this point.
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

      let mut ctx = TeardownCtx { heap };
      self.microtasks.teardown(&mut ctx);
    }

    self.microtasks.end_checkpoint();

    match termination_err {
      Some(e) => Err(e),
      None => match first_err {
        None => Ok(()),
        Some(e) => Err(e),
      },
    }
  }

  /// Replace the VM's unscoped intrinsics fallback.
  ///
  /// In normal operation, realms are created via [`crate::Realm::new`], which registers per-realm
  /// intrinsics in [`Vm::realm_states`]. Most runtime operations select intrinsics based on the
  /// current execution context (`Vm::current_realm`).
  ///
  /// This API exists for low-level embeddings that want to install intrinsics without associating
  /// them with a concrete [`RealmId`]. When a realm is active, its per-realm intrinsics take
  /// precedence.
  pub fn set_intrinsics(&mut self, intrinsics: Intrinsics) {
    self.intrinsics = Some(intrinsics);
    // Without an explicit realm id, we cannot safely associate these intrinsics with a specific
    // realm for teardown purposes.
    self.intrinsics_realm = None;
  }

  pub(crate) fn set_intrinsics_for_realm(&mut self, realm: RealmId, intrinsics: Intrinsics) {
    if !self.realm_states.contains_key(&realm) {
      // Best-effort: if we cannot reserve, leave intrinsics unchanged.
      if self.realm_states.try_reserve(1).is_ok() {
        self.realm_states.insert(
          realm,
          RealmState {
            intrinsics,
            global_var_names: HashSet::new(),
            math_random_state: self.options.math_random_seed,
          },
        );
      }
    }
    if let Some(state) = self.realm_states.get_mut(&realm) {
      state.intrinsics = intrinsics;
      // Intrinsics are installed per realm. Treat this as the realm initialization boundary for
      // `Math.random()` and reset the per-realm PRNG state.
      state.math_random_state = self.options.math_random_seed;
    }
    self.intrinsics_realm = Some(realm);
  }

  /// Takes ownership of the current realm's global `var`/function declaration name set.
  ///
  /// This models the GlobalEnvironmentRecord internal `[[VarNames]]` list for the current realm
  /// (as determined by [`Vm::current_realm`] or [`Vm::intrinsics_realm`]).
  pub fn take_global_var_names(&mut self) -> HashSet<String> {
    let Some(realm) = self.current_realm().or(self.intrinsics_realm) else {
      return HashSet::new();
    };
    self
      .realm_states
      .get_mut(&realm)
      .map(|state| mem::take(&mut state.global_var_names))
      .unwrap_or_default()
  }

  /// Replace the current realm's global `var`/function declaration name set.
  pub fn set_global_var_names(&mut self, names: HashSet<String>) {
    if let Some(realm) = self.current_realm().or(self.intrinsics_realm) {
      if let Some(state) = self.realm_states.get_mut(&realm) {
        state.global_var_names = names;
      }
    }
  }

  /// Returns the current realm's deterministic `Math.random()` state.
  pub fn math_random_state(&self) -> u64 {
    let Some(realm) = self.current_realm().or(self.intrinsics_realm) else {
      return self.unscoped_math_random_state;
    };
    self
      .realm_states
      .get(&realm)
      .map(|state| state.math_random_state)
      .unwrap_or(self.unscoped_math_random_state)
  }

  /// Replace the current realm's deterministic `Math.random()` state.
  pub fn set_math_random_state(&mut self, state: u64) {
    if let Some(realm) = self.current_realm().or(self.intrinsics_realm) {
      if let Some(realm_state) = self.realm_states.get_mut(&realm) {
        realm_state.math_random_state = state;
        return;
      }
    }
    self.unscoped_math_random_state = state;
  }

  /// Returns the VM's initialized intrinsics, if any.
  ///
  /// Intrinsics are installed when creating a [`crate::Realm`]. Some host operations (e.g. WebIDL
  /// conversions or native builtins) may require access to well-known symbols or prototypes, and
  /// should treat `None` as "realm not initialized".
  pub fn intrinsics(&self) -> Option<Intrinsics> {
    let realm = self.current_realm().or(self.intrinsics_realm);
    if let Some(realm) = realm {
      if let Some(state) = self.realm_states.get(&realm) {
        return Some(state.intrinsics);
      }
    }
    self.intrinsics
  }

  pub fn interrupt_handle(&self) -> InterruptHandle {
    self.interrupt_handle.clone()
  }

  /// Attach an embedding-owned [`ModuleGraph`] to this VM.
  ///
  /// This is used by the AST interpreter (`exec.rs`) to implement dynamic `import()` from nested
  /// ECMAScript function calls and Promise jobs, where an explicit `&mut ModuleGraph` is not readily
  /// available.
  ///
  /// See also [`Vm::module_graph_ptr`].
  ///
  /// For end-to-end module embedding guidance (static graph loading, dynamic `import()`, top-level
  /// `await`, and lifetime requirements for this pointer), see [`crate::docs::modules`].
  pub fn set_module_graph(&mut self, graph: &mut ModuleGraph) {
    self.module_graph = Some(graph as *mut ModuleGraph);
  }

  /// Clear any attached [`ModuleGraph`].
  pub fn clear_module_graph(&mut self) {
    self.module_graph = None;
  }

  /// Returns the raw pointer to the attached [`ModuleGraph`], if any.
  ///
  /// This is intentionally a raw pointer to avoid borrowing `&mut Vm` for the duration of module
  /// graph access (the module graph is embedding-owned, not VM-owned).
  ///
  /// See [`Vm::set_module_graph`] and [`crate::docs::modules`].
  pub fn module_graph_ptr(&self) -> Option<*mut ModuleGraph> {
    self.module_graph
  }

  /// Clear the interrupt flag back to `false`.
  pub fn reset_interrupt(&self) {
    self.interrupt_handle.reset();
  }

  /// Attach arbitrary embedding state to this VM.
  ///
  /// Native (host-implemented) call/construct handlers receive both:
  /// - `&mut Vm` (engine state), and
  /// - `&mut dyn VmHost` (embedder-provided host context).
  ///
  /// Most host embeddings should thread shared state (DOM, event loop, wrapper caches, etc.) via
  /// [`VmHost`]. `Vm::user_data` is provided as a convenience for state that is logically owned by
  /// the VM itself, or when a host does not have access to a host-context object.
  ///
  /// The stored value can be downcast via [`Vm::user_data`] / [`Vm::user_data_mut`].
  pub fn set_user_data<T: Any>(&mut self, data: T) {
    // Avoid aborting the process on allocator OOM. `set_user_data` cannot surface an error, so
    // treat allocation failure as a best-effort no-op and leave any existing user data in place.
    if let Ok(boxed) = crate::fallible_alloc::box_try_new_vm(data) {
      self.user_data = Some(boxed);
    }
  }

  /// Borrow the embedded user data if it is of type `T`.
  pub fn user_data<T: Any>(&self) -> Option<&T> {
    self.user_data.as_deref()?.downcast_ref::<T>()
  }

  /// Mutably borrow the embedded user data if it is of type `T`.
  pub fn user_data_mut<T: Any>(&mut self) -> Option<&mut T> {
    self.user_data.as_deref_mut()?.downcast_mut::<T>()
  }

  /// Take ownership of the embedded user data if it is of type `T`.
  ///
  /// If the stored value exists but is not of type `T`, returns `None` and leaves the user data
  /// untouched.
  pub fn take_user_data<T: Any>(&mut self) -> Option<T> {
    let user_data = self.user_data.take()?;
    match user_data.downcast::<T>() {
      Ok(data) => Some(*data),
      Err(original) => {
        self.user_data = Some(original);
        None
      }
    }
  }

  /// Returns the current execution budget (including remaining fuel/deadline).
  #[inline]
  pub fn budget(&self) -> Budget {
    self.budget.budget.clone()
  }

  pub fn set_budget(&mut self, budget: Budget) {
    self.budget = BudgetState::new(budget);
  }

  /// Replace the current budget state and return an opaque token for restoring it later.
  pub fn swap_budget_state(&mut self, budget: Budget) -> BudgetStateToken {
    let previous = mem::replace(&mut self.budget, BudgetState::new(budget));
    BudgetStateToken { state: previous }
  }

  /// Restore a previously saved budget state.
  pub fn restore_budget_state(&mut self, token: BudgetStateToken) {
    self.budget = token.state;
  }

  /// Reset this VM's execution budget to its construction defaults.
  ///
  /// This is intended for long-lived VM embeddings: call once per task/script/job to apply fresh
  /// fuel/deadline limits relative to "now".
  pub fn reset_budget_to_default(&mut self) {
    let deadline = self
      .options
      .default_deadline
      .and_then(|duration| Instant::now().checked_add(duration));
    let budget = Budget {
      fuel: self.options.default_fuel,
      deadline,
      check_time_every: self.options.check_time_every,
    };
    self.set_budget(budget);
  }

  /// Temporarily replace the VM budget, restoring the previous budget when the returned guard is
  /// dropped.
  ///
  /// The previous budget's tick counter is also restored on drop so nested budget scopes do not
  /// affect each other's deadline checking cadence.
  pub fn push_budget(&mut self, budget: Budget) -> BudgetGuard<'_> {
    let previous = mem::replace(&mut self.budget, BudgetState::new(budget));
    BudgetGuard {
      vm: self,
      previous: Some(previous),
    }
  }

  pub fn register_native_call(&mut self, f: NativeCall) -> Result<NativeFunctionId, VmError> {
    let len = self.native_calls.len();
    #[cfg(test)]
    let len = self.native_calls_len_override.unwrap_or(len);
    let idx = u32::try_from(len)
      .map_err(|_| VmError::LimitExceeded("too many native call handlers registered"))?;

    // Fallible growth so hostile/buggy embeddings can't abort the process on allocator OOM.
    self
      .native_calls
      .try_reserve(1)
      .map_err(|_| VmError::OutOfMemory)?;
    self.native_calls.push(f);
    Ok(NativeFunctionId(idx))
  }

  pub fn register_native_construct(
    &mut self,
    f: NativeConstruct,
  ) -> Result<NativeConstructId, VmError> {
    let len = self.native_constructs.len();
    #[cfg(test)]
    let len = self.native_constructs_len_override.unwrap_or(len);
    let idx = u32::try_from(len)
      .map_err(|_| VmError::LimitExceeded("too many native construct handlers registered"))?;

    self
      .native_constructs
      .try_reserve(1)
      .map_err(|_| VmError::OutOfMemory)?;
    self.native_constructs.push(f);
    Ok(NativeConstructId(idx))
  }

  pub(crate) fn register_ecma_function(
    &mut self,
    source: Arc<SourceText>,
    span_start: u32,
    span_end: u32,
    kind: EcmaFunctionKind,
  ) -> Result<EcmaFunctionId, VmError> {
    let key = EcmaFunctionKey {
      source: source.cache_key_ptr(),
      span_start,
      span_end,
      kind,
    };
    if let Some(id) = self.ecma_function_cache.get(&key).copied() {
      return Ok(id);
    }

    let idx = u32::try_from(self.ecma_functions.len())
      .map_err(|_| VmError::LimitExceeded("too many ECMAScript functions registered"))?;

    self
      .ecma_functions
      .try_reserve(1)
      .map_err(|_| VmError::OutOfMemory)?;
    // `HashMap::insert` can abort the process on allocator OOM during table growth. Reserve both the
    // vector slot and the cache entry up-front so we never partially update state.
    self
      .ecma_function_cache
      .try_reserve(1)
      .map_err(|_| VmError::OutOfMemory)?;

    let prefix_len = match kind {
      EcmaFunctionKind::Decl => 0,
      EcmaFunctionKind::Expr => 1,
      EcmaFunctionKind::ObjectMember => 2,
      // Parse class member snippets by wrapping them in a derived class expression:
      // `(class extends null { <snippet> })`.
      //
      // Using `extends null` ensures `super()` is syntactically permitted in constructor bodies
      // (which would otherwise be rejected in a non-derived `class { ... }` wrapper).
      EcmaFunctionKind::ClassMember => 21, // "(class extends null {".
      // Parse field initializer expressions as the body of a synthetic class *method*:
      // `(class extends null {m(){return <snippet>;}})`.
      //
      // Field initializers have a lexical `super` binding (for `super.prop`, but not `super()`).
      // Wrapping in a method body preserves that grammar context during lazy parsing.
      EcmaFunctionKind::ClassFieldInitializer => 32, // "(class extends null {m(){return ".
    };

    self.ecma_functions.push(EcmaFunctionCode {
      source,
      span_start,
      span_end,
      kind,
      prefix_len,
      parsed: None,
      parsed_memory: None,
    });

    let id = EcmaFunctionId(idx);
    self.ecma_function_cache.insert(key, id);
    Ok(id)
  }

  pub(crate) fn ecma_function_source_span(
    &self,
    id: EcmaFunctionId,
  ) -> Option<(Arc<SourceText>, u32, u32, EcmaFunctionKind)> {
    let code = self.ecma_functions.get(id.0 as usize)?;
    Some((code.source.clone(), code.span_start, code.span_end, code.kind))
  }

  pub(crate) fn ecma_function_ast(
    &mut self,
    heap: &mut Heap,
    id: EcmaFunctionId,
  ) -> Result<Arc<Node<Func>>, VmError> {
    let (source, span_start, span_end, kind) = {
      let code = self
        .ecma_functions
        .get(id.0 as usize)
        .ok_or_else(|| VmError::invalid_handle())?;
      if let Some(parsed) = &code.parsed {
        return Ok(parsed.clone());
      }
      (
        code.source.clone(),
        code.span_start,
        code.span_end,
        code.kind,
      )
    };

    let text: &str = &source.text;
    let mut start = span_start as usize;
    let mut end = span_end as usize;
    start = start.min(text.len());
    end = end.min(text.len());
    if start > end {
      return Err(VmError::Unimplemented(
        "invalid ECMAScript function source span",
      ));
    }
    let snippet = text.get(start..end).ok_or(VmError::Unimplemented(
      "invalid ECMAScript function source slice",
    ))?;

    let script_opts = ParseOptions {
      dialect: Dialect::Ecma,
      source_type: SourceType::Script,
    };
    let module_opts = ParseOptions {
      dialect: Dialect::Ecma,
      source_type: SourceType::Module,
    };

    fn parse_top(
      vm: &mut Vm,
      src: &str,
      script: ParseOptions,
      module: ParseOptions,
      allow_enclosing_meta_properties: bool,
    ) -> Result<Node<parse_js::ast::stx::TopLevel>, VmError> {
      let parse = |vm: &mut Vm, src: &str, opts: ParseOptions| {
        if allow_enclosing_meta_properties {
          vm.parse_top_level_with_budget_allowing_enclosing_meta_properties(src, opts)
        } else {
          vm.parse_top_level_with_budget(src, opts)
        }
      };

      match parse(vm, src, script) {
        Ok(top) => Ok(top),
        Err(err @ VmError::Syntax(_)) => match parse(vm, src, module) {
          Ok(top) => Ok(top),
          Err(module_err) => {
            if matches!(module_err, VmError::Syntax(_)) {
              Err(err)
            } else {
              Err(module_err)
            }
          }
        },
        Err(err) => Err(err),
      }
    }

    let mut wrapped: String = String::new();
    let top = match kind {
      // Most function snippets originate from classic scripts, which must be parsed in
      // `SourceType::Script` mode to preserve sloppy-mode semantics and grammar.
      //
      // However, module-defined functions can include module-only syntax within their snippet span:
      // - `export` / `export default` prefixes on declarations
      // - `import.meta` inside function bodies
      //
      // In that case, parsing as a script will fail with a SyntaxError; retry parsing as a module.
      EcmaFunctionKind::Decl => parse_top(self, snippet, script_opts, module_opts, false)?,
      EcmaFunctionKind::ObjectMember => {
        let capacity = snippet.len().checked_add(4).ok_or(VmError::OutOfMemory)?;
        wrapped
          .try_reserve(capacity)
          .map_err(|_| VmError::OutOfMemory)?;
        wrapped.push_str("({");
        wrapped.push_str(snippet);
        wrapped.push_str("})");
        parse_top(self, &wrapped, script_opts, module_opts, false)?
      }
      EcmaFunctionKind::ClassMember => {
        let capacity = snippet.len().checked_add(23).ok_or(VmError::OutOfMemory)?;
        wrapped
          .try_reserve(capacity)
          .map_err(|_| VmError::OutOfMemory)?;
        wrapped.push_str("(class extends null {");
        wrapped.push_str(snippet);
        wrapped.push_str("})");
        parse_top(self, &wrapped, script_opts, module_opts, false)?
      }
      EcmaFunctionKind::ClassFieldInitializer => {
        wrapped.clear();
        // "(class extends null {m(){return " + snippet + ";}})"
        //
        // Wrapping as a class method (rather than a plain `function(){...}`) is required so `super`
        // property references are parsed successfully inside the initializer expression.
        let capacity = snippet.len().checked_add(36).ok_or(VmError::OutOfMemory)?;
        wrapped
          .try_reserve(capacity)
          .map_err(|_| VmError::OutOfMemory)?;
        wrapped.push_str("(class extends null {m(){return ");
        wrapped.push_str(snippet);
        wrapped.push_str(";}})");
        // Field initializers can contain `super` / `new.target` expressions provided by an enclosing
        // class body. Allow parsing them in a permissive enclosing-meta-property context.
        parse_top(self, &wrapped, script_opts, module_opts, true)?
      }
      EcmaFunctionKind::Expr => {
        wrapped.clear();
        let capacity = snippet.len().checked_add(2).ok_or(VmError::OutOfMemory)?;
        wrapped
          .try_reserve(capacity)
          .map_err(|_| VmError::OutOfMemory)?;
        wrapped.push('(');
        wrapped.push_str(snippet);
        wrapped.push(')');

        // Parse function/arrow expression snippets in a permissive "enclosing function"
        // meta-property context.
        //
        // `vm-js` reparses nested function expressions lazily by slicing and parsing their source
        // spans. When the snippet is an arrow function, `new.target` / `super` expressions are
        // syntactically valid only if provided by an enclosing non-arrow function or class
        // element; that lexical context is not present when parsing the snippet as a standalone
        // script.
        //
        // We conservatively allow these meta-properties here since the enclosing context was
        // already validated by the original parse of the full source.
        parse_top(self, &wrapped, script_opts, module_opts, true)?
      }
    };

    let parsed_source_len = if wrapped.is_empty() {
      snippet.len()
    } else {
      wrapped.len()
    };

    // `parse-js` AST nodes can be significantly larger than the original source (each token and
    // syntactic construct becomes one-or-more Rust structs). Use a conservative multiplier so
    // hostile input can't bypass heap limits by forcing large cached ASTs.
    let estimated_ast_bytes = parsed_source_len.saturating_mul(4);
    let token = heap.charge_external(estimated_ast_bytes)?;

    let mut body = top.stx.body;
    if body.len() != 1 {
      return Err(VmError::Unimplemented(
        "ECMAScript function snippet did not parse to a single statement",
      ));
    }

    let stmt = body.pop().ok_or(VmError::Unimplemented(
      "missing statement in parsed function snippet",
    ))?;

    let func = match kind {
      EcmaFunctionKind::Decl => match *stmt.stx {
        Stmt::FunctionDecl(decl) => decl.stx.function,
        _ => {
          return Err(VmError::Unimplemented(
            "ECMAScript function declaration snippet did not parse as a function declaration",
          ));
        }
      },
      EcmaFunctionKind::Expr => match *stmt.stx {
        Stmt::Expr(expr_stmt) => {
          let expr = expr_stmt.stx.expr;
          match *expr.stx {
            AstExpr::Func(func_expr) => func_expr.stx.func,
            AstExpr::ArrowFunc(arrow_expr) => arrow_expr.stx.func,
            _ => {
              return Err(VmError::Unimplemented(
                "ECMAScript function expression snippet did not parse as a function expression",
              ));
            }
          }
        }
        _ => {
          return Err(VmError::Unimplemented(
            "ECMAScript function expression snippet did not parse as an expression statement",
          ));
        }
      },
      EcmaFunctionKind::ClassFieldInitializer => match *stmt.stx {
        Stmt::Expr(expr_stmt) => {
          let expr = expr_stmt.stx.expr;
          match *expr.stx {
            AstExpr::Class(class_expr) => {
              let member = class_expr.stx.members.into_iter().next().ok_or(VmError::Unimplemented(
                "ECMAScript class field initializer snippet did not contain any members",
              ))?;
              match member.stx.val {
                ClassOrObjVal::Method(method) => {
                  let ClassOrObjMethod { func } = *method.stx;
                  func
                }
                _ => {
                  return Err(VmError::Unimplemented(
                    "ECMAScript class field initializer snippet did not parse as a method",
                  ));
                }
              }
            }
            _ => {
              return Err(VmError::Unimplemented(
                "ECMAScript class field initializer snippet did not parse as a class expression",
              ));
            }
          }
        }
        _ => {
          return Err(VmError::Unimplemented(
            "ECMAScript class field initializer snippet did not parse as an expression statement",
          ));
        }
      },
      EcmaFunctionKind::ObjectMember => match *stmt.stx {
        Stmt::Expr(expr_stmt) => {
          let expr = expr_stmt.stx.expr;
          match *expr.stx {
            AstExpr::LitObj(obj_expr) => {
              let member = obj_expr.stx.members.into_iter().next().ok_or(
                VmError::Unimplemented("ECMAScript object member snippet did not contain any members"),
              )?;
              let ObjMember { typ } = *member.stx;
              match typ {
                ObjMemberType::Valued { val, .. } => match val {
                  ClassOrObjVal::Method(method) => {
                    let ClassOrObjMethod { func } = *method.stx;
                    func
                  }
                  ClassOrObjVal::Getter(getter) => {
                    let ClassOrObjGetter { func } = *getter.stx;
                    func
                  }
                  ClassOrObjVal::Setter(setter) => {
                    let ClassOrObjSetter { func } = *setter.stx;
                    func
                  }
                  _ => {
                    return Err(VmError::Unimplemented(
                      "ECMAScript object member snippet did not parse as a method/getter/setter",
                    ));
                  }
                },
                _ => {
                  return Err(VmError::Unimplemented(
                    "ECMAScript object member snippet did not parse as a valued member",
                  ));
                }
              }
            }
            _ => {
              return Err(VmError::Unimplemented(
                "ECMAScript object member snippet did not parse as an object literal expression",
              ));
            }
          }
        }
        _ => {
          return Err(VmError::Unimplemented(
            "ECMAScript object member snippet did not parse as an expression statement",
          ));
        }
      },
      EcmaFunctionKind::ClassMember => match *stmt.stx {
        Stmt::Expr(expr_stmt) => {
          let expr = expr_stmt.stx.expr;
          match *expr.stx {
            AstExpr::Class(class_expr) => {
              let member = class_expr.stx.members.into_iter().next().ok_or(VmError::Unimplemented(
                "ECMAScript class member snippet did not contain any members",
              ))?;
              match member.stx.val {
                ClassOrObjVal::Method(method) => {
                  let ClassOrObjMethod { func } = *method.stx;
                  func
                }
                ClassOrObjVal::Getter(getter) => {
                  let ClassOrObjGetter { func } = *getter.stx;
                  func
                }
                ClassOrObjVal::Setter(setter) => {
                  let ClassOrObjSetter { func } = *setter.stx;
                  func
                }
                _ => {
                  return Err(VmError::Unimplemented(
                    "ECMAScript class member snippet did not parse as a method/getter/setter",
                  ));
                }
              }
            }
            _ => {
              return Err(VmError::Unimplemented(
                "ECMAScript class member snippet did not parse as a class expression",
              ));
            }
          }
        }
        _ => {
          return Err(VmError::Unimplemented(
            "ECMAScript class member snippet did not parse as an expression statement",
          ));
        }
      },
    };

    let func = arc_try_new_vm(func)?;

    let slot = self
      .ecma_functions
      .get_mut(id.0 as usize)
      .ok_or_else(|| VmError::invalid_handle())?;
    slot.parsed = Some(func.clone());
    slot.parsed_memory = Some(token);
    Ok(func)
  }

  fn dispatch_native_call(
    &mut self,
    call_id: NativeFunctionId,
    host: &mut dyn VmHost,
    scope: &mut Scope<'_>,
    hooks: &mut dyn VmHostHooks,
    callee: GcObject,
    this: Value,
    args: &[Value],
  ) -> Result<Value, VmError> {
    let f = self
      .native_calls
      .get(call_id.0 as usize)
      .copied()
      .ok_or(VmError::Unimplemented("unknown native function id"))?;
    // A buggy embedding must never be able to unwind through the VM and bypass cleanup paths
    // (microtask queue restoration, root scopes, execution contexts, etc).
    //
    // If a native handler panics, treat it as a fatal host contract violation and surface it as a
    // non-catchable VM error.
    let res = catch_unwind(AssertUnwindSafe(|| f(self, scope, host, hooks, callee, this, args)));
    match res {
      Ok(result) => result,
      Err(_) => Err(VmError::InvariantViolation("native call panicked")),
    }
  }

  fn dispatch_native_construct(
    &mut self,
    construct_id: NativeConstructId,
    host: &mut dyn VmHost,
    scope: &mut Scope<'_>,
    hooks: &mut dyn VmHostHooks,
    callee: GcObject,
    args: &[Value],
    new_target: Value,
  ) -> Result<Value, VmError> {
    let construct = self
      .native_constructs
      .get(construct_id.0 as usize)
      .copied()
      .ok_or(VmError::Unimplemented("unknown native constructor id"))?;
    let res = catch_unwind(AssertUnwindSafe(|| {
      construct(self, scope, host, hooks, callee, args, new_target)
    }));
    match res {
      Ok(result) => result,
      Err(_) => Err(VmError::InvariantViolation("native construct panicked")),
    }
  }

  /// Pushes a stack frame and returns an RAII guard that will pop it on drop.
  pub fn enter_frame(&mut self, frame: StackFrame) -> Result<VmFrameGuard<'_>, VmError> {
    VmFrameGuard::new(self, frame)
  }

  pub fn push_frame(&mut self, frame: StackFrame) -> Result<(), VmError> {
    if self.stack.len() >= self.options.max_stack_depth {
      // Exceeding the VM's maximum stack depth should surface to JavaScript as a catchable
      // RangeError (like other engines' "Maximum call stack size exceeded"), not as a hard
      // termination.
      //
      // This limit is a hard safety boundary: we must not continue pushing frames and risk
      // overflowing the native stack.
      return Err(VmError::RangeError("Maximum call stack size exceeded"));
    }
    // `Vec::push` can abort the process on allocator OOM; reserve fallibly first.
    self
      .stack
      .try_reserve(1)
      .map_err(|_| VmError::OutOfMemory)?;
    self.stack.push(frame);
    Ok(())
  }

  pub fn pop_frame(&mut self) {
    self.stack.pop();
  }

  pub fn capture_stack(&self) -> Vec<StackFrame> {
    // `self.stack` is maintained in call-stack order (outermost → innermost). Stack traces are
    // conventionally rendered with the most recent frame first, so reverse during capture.
    let len = self.stack.len();
    let mut out: Vec<StackFrame> = Vec::new();
    if out.try_reserve_exact(len).is_err() {
      // Best-effort stack capture: if we cannot allocate the trace vector under memory pressure,
      // return an empty stack rather than aborting the process.
      return out;
    }
    for frame in self.stack.iter().rev() {
      // Safe: we reserved enough capacity above, so `push` cannot allocate.
      out.push(frame.clone());
    }
    out
  }

  fn update_top_frame_location(&mut self, call_site: &CallSite) {
    if let Some(top) = self.stack.last_mut() {
      top.source = call_site.source.clone();
      top.line = call_site.line;
      top.col = call_site.col;
    }
  }

  /// Pushes an [`ExecutionContext`] onto the execution context stack.
  pub fn push_execution_context(&mut self, ctx: ExecutionContext) -> Result<(), VmError> {
    // `Vec::push` can abort the process on allocator OOM; reserve fallibly first.
    self
      .execution_context_stack
      .try_reserve(1)
      .map_err(|_| VmError::OutOfMemory)?;
    self.execution_context_stack.push(ctx);
    Ok(())
  }

  /// Temporarily override the active host hook implementation, restoring the previous override
  /// when the returned guard is dropped.
  ///
  /// This is a safer alternative to manual `push_active_host_hooks` / `pop_active_host_hooks`
  /// sequences, ensuring the VM never retains a dangling raw pointer on early returns or panics.
  pub(crate) fn push_active_host_hooks_guard(
    &mut self,
    host: &mut dyn VmHostHooks,
  ) -> ActiveHostHooksGuard<'_> {
    let previous = self.push_active_host_hooks(host);
    ActiveHostHooksGuard {
      vm: self,
      previous,
    }
  }

  pub(crate) fn push_active_host_hooks(
    &mut self,
    host: &mut dyn VmHostHooks,
  ) -> Option<*mut (dyn VmHostHooks + 'static)> {
    let prev = self.host_hooks_override;
    self.host_hooks_override = Some(Self::erase_host_hooks_lifetime(host));
    prev
  }

  pub(crate) fn pop_active_host_hooks(
    &mut self,
    previous: Option<*mut (dyn VmHostHooks + 'static)>,
  ) {
    self.host_hooks_override = previous;
  }

  /// Pops the top [`ExecutionContext`] from the execution context stack.
  pub fn pop_execution_context(&mut self) -> Option<ExecutionContext> {
    self.execution_context_stack.pop()
  }

  /// Pushes an [`ExecutionContext`] and returns an RAII guard that will pop it on drop.
  pub fn execution_context_guard(
    &mut self,
    ctx: ExecutionContext,
  ) -> Result<ExecutionContextGuard<'_>, VmError> {
    ExecutionContextGuard::new(self, ctx)
  }

  /// Returns the active script or module, if any.
  ///
  /// This implements ECMA-262's
  /// [`GetActiveScriptOrModule`](https://tc39.es/ecma262/#sec-getactivescriptormodule) abstract
  /// operation.
  ///
  /// FastRender also vendors an offline copy of the spec at:
  /// [`specs/tc39-ecma262/spec.html#sec-getactivescriptormodule`](specs/tc39-ecma262/spec.html#sec-getactivescriptormodule)
  ///
  /// The scan skips execution contexts whose `script_or_module` is `None` (for example, host work
  /// such as Promise jobs or embedder callbacks).
  ///
  /// This will be used by module features such as `import.meta` and dynamic `import()` once the
  /// module system is implemented.
  pub fn get_active_script_or_module(&self) -> Option<ScriptOrModule> {
    self
      .execution_context_stack
      .iter()
      .rev()
      .find_map(|ctx| ctx.script_or_module)
  }

  /// Generates a fresh [`ScriptId`] for a classic script execution.
  pub(crate) fn fresh_script_id(&mut self) -> Result<ScriptId, VmError> {
    let raw = self.next_script_id;
    self.next_script_id = self
      .next_script_id
      .checked_add(1)
      .ok_or(VmError::LimitExceeded("ScriptId overflow"))?;
    Ok(ScriptId::from_raw(raw))
  }

  pub(crate) fn intern_script_or_module(
    &mut self,
    script_or_module: ScriptOrModule,
  ) -> Result<NonZeroU32, VmError> {
    if let Some(idx) = self
      .script_or_module_table
      .iter()
      .position(|&v| v == script_or_module)
    {
      if idx >= u32::MAX as usize {
        return Err(VmError::OutOfMemory);
      }
      // Index is offset by 1 so `0` can represent "no script/module".
      let n = (idx as u32) + 1;
      return NonZeroU32::new(n).ok_or(VmError::InvariantViolation(
        "script/module token should be non-zero",
      ));
    }

    // `Vec::push` can abort the process on allocator OOM; reserve fallibly first.
    self
      .script_or_module_table
      .try_reserve(1)
      .map_err(|_| VmError::OutOfMemory)?;

    let idx = self.script_or_module_table.len();
    if idx >= u32::MAX as usize {
      return Err(VmError::OutOfMemory);
    }
    self.script_or_module_table.push(script_or_module);

    let n = (idx as u32).wrapping_add(1);
    NonZeroU32::new(n).ok_or(VmError::OutOfMemory)
  }

  pub fn resolve_script_or_module_token(&self, token: NonZeroU32) -> Option<ScriptOrModule> {
    let idx = token.get().wrapping_sub(1) as usize;
    self.script_or_module_table.get(idx).copied()
  }

  #[inline]
  pub(crate) fn resolve_script_or_module_token_opt(
    &self,
    token: Option<NonZeroU32>,
  ) -> Option<ScriptOrModule> {
    token.and_then(|t| self.resolve_script_or_module_token(t))
  }

  pub(crate) fn get_or_create_import_meta_object(
    &mut self,
    scope: &mut Scope<'_>,
    hooks: &mut dyn VmHostHooks,
    module: ModuleId,
  ) -> Result<GcObject, VmError> {
    let modules_ptr = self
      .module_graph_ptr()
      .ok_or(VmError::Unimplemented("import.meta requires a module graph"))?;
    // Safety: `Vm::module_graph_ptr` is only set by embeddings that ensure the graph outlives the
    // VM (see `Vm::set_module_graph` docs). `ModuleGraph::{evaluate,evaluate_with_scope}` installs
    // a temporary pointer via `ModuleGraphPtrGuard` so `import.meta` can consult per-graph caches.
    let modules = unsafe { &mut *modules_ptr };
    modules.get_or_create_import_meta_object(self, scope, hooks, module)
  }

  pub(crate) fn get_or_create_template_object(
    &mut self,
    scope: &mut Scope<'_>,
    source: Arc<SourceText>,
    span_start: u32,
    span_end: u32,
    raw_parts: &[Box<[u16]>],
    cooked_parts: &[Option<Box<[u16]>>],
  ) -> Result<GcObject, VmError> {
    // `GetTemplateObject` is realm-scoped. When running code through the higher-level runtime
    // (`JsRuntime`/`Evaluator`), the realm is tracked via an execution context.
    //
    // Some internal tests call into the VM without an explicit execution context; in that case,
    // fall back to the VM's default realm (`intrinsics_realm`).
    let realm = self
      .current_realm()
      .or(self.intrinsics_realm)
      .ok_or(VmError::Unimplemented("template literal requires active realm"))?;

    let key = TemplateRegistryKey {
      realm,
      source: source.cache_key_ptr(),
      span_start,
      span_end,
    };

    if let Some(entry) = self.template_registry.get(&key) {
      let Some(Value::Object(obj)) = scope.heap().get_root(entry.root) else {
        return Err(VmError::invalid_handle());
      };
      return Ok(obj);
    }

    let intr = self
      .intrinsics()
      .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;

    if raw_parts.len() != cooked_parts.len() {
      return Err(VmError::InvariantViolation(
        "template literal raw/cooked length mismatch",
      ));
    }

    // Allocate with `length = 0` and let the array exotic `length` semantics update as we define
    // indexed elements. This avoids an extra `O(N)` scan before we start ticking in the main
    // segment loop.
    let cooked = scope.alloc_array(0)?;
    scope.push_root(Value::Object(cooked))?;
    scope
      .heap_mut()
      .object_set_prototype(cooked, Some(intr.array_prototype()))?;

    let raw = scope.alloc_array(0)?;
    scope.push_root(Value::Object(raw))?;
    scope
      .heap_mut()
      .object_set_prototype(raw, Some(intr.array_prototype()))?;

    let segments = raw_parts
      .iter()
      .zip(cooked_parts.iter())
      .enumerate();

    for (idx, (raw_units, cooked_units)) in segments {
      // Per-segment tick: tagged templates can contain large numbers of segments, and creating the
      // template object involves allocation + property definition even when no nested expressions
      // are evaluated.
      self.tick()?;

      let idx_u32 = u32::try_from(idx).map_err(|_| {
        VmError::Unimplemented("tagged template with more than 2^32-1 segments")
      })?;

      let mut elem_scope = scope.reborrow();

      let key = elem_scope.alloc_array_index_key(idx_u32)?;
      let key_root = match key {
        PropertyKey::String(s) => Value::String(s),
        PropertyKey::Symbol(s) => Value::Symbol(s),
      };
      elem_scope.push_root(key_root)?;

      let cooked_value = match cooked_units {
        Some(units) => {
          let s = elem_scope.alloc_string_from_code_units(units.as_ref())?;
          elem_scope.push_root(Value::String(s))?;
          Value::String(s)
        }
        None => Value::Undefined,
      };

      elem_scope.define_property(
        cooked,
        key,
        PropertyDescriptor {
          enumerable: true,
          configurable: false,
          kind: PropertyKind::Data {
            value: cooked_value,
            writable: false,
          },
        },
      )?;

      let raw_s = elem_scope.alloc_string_from_code_units(raw_units.as_ref())?;
      elem_scope.push_root(Value::String(raw_s))?;
      elem_scope.define_property(
        raw,
        key,
        PropertyDescriptor {
          enumerable: true,
          configurable: false,
          kind: PropertyKind::Data {
            value: Value::String(raw_s),
            writable: false,
          },
        },
      )?;
    }

    // Define the non-enumerable `.raw` property.
    let raw_key_s = scope.alloc_string("raw")?;
    scope.push_root(Value::String(raw_key_s))?;
    scope.define_property(
      cooked,
      PropertyKey::from_string(raw_key_s),
      PropertyDescriptor {
        enumerable: false,
        configurable: false,
        kind: PropertyKind::Data {
          value: Value::Object(raw),
          writable: false,
        },
      },
    )?;

    // Freeze both arrays (spec `SetIntegrityLevel(..., frozen)`).
    scope.heap_mut().array_set_length_writable(cooked, false)?;
    scope.heap_mut().array_set_length_writable(raw, false)?;
    scope.object_prevent_extensions(cooked)?;
    scope.object_prevent_extensions(raw)?;

    // Keep the template object alive across GC by storing it as a persistent root.
    let root = scope.heap_mut().add_root(Value::Object(cooked))?;

    if self.template_registry.try_reserve(1).is_err() {
      scope.heap_mut().remove_root(root);
      return Err(VmError::OutOfMemory);
    }

    self.template_registry.insert(
      key,
      TemplateRegistryEntry {
        source,
        root,
      },
    );

    Ok(cooked)
  }

  /// Returns the realm of the currently-running execution context, if any.
  pub fn current_realm(&self) -> Option<RealmId> {
    self.execution_context_stack.last().map(|ctx| ctx.realm)
  }

  /// Returns the VM's default realm id, if any.
  ///
  /// This is useful as a "default realm" in host entry points (like module linking/instantiation)
  /// that may run without an active execution context.
  pub(crate) fn intrinsics_realm(&self) -> Option<RealmId> {
    self.intrinsics_realm
  }

  fn terminate(&self, reason: TerminationReason) -> VmError {
    VmError::Termination(Termination::new(reason, self.capture_stack()))
  }

  /// Consume one VM "tick": checks fuel/deadline/interrupt state.
  ///
  /// ## Tick policy
  ///
  /// `Vm` itself does not prescribe what a tick means; ticks are an execution-engine-defined unit
  /// of work.
  ///
  /// The current AST evaluator (`exec.rs`) charges **one tick** at the start of every statement and
  /// expression evaluation. Additional ticks are charged in a few internal loops that may
  /// otherwise run without evaluating any statements/expressions (e.g. `for(;;){}` with an empty
  /// body), in some literal-construction loops (arrays/objects/tagged templates), and when
  /// entering [`Vm::call`] / [`Vm::construct`].
  pub fn tick(&mut self) -> Result<(), VmError> {
    if let Some(fuel) = &mut self.budget.budget.fuel {
      if *fuel == 0 {
        return Err(self.terminate(TerminationReason::OutOfFuel));
      }
      *fuel -= 1;
    }

    self.budget.ticks = self.budget.ticks.wrapping_add(1);

    if self.interrupt.is_interrupted() {
      return Err(self.terminate(TerminationReason::Interrupted));
    }

    if let Some(deadline) = self.budget.budget.deadline {
      let interval = self.budget.budget.check_time_every.max(1) as u64;
      if self.budget.ticks % interval == 0 && Instant::now() >= deadline {
        return Err(self.terminate(TerminationReason::DeadlineExceeded));
      }
    }

    Ok(())
  }

  fn parse_top_level_with_budget_impl(
    &mut self,
    source: &str,
    opts: ParseOptions,
    meta_property_context: MetaPropertyContext,
  ) -> Result<Node<parse_js::ast::stx::TopLevel>, VmError> {
    let mut parse_once = |allow_top_level_await_in_script: bool| -> Result<Node<parse_js::ast::stx::TopLevel>, VmError> {
      // Ensure fuel/deadline/interrupt budgets apply *during parsing* as well as during evaluation.
      self.tick()?;

      const PARSE_TICK_EVERY: u64 = 1024;
      let mut steps: u64 = 0;
      let mut tick_err: Option<VmError> = None;

      let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        parse_with_options_cancellable_by_with_init(
          source,
          opts,
          || {
            steps = steps.wrapping_add(1);
            if steps % PARSE_TICK_EVERY == 0 {
              if let Err(err) = self.tick() {
                tick_err = Some(err);
                return true;
              }
            }
            false
          },
          |p| {
            p.set_initial_meta_property_context(
              meta_property_context.allow_new_target(),
              meta_property_context.allow_super_property(),
              meta_property_context.allow_super_call(),
            );
            if allow_top_level_await_in_script {
              p.set_allow_top_level_await_in_script(true);
            }
          },
        )
      }));
      let res = match res {
        Ok(res) => res,
        Err(_) => {
          return Err(VmError::InvariantViolation(
            "parse-js panicked while parsing source",
          ));
        }
      };

      match res {
        Ok(top) => Ok(top),
        Err(err) if err.typ == SyntaxErrorType::Cancelled => Err(tick_err.unwrap_or_else(|| {
          VmError::Termination(Termination::new(TerminationReason::Interrupted, Vec::new()))
        })),
        Err(err) => {
          // `parse-js` performs some spec-driven validations that overlap with `vm-js` early errors.
          // Preserve `vm-js`'s stable diagnostic code for these cases.
          if matches!(
            err.typ,
            SyntaxErrorType::ExpectedSyntax(
              "'arguments' is not allowed in class field initializer or static initialization block"
            )
          ) {
            let mut diag = err.to_diagnostic(FileId(0));
            diag.code = "VMJS0004".into();
            return Err(VmError::Syntax(vec![diag]));
          }
          Err(VmError::Syntax(vec![err.to_diagnostic(FileId(0))]))
        }
      }
    };

    let first = parse_once(false);
    let should_retry =
      matches!(&first, Err(VmError::Syntax(_))) && matches!(opts.source_type, SourceType::Script);
    if !should_retry {
      return first;
    }

    // If parsing a classic script fails, retry with top-level `await` enabled. This implements async
    // classic scripts without breaking valid scripts that use `await` as an identifier.
    let retry = parse_once(true);
    if retry.is_ok() {
      return retry;
    }
    first
  }

  pub(crate) fn parse_top_level_with_budget(
    &mut self,
    source: &str,
    opts: ParseOptions,
  ) -> Result<Node<parse_js::ast::stx::TopLevel>, VmError> {
    self.parse_top_level_with_budget_impl(source, opts, MetaPropertyContext::SCRIPT)
  }

  pub(crate) fn parse_top_level_with_budget_allowing_enclosing_meta_properties(
    &mut self,
    source: &str,
    opts: ParseOptions,
  ) -> Result<Node<parse_js::ast::stx::TopLevel>, VmError> {
    self.parse_top_level_with_budget_impl(source, opts, MetaPropertyContext::ALL)
  }

  pub(crate) fn parse_top_level_with_budget_with_meta_property_context(
    &mut self,
    source: &str,
    opts: ParseOptions,
    meta_property_context: MetaPropertyContext,
  ) -> Result<Node<parse_js::ast::stx::TopLevel>, VmError> {
    self.parse_top_level_with_budget_impl(source, opts, meta_property_context)
  }

  /// Calls `callee` with the provided `this` value and arguments.
  ///
  /// `host` is embedder-provided context (DOM/event loop/etc) forwarded to native call handlers.
  ///
  /// # Rooting
  ///
  /// The returned [`Value`] is **not automatically rooted**. If the caller will perform any
  /// additional allocations that could trigger GC, it must root the returned value itself (for
  /// example with `scope.push_root(result)`.
  ///
  /// This method roots `callee`, `this`, and `args` for the duration of the call using a temporary
  /// child [`Scope`].
  pub fn call(
    &mut self,
    host: &mut dyn VmHost,
    scope: &mut Scope<'_>,
    callee: Value,
    this: Value,
    args: &[Value],
  ) -> Result<Value, VmError> {
    self.call_entry(host, scope, callee, this, args, None)
  }

  pub fn call_at_location(
    &mut self,
    host: &mut dyn VmHost,
    scope: &mut Scope<'_>,
    callee: Value,
    this: Value,
    args: &[Value],
    call_site_source: &SourceText,
    call_site_offset: u32,
  ) -> Result<Value, VmError> {
    let call_site = CallSite::from_source_offset(call_site_source, call_site_offset);
    self.call_entry(host, scope, callee, this, args, Some(call_site))
  }

  fn call_entry(
    &mut self,
    host: &mut dyn VmHost,
    scope: &mut Scope<'_>,
    callee: Value,
    this: Value,
    args: &[Value],
    call_site: Option<CallSite>,
  ) -> Result<Value, VmError> {
    if let Some(hooks_ptr) = self.host_hooks_override {
      // SAFETY: `host_hooks_override` is only set while a host hooks implementation is mutably
      // borrowed by an embedder entry point (for example `Vm::call_with_host_and_hooks` or
      // `JsRuntime::exec_script_source_with_hooks`).
      let hooks = unsafe { &mut *hooks_ptr };
      return self.call_impl(host, scope, hooks, callee, this, args, call_site);
    }

    // `call_with_host_and_hooks` requires `&mut self` plus an independent `&mut hooks`, but `Vm`
    // stores a default microtask queue inside itself. Temporarily move it out so it can serve as
    // the host hook implementation for this call.
    let mut hooks = mem::take(&mut self.microtasks);
    let result = {
      let mut vm_hooks = self.push_active_host_hooks_guard(&mut hooks);
      vm_hooks.call_impl(host, scope, &mut hooks, callee, this, args, call_site)
    };
    // If a native handler enqueued jobs directly onto the VM-owned microtask queue while it was
    // temporarily moved out (via `vm.microtask_queue_mut()`), merge them back before restoring.
    struct MergeCtx<'a> {
      heap: &'a mut Heap,
    }
    impl VmJobContext for MergeCtx<'_> {
      fn call(
        &mut self,
        _hooks: &mut dyn VmHostHooks,
        _callee: Value,
        _this: Value,
        _args: &[Value],
      ) -> Result<Value, VmError> {
        Err(VmError::Unimplemented("MergeCtx::call"))
      }

      fn construct(
        &mut self,
        _hooks: &mut dyn VmHostHooks,
        _callee: Value,
        _args: &[Value],
        _new_target: Value,
      ) -> Result<Value, VmError> {
        Err(VmError::Unimplemented("MergeCtx::construct"))
      }

      fn add_root(&mut self, value: Value) -> Result<RootId, VmError> {
        self.heap.add_root(value)
      }

      fn remove_root(&mut self, id: RootId) {
        self.heap.remove_root(id);
      }
    }

    let mut ctx = MergeCtx { heap: scope.heap_mut() };
    let mut merge_err: Option<VmError> = None;
    while let Some((realm, job)) = self.microtasks.pop_front() {
      if let Err(err) = hooks.host_enqueue_promise_job_fallible(&mut ctx, job, realm) {
        merge_err = Some(err);
        break;
      }
    }
    if merge_err.is_some() {
      // Discard any remaining jobs in the temporary queue before restoring the main queue.
      self.microtasks.teardown(&mut ctx);
    }
    self.microtasks = hooks;
    if let Some(err) = merge_err {
      return Err(err);
    }
    result
  }

  /// Convenience wrapper around [`Vm::call`] that passes a dummy host context (`()`).
  ///
  /// This uses the currently-active host hook implementation (either an embedder-installed host
  /// hooks override, or the VM-owned microtask queue).
  ///
  /// This exists for internal engine/tests that do not have embedder state available. Host
  /// embeddings should prefer [`Vm::call`] so native handlers can access real host context.
  #[inline]
  pub fn call_without_host(
    &mut self,
    scope: &mut Scope<'_>,
    callee: Value,
    this: Value,
    args: &[Value],
  ) -> Result<Value, VmError> {
    let mut dummy_host = ();
    self.call(&mut dummy_host, scope, callee, this, args)
  }

  /// Calls `callee` with the provided `this` value and arguments, using a custom host hook
  /// implementation.
  ///
  /// ## ⚠️ Dummy `VmHost` context
  ///
  /// This method does **not** accept an embedder [`VmHost`] and will always pass a dummy `VmHost`
  /// value (`()`) to native call handlers.
  ///
  /// It exists primarily for engine internals and tests that need to run JS while supplying a
  /// custom [`VmHostHooks`] implementation (for example, to route `HostEnqueuePromiseJob` into a
  /// host-owned microtask queue), but do not have any embedder host state available.
  ///
  /// Embeddings that need native handlers to observe real host context (DOM/event loop/etc) should
  /// use:
  /// - [`Vm::call_with_host_and_hooks`] (explicit host context + hooks), or
  /// - [`Vm::call`] (explicit host context; uses the VM-owned microtask queue).
  pub fn call_with_host(
    &mut self,
    scope: &mut Scope<'_>,
    host: &mut dyn VmHostHooks,
    callee: Value,
    this: Value,
    args: &[Value],
  ) -> Result<Value, VmError> {
    let mut dummy_host = ();
    self.call_with_host_and_hooks(&mut dummy_host, scope, host, callee, this, args)
  }

  /// Calls `callee` with the provided `this` value and arguments, using an explicit embedder host
  /// context and host hook implementation.
  #[inline]
  pub fn call_with_host_and_hooks(
    &mut self,
    host: &mut dyn VmHost,
    scope: &mut Scope<'_>,
    hooks: &mut dyn VmHostHooks,
    callee: Value,
    this: Value,
    args: &[Value],
  ) -> Result<Value, VmError> {
    let mut vm_hooks = self.push_active_host_hooks_guard(hooks);
    vm_hooks.call_impl(host, scope, hooks, callee, this, args, None)
  }

  pub(crate) fn call_with_host_and_hooks_at_location(
    &mut self,
    host: &mut dyn VmHost,
    scope: &mut Scope<'_>,
    hooks: &mut dyn VmHostHooks,
    callee: Value,
    this: Value,
    args: &[Value],
    call_site_source: &SourceText,
    call_site_offset: u32,
  ) -> Result<Value, VmError> {
    let call_site = CallSite::from_source_offset(call_site_source, call_site_offset);
    let mut vm_hooks = self.push_active_host_hooks_guard(hooks);
    vm_hooks.call_impl(host, scope, hooks, callee, this, args, Some(call_site))
  }

  fn push_roots_with_ticks(
    &mut self,
    scope: &mut Scope<'_>,
    values: &[Value],
  ) -> Result<(), VmError> {
    let mut start = 0;
    while start < values.len() {
      let end = values
        .len()
        .min(start.saturating_add(ARG_HANDLING_CHUNK_SIZE));
      let chunk = &values[start..end];
      let remaining = &values[end..];
      scope.push_roots_with_extra_roots(chunk, remaining, &[])?;
      start = end;
      if start < values.len() {
        self.tick()?;
      }
    }
    Ok(())
  }

  fn vec_extend_from_slice_with_ticks(
    &mut self,
    out: &mut Vec<Value>,
    values: &[Value],
  ) -> Result<(), VmError> {
    let mut start = 0;
    while start < values.len() {
      let end = values
        .len()
        .min(start.saturating_add(ARG_HANDLING_CHUNK_SIZE));
      out.extend_from_slice(&values[start..end]);
      start = end;
      if start < values.len() {
        self.tick()?;
      }
    }
    Ok(())
  }

  fn call_impl(
    &mut self,
    host: &mut dyn VmHost,
    scope: &mut Scope<'_>,
    hooks: &mut dyn VmHostHooks,
    callee: Value,
    this: Value,
    args: &[Value],
    call_site: Option<CallSite>,
  ) -> Result<Value, VmError> {
    // Establish an `ExecutionContext` derived from the callee when needed.
    //
    // Most calls happen with an existing execution context (from a script/module entry point), so
    // historically `vm-js` only synthesized one for Promise jobs / host callbacks where the stack is
    // empty. However, spec features like dynamic `import()` and `import.meta` consult
    // `GetActiveScriptOrModule`, which is determined by the *currently running execution context*.
    //
    // When calling a function imported from a different module, the callee's `[[ScriptOrModule]]`
    // must become active for the duration of the call so host module hooks observe the correct
    // referrer.
    if let Value::Object(obj) = callee {
      if let Some(realm) = scope.heap().get_function_job_realm(obj) {
        let script_or_module_token = scope.heap().get_function_script_or_module_token(obj);
        let script_or_module = self.resolve_script_or_module_token_opt(script_or_module_token);

        let need_ctx = self.current_realm().is_none()
          || self.current_realm() != Some(realm)
          || (script_or_module.is_some() && script_or_module != self.get_active_script_or_module());

        if need_ctx {
          let ctx = ExecutionContext { realm, script_or_module };
          let mut vm_ctx = self.execution_context_guard(ctx)?;
          let prev_state = vm_ctx.load_realm_state(scope.heap_mut(), realm)?;
          let result = vm_ctx.call_impl_inner(host, scope, hooks, callee, this, args, call_site);
          drop(vm_ctx);
          self.restore_realm_state(scope.heap_mut(), prev_state)?;
          return result;
        }
      }
    }

    self.call_impl_inner(host, scope, hooks, callee, this, args, call_site)
  }

  fn call_impl_inner(
    &mut self,
    host: &mut dyn VmHost,
    scope: &mut Scope<'_>,
    hooks: &mut dyn VmHostHooks,
    callee: Value,
    this: Value,
    args: &[Value],
    call_site: Option<CallSite>,
  ) -> Result<Value, VmError> {
    if let Some(call_site) = call_site.as_ref() {
      self.update_top_frame_location(call_site);
    }

    let mut scope = scope.reborrow();
    let callee_obj = match callee {
      Value::Object(obj) => obj,
      _ => {
        self.tick()?;
        let roots = [callee, this];
        scope.push_roots_with_extra_roots(&roots, args, &[])?;
        self.push_roots_with_ticks(&mut scope, args)?;
        return Err(coerce_error_to_throw(
          self,
          &mut scope,
          VmError::NotCallable,
        ));
      }
    };
    // --- Proxy [[Call]] dispatch ---
    //
    // Callable Proxy objects are not `HeapObject::Function`, so `get_function_call_handler` would
    // normally treat them as non-callable. Detect Proxy objects explicitly and follow the spec
    // algorithm:
    // - throw on revoked proxies
    // - if `handler.apply` exists, call it
    // - otherwise forward to `target`
    let proxy_data = match scope.heap().get_proxy_data(callee_obj) {
      Ok(d) => d,
      Err(e) => {
        self.tick()?;
        let roots = [callee, this];
        scope.push_roots_with_extra_roots(&roots, args, &[])?;
        self.push_roots_with_ticks(&mut scope, args)?;
        return Err(coerce_error_to_throw(self, &mut scope, e));
      }
    };
    if let Some(proxy) = proxy_data {
      // Construct a synthetic frame so revoked-proxy errors capture a stack.
      let frame = StackFrame {
        function: None,
        source: Arc::<str>::from("<proxy>"),
        line: 0,
        col: 0,
      };

      let mut vm = self.enter_frame(frame)?;
      vm.tick()?;

      // Root all inputs robustly; see the ordinary function call path below for rationale.
      let roots = [callee, this];
      scope.push_roots_with_extra_roots(&roots, args, &[])?;
      vm.push_roots_with_ticks(&mut scope, args)?;

      let (Some(target), Some(handler)) = (proxy.target, proxy.handler) else {
        let err = VmError::TypeError("Cannot perform 'apply' on a proxy that has been revoked");
        let err = coerce_error_to_throw(&vm, &mut scope, err);
        return match err {
          VmError::Throw(value) => Err(VmError::ThrowWithStack {
            value,
            stack: vm.capture_stack(),
          }),
          other => Err(other),
        };
      };
      // Root `target`/`handler` across trap lookup/invocation.
      //
      // `GetMethod(handler, "apply")` can invoke user JS via accessors. That JS can revoke this
      // Proxy (clearing its internal slots) and then trigger a GC, which would otherwise allow the
      // `target` object to be collected while this call operation still needs it.
      scope.push_roots(&[Value::Object(target), Value::Object(handler)])?;

      // Non-callable proxies do not have a `[[Call]]` internal method.
      if !scope.heap().is_callable(Value::Object(target))? {
        let err = coerce_error_to_throw(&vm, &mut scope, VmError::NotCallable);
        return match err {
          VmError::Throw(value) => Err(VmError::ThrowWithStack {
            value,
            stack: vm.capture_stack(),
          }),
          other => Err(other),
        };
      }

      let apply_key_s = scope.alloc_string("apply")?;
      scope.push_root(Value::String(apply_key_s))?;
      let apply_key = PropertyKey::from_string(apply_key_s);

      let trap = vm.get_method_with_host_and_hooks(host, &mut scope, hooks, Value::Object(handler), apply_key)?;
      let result = match trap {
        None => vm.call_with_host_and_hooks(
          host,
          &mut scope,
          hooks,
          Value::Object(target),
          this,
          args,
        ),
        Some(trap) => {
          let arg_array = crate::spec_ops::create_array_from_list(&mut vm, &mut scope, args)?;
          let trap_args = [Value::Object(target), this, Value::Object(arg_array)];
          vm.call_with_host_and_hooks(
            host,
            &mut scope,
            hooks,
            trap,
            Value::Object(handler),
            &trap_args,
          )
        }
      };

      // Capture a stack trace for thrown exceptions before the current frame is popped.
      let result = match result {
        Err(e) => Err(coerce_error_to_throw(&vm, &mut scope, e)),
        Ok(v) => Ok(v),
      };

      return match result {
        Err(VmError::Throw(value)) => Err(VmError::ThrowWithStack {
          value,
          stack: vm.capture_stack(),
        }),
        other => other,
      };
    }

    let call_handler = match scope.heap().get_function_call_handler(callee_obj) {
      Ok(h) => h,
      Err(e) => {
        self.tick()?;
        let roots = [callee, this];
        scope.push_roots_with_extra_roots(&roots, args, &[])?;
        self.push_roots_with_ticks(&mut scope, args)?;
        return Err(coerce_error_to_throw(self, &mut scope, e));
      }
    };

    let function_name = scope
      .heap()
      .get_function_name(callee_obj)
      .ok()
      .and_then(|name| scope.heap().get_string(name).ok())
      .and_then(|name| {
        // Best-effort: if UTF-16→UTF-8 conversion fails (OOM) or exceeds a small cap, drop the
        // function name rather than turning the throw into an OOM/abort.
        let (utf8, truncated) = crate::string::utf16_to_utf8_lossy_bounded(
          name.as_code_units(),
          MAX_STACK_FRAME_FUNCTION_NAME_BYTES,
        )
        .ok()?;
        if truncated || utf8.is_empty() {
          return None;
        }
        Arc::<str>::try_from(utf8).ok()
      });

    let (source, line, col) = if let Some(call_site) = call_site.as_ref() {
      (call_site.source.clone(), call_site.line, call_site.col)
    } else {
      match &call_handler {
        CallHandler::Native(_) => (Arc::<str>::from("<native>"), 0, 0),
        CallHandler::Ecma(code_id) => match self.ecma_functions.get(code_id.0 as usize) {
          Some(code) => {
            let (line, col) = code.source.line_col(code.span_start);
            (code.source.name.clone(), line, col)
          }
          None => (Arc::<str>::from("<call>"), 0, 0),
        },
        CallHandler::User(func) => {
          let source = func.script.source.name.clone();
          let (line, col) = func
            .script
            .hir
            .body(func.body)
            .map(|body| func.script.source.line_col(body.span.start))
            .unwrap_or((0, 0));
          (source, line, col)
        }
      }
    };
    let frame = StackFrame {
      function: function_name,
      source,
      line,
      col,
    };

    let mut vm = self.enter_frame(frame)?;
    vm.tick()?;

    // Root all inputs in a way that is robust against GC triggering while we grow the root stack.
    //
    // `push_root`/`push_roots` can trigger GC when growing `root_stack`, so ensure any not-yet-pushed
    // values are treated as extra roots during that operation.
    let roots = [callee, this];
    scope.push_roots_with_extra_roots(&roots, args, &[])?;
    vm.push_roots_with_ticks(&mut scope, args)?;

    // Bound function dispatch: if the callee has `[[BoundTargetFunction]]`, forward the call to
    // the target with the bound `this` and arguments.
    if let Ok(func) = scope.heap().get_function(callee_obj) {
      if let Some(bound_target) = func.bound_target {
        let bound_this = func.bound_this.unwrap_or(Value::Undefined);
        let bound_args = func.bound_args.as_deref().unwrap_or(&[]);

        let total_len = bound_args
          .len()
          .checked_add(args.len())
          .ok_or(VmError::OutOfMemory)?;
        let mut combined: Vec<Value> = Vec::new();
        combined
          .try_reserve_exact(total_len)
          .map_err(|_| VmError::OutOfMemory)?;
        vm.vec_extend_from_slice_with_ticks(&mut combined, bound_args)?;
        vm.vec_extend_from_slice_with_ticks(&mut combined, args)?;

        vm.push_roots_with_ticks(&mut scope, &combined)?;

        return vm.call_impl(
          host,
          &mut scope,
          hooks,
          Value::Object(bound_target),
          bound_this,
          &combined,
          call_site.clone(),
        );
      }
    }

    let result = match call_handler {
      CallHandler::Native(call_id) => {
        vm.dispatch_native_call(call_id, host, &mut scope, hooks, callee_obj, this, args)
      }
      CallHandler::Ecma(code_id) => {
        vm.call_ecma_function(&mut scope, host, hooks, code_id, callee_obj, this, args)
      }
      CallHandler::User(func) => vm.call_user_function(&mut scope, host, hooks, func, callee_obj, this, args),
    };

    // Capture a stack trace for thrown exceptions before the current frame is popped.
    let result = match result {
      Err(e) => Err(coerce_error_to_throw(&vm, &mut scope, e)),
      Ok(v) => Ok(v),
    };

    match result {
      Err(VmError::Throw(value)) => Err(VmError::ThrowWithStack {
        value,
        stack: vm.capture_stack(),
      }),
      other => other,
    }
  }

  /// ECMAScript `Get(O, P)`.
  ///
  /// ## ⚠️ Dummy `VmHost` context
  ///
  /// This convenience wrapper can invoke user JS via accessors but will pass a **dummy host
  /// context** (`()`) to any native call/construct handlers reached through those invocations.
  ///
  /// Embeddings that need native handlers to observe real host state should prefer
  /// [`Vm::get_with_host_and_hooks`].
  pub fn get(
    &mut self,
    scope: &mut Scope<'_>,
    obj: GcObject,
    key: PropertyKey,
  ) -> Result<Value, VmError> {
    // `Get(O, P)` uses `receiver = O`.
    scope.get(self, obj, key, Value::Object(obj))
  }

  /// ECMAScript `Get(O, P)` using an explicit embedder host context and host hook implementation.
  ///
  /// This dispatches to Proxy objects' `[[Get]]` algorithm when `obj` is a Proxy.
  pub fn get_with_host_and_hooks(
    &mut self,
    host: &mut dyn VmHost,
    scope: &mut Scope<'_>,
    hooks: &mut dyn VmHostHooks,
    obj: GcObject,
    key: PropertyKey,
  ) -> Result<Value, VmError> {
    scope.get_with_host_and_hooks(self, host, hooks, obj, key, Value::Object(obj))
  }

  /// ECMAScript `GetMethod(O, P)` where `O` is already known to be an object.
  pub fn get_method_from_object(
    &mut self,
    scope: &mut Scope<'_>,
    obj: GcObject,
    key: PropertyKey,
  ) -> Result<Option<Value>, VmError> {
    let value = self.get(scope, obj, key)?;
    if matches!(value, Value::Undefined | Value::Null) {
      return Ok(None);
    }

    if !scope.heap().is_callable(value)? {
      return Err(VmError::TypeError("GetMethod: target is not callable"));
    }
    Ok(Some(value))
  }

  /// ECMAScript `GetMethod(V, P)` (partial).
  ///
  /// Note: `GetMethod` uses `GetV`, which in turn uses `ToObject`. Full `ToObject` boxing semantics
  /// are implemented by boxing primitives via the intrinsic `Object` constructor.
  ///
  /// ## ⚠️ Dummy `VmHost` context
  ///
  /// This convenience wrapper can invoke user JS via accessors but will pass a **dummy host
  /// context** (`()`) to any native call/construct handlers reached through those invocations.
  ///
  /// Embeddings that need native handlers to observe real host state should prefer
  /// [`Vm::get_method_with_host_and_hooks`].
  pub fn get_method(
    &mut self,
    scope: &mut Scope<'_>,
    value: Value,
    key: PropertyKey,
  ) -> Result<Option<Value>, VmError> {
    // Root inputs for the duration of the operation: `GetV` can allocate when boxing primitives
    // (ToObject) and when invoking accessor getters.
    let mut scope = scope.reborrow();
    let key_root = match key {
      PropertyKey::String(s) => Value::String(s),
      PropertyKey::Symbol(s) => Value::Symbol(s),
    };
    scope.push_roots(&[value, key_root])?;

    // GetV(V, P): ToObject(V) then Get(O, P) with receiver = V.
    let receiver = value;
    let obj = match value {
      Value::Object(obj) => obj,
      Value::Undefined | Value::Null => {
        return Err(VmError::TypeError(
          "GetMethod: cannot convert null/undefined to object",
        ))
      }
      other => {
        let intr = self
          .intrinsics()
          .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
        let object_ctor = Value::Object(intr.object_constructor());
        scope.push_root(object_ctor)?;

        let boxed = self.call_without_host(&mut scope, object_ctor, Value::Undefined, &[other])?;
        let Value::Object(boxed_obj) = boxed else {
          return Err(VmError::InvariantViolation(
            "Object(..) conversion returned non-object",
          ));
        };
        scope.push_root(boxed)?;
        boxed_obj
      }
    };

    // GetMethod: callability checks and `null`/`undefined` normalization.
    let func = scope.get(self, obj, key, receiver)?;
    if matches!(func, Value::Undefined | Value::Null) {
      return Ok(None);
    }
    if !scope.heap().is_callable(func)? {
      return Err(VmError::TypeError("GetMethod: target is not callable"));
    }
    Ok(Some(func))
  }

  /// ECMAScript `GetMethod(V, P)` (partial), using an explicit embedder host context and host hook
  /// implementation.
  pub fn get_method_with_host_and_hooks(
    &mut self,
    host: &mut dyn VmHost,
    scope: &mut Scope<'_>,
    hooks: &mut dyn VmHostHooks,
    value: Value,
    key: PropertyKey,
  ) -> Result<Option<Value>, VmError> {
    crate::spec_ops::get_method_with_host_and_hooks(self, scope, host, hooks, value, key)
  }

  /// Constructs `callee` with the provided arguments and `new_target`.
  ///
  /// `host` is embedder-provided context (DOM/event loop/etc) forwarded to native constructor
  /// handlers.
  ///
  /// # Rooting
  ///
  /// The returned [`Value`] is **not automatically rooted**. If the caller will perform any
  /// additional allocations that could trigger GC, it must root the returned value itself (for
  /// example with `scope.push_root(result)`.
  ///
  /// This method roots `callee`, `new_target`, and `args` for the duration of construction using a
  /// temporary child [`Scope`].
  pub fn construct(
    &mut self,
    host: &mut dyn VmHost,
    scope: &mut Scope<'_>,
    callee: Value,
    args: &[Value],
    new_target: Value,
  ) -> Result<Value, VmError> {
    self.construct_entry(host, scope, callee, args, new_target, None)
  }

  pub fn construct_at_location(
    &mut self,
    host: &mut dyn VmHost,
    scope: &mut Scope<'_>,
    callee: Value,
    args: &[Value],
    new_target: Value,
    call_site_source: &SourceText,
    call_site_offset: u32,
  ) -> Result<Value, VmError> {
    let call_site = CallSite::from_source_offset(call_site_source, call_site_offset);
    self.construct_entry(host, scope, callee, args, new_target, Some(call_site))
  }

  fn construct_entry(
    &mut self,
    host: &mut dyn VmHost,
    scope: &mut Scope<'_>,
    callee: Value,
    args: &[Value],
    new_target: Value,
    call_site: Option<CallSite>,
  ) -> Result<Value, VmError> {
    if let Some(hooks_ptr) = self.host_hooks_override {
      // SAFETY: see `Vm::call` for the safety contract of `host_hooks_override`.
      let hooks = unsafe { &mut *hooks_ptr };
      return self.construct_impl(host, scope, hooks, callee, args, new_target, call_site);
    }

    let mut hooks = mem::take(&mut self.microtasks);
    let result = {
      let mut vm_hooks = self.push_active_host_hooks_guard(&mut hooks);
      vm_hooks.construct_impl(host, scope, &mut hooks, callee, args, new_target, call_site)
    };
    struct MergeCtx<'a> {
      heap: &'a mut Heap,
    }
    impl VmJobContext for MergeCtx<'_> {
      fn call(
        &mut self,
        _hooks: &mut dyn VmHostHooks,
        _callee: Value,
        _this: Value,
        _args: &[Value],
      ) -> Result<Value, VmError> {
        Err(VmError::Unimplemented("MergeCtx::call"))
      }

      fn construct(
        &mut self,
        _hooks: &mut dyn VmHostHooks,
        _callee: Value,
        _args: &[Value],
        _new_target: Value,
      ) -> Result<Value, VmError> {
        Err(VmError::Unimplemented("MergeCtx::construct"))
      }

      fn add_root(&mut self, value: Value) -> Result<RootId, VmError> {
        self.heap.add_root(value)
      }

      fn remove_root(&mut self, id: RootId) {
        self.heap.remove_root(id);
      }
    }

    let mut ctx = MergeCtx { heap: scope.heap_mut() };
    let mut merge_err: Option<VmError> = None;
    while let Some((realm, job)) = self.microtasks.pop_front() {
      if let Err(err) = hooks.host_enqueue_promise_job_fallible(&mut ctx, job, realm) {
        merge_err = Some(err);
        break;
      }
    }
    if merge_err.is_some() {
      self.microtasks.teardown(&mut ctx);
    }
    self.microtasks = hooks;
    if let Some(err) = merge_err {
      return Err(err);
    }
    result
  }

  /// Convenience wrapper around [`Vm::construct`] that passes a dummy host context (`()`).
  ///
  /// This uses the currently-active host hook implementation (either an embedder-installed host
  /// hooks override, or the VM-owned microtask queue).
  pub fn construct_without_host(
    &mut self,
    scope: &mut Scope<'_>,
    callee: Value,
    args: &[Value],
    new_target: Value,
  ) -> Result<Value, VmError> {
    let mut dummy_host = ();
    self.construct(&mut dummy_host, scope, callee, args, new_target)
  }

  /// Constructs `callee` with the provided arguments and `new_target`, using a custom host hook
  /// implementation.
  ///
  /// ## ⚠️ Dummy `VmHost` context
  ///
  /// This method does **not** accept an embedder [`VmHost`] and will always pass a dummy `VmHost`
  /// value (`()`) to native construct handlers.
  ///
  /// It exists primarily for engine internals and tests that need to run JS while supplying a
  /// custom [`VmHostHooks`] implementation (for example, to route `HostEnqueuePromiseJob` into a
  /// host-owned microtask queue), but do not have any embedder host state available.
  ///
  /// Embeddings that need native handlers to observe real host context (DOM/event loop/etc) should
  /// use:
  /// - [`Vm::construct_with_host_and_hooks`] (explicit host context + hooks), or
  /// - [`Vm::construct`] (explicit host context; uses the VM-owned microtask queue).
  pub fn construct_with_host(
    &mut self,
    scope: &mut Scope<'_>,
    host: &mut dyn VmHostHooks,
    callee: Value,
    args: &[Value],
    new_target: Value,
  ) -> Result<Value, VmError> {
    let mut dummy_host = ();
    self.construct_with_host_and_hooks(&mut dummy_host, scope, host, callee, args, new_target)
  }

  /// Constructs `callee` with the provided arguments and `new_target`, using an explicit embedder
  /// host context and host hook implementation.
  #[inline]
  pub fn construct_with_host_and_hooks(
    &mut self,
    host: &mut dyn VmHost,
    scope: &mut Scope<'_>,
    hooks: &mut dyn VmHostHooks,
    callee: Value,
    args: &[Value],
    new_target: Value,
  ) -> Result<Value, VmError> {
    let mut vm_hooks = self.push_active_host_hooks_guard(hooks);
    vm_hooks.construct_impl(host, scope, hooks, callee, args, new_target, None)
  }

  pub(crate) fn construct_with_host_and_hooks_at_location(
    &mut self,
    host: &mut dyn VmHost,
    scope: &mut Scope<'_>,
    hooks: &mut dyn VmHostHooks,
    callee: Value,
    args: &[Value],
    new_target: Value,
    call_site_source: &SourceText,
    call_site_offset: u32,
  ) -> Result<Value, VmError> {
    let call_site = CallSite::from_source_offset(call_site_source, call_site_offset);
    let mut vm_hooks = self.push_active_host_hooks_guard(hooks);
    vm_hooks.construct_impl(host, scope, hooks, callee, args, new_target, Some(call_site))
  }

  fn construct_impl(
    &mut self,
    host: &mut dyn VmHost,
    scope: &mut Scope<'_>,
    hooks: &mut dyn VmHostHooks,
    callee: Value,
    args: &[Value],
    new_target: Value,
    call_site: Option<CallSite>,
  ) -> Result<Value, VmError> {
    // Like `Vm::call_impl`, construct operations can be invoked from Promise jobs / host callbacks
    // without an active execution context. Additionally, constructing functions imported from other
    // modules should activate the callee's `[[ScriptOrModule]]` for the duration of the call so host
    // module hooks observe the correct referrer.
    if let Value::Object(obj) = callee {
      if let Some(realm) = scope.heap().get_function_job_realm(obj) {
        let script_or_module_token = scope.heap().get_function_script_or_module_token(obj);
        let script_or_module = self.resolve_script_or_module_token_opt(script_or_module_token);

        let need_ctx = self.current_realm().is_none()
          || self.current_realm() != Some(realm)
          || (script_or_module.is_some() && script_or_module != self.get_active_script_or_module());

        if need_ctx {
          let ctx = ExecutionContext { realm, script_or_module };
          let mut vm_ctx = self.execution_context_guard(ctx)?;
          let prev_state = vm_ctx.load_realm_state(scope.heap_mut(), realm)?;
          let result = vm_ctx.construct_impl_inner(host, scope, hooks, callee, args, new_target, call_site);
          drop(vm_ctx);
          self.restore_realm_state(scope.heap_mut(), prev_state)?;
          return result;
        }
      }
    }

    self.construct_impl_inner(host, scope, hooks, callee, args, new_target, call_site)
  }

  fn construct_impl_inner(
    &mut self,
    host: &mut dyn VmHost,
    scope: &mut Scope<'_>,
    hooks: &mut dyn VmHostHooks,
    callee: Value,
    args: &[Value],
    new_target: Value,
    call_site: Option<CallSite>,
  ) -> Result<Value, VmError> {
    if let Some(call_site) = call_site.as_ref() {
      self.update_top_frame_location(call_site);
    }

    let mut scope = scope.reborrow();
    let callee_obj = match callee {
      Value::Object(obj) => obj,
      _ => {
        self.tick()?;
        let roots = [callee, new_target];
        scope.push_roots_with_extra_roots(&roots, args, &[])?;
        self.push_roots_with_ticks(&mut scope, args)?;
        return Err(coerce_error_to_throw(
          self,
          &mut scope,
          VmError::NotConstructable,
        ));
      }
    };
    // --- Proxy [[Construct]] dispatch ---
    let proxy_data = match scope.heap().get_proxy_data(callee_obj) {
      Ok(d) => d,
      Err(e) => {
        self.tick()?;
        let roots = [callee, new_target];
        scope.push_roots_with_extra_roots(&roots, args, &[])?;
        self.push_roots_with_ticks(&mut scope, args)?;
        return Err(coerce_error_to_throw(self, &mut scope, e));
      }
    };
    if let Some(proxy) = proxy_data {
      let frame = StackFrame {
        function: None,
        source: Arc::<str>::from("<proxy>"),
        line: 0,
        col: 0,
      };

      let mut vm = self.enter_frame(frame)?;
      vm.tick()?;

      // Root all inputs robustly; see the ordinary construct path below for rationale.
      let roots = [callee, new_target];
      scope.push_roots_with_extra_roots(&roots, args, &[])?;
      vm.push_roots_with_ticks(&mut scope, args)?;

      let (Some(target), Some(handler)) = (proxy.target, proxy.handler) else {
        let err = VmError::TypeError("Cannot perform 'construct' on a proxy that has been revoked");
        let err = coerce_error_to_throw(&vm, &mut scope, err);
        return match err {
          VmError::Throw(value) => Err(VmError::ThrowWithStack {
            value,
            stack: vm.capture_stack(),
          }),
          other => Err(other),
        };
      };
      // Root `target`/`handler` across trap lookup/invocation; see comment in the Proxy `[[Call]]`
      // dispatch above.
      scope.push_roots(&[Value::Object(target), Value::Object(handler)])?;

      // Non-constructable proxies do not have a `[[Construct]]` internal method.
      if !scope.heap().is_constructor(Value::Object(target))? {
        let err = coerce_error_to_throw(&vm, &mut scope, VmError::NotConstructable);
        return match err {
          VmError::Throw(value) => Err(VmError::ThrowWithStack {
            value,
            stack: vm.capture_stack(),
          }),
          other => Err(other),
        };
      }

      let construct_key_s = scope.alloc_string("construct")?;
      scope.push_root(Value::String(construct_key_s))?;
      let construct_key = PropertyKey::from_string(construct_key_s);

      let trap = vm.get_method_with_host_and_hooks(
        host,
        &mut scope,
        hooks,
        Value::Object(handler),
        construct_key,
      )?;

      let result = match trap {
        None => vm.construct_with_host_and_hooks(
          host,
          &mut scope,
          hooks,
          Value::Object(target),
          args,
          new_target,
        ),
        Some(trap) => {
          let arg_array = crate::spec_ops::create_array_from_list(&mut vm, &mut scope, args)?;
          let trap_args = [Value::Object(target), Value::Object(arg_array), new_target];
          let new_obj = vm.call_with_host_and_hooks(
            host,
            &mut scope,
            hooks,
            trap,
            Value::Object(handler),
            &trap_args,
          )?;
          match new_obj {
            Value::Object(_) => Ok(new_obj),
            _ => Err(VmError::TypeError("Proxy construct trap returned non-object")),
          }
        }
      };

      // Capture a stack trace for thrown exceptions before the current frame is popped.
      let result = match result {
        Err(e) => Err(coerce_error_to_throw(&vm, &mut scope, e)),
        Ok(v) => Ok(v),
      };

      return match result {
        Err(VmError::Throw(value)) => Err(VmError::ThrowWithStack {
          value,
          stack: vm.capture_stack(),
        }),
        other => other,
      };
    }

    let construct_handler = match scope.heap().get_function_construct_handler(callee_obj) {
      Ok(Some(h)) => h,
      Ok(None) => {
        self.tick()?;
        let roots = [callee, new_target];
        scope.push_roots_with_extra_roots(&roots, args, &[])?;
        self.push_roots_with_ticks(&mut scope, args)?;
        return Err(coerce_error_to_throw(
          self,
          &mut scope,
          VmError::NotConstructable,
        ));
      }
      Err(e) => {
        self.tick()?;
        let roots = [callee, new_target];
        scope.push_roots_with_extra_roots(&roots, args, &[])?;
        self.push_roots_with_ticks(&mut scope, args)?;
        return Err(coerce_error_to_throw(self, &mut scope, e));
      }
    };

    let function_name = scope
      .heap()
      .get_function_name(callee_obj)
      .ok()
      .and_then(|name| scope.heap().get_string(name).ok())
      .and_then(|name| {
        let (utf8, truncated) = crate::string::utf16_to_utf8_lossy_bounded(
          name.as_code_units(),
          MAX_STACK_FRAME_FUNCTION_NAME_BYTES,
        )
        .ok()?;
        if truncated || utf8.is_empty() {
          return None;
        }
        Arc::<str>::try_from(utf8).ok()
      });

    let (source, line, col) = if let Some(call_site) = call_site.as_ref() {
      (call_site.source.clone(), call_site.line, call_site.col)
    } else {
      match &construct_handler {
        ConstructHandler::Native(_) => (Arc::<str>::from("<native>"), 0, 0),
        ConstructHandler::Ecma(code_id) => match self.ecma_functions.get(code_id.0 as usize) {
          Some(code) => {
            let (line, col) = code.source.line_col(code.span_start);
            (code.source.name.clone(), line, col)
          }
          None => (Arc::<str>::from("<call>"), 0, 0),
        },
        ConstructHandler::User => {
          let call_handler = scope.heap().get_function_call_handler(callee_obj)?;
          let CallHandler::User(func) = call_handler else {
            return Err(VmError::InvariantViolation(
              "ConstructHandler::User used on non-user function",
            ));
          };
          let source = func.script.source.name.clone();
          let (line, col) = func
            .script
            .hir
            .body(func.body)
            .map(|body| func.script.source.line_col(body.span.start))
            .unwrap_or((0, 0));
          (source, line, col)
        }
      }
    };
    let frame = StackFrame {
      function: function_name,
      source,
      line,
      col,
    };

    let mut vm = self.enter_frame(frame)?;
    vm.tick()?;

    // Root all inputs robustly; see `Vm::call` for rationale.
    let roots = [callee, new_target];
    scope.push_roots_with_extra_roots(&roots, args, &[])?;
    vm.push_roots_with_ticks(&mut scope, args)?;

    // Bound function dispatch: if the callee has `[[BoundTargetFunction]]`, forward construction to
    // the target with concatenated arguments.
    if let Ok(func) = scope.heap().get_function(callee_obj) {
      if let Some(bound_target) = func.bound_target {
        let bound_args = func.bound_args.as_deref().unwrap_or(&[]);

        let total_len = bound_args
          .len()
          .checked_add(args.len())
          .ok_or(VmError::OutOfMemory)?;
        let mut combined: Vec<Value> = Vec::new();
        combined
          .try_reserve_exact(total_len)
          .map_err(|_| VmError::OutOfMemory)?;
        vm.vec_extend_from_slice_with_ticks(&mut combined, bound_args)?;
        vm.vec_extend_from_slice_with_ticks(&mut combined, args)?;

        // ECMA-262: if `new_target` is the bound function itself, forward `new_target` as the
        // target function.
        let forwarded_new_target = if new_target == callee {
          Value::Object(bound_target)
        } else {
          new_target
        };

        vm.push_roots_with_ticks(&mut scope, &combined)?;

        return vm.construct_impl(
          host,
          &mut scope,
          hooks,
          Value::Object(bound_target),
          &combined,
          forwarded_new_target,
          call_site.clone(),
        );
      }
    }

    let result = match construct_handler {
      ConstructHandler::Native(construct_id) => vm.dispatch_native_construct(
        construct_id,
        host,
        &mut scope,
        hooks,
        callee_obj,
        args,
        new_target,
      ),
      ConstructHandler::Ecma(code_id) => vm.construct_ecma_function(
        &mut scope, host, hooks, code_id, callee_obj, args, new_target,
      ),
      ConstructHandler::User => {
        let call_handler = scope.heap().get_function_call_handler(callee_obj)?;
        let CallHandler::User(func) = call_handler else {
          return Err(VmError::InvariantViolation(
            "ConstructHandler::User used on non-user function",
          ));
        };
        vm.construct_user_function(&mut scope, host, hooks, func, callee_obj, args, new_target)
      }
    };

    // Capture a stack trace for thrown exceptions before the current frame is popped.
    let result = match result {
      Err(e) => Err(coerce_error_to_throw(&vm, &mut scope, e)),
      Ok(v) => Ok(v),
    };

    match result {
      Err(VmError::Throw(value)) => Err(VmError::ThrowWithStack {
        value,
        stack: vm.capture_stack(),
      }),
      other => other,
    }
  }

  fn call_ecma_function(
    &mut self,
    scope: &mut Scope<'_>,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    code_id: EcmaFunctionId,
    callee: crate::GcObject,
    this: Value,
    args: &[Value],
  ) -> Result<Value, VmError> {
    let (this_mode, is_strict, realm, outer, bound_this, bound_new_target, meta_property_context) = {
      let f = scope.heap().get_function(callee)?;
      (
        f.this_mode,
        f.is_strict,
        f.realm,
        f.closure_env,
        f.bound_this,
        f.bound_new_target,
        f.meta_property_context,
      )
    };

    let this = match this_mode {
      ThisMode::Lexical => bound_this.ok_or(VmError::Unimplemented(
        "arrow function missing captured lexical this",
      ))?,
      ThisMode::Strict => this,
      ThisMode::Global => match this {
        Value::Undefined | Value::Null => match realm {
          Some(global) => Value::Object(global),
          None => this,
        },
        Value::Object(_) => this,
        other => {
          // Sloppy-mode `this` binding: primitives are boxed via `ToObject`.
          //
          // This matches ECMA-262 `ThisMode::Global` semantics (ES5 `thisArg` conversion):
          // - `null`/`undefined` become the Realm's global object
          // - primitives become their wrapper objects (Number/String/Boolean/Symbol/BigInt)
          let boxed = scope.to_object(self, host, hooks, other)?;
          let boxed = Value::Object(boxed);
          scope.push_root(boxed)?;
          boxed
        }
      },
    };

    let new_target = match this_mode {
      ThisMode::Lexical => bound_new_target.ok_or(VmError::Unimplemented(
        "arrow function missing captured lexical new.target",
      ))?,
      ThisMode::Strict | ThisMode::Global => Value::Undefined,
    };

    let global_object = realm.ok_or(VmError::Unimplemented(
      "ECMAScript function missing [[Realm]]",
    ))?;

    let func_ast = self.ecma_function_ast(scope.heap_mut(), code_id)?;
    let code_meta = self
      .ecma_functions
      .get(code_id.0 as usize)
      .ok_or_else(|| VmError::invalid_handle())?;

    let func_env = scope.env_create(outer)?;
    let mut env =
      RuntimeEnv::new_with_var_env(scope.heap_mut(), global_object, func_env, func_env)?;
    env.set_meta_property_context(meta_property_context);

    let is_async = func_ast.stx.async_ && !func_ast.stx.generator;
    let result = crate::exec::run_ecma_function(
      self,
      scope,
      host,
      hooks,
      callee,
      &mut env,
      code_meta.source.clone(),
      code_meta.span_start,
      code_meta.prefix_len,
      is_strict,
      this,
      new_target,
      func_ast.clone(),
      args,
    );

    if !is_async {
      env.teardown(scope.heap_mut());
    }
    match result {
      Ok((value, _final_this)) => Ok(value),
      Err(err) => Err(err),
    }
  }

  fn call_user_function(
    &mut self,
    scope: &mut Scope<'_>,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    func: crate::CompiledFunctionRef,
    callee: crate::GcObject,
    this: Value,
    args: &[Value],
  ) -> Result<Value, VmError> {
    let (
      this_mode,
      is_strict,
      realm,
      outer,
      bound_this,
      bound_new_target,
      func_data,
      meta_property_context,
    ) = {
      let f = scope.heap().get_function(callee)?;
      (
        f.this_mode,
        f.is_strict,
        f.realm,
        f.closure_env,
        f.bound_this,
        f.bound_new_target,
        f.data,
        f.meta_property_context,
      )
    };

    // User functions may be allocated without a realm in unit tests / low-level embeddings. To
    // preserve sloppy-mode `this` semantics and allow identifier resolution to fall back to a global
    // object, synthesize a minimal global object if needed.
    //
    // If intrinsics have been initialized (i.e. a `Realm::new` has run), prefer the realm's global
    // object instead of creating a fresh empty object: that ensures standard globals like `Symbol`
    // exist even when a function object was allocated without explicit `[[Realm]]` metadata.
    let global_object = match realm {
      Some(obj) => obj,
      None => {
        let mut inferred_global: Option<GcObject> = None;
        if let Some(intr) = self.intrinsics() {
          // Recover the realm global object from the intrinsic `%Object%` constructor's `[[Realm]]`
          // metadata.
          inferred_global = scope
            .heap()
            .get_function(intr.object_constructor())
            .ok()
            .and_then(|f| f.realm);
        }

        if let Some(global_object) = inferred_global {
          scope
            .heap_mut()
            .set_function_realm(callee, global_object)?;
          global_object
        } else {
          let mut init_scope = scope.reborrow();
          init_scope.push_root(Value::Object(callee))?;
          let global_object = init_scope.alloc_object()?;
          init_scope.push_root(Value::Object(global_object))?;
          // When running without a full Realm, we still need a minimal global object for sloppy-mode
          // `this` binding and unresolvable identifier lookups. Provide the standard `undefined`
          // global so compiled code can evaluate expressions like `...undefined` without throwing.
          let undefined_key_s = init_scope.alloc_string("undefined")?;
          init_scope.push_root(Value::String(undefined_key_s))?;
          let undefined_key = PropertyKey::from_string(undefined_key_s);
          init_scope.define_property(
            global_object,
            undefined_key,
            PropertyDescriptor {
              enumerable: false,
              configurable: false,
              kind: PropertyKind::Data {
                value: Value::Undefined,
                writable: false,
              },
            },
          )?;
          init_scope.heap_mut().set_function_realm(callee, global_object)?;
          global_object
        }
      }
    };

    // Some compiled functions may be executed via the AST interpreter when the compiled (HIR)
    // executor does not yet support a feature (call-time fallback). These are still allocated as
    // compiled user functions (`CallHandler::User`) so surrounding compiled script execution can
    // proceed without falling back to the interpreter for the entire script.
    if let FunctionData::EcmaFallback { code_id } | FunctionData::AsyncEcmaFallback { code_id } = func_data {
      // Ensure the function has a realm/global object set (see `global_object` synthesis above).
      return self.call_ecma_function(scope, host, hooks, code_id, callee, this, args);
    }

    let this = match this_mode {
      ThisMode::Lexical => bound_this.ok_or(VmError::Unimplemented(
        "arrow function missing captured lexical this",
      ))?,
      ThisMode::Strict => this,
      ThisMode::Global => match this {
        Value::Undefined | Value::Null => Value::Object(global_object),
        Value::Object(_) => this,
        other => {
          // Sloppy-mode `this` binding: primitives are boxed via `ToObject`.
          let boxed = scope.to_object(self, host, hooks, other)?;
          let boxed = Value::Object(boxed);
          scope.push_root(boxed)?;
          boxed
        }
      },
    };

    let new_target = match this_mode {
      ThisMode::Lexical => bound_new_target.ok_or(VmError::Unimplemented(
        "arrow function missing captured lexical new.target",
      ))?,
      ThisMode::Strict | ThisMode::Global => Value::Undefined,
    };

    let home_object = scope.heap().get_function_home_object(callee)?;

    let func_env = scope.env_create(outer)?;
    let mut env = RuntimeEnv::new_with_var_env(scope.heap_mut(), global_object, func_env, func_env)?;
    env.set_meta_property_context(meta_property_context);

    let is_async = func
      .script
      .hir
      .body(func.body)
      .and_then(|b| b.function.as_ref())
      // Treat only async non-generator functions as "async" for teardown purposes. HIR generator
      // support (including async generators) is not implemented, so generator calls must still tear
      // down eagerly.
      .map(|m| m.async_ && !m.generator)
      .ok_or(VmError::InvariantViolation(
        "compiled function body missing metadata",
      ))?;

    let result = crate::hir_exec::run_compiled_function(
      self,
      scope,
      host,
      hooks,
      &mut env,
      func,
      is_strict,
      this,
      /* this_initialized */ true,
      new_target,
      home_object,
      args,
      /* class_constructor */ None,
      /* derived_constructor */ false,
      /* this_root_idx */ None,
    );

    // Compiled async functions transfer ownership of their environment into an async continuation
    // when they suspend. In that case, the continuation is responsible for tearing down the env.
    //
    // Synchronous compiled functions still tear down eagerly.
    if !is_async {
      env.teardown(scope.heap_mut());
    }
    result
  }

  fn construct_user_function(
    &mut self,
    scope: &mut Scope<'_>,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    func: crate::CompiledFunctionRef,
    callee: crate::GcObject,
    args: &[Value],
    new_target: Value,
  ) -> Result<Value, VmError> {
    let (is_strict, realm, outer, func_data, meta_property_context) = {
      let f = scope.heap().get_function(callee)?;
      (f.is_strict, f.realm, f.closure_env, f.data, f.meta_property_context)
    };

    // See `call_user_function`: user functions can exist without a realm in low-level embeddings /
    // unit tests. Synthesize one if needed so sloppy-mode semantics and global identifier fallback
    // remain usable.
    let global_object = match realm {
      Some(obj) => obj,
      None => {
        let mut init_scope = scope.reborrow();
        init_scope.push_root(Value::Object(callee))?;
        let global_object = init_scope.alloc_object()?;
        init_scope.push_root(Value::Object(global_object))?;
        init_scope.heap_mut().set_function_realm(callee, global_object)?;
        global_object
      }
    };

    // Derived class constructors (`class D extends B { constructor(){ super(); } }`) do not
    // allocate `this` up-front; `this` is initialized by `super()` (i.e. by constructing the
    // superclass with `newTarget`).
    let (class_constructor, derived_constructor) = match func_data {
      FunctionData::ClassConstructorBody { class_constructor } => {
        let super_value =
          crate::class_fields::class_constructor_super_value(scope, class_constructor)?;
        (Some(class_constructor), !matches!(super_value, Value::Undefined))
      }
      _ => (None, false),
    };

    let mut this_scope = scope.reborrow();
    this_scope.push_root(new_target)?;

    let (this_value, this_initialized, this_root_idx, this_obj) = if derived_constructor {
      // Reserve a root-stack slot for the derived constructor `this` value.
      //
      // `this` is initialized by `super()`, but the evaluator's `this` field is not traced by GC, so
      // `super()` must update this root slot once it returns.
      let this_root_idx = this_scope.heap().root_stack.len();
      this_scope.push_root(Value::Undefined)?;
      (Value::Undefined, false, Some(this_root_idx), None)
    } else {
      // GetPrototypeFromConstructor(newTarget, %Object.prototype%), but tolerate missing intrinsics
      // by falling back to the heap's best-effort default prototype.
      let proto = match new_target {
        Value::Object(new_target_obj) => {
          let default_proto = this_scope.heap().default_object_prototype();

          let mut proto_scope = this_scope.reborrow();
          proto_scope.push_root(Value::Object(new_target_obj))?;
          if let Some(default_proto) = default_proto {
            proto_scope.push_root(Value::Object(default_proto))?;
          }

          let key_s = proto_scope.alloc_string("prototype")?;
          proto_scope.push_root(Value::String(key_s))?;
          let key = PropertyKey::from_string(key_s);
          let proto_val = proto_scope.get_with_host_and_hooks(
            self,
            host,
            hooks,
            new_target_obj,
            key,
            Value::Object(new_target_obj),
          )?;
          match proto_val {
            Value::Object(o) => Some(o),
            _ => default_proto,
          }
        }
        _ => this_scope.heap().default_object_prototype(),
      };

      let this_obj = this_scope.alloc_object_with_prototype(proto)?;
      this_scope.push_root(Value::Object(this_obj))?;
      (Value::Object(this_obj), true, None, Some(this_obj))
    };

    let func_env = this_scope.env_create(outer)?;
    let mut env =
      RuntimeEnv::new_with_var_env(this_scope.heap_mut(), global_object, func_env, func_env)?;
    env.set_meta_property_context(meta_property_context);

    let home_object = this_scope.heap().get_function_home_object(callee)?;
    let is_async = func
      .script
      .hir
      .body(func.body)
      .and_then(|b| b.function.as_ref())
      .map(|m| m.async_)
      .ok_or(VmError::InvariantViolation(
        "compiled function body missing metadata",
      ))?;

    let result = crate::hir_exec::run_compiled_function(
      self,
      &mut this_scope,
      host,
      hooks,
      &mut env,
      func,
      is_strict,
      this_value,
      this_initialized,
      new_target,
      home_object,
      args,
      class_constructor,
      derived_constructor,
      this_root_idx,
    );

    if !is_async {
      env.teardown(this_scope.heap_mut());
    }

    let return_value = result?;
    match return_value {
      // ECMA-262: if the constructor explicitly returns an object, that becomes the result of
      // construction (regardless of constructor kind).
      Value::Object(o) => Ok(Value::Object(o)),
      // `return;` / no explicit return / `return undefined;` -> return `this`.
      //
      // Derived constructors are special only in that they may still have an uninitialized `this`
      // binding if `super()` was never called; in that case, returning `undefined` must throw a
      // ReferenceError.
      Value::Undefined => {
        if derived_constructor {
          let this_root_idx = this_root_idx.ok_or(VmError::InvariantViolation(
            "derived constructor missing this root slot",
          ))?;
          match this_scope
            .heap()
            .root_stack
            .get(this_root_idx)
            .copied()
            .unwrap_or(Value::Undefined)
          {
            Value::Object(o) => Ok(Value::Object(o)),
            // For derived constructors, `return;` (or no explicit return) yields `undefined` and
            // therefore returns `this` instead. If `super()` was never called, `this` is
            // uninitialized and this must throw a ReferenceError.
            _ => {
              let intr = self
                .intrinsics()
                .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
              let err = crate::new_reference_error(
                &mut this_scope,
                intr,
                "Derived constructor did not initialize `this` via super()",
              )?;
              Err(VmError::Throw(err))
            }
          }
        } else {
          // Base/ordinary constructors always allocate `this` up-front.
          let this_obj = this_obj.ok_or(VmError::InvariantViolation(
            "base constructor missing allocated this object",
          ))?;
          Ok(Value::Object(this_obj))
        }
      },
      // Derived constructors may only return an object or `undefined`. Any other explicit non-object
      // return value must throw a TypeError.
      _ if derived_constructor => Err(VmError::TypeError(
        "Derived constructors may only return an object or undefined",
      )),
      // Base/ordinary constructors ignore explicit non-object return values.
      _ => {
        let this_obj = this_obj.ok_or(VmError::InvariantViolation(
          "base constructor missing allocated this object",
        ))?;
        Ok(Value::Object(this_obj))
      }
    }
  }

  fn construct_ecma_function(
    &mut self,
    scope: &mut Scope<'_>,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    code_id: EcmaFunctionId,
    callee: crate::GcObject,
    args: &[Value],
    new_target: Value,
  ) -> Result<Value, VmError> {
    let (is_strict, global_object, outer, func_data, meta_property_context) = {
      let f = scope.heap().get_function(callee)?;
      (
        f.is_strict,
        f.realm.ok_or(VmError::Unimplemented(
          "ECMAScript function missing [[Realm]]",
        ))?,
        f.closure_env,
        f.data,
        f.meta_property_context,
      )
    };

    // Derived class constructors (`class D extends B { constructor(){ super(); } }`) do not
    // allocate `this` up-front; `this` is initialized by `super()` (i.e. by constructing the
    // superclass with `newTarget`).
    let is_derived_class_ctor_body = match func_data {
      FunctionData::ClassConstructorBody { class_constructor } => {
        let super_value =
          crate::class_fields::class_constructor_super_value(scope, class_constructor)?;
        !matches!(super_value, Value::Undefined)
      }
      _ => false,
    };

    let mut this_scope = scope.reborrow();
    let this_value = if is_derived_class_ctor_body {
      Value::Undefined
    } else {
      // Base/ordinary constructor: allocate `this` via `GetPrototypeFromConstructor`.
      let default_proto = self
        .intrinsics()
        .ok_or(VmError::Unimplemented("intrinsics not initialized"))?
        .object_prototype();
      let proto = crate::spec_ops::get_prototype_from_constructor_with_host_and_hooks(
        self,
        &mut this_scope,
        host,
        hooks,
        new_target,
        default_proto,
      )?;
      let this_obj = this_scope.alloc_object_with_prototype(Some(proto))?;
      this_scope.push_root(Value::Object(this_obj))?;
      Value::Object(this_obj)
    };

    let func_env = this_scope.env_create(outer)?;
    let mut env =
      RuntimeEnv::new_with_var_env(this_scope.heap_mut(), global_object, func_env, func_env)?;
    env.set_meta_property_context(meta_property_context);

    let func_ast = self.ecma_function_ast(this_scope.heap_mut(), code_id)?;
    let code_meta = self
      .ecma_functions
      .get(code_id.0 as usize)
      .ok_or_else(|| VmError::invalid_handle())?;

    let result = crate::exec::run_ecma_function(
      self,
      &mut this_scope,
      host,
      hooks,
      callee,
      &mut env,
      code_meta.source.clone(),
      code_meta.span_start,
      code_meta.prefix_len,
      is_strict,
      this_value,
      new_target,
      func_ast.clone(),
      args,
    );

    env.teardown(this_scope.heap_mut());

    let (return_value, final_this) = result?;
    // Constructors have special return-value semantics:
    // - If the constructor returns an Object, that becomes the result of construction.
    // - Otherwise the result is `this`.
    //
    // Derived constructors have additional constraints:
    // - If they return `undefined`, the result is `this` (which must have been initialized via
    //   `super()`).
    // - If they return any other non-object value (including `null`), it is a TypeError.
    if is_derived_class_ctor_body {
      match return_value {
        // ECMA-262: if the constructor explicitly returns an object, that becomes the result of
        // construction (regardless of whether `this` was initialized).
        Value::Object(o) => Ok(Value::Object(o)),
        // `return;` / no explicit return.
        //
        // Per ECMA-262, constructors yield `this` when returning `undefined`. Derived constructors
        // are special in that `this` is uninitialized until `super()` returns.
        Value::Undefined => match final_this {
          Value::Object(o) => Ok(Value::Object(o)),
          _ => {
            let intr = self
              .intrinsics()
              .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
            let err = crate::new_reference_error(
              &mut this_scope,
              intr,
              "Derived constructor did not initialize `this` via super()",
            )?;
            Err(VmError::Throw(err))
          }
        },
        // Derived constructors are not allowed to return non-object, non-undefined values.
        _ => Err(VmError::TypeError(
          "Derived constructors may only return an object or undefined",
        )),
      }
    } else {
      match return_value {
        // ECMA-262: if the constructor explicitly returns an object, that becomes the result of
        // construction.
        Value::Object(o) => Ok(Value::Object(o)),

        // Base/ordinary constructors ignore non-object return values and instead yield `this`.
        _ => match final_this {
          Value::Object(o) => Ok(Value::Object(o)),
          _ => Err(VmError::InvariantViolation(
            "constructor did not produce an object `this`",
          )),
        },
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::Job;
  use crate::JobKind;
  use crate::Value;
  use crate::WeakGcObject;
  use std::sync::atomic::{AtomicBool, Ordering};
  use std::sync::Arc;

  fn value_to_string(rt: &crate::JsRuntime, value: Value) -> String {
    let Value::String(s) = value else {
      panic!("expected string, got {value:?}");
    };
    rt.heap.get_string(s).unwrap().to_utf8_lossy()
  }

  fn new_runtime() -> crate::JsRuntime {
    let vm = Vm::new(VmOptions::default());
    // Match other tests: keep heap reasonably small to exercise GC paths without being flaky.
    let heap = Heap::new(crate::HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
    crate::JsRuntime::new(vm, heap).unwrap()
  }

  fn noop_call(
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

  fn noop_construct(
    _vm: &mut Vm,
    _scope: &mut Scope<'_>,
    _host: &mut dyn VmHost,
    _hooks: &mut dyn VmHostHooks,
    _callee: GcObject,
    _args: &[Value],
    _new_target: Value,
  ) -> Result<Value, VmError> {
    Ok(Value::Undefined)
  }

  fn panic_call(
    _vm: &mut Vm,
    _scope: &mut Scope<'_>,
    _host: &mut dyn VmHost,
    _hooks: &mut dyn VmHostHooks,
    _callee: GcObject,
    _this: Value,
    _args: &[Value],
  ) -> Result<Value, VmError> {
    panic!("host call handler panicked");
  }

  fn panic_construct(
    _vm: &mut Vm,
    _scope: &mut Scope<'_>,
    _host: &mut dyn VmHost,
    _hooks: &mut dyn VmHostHooks,
    _callee: GcObject,
    _args: &[Value],
    _new_target: Value,
  ) -> Result<Value, VmError> {
    panic!("host construct handler panicked");
  }

  #[test]
  fn max_stack_depth_exceeded_surfaces_as_catchable_range_error() -> Result<(), VmError> {
    fn new_runtime(max_stack_depth: usize) -> Result<crate::JsRuntime, VmError> {
      let mut opts = VmOptions::default();
      opts.max_stack_depth = max_stack_depth;
      let vm = Vm::new(opts);
      let heap = Heap::new(crate::HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
      crate::JsRuntime::new(vm, heap)
    }

    let max_stack_depth = 16;

    // Uncaught recursion should surface to the host as a thrown RangeError, not a termination.
    let mut rt = new_runtime(max_stack_depth)?;
    let err = rt
      .exec_script("function f(){ return f(); } f();")
      .expect_err("expected recursion to throw");
    let VmError::ThrowWithStack { value, stack } = err else {
      panic!("expected ThrowWithStack, got: {err:?}");
    };
    assert_eq!(stack.len(), max_stack_depth);

    let Value::Object(obj) = value else {
      panic!("expected error object, got: {value:?}");
    };

    // Verify it's a RangeError by reading own `name` data property (no user code).
    let mut scope = rt.heap.scope();
    scope.push_root(Value::Object(obj))?;
    let key_s = scope.alloc_string("name")?;
    scope.push_root(Value::String(key_s))?;
    let key = crate::PropertyKey::from_string(key_s);
    let name_val = scope
      .heap()
      .object_get_own_data_property_value(obj, &key)?
      .expect("error.name should exist");
    let Value::String(name_s) = name_val else {
      panic!("expected error.name to be a string, got {name_val:?}");
    };
    let name = scope.heap().get_string(name_s)?.to_utf8_lossy();
    assert_eq!(name, "RangeError");

    // The error should also be catchable inside JS.
    let mut rt = new_runtime(max_stack_depth)?;
    let ok = rt.exec_script(
      r#"
      var ok = false;
      function f(){ return f(); }
      try { f(); } catch (e) { ok = !!(e && e.name === 'RangeError'); }
      ok
      "#,
    )?;
    assert_eq!(ok, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn active_host_hooks_guard_restores_override_on_early_return() {
    use crate::microtasks::MicrotaskQueue;

    fn install_then_fail(vm: &mut Vm, hooks: &mut dyn VmHostHooks) -> Result<(), VmError> {
      let _guard = vm.push_active_host_hooks_guard(hooks);
      // Use `?` so we exercise early-return (and therefore Drop) behavior.
      Err(VmError::OutOfMemory)?;
      Ok(())
    }

    let mut vm = Vm::new(VmOptions::default());
    let mut hooks = MicrotaskQueue::new();
    assert_eq!(vm.active_host_hooks_ptr(), None);

    let err = install_then_fail(&mut vm, &mut hooks).unwrap_err();
    assert!(matches!(err, VmError::OutOfMemory));

    // The hooks override stores a raw pointer; ensure it is not left dangling after the early
    // return.
    assert_eq!(vm.active_host_hooks_ptr(), None);
  }

  #[test]
  fn registering_too_many_native_handlers_returns_error_instead_of_panicking() {
    // Only meaningful on platforms where `usize` can exceed `u32::MAX`.
    if usize::BITS <= 32 {
      return;
    }

    let mut vm = Vm::new(VmOptions::default());
    // Avoid allocating huge vectors; this only affects the `u32::try_from(len)` conversion.
    vm.native_calls_len_override = Some((u32::MAX as usize) + 1);
    vm.native_constructs_len_override = Some((u32::MAX as usize) + 1);

    let err = vm.register_native_call(noop_call).unwrap_err();
    assert!(matches!(err, VmError::LimitExceeded(_)));

    let err = vm.register_native_construct(noop_construct).unwrap_err();
    assert!(matches!(err, VmError::LimitExceeded(_)));

    // Ensure no handlers were recorded.
    assert_eq!(vm.native_calls.len(), 0);
    assert_eq!(vm.native_constructs.len(), 0);
  }

  #[test]
  fn intrinsics_do_not_register_duplicate_native_calls() -> Result<(), VmError> {
    fn count_native_call(vm: &Vm, f: NativeCall) -> usize {
      vm.native_calls
        .iter()
        .filter(|&&call| std::ptr::fn_addr_eq(call, f))
        .count()
    }

    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(crate::HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
    let rt = crate::JsRuntime::new(vm, heap)?;

    assert_eq!(count_native_call(&rt.vm, crate::builtins::array_prototype_at), 1);
    assert_eq!(count_native_call(&rt.vm, crate::builtins::promise_all), 1);
    assert_eq!(count_native_call(&rt.vm, crate::builtins::promise_race), 1);
    assert_eq!(
      count_native_call(&rt.vm, crate::builtins::promise_all_settled),
      1
    );
    assert_eq!(count_native_call(&rt.vm, crate::builtins::promise_any), 1);
    assert_eq!(count_native_call(&rt.vm, crate::builtins::promise_species_get), 1);

    Ok(())
  }

  #[test]
  fn async_from_sync_iterator_internal_native_call_ids_are_cached() -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());

    let next_first = vm.async_from_sync_iterator_next_call_id()?;
    let next_len = vm.native_calls.len();
    let next_second = vm.async_from_sync_iterator_next_call_id()?;
    assert_eq!(next_first, next_second);
    assert_eq!(next_len, vm.native_calls.len());

    let return_first = vm.async_from_sync_iterator_return_call_id()?;
    let return_len = vm.native_calls.len();
    let return_second = vm.async_from_sync_iterator_return_call_id()?;
    assert_eq!(return_first, return_second);
    assert_eq!(return_len, vm.native_calls.len());

    let throw_first = vm.async_from_sync_iterator_throw_call_id()?;
    let throw_len = vm.native_calls.len();
    let throw_second = vm.async_from_sync_iterator_throw_call_id()?;
    assert_eq!(throw_first, throw_second);
    assert_eq!(throw_len, vm.native_calls.len());

    let unwrap_first = vm.async_from_sync_iterator_unwrap_call_id()?;
    let unwrap_len = vm.native_calls.len();
    let unwrap_second = vm.async_from_sync_iterator_unwrap_call_id()?;
    assert_eq!(unwrap_first, unwrap_second);
    assert_eq!(unwrap_len, vm.native_calls.len());

    let close_first = vm.async_from_sync_iterator_close_call_id()?;
    let close_len = vm.native_calls.len();
    let close_second = vm.async_from_sync_iterator_close_call_id()?;
    assert_eq!(close_first, close_second);
    assert_eq!(close_len, vm.native_calls.len());

    let close_fulfilled_first = vm.async_iterator_close_on_fulfilled_call_id()?;
    let close_fulfilled_len = vm.native_calls.len();
    let close_fulfilled_second = vm.async_iterator_close_on_fulfilled_call_id()?;
    assert_eq!(close_fulfilled_first, close_fulfilled_second);
    assert_eq!(close_fulfilled_len, vm.native_calls.len());

    let close_rejected_first = vm.async_iterator_close_on_rejected_call_id()?;
    let close_rejected_len = vm.native_calls.len();
    let close_rejected_second = vm.async_iterator_close_on_rejected_call_id()?;
    assert_eq!(close_rejected_first, close_rejected_second);
    assert_eq!(close_rejected_len, vm.native_calls.len());
    Ok(())
  }

  #[test]
  fn native_call_panics_are_caught_and_reported_as_invariant_violation() -> Result<(), VmError> {
    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(crate::HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut rt = crate::JsRuntime::new(vm, heap)?;

    rt.register_global_native_function("panicFn", panic_call, 0)?;

    let err = rt
      .exec_script(r#"try { panicFn(); "ok"; } catch (e) { "caught"; }"#)
      .unwrap_err();

    assert!(
      !err.is_throw_completion(),
      "native call panic should surface as a non-throw fatal error, got {err:?}"
    );

    match err {
      VmError::InvariantViolation(msg) => assert_eq!(msg, "native call panicked"),
      other => panic!("expected invariant violation, got {other:?}"),
    }

    Ok(())
  }

  #[test]
  fn native_construct_panics_are_caught_and_reported_as_invariant_violation() -> Result<(), VmError> {
    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(crate::HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut rt = crate::JsRuntime::new(vm, heap)?;

    let call_id = rt.vm.register_native_call(noop_call)?;
    let construct_id = rt.vm.register_native_construct(panic_construct)?;

    let global = rt.realm().global_object();
    {
      let mut scope = rt.heap.scope();
      let name = scope.alloc_string("PanicCtor")?;
      let func = scope.alloc_native_function(call_id, Some(construct_id), name, 0)?;
      scope.create_data_property(global, PropertyKey::from_string(name), Value::Object(func))?;
    }

    let err = rt
      .exec_script(r#"try { new PanicCtor(); "ok"; } catch (e) { "caught"; }"#)
      .unwrap_err();

    assert!(
      !err.is_throw_completion(),
      "native construct panic should surface as a non-throw fatal error, got {err:?}"
    );

    match err {
      VmError::InvariantViolation(msg) => assert_eq!(msg, "native construct panicked"),
      other => panic!("expected invariant violation, got {other:?}"),
    }

    Ok(())
  }

  #[test]
  fn dynamic_import_internal_native_call_ids_are_cached() -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());

    let fulfilled_first = vm.dynamic_import_eval_on_fulfilled_call_id()?;
    let len = vm.native_calls.len();
    let fulfilled_second = vm.dynamic_import_eval_on_fulfilled_call_id()?;
    assert_eq!(fulfilled_first, fulfilled_second);
    assert_eq!(len, vm.native_calls.len());

    let rejected_first = vm.dynamic_import_eval_on_rejected_call_id()?;
    let len = vm.native_calls.len();
    let rejected_second = vm.dynamic_import_eval_on_rejected_call_id()?;
    assert_eq!(rejected_first, rejected_second);
    assert_eq!(len, vm.native_calls.len());

    Ok(())
  }

  #[test]
  fn module_namespace_getter_call_id_is_cached() -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());

    let first = vm.module_namespace_getter_call_id()?;
    let len = vm.native_calls.len();
    let second = vm.module_namespace_getter_call_id()?;
    assert_eq!(first, second);
    assert_eq!(len, vm.native_calls.len());
    Ok(())
  }

  #[test]
  fn teardown_microtasks_clears_hir_async_continuations_and_roots() -> Result<(), VmError> {
    let mut rt = new_runtime();
    let script = crate::CompiledScript::compile_script(
      &mut rt.heap,
      "<inline>",
      r#"
        await 1;
      "#,
    )?;
    assert!(
      script.contains_top_level_await,
      "expected compiled script to contain top-level await"
    );

    let baseline_roots = rt.heap.persistent_root_count();
    let baseline_env_roots = rt.heap.persistent_env_root_count();

    for _ in 0..8 {
      let _promise = rt.exec_compiled_script(script.clone())?;

      // The script should suspend at the top-level await, leaving behind a queued Promise job and a
      // stored continuation.
      assert!(
        !rt.vm.microtask_queue().is_empty(),
        "expected top-level await to enqueue a microtask"
      );
      assert!(
        rt.vm.async_continuation_count() > 0,
        "expected top-level await to store an async continuation"
      );

      rt.vm.teardown_microtasks(&mut rt.heap);

      assert!(rt.vm.microtask_queue().is_empty());
      assert_eq!(rt.vm.async_continuation_count(), 0);
      assert_eq!(
        rt.heap.persistent_root_count(),
        baseline_roots,
        "expected persistent value roots to return to baseline after teardown"
      );
      assert_eq!(
        rt.heap.persistent_env_root_count(),
        baseline_env_roots,
        "expected persistent env roots to return to baseline after teardown"
      );

      // Ensure heap allocations from the abandoned async execution are collectible and won't cause
      // the loop to OOM under tight `HeapLimits`.
      rt.heap.collect_garbage();
    }

    Ok(())
  }

  #[test]
  fn teardown_realm_clears_template_registry_and_intrinsics() -> Result<(), VmError> {
    use crate::exec::eval_script_with_host_and_hooks;
    use crate::microtasks::MicrotaskQueue;
    use crate::{ExecutionContext, HeapLimits, Realm};

    let mut vm = Vm::new(VmOptions::default());
    // This test exercises two live realms simultaneously. Keep limits reasonably small to catch
    // leaks, but large enough for allocating two sets of intrinsics and globals.
    let mut heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
    let mut realm = Realm::new(&mut vm, &mut heap)?;
    let realm_id = realm.id();

    // Execute a script with a tagged template literal so the VM populates its template registry
    // (`GetTemplateObject` cache) with a persistent root.
    {
      let exec_ctx = ExecutionContext {
        realm: realm_id,
        script_or_module: None,
      };
      let mut vm_ctx = vm.execution_context_guard(exec_ctx)?;
      let mut scope = heap.scope();
      let source_string = scope.alloc_string("function tag(s) { return s; } tag`hello`;")?;
      let mut host = ();
      let mut hooks = MicrotaskQueue::new();
      let _ = eval_script_with_host_and_hooks(
        &mut *vm_ctx,
        &mut scope,
        &mut host,
        &mut hooks,
        source_string,
      )?;
      // The test script should not enqueue Promise jobs.
      assert!(hooks.is_empty());
    }

    // Extract the cached template root id and object handle.
    let (template_root, template_obj) = {
      let (_key, entry) = vm
        .template_registry
        .iter()
        .find(|(k, _)| k.realm == realm_id)
        .expect("expected template registry entry for realm");
      let template_root = entry.root;
      let Some(Value::Object(template_obj)) = heap.get_root(template_root) else {
        panic!("expected template object to be held by persistent root");
      };
      (template_root, template_obj)
    };

    // Realm teardown should unregister realm-owned roots, but should not touch VM-owned template
    // roots.
    realm.teardown(&mut heap);
    assert_eq!(
      heap.persistent_root_count(),
      1,
      "expected template root to remain after Realm::teardown"
    );
    assert!(heap.get_root(template_root).is_some());

    // VM teardown should remove template registry entries (and their roots) and clear intrinsics.
    vm.teardown_realm(&mut heap, realm_id);
    assert!(heap.get_root(template_root).is_none());
    assert!(!vm.template_registry.keys().any(|k| k.realm == realm_id));
    assert!(vm.intrinsics().is_none());

    // After GC, the template object should no longer be live.
    heap.collect_garbage();
    assert!(!heap.is_valid_object(template_obj));
    Ok(())
  }

  #[test]
  fn multi_realm_global_var_names_and_math_random_are_isolated() -> Result<(), VmError> {
    use crate::exec::eval_script_with_host_and_hooks;
    use crate::microtasks::MicrotaskQueue;
    use crate::{ExecutionContext, HeapLimits, Realm};

    fn eval_in_realm(
      vm: &mut Vm,
      heap: &mut Heap,
      realm: RealmId,
      source: &str,
    ) -> Result<Value, VmError> {
      let exec_ctx = ExecutionContext {
        realm,
        script_or_module: None,
      };
      let mut vm_ctx = vm.execution_context_guard(exec_ctx)?;
      let prev_state = vm_ctx.load_realm_state(heap, realm)?;

      let result: Result<Value, VmError> = (|| {
      let mut scope = heap.scope();
      let source_string = scope.alloc_string(source)?;
      let mut host = ();
      let mut hooks = MicrotaskQueue::new();
      let value = eval_script_with_host_and_hooks(
        &mut *vm_ctx,
        &mut scope,
        &mut host,
        &mut hooks,
        source_string,
      )?;
      // These tests do not involve Promises; ensure no jobs were enqueued.
      assert!(hooks.is_empty());
        Ok(value)
      })();

      drop(vm_ctx);
      let restore_res = vm.restore_realm_state(heap, prev_state);
      match (result, restore_res) {
        (Ok(v), Ok(())) => Ok(v),
        (Err(err), Ok(())) => Err(err),
        (Ok(_), Err(err)) => Err(err),
        (Err(err), Err(_)) => Err(err),
      }
    }

    fn value_as_f64(v: Value) -> f64 {
      match v {
        Value::Number(n) => n,
        other => panic!("expected number, got {other:?}"),
      }
    }

    fn value_is_undefined(v: Value) -> bool {
      matches!(v, Value::Undefined)
    }

    let mut vm = Vm::new(VmOptions::default());
    // Two realms keep their intrinsics alive simultaneously; use a bit more headroom than the
    // single-realm tests while still exercising heap limit paths.
    let mut heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));

    let mut realm_a = Realm::new(&mut vm, &mut heap)?;
    let realm_a_id = realm_a.id();

    // If realm initialization fails part-way through, make sure we tear down any successfully
    // created realms so their persistent GC roots are not leaked.
    let mut realm_b = match Realm::new(&mut vm, &mut heap) {
      Ok(realm) => realm,
      Err(err) => {
        realm_a.teardown(&mut heap);
        return Err(err);
      }
    };
    let realm_b_id = realm_b.id();

    let result: Result<(), VmError> = (|| {
      // Declare globals in realm A. Realm B was created last, so any "single active realm" bugs
      // will typically clobber realm B state when running this script.
      let _ = eval_in_realm(
        &mut vm,
        &mut heap,
        realm_a_id,
        "var x = 1; function f(){ return 42; }",
      )?;

      // Realm B should not observe realm A's var-created globals.
      let b_x = eval_in_realm(&mut vm, &mut heap, realm_b_id, "globalThis.x")?;
      assert!(
        value_is_undefined(b_x),
        "expected realm B globalThis.x to be undefined, got {b_x:?}"
      );

      // Realm B should not observe realm A's `[[VarNames]]` list during GlobalDeclarationInstantiation.
      // If var-name tracking leaks across realms, this would throw a SyntaxError.
      let b_let_x = eval_in_realm(&mut vm, &mut heap, realm_b_id, "let x = 2; x")?;
      assert_eq!(b_let_x, Value::Number(2.0));

      // Realm A should retain its global binding.
      let a_x = eval_in_realm(&mut vm, &mut heap, realm_a_id, "globalThis.x")?;
      assert_eq!(a_x, Value::Number(1.0));

      // `Math.random()` PRNG state should be isolated per realm. Since both realms start from the
      // same deterministic seed, the per-realm sequences should match.
      let a_r1 = value_as_f64(eval_in_realm(&mut vm, &mut heap, realm_a_id, "Math.random()")?);
      let b_r1 = value_as_f64(eval_in_realm(&mut vm, &mut heap, realm_b_id, "Math.random()")?);
      assert_eq!(a_r1.to_bits(), b_r1.to_bits());

      let a_r2 = value_as_f64(eval_in_realm(&mut vm, &mut heap, realm_a_id, "Math.random()")?);
      let b_r2 = value_as_f64(eval_in_realm(&mut vm, &mut heap, realm_b_id, "Math.random()")?);
      assert_eq!(a_r2.to_bits(), b_r2.to_bits());
      assert_ne!(a_r1.to_bits(), a_r2.to_bits());

      // Constructor prototype objects should inherit from the current realm's %Object.prototype%.
      // This depends on the heap's default prototype being updated when switching realms.
      let a_proto_ok = eval_in_realm(
        &mut vm,
        &mut heap,
        realm_a_id,
        "function C(){} Object.getPrototypeOf(C.prototype) === Object.prototype",
      )?;
      assert_eq!(a_proto_ok, Value::Bool(true));

      let b_proto_ok = eval_in_realm(
        &mut vm,
        &mut heap,
        realm_b_id,
        "function D(){} Object.getPrototypeOf(D.prototype) === Object.prototype",
      )?;
      assert_eq!(b_proto_ok, Value::Bool(true));

      Ok(())
    })();

    // Avoid leaking persistent roots: always tear down realms, even if script evaluation failed.
    realm_a.teardown(&mut heap);
    realm_b.teardown(&mut heap);

    result
  }

  #[test]
  fn stack_depth_exhaustion_throws_catchable_range_error() -> Result<(), VmError> {
    const MAX_STACK_DEPTH: usize = 32;

    // --- AST interpreter path: exception is catchable in JS. ---
    {
      let vm = Vm::new(VmOptions {
        max_stack_depth: MAX_STACK_DEPTH,
        ..VmOptions::default()
      });
      let heap = Heap::new(crate::HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
      let mut rt = crate::JsRuntime::new(vm, heap)?;

      let ok = rt.exec_script(
        r#"
          function f() { return f(); }
          let ok = false;
          try { f(); } catch (e) { ok = e instanceof RangeError; }
          ok;
        "#,
      )?;
      assert_eq!(ok, Value::Bool(true));
    }

    // --- AST interpreter path: uncaught overflow surfaces as a JS throw, not termination. ---
    {
      let vm = Vm::new(VmOptions {
        max_stack_depth: MAX_STACK_DEPTH,
        ..VmOptions::default()
      });
      let heap = Heap::new(crate::HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
      let mut rt = crate::JsRuntime::new(vm, heap)?;

      let err = rt
        .exec_script(
          r#"
            function f() { return f(); }
            f();
          "#,
        )
        .unwrap_err();
      assert!(
        !matches!(err, VmError::Termination(_)),
        "expected stack overflow to be a catchable RangeError, got {err:?}"
      );
      assert!(
        matches!(err, VmError::Throw(_) | VmError::ThrowWithStack { .. }),
        "expected stack overflow to surface as a JS throw, got {err:?}"
      );
    }

    // --- Compiled (HIR) execution path: exception is catchable in JS. ---
    {
      let vm = Vm::new(VmOptions {
        max_stack_depth: MAX_STACK_DEPTH,
        ..VmOptions::default()
      });
      let heap = Heap::new(crate::HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
      let mut rt = crate::JsRuntime::new(vm, heap)?;

      let script = crate::CompiledScript::compile_script(
        &mut rt.heap,
        "<inline>",
        r#"
          function f() { return f(); }
          let ok = false;
          try { f(); } catch (e) { ok = e instanceof RangeError; }
          ok;
        "#,
      )?;

      let ok = rt.exec_compiled_script(script)?;
      assert_eq!(ok, Value::Bool(true));
    }

    Ok(())
  }

  #[test]
  fn for_await_of_does_not_register_native_calls_per_iteration() -> Result<(), VmError> {
    let vm = Vm::new(VmOptions::default());
    // `for await...of` allocates and runs a fair amount of Promise/job machinery (async-from-sync
    // iterator wrappers, `PromiseResolve`, and `PerformPromiseThen` reaction wiring). Keep this
    // small to exercise GC paths, but allow enough headroom to avoid spurious `VmError::OutOfMemory`
    // failures as vm-js evolves.
    let heap = Heap::new(crate::HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
    let mut rt = crate::JsRuntime::new(vm, heap)?;

    rt.exec_script(
      r#"
        var out = "";
        async function f() {
          out = "";
          for await (const x of [Promise.resolve("a"), Promise.resolve("b"), Promise.resolve("c")]) {
            out += x;
          }
        }
      "#,
    )?;

    let before = rt.vm.native_call_count();

    rt.exec_script("f(); out")?;
    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;
    let out_value = rt.exec_script("out")?;
    assert_eq!(value_to_string(&rt, out_value), "abc");
    let after_first = rt.vm.native_call_count();

    rt.exec_script("f(); out")?;
    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;
    let out_value = rt.exec_script("out")?;
    assert_eq!(value_to_string(&rt, out_value), "abc");
    let after_second = rt.vm.native_call_count();

    assert_eq!(
      after_first,
      after_second,
      "for await..of should not register new native calls after first use (native_calls: {before} -> {after_first} -> {after_second})"
    );

    Ok(())
  }

  #[test]
  fn async_shift_compound_assignments_support_await_and_private_ordering() -> Result<(), VmError> {
    let vm = Vm::new(VmOptions::default());
    // Keep this relatively small to exercise GC paths while still leaving headroom for async/Promise
    // machinery. (This matches other async-focused unit tests in this module.)
    let heap = Heap::new(crate::HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
    let mut rt = crate::JsRuntime::new(vm, heap)?;

    rt.exec_script(
      r#"
        var log = [];
        var obj = {
          get x() { log.push("get"); return 1; },
          set x(v) { log.push("set:" + v); },
        };

        var a = 1;
        var b = 8;
        var c = -1;
        var d = 4n;
        var e = 4n;
        var m = 1n;
        var err = null;
        var err2 = null;

        async function f() {
          // Ensure compound assignment evaluation order: GetValue(obj.x) before evaluating RHS.
          obj.x <<= await (log.push("rhs"), Promise.resolve(1));

          a <<= await Promise.resolve(2);
          b >>= await Promise.resolve(1);
          c >>>= await Promise.resolve(1);
          d <<= await Promise.resolve(1n);

          try { e >>>= await Promise.resolve(1n); } catch (ex) { err = ex; }
          try { m <<= await Promise.resolve(1); } catch (ex) { err2 = ex; }
        }
      "#,
    )?;

    rt.exec_script("f()")?;
    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let log_value = rt.exec_script("log.join(',')")?;
    assert_eq!(value_to_string(&rt, log_value), "get,rhs,set:2");

    assert_eq!(rt.exec_script("a")?, Value::Number(4.0));
    assert_eq!(rt.exec_script("b")?, Value::Number(4.0));
    assert_eq!(rt.exec_script("c")?, Value::Number(2147483647.0));

    let d_str = rt.exec_script("d.toString()")?;
    assert_eq!(value_to_string(&rt, d_str), "8");

    let err_msg = rt.exec_script("err && err.message")?;
    assert_eq!(
      value_to_string(&rt, err_msg),
      "BigInt does not support unsigned right shift"
    );

    let err2_msg = rt.exec_script("err2 && err2.message")?;
    assert_eq!(
      value_to_string(&rt, err2_msg),
      "Cannot mix BigInt and other types"
    );

    Ok(())
  }

  #[test]
  fn arrow_this_in_derived_constructor_observes_initialized_this_after_super() {
    let mut rt = new_runtime();
    let value = rt
      .exec_script(
        r#"
        class B {}
        class D extends B {
          constructor() {
            let f = () => this;
            super();
            // Return an object wrapper so the arrow's return value is observable even if it is
            // `undefined` (constructor primitive return values are ignored).
            return { v: f() };
          }
        }
        const o = new D();
        o.v instanceof D
      "#,
      )
      .unwrap();
    assert_eq!(value, Value::Bool(true));
  }

  #[test]
  fn arrow_this_before_super_throws_reference_error() {
    let mut rt = new_runtime();
    let value = rt
      .exec_script(
        r#"
        let err;
        class B {}
        class D extends B {
          constructor() {
            let f = () => this;
            try { f(); } catch (e) { err = e.name; }
            super();
          }
        }
        new D();
        err
      "#,
      )
      .unwrap();
    assert_eq!(value_to_string(&rt, value), "ReferenceError");
  }

  #[test]
  fn microtask_checkpoint_treats_out_of_memory_as_hard_stop_and_discards_remaining_jobs(
  ) -> Result<(), VmError> {
    use crate::HeapLimits;

    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let baseline_roots = heap.persistent_root_count();

    // Allocate an object and keep only a weak handle so the only strong reference is the job's
    // persistent root.
    let obj = {
      let mut scope = heap.scope();
      scope.alloc_object()?
    };
    let weak = WeakGcObject::from(obj);
    let obj_root = heap.add_root(Value::Object(obj))?;
    assert_eq!(heap.persistent_root_count(), baseline_roots + 1);

    // First job: fail with OOM.
    let job_oom = Job::new(JobKind::Promise, |_ctx, _host| Err(VmError::OutOfMemory))?;

    // Second job: would run if the checkpoint continued draining after OOM.
    let ran = Arc::new(AtomicBool::new(false));
    let ran_clone = ran.clone();
    let job_should_not_run = Job::new(JobKind::Promise, move |_ctx, _host| {
      ran_clone.store(true, Ordering::SeqCst);
      Ok(())
    })?
    .with_roots(vec![obj_root]);

    vm
      .microtask_queue_mut()
      .enqueue_promise_job(job_oom, None);
    vm
      .microtask_queue_mut()
      .enqueue_promise_job(job_should_not_run, None);

    let mut dummy_host = ();
    let err = vm
      .perform_microtask_checkpoint_with_host(&mut dummy_host, &mut heap)
      .unwrap_err();
    assert!(matches!(err, VmError::OutOfMemory));

    // The OOM should be treated as a hard stop: the later job must be discarded, not executed.
    assert!(
      !ran.load(Ordering::SeqCst),
      "expected microtask checkpoint to discard remaining jobs after OOM"
    );
    assert!(
      vm.microtask_queue().is_empty(),
      "expected remaining jobs to be discarded after OOM"
    );
    assert_eq!(
      heap.persistent_root_count(),
      baseline_roots,
      "expected job roots to be cleaned up when discarding jobs after OOM"
    );

    // After GC, the rooted object should no longer be live.
    heap.collect_garbage();
    assert_eq!(weak.upgrade(&heap), None);

    Ok(())
  }

  #[test]
  fn derived_constructor_without_super_throws_reference_error() -> Result<(), VmError> {
    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(crate::HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut rt = crate::JsRuntime::new(vm, heap)?;

    let value = rt.exec_script(
      r#"
        var name;
        try {
          class A {}
          class B extends A { constructor() { } }
          new B();
          name = "no";
        } catch (e) {
          name = e.name;
        }
        name
      "#,
    )?;
    assert_eq!(value_to_string(&rt, value), "ReferenceError");

    // A derived constructor may return an object explicitly without calling `super()`.
    let value = rt.exec_script(
      r#"
        class A {}
        class B extends A { constructor() { return {}; } }
        typeof new B()
      "#,
    )?;
    assert_eq!(value_to_string(&rt, value), "object");

    Ok(())
  }
}
