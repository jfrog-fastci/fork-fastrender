use crate::error::{Error, Result};
use crate::render_control;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::marker::PhantomData;
use std::ptr;
use std::rc::Rc;
use std::time::{Duration, Instant};

use vm_js::{
  Budget, Heap, HeapLimits, Job, JobKind, NativeFunctionId, Realm, RealmId, RootId, Scope, Value,
  Vm, VmError, VmHostHooks, VmJobContext, VmOptions,
};

use super::event_loop::{EventLoop, TimerId};
use super::script_scheduler::ScriptExecutor;
use super::ScriptElementSpec;

/// FastRender embedding of `ecma-rs`'s `vm-js` primitives.
///
/// This is an MVP JS host/runtime that:
/// - owns the `vm-js` heap + VM + a single document realm,
/// - installs minimal Window-ish globals (`queueMicrotask`, timers, and a Promise-shaped stub),
/// - integrates ECMAScript Promise jobs with the FastRender [`EventLoop`] microtask queue.
///
/// Note: `vm-js` is still early-stage; this runtime includes a small AST interpreter for a subset
/// of JavaScript needed by FastRender's early scripting and scheduling tests.
pub struct EcmaVmRuntime<State: 'static> {
  pub state: State,
  heap: Heap,
  vm: Vm,
  realm: Realm,
  env: Env,
  timers: HashMap<TimerId, TimerEntry>,
  config: EcmaVmRuntimeConfig,
  /// Used only to satisfy the `vm-js` host hook shape. We have a single realm for now.
  realm_id: RealmId,
  /// Optional error captured by host hooks that cannot return a `Result` (e.g. enqueue failures).
  pending_host_error: Option<Error>,
  /// Stable native-call handler id for `promise.then`.
  promise_then_call_id: NativeFunctionId,
  _marker: PhantomData<Rc<()>>, // !Send/!Sync: JS host is single-threaded.
}

#[derive(Debug, Clone)]
pub struct EcmaVmRuntimeConfig {
  pub heap_limits: HeapLimits,
  /// Optional per-script fuel budget.
  pub fuel: Option<u64>,
  /// Optional per-script wall-time budget.
  ///
  /// Applied relative to the start of each script/job/timer callback.
  pub deadline: Option<Duration>,
  pub check_time_every: u32,
}

impl Default for EcmaVmRuntimeConfig {
  fn default() -> Self {
    Self {
      heap_limits: HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024),
      fuel: None,
      deadline: None,
      check_time_every: 100,
    }
  }
}

#[derive(Debug, Clone)]
struct TimerEntry {
  callback: RootId,
  args: Vec<RootId>,
}

impl<State: 'static> EcmaVmRuntime<State> {
  pub fn new(state: State, config: EcmaVmRuntimeConfig) -> Result<Self> {
    let heap = Heap::new(config.heap_limits);

    let vm_options = VmOptions {
      default_fuel: config.fuel,
      default_deadline: config.deadline,
      check_time_every: config.check_time_every,
      interrupt_flag: Some(render_control::interrupt_flag()),
      ..VmOptions::default()
    };
    let mut vm = Vm::new(vm_options);

    // Register the shared call handler for Promise `.then` once per VM.
    let promise_then_call_id = vm
      .register_native_call(native_promise_then::<State>)
      .map_err(map_vm_error)?;

    let mut heap = heap;
    let realm = Realm::new(&mut vm, &mut heap).map_err(map_vm_error)?;

    let mut rt = Self {
      state,
      heap,
      vm,
      realm,
      env: Env::default(),
      timers: HashMap::new(),
      config,
      realm_id: RealmId::from_raw(1),
      pending_host_error: None,
      promise_then_call_id,
      _marker: PhantomData,
    };

    rt.install_base_globals()?;
    Ok(rt)
  }

  fn install_base_globals(&mut self) -> Result<()> {
    // Global constants.
    //
    // `Realm::new` defines `undefined`/`globalThis` on the global object; we also define them as
    // interpreter bindings so identifier lookups succeed in the MVP evaluator.
    self
      .env
      .declare_var(&mut self.heap, "undefined")
      .map_err(map_vm_error)?;
    self
      .env
      .set(&mut self.heap, "undefined", Value::Undefined)
      .map_err(map_vm_error)?;

    self
      .env
      .declare_var(&mut self.heap, "globalThis")
      .map_err(map_vm_error)?;
    self
      .env
      .set(
        &mut self.heap,
        "globalThis",
        Value::Object(self.realm.global_object()),
      )
      .map_err(map_vm_error)?;

    // queueMicrotask(fn)
    self.define_global_native_function("queueMicrotask", 1, native_queue_microtask::<State>)?;

    // Timers.
    self.define_global_native_function("setTimeout", 1, native_set_timeout::<State>)?;
    self.define_global_native_function("clearTimeout", 1, native_clear_timeout::<State>)?;
    self.define_global_native_function("setInterval", 1, native_set_interval::<State>)?;
    self.define_global_native_function("clearInterval", 1, native_clear_interval::<State>)?;

    // Minimal Promise: Promise.resolve().then(cb)
    self.install_promise_object()?;

    Ok(())
  }

  fn install_promise_object(&mut self) -> Result<()> {
    let resolve_fn = self.alloc_native_function("resolve", 1, native_promise_resolve::<State>)?;
    let promise_obj = {
      let mut scope = self.heap.scope();
      // Root `resolve_fn` across object/key allocation.
      scope.push_root(resolve_fn).map_err(map_vm_error)?;
      // Promise is a function in real JS; for MVP, an ordinary object with a `resolve` method is
      // sufficient for microtask coverage tests.
      let obj = scope
        .alloc_object_with_prototype(Some(self.realm.intrinsics().object_prototype()))
        .map_err(map_vm_error)?;
      scope.push_root(Value::Object(obj)).map_err(map_vm_error)?;

      let resolve_key = prop_key(&mut scope, "resolve").map_err(map_vm_error)?;
      define_value(&mut scope, obj, resolve_key, resolve_fn).map_err(map_vm_error)?;
      Value::Object(obj)
    };

    self.define_global_var("Promise", promise_obj)?;
    Ok(())
  }

  fn alloc_native_function(
    &mut self,
    name: &str,
    length: u32,
    call: vm_js::NativeCall,
  ) -> Result<Value> {
    let call_id = self.vm.register_native_call(call).map_err(map_vm_error)?;
    let mut scope = self.heap.scope();
    let name_s = scope.alloc_string(name).map_err(map_vm_error)?;
    scope
      .push_root(Value::String(name_s))
      .map_err(map_vm_error)?;
    let func = scope
      .alloc_native_function(call_id, None, name_s, length)
      .map_err(map_vm_error)?;
    Ok(Value::Object(func))
  }

  fn define_global_native_function(
    &mut self,
    name: &str,
    length: u32,
    call: vm_js::NativeCall,
  ) -> Result<Value> {
    let func = self.alloc_native_function(name, length, call)?;
    self.define_global_var(name, func)?;
    Ok(func)
  }

  fn define_global_var(&mut self, name: &str, value: Value) -> Result<()> {
    // Store in the interpreter env (identifier lookups).
    self
      .env
      .declare_var(&mut self.heap, name)
      .map_err(map_vm_error)?;
    self
      .env
      .set(&mut self.heap, name, value)
      .map_err(map_vm_error)?;

    // And on the realm global object for spec shape.
    let mut scope = self.heap.scope();
    let global = self.realm.global_object();
    scope
      .push_root(Value::Object(global))
      .map_err(map_vm_error)?;
    scope.push_root(value).map_err(map_vm_error)?;

    let key = prop_key(&mut scope, name).map_err(map_vm_error)?;
    define_value(&mut scope, global, key, value).map_err(map_vm_error)?;
    Ok(())
  }

  fn global_this(&self) -> Value {
    Value::Object(self.realm.global_object())
  }

  fn reset_budget_for_run(&mut self) {
    let render_remaining = render_control::active_deadline().and_then(|d| d.remaining_timeout());

    let deadline_duration = match (self.config.deadline, render_remaining) {
      (Some(a), Some(b)) => Some(if a < b { a } else { b }),
      (Some(a), None) => Some(a),
      (None, Some(b)) => Some(b),
      (None, None) => None,
    };

    let deadline = deadline_duration.and_then(|d| Instant::now().checked_add(d));

    self.vm.set_budget(Budget {
      fuel: self.config.fuel,
      deadline,
      check_time_every: self.config.check_time_every,
    });
  }

  fn execute_script_text(&mut self, script_text: &str) -> Result<()> {
    if let Some(err) = self.pending_host_error.take() {
      return Err(err);
    }

    self.reset_budget_for_run();

    let opts = parse_js::ParseOptions {
      dialect: parse_js::Dialect::Ecma,
      source_type: parse_js::SourceType::Script,
    };
    let top = parse_js::parse_with_options(script_text, opts)
      .map_err(|err| Error::Other(format!("JS parse error: {err}")))?;

    let global_this = self.global_this();
    let mut evaluator = Evaluator {
      vm: &mut self.vm,
      env: &mut self.env,
      global_this,
    };

    for stmt in &top.stx.body {
      let mut scope = self.heap.scope();
      evaluator
        .eval_stmt(&mut scope, stmt)
        .map_err(map_vm_error)?;
      if let Some(err) = self.pending_host_error.take() {
        return Err(err);
      }
    }

    Ok(())
  }
}

struct Evaluator<'a> {
  vm: &'a mut Vm,
  env: &'a mut Env,
  global_this: Value,
}

impl Evaluator<'_> {
  fn eval_stmt(
    &mut self,
    scope: &mut Scope<'_>,
    stmt: &parse_js::ast::node::Node<parse_js::ast::stmt::Stmt>,
  ) -> std::result::Result<(), VmError> {
    // One tick per statement.
    self.vm.tick()?;

    match &*stmt.stx {
      parse_js::ast::stmt::Stmt::Empty(_) => Ok(()),
      parse_js::ast::stmt::Stmt::Expr(expr_stmt) => {
        let _ = self.eval_expr(scope, &expr_stmt.stx.expr)?;
        Ok(())
      }
      parse_js::ast::stmt::Stmt::VarDecl(var_decl) => self.eval_var_decl(scope, &var_decl.stx),
      _ => Err(VmError::Unimplemented("statement type")),
    }
  }

  fn eval_var_decl(
    &mut self,
    scope: &mut Scope<'_>,
    decl: &parse_js::ast::stmt::decl::VarDecl,
  ) -> std::result::Result<(), VmError> {
    for declarator in &decl.declarators {
      let name = expect_simple_binding_identifier(&declarator.pattern.stx)?;
      let value = match &declarator.initializer {
        Some(init) => self.eval_expr(scope, init)?,
        None => Value::Undefined,
      };

      self.env.declare_var(scope.heap_mut(), name)?;
      self.env.set(scope.heap_mut(), name, value)?;
    }
    Ok(())
  }

  fn eval_expr(
    &mut self,
    scope: &mut Scope<'_>,
    expr: &parse_js::ast::node::Node<parse_js::ast::expr::Expr>,
  ) -> std::result::Result<Value, VmError> {
    use parse_js::ast::expr::Expr;

    match &*expr.stx {
      Expr::LitNum(node) => Ok(Value::Number(node.stx.value.0)),
      Expr::LitBool(node) => Ok(Value::Bool(node.stx.value)),
      Expr::LitNull(_) => Ok(Value::Null),
      Expr::LitStr(node) => {
        let s = scope.alloc_string(&node.stx.value)?;
        Ok(Value::String(s))
      }
      Expr::Id(node) => self
        .env
        .get(scope.heap(), &node.stx.name)
        .ok_or(VmError::Unimplemented("unbound identifier")),
      Expr::This(_) => Ok(self.global_this),
      Expr::Member(node) => self.eval_member_expr(scope, &node.stx),
      Expr::Call(node) => self.eval_call_expr(scope, &node.stx),
      _ => Err(VmError::Unimplemented("expression type")),
    }
  }

  fn eval_member_expr(
    &mut self,
    scope: &mut Scope<'_>,
    expr: &parse_js::ast::expr::MemberExpr,
  ) -> std::result::Result<Value, VmError> {
    if expr.optional_chaining {
      return Err(VmError::Unimplemented("optional chaining"));
    }

    let obj_value = self.eval_expr(scope, &expr.left)?;

    // Root `obj_value` across `alloc_string`, which could trigger a GC.
    let mut child = scope.reborrow();
    child.push_root(obj_value)?;

    let Value::Object(obj) = obj_value else {
      return Err(VmError::Unimplemented("member access on non-object"));
    };
    let key_s = child.alloc_string(&expr.right)?;
    child.push_root(Value::String(key_s))?;
    let key = vm_js::PropertyKey::String(key_s);
    child.ordinary_get(self.vm, obj, key, obj_value)
  }

  fn eval_call_expr(
    &mut self,
    scope: &mut Scope<'_>,
    expr: &parse_js::ast::expr::CallExpr,
  ) -> std::result::Result<Value, VmError> {
    if expr.optional_chaining {
      return Err(VmError::Unimplemented("optional chaining"));
    }

    // Evaluate callee and determine `this` for the call.
    let (callee, this) = match &*expr.callee.stx {
      parse_js::ast::expr::Expr::Member(member) => {
        let obj_value = self.eval_expr(scope, &member.stx.left)?;
        let mut child = scope.reborrow();
        child.push_root(obj_value)?;

        let Value::Object(obj) = obj_value else {
          return Err(VmError::Unimplemented("call receiver is not an object"));
        };
        let key_s = child.alloc_string(&member.stx.right)?;
        child.push_root(Value::String(key_s))?;
        let key = vm_js::PropertyKey::String(key_s);
        let func = child.ordinary_get(self.vm, obj, key, obj_value)?;
        (func, obj_value)
      }
      _ => {
        let func = self.eval_expr(scope, &expr.callee)?;
        (func, self.global_this)
      }
    };

    let mut call_scope = scope.reborrow();
    call_scope.push_root(callee)?;
    call_scope.push_root(this)?;

    let mut args: Vec<Value> = Vec::with_capacity(expr.arguments.len());
    for arg in &expr.arguments {
      if arg.stx.spread {
        return Err(VmError::Unimplemented("spread arguments"));
      }
      let v = self.eval_expr(&mut call_scope, &arg.stx.value)?;
      call_scope.push_root(v)?;
      args.push(v);
    }

    self.vm.call(&mut call_scope, callee, this, &args)
  }
}

impl<State: 'static> Drop for EcmaVmRuntime<State> {
  fn drop(&mut self) {
    self.realm.teardown(&mut self.heap);
  }
}

// --- Event loop execution context plumbing ---

#[derive(Clone, Copy)]
struct RawExecCtx {
  host: *mut (),
  event_loop: *mut (),
}

thread_local! {
  static EXEC_CTX: Cell<RawExecCtx> = Cell::new(RawExecCtx { host: ptr::null_mut(), event_loop: ptr::null_mut() });
}

struct ExecCtxGuard {
  previous: RawExecCtx,
}

impl ExecCtxGuard {
  fn install<State: 'static>(
    host: &mut EcmaVmRuntime<State>,
    event_loop: &mut EventLoop<EcmaVmRuntime<State>>,
  ) -> Self {
    let next = RawExecCtx {
      host: host as *mut _ as *mut (),
      event_loop: event_loop as *mut _ as *mut (),
    };
    let previous = EXEC_CTX.with(|cell| {
      let prev = cell.get();
      cell.set(next);
      prev
    });
    Self { previous }
  }

  fn with_current<State: 'static, R>(
    f: impl FnOnce(*mut EcmaVmRuntime<State>, *mut EventLoop<EcmaVmRuntime<State>>) -> R,
  ) -> R {
    EXEC_CTX.with(|cell| {
      let ctx = cell.get();
      assert!(
        !ctx.host.is_null() && !ctx.event_loop.is_null(),
        "vm-js host hook called outside of an active JS execution context"
      );
      // SAFETY: `ExecCtxGuard` installs pointers that are valid for the duration of script/job
      // execution. This intentionally bypasses Rust's aliasing rules to allow `vm-js` native
      // functions (which do not carry host references) to access the FastRender event loop.
      f(
        ctx.host as *mut EcmaVmRuntime<State>,
        ctx.event_loop as *mut EventLoop<EcmaVmRuntime<State>>,
      )
    })
  }
}

impl Drop for ExecCtxGuard {
  fn drop(&mut self) {
    let previous = self.previous;
    EXEC_CTX.with(|cell| cell.set(previous));
  }
}

// --- vm-js host hooks ---

impl<State: 'static> VmHostHooks for EcmaVmRuntime<State> {
  fn host_enqueue_promise_job(&mut self, job: Job, _realm: Option<RealmId>) {
    // This hook cannot return `Result`, so we stash enqueue errors and surface them at the next
    // script/job boundary.
    if self.pending_host_error.is_some() {
      let mut ctx = FastRenderJobContext::new(self);
      job.discard(&mut ctx);
      return;
    }

    // If enqueue fails, we need to retain the `Job` so we can `discard` it (which cleans up any
    // persistent roots the job carries).
    let job_cell: Rc<RefCell<Option<Job>>> = Rc::new(RefCell::new(Some(job)));
    let job_cell_for_task = Rc::clone(&job_cell);

    let enqueue_result: std::result::Result<(), Error> =
      ExecCtxGuard::with_current::<State, _>(|_host_ptr, event_loop_ptr| unsafe {
        (&mut *event_loop_ptr).queue_microtask(move |host, event_loop| {
          let job = job_cell_for_task
            .borrow_mut()
            .take()
            .expect("vm-js Job should be present when microtask runs");

          let _guard = ExecCtxGuard::install(host, event_loop);
          host.reset_budget_for_run();

          let mut ctx = FastRenderJobContext::new(host);
          job.run(&mut ctx, host).map_err(map_vm_error)?;
          if let Some(err) = host.pending_host_error.take() {
            return Err(err);
          }
          Ok(())
        })
      });

    if let Err(err) = enqueue_result {
      if let Some(job) = job_cell.borrow_mut().take() {
        let mut ctx = FastRenderJobContext::new(self);
        job.discard(&mut ctx);
      }
      self.pending_host_error.get_or_insert(err);
    }
  }

  fn host_call_job_callback(
    &mut self,
    ctx: &mut dyn VmJobContext,
    callback: &vm_js::JobCallback,
    this_argument: Value,
    arguments: &[Value],
  ) -> std::result::Result<Value, VmError> {
    ctx.call(
      self,
      Value::Object(callback.callback_object()),
      this_argument,
      arguments,
    )
  }
}

struct FastRenderJobContext {
  vm: *mut Vm,
  heap: *mut Heap,
}

impl FastRenderJobContext {
  fn new<State: 'static>(host: &mut EcmaVmRuntime<State>) -> Self {
    Self {
      vm: &mut host.vm as *mut Vm,
      heap: &mut host.heap as *mut Heap,
    }
  }
}

impl VmJobContext for FastRenderJobContext {
  fn call(
    &mut self,
    host: &mut dyn VmHostHooks,
    callee: Value,
    this: Value,
    args: &[Value],
  ) -> std::result::Result<Value, VmError> {
    // SAFETY: `FastRenderJobContext` is only used while `EcmaVmRuntime` is alive. This uses raw
    // pointers so we can borrow the VM/heap mutably while also providing a `&mut dyn VmHostHooks`
    // implementation to `vm-js` jobs (needed for promise/microtask scheduling via host hooks).
    unsafe {
      let heap = &mut *self.heap;
      let vm = &mut *self.vm;
      let mut scope = heap.scope();
      vm.call_with_host(&mut scope, host, callee, this, args)
    }
  }

  fn construct(
    &mut self,
    host: &mut dyn VmHostHooks,
    callee: Value,
    args: &[Value],
    new_target: Value,
  ) -> std::result::Result<Value, VmError> {
    unsafe {
      let heap = &mut *self.heap;
      let vm = &mut *self.vm;
      let mut scope = heap.scope();
      vm.construct_with_host(&mut scope, host, callee, args, new_target)
    }
  }

  fn add_root(&mut self, value: Value) -> std::result::Result<RootId, VmError> {
    unsafe { (&mut *self.heap).add_root(value) }
  }

  fn remove_root(&mut self, id: RootId) {
    unsafe { (&mut *self.heap).remove_root(id) }
  }
}

struct HeapRootContext<'a> {
  heap: &'a mut Heap,
}

impl VmJobContext for HeapRootContext<'_> {
  fn call(
    &mut self,
    _host: &mut dyn VmHostHooks,
    _callee: Value,
    _this: Value,
    _args: &[Value],
  ) -> std::result::Result<Value, VmError> {
    Err(VmError::Unimplemented("HeapRootContext::call"))
  }

  fn construct(
    &mut self,
    _host: &mut dyn VmHostHooks,
    _callee: Value,
    _args: &[Value],
    _new_target: Value,
  ) -> std::result::Result<Value, VmError> {
    Err(VmError::Unimplemented("HeapRootContext::construct"))
  }

  fn add_root(&mut self, value: Value) -> std::result::Result<RootId, VmError> {
    self.heap.add_root(value)
  }

  fn remove_root(&mut self, id: RootId) {
    self.heap.remove_root(id);
  }
}

// --- ScriptScheduler adapter ---

impl<State: 'static> ScriptExecutor for EcmaVmRuntime<State> {
  fn execute_classic_script(
    &mut self,
    script_text: &str,
    _spec: &ScriptElementSpec,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<()> {
    let _guard = ExecCtxGuard::install(self, event_loop);
    self.execute_script_text(script_text)
  }
}

// --- Helpers ---

fn map_vm_error(err: VmError) -> Error {
  Error::Other(format!("JS error: {err:?}"))
}

fn prop_key(scope: &mut Scope<'_>, name: &str) -> std::result::Result<vm_js::PropertyKey, VmError> {
  let s = scope.alloc_string(name)?;
  scope.push_root(Value::String(s))?;
  Ok(vm_js::PropertyKey::from_string(s))
}

fn define_value(
  scope: &mut Scope<'_>,
  obj: vm_js::GcObject,
  key: vm_js::PropertyKey,
  value: Value,
) -> std::result::Result<(), VmError> {
  scope.define_property(
    obj,
    key,
    vm_js::PropertyDescriptor {
      enumerable: false,
      configurable: true,
      kind: vm_js::PropertyKind::Data {
        value,
        writable: true,
      },
    },
  )
}

fn expect_simple_binding_identifier<'a>(
  pat_decl: &'a parse_js::ast::stmt::decl::PatDecl,
) -> std::result::Result<&'a str, VmError> {
  match &*pat_decl.pat.stx {
    parse_js::ast::expr::pat::Pat::Id(id) => Ok(&id.stx.name),
    _ => Err(VmError::Unimplemented("destructuring patterns")),
  }
}

fn to_timer_id(value: Value) -> TimerId {
  match value {
    Value::Number(n) if n.is_finite() => n.trunc() as i32,
    Value::Bool(true) => 1,
    Value::Bool(false) | Value::Undefined | Value::Null => 0,
    _ => 0,
  }
}

fn to_timeout_ms(value: Value) -> i64 {
  match value {
    Value::Number(n) if n.is_finite() => n.trunc() as i64,
    Value::Bool(true) => 1,
    Value::Bool(false) | Value::Undefined | Value::Null => 0,
    _ => 0,
  }
}

fn throw_type_error(scope: &mut Scope<'_>, message: &str) -> VmError {
  let s = scope
    .alloc_string(&format!("TypeError: {message}"))
    .map_err(|err| match err {
      VmError::OutOfMemory => VmError::OutOfMemory,
      other => other,
    });

  match s {
    Ok(s) => VmError::Throw(Value::String(s)),
    Err(err) => err,
  }
}

// --- Native web API globals ---

fn native_queue_microtask<State: 'static>(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHostHooks,
  _callee: vm_js::GcObject,
  _this: Value,
  args: &[Value],
) -> std::result::Result<Value, VmError> {
  let callback = args.get(0).copied().unwrap_or(Value::Undefined);
  if matches!(callback, Value::String(_)) {
    return Err(throw_type_error(
      scope,
      "queueMicrotask does not currently support string callbacks",
    ));
  }
  if !scope.heap().is_callable(callback)? {
    return Err(throw_type_error(
      scope,
      "queueMicrotask callback is not callable",
    ));
  }

  let callback_root = scope.heap_mut().add_root(callback)?;

  let queued = ExecCtxGuard::with_current::<State, _>(|host_ptr, event_loop_ptr| unsafe {
    match (&mut *event_loop_ptr).queue_microtask(move |host, event_loop| {
      let _guard = ExecCtxGuard::install(host, event_loop);
      host.reset_budget_for_run();

      let global_this = Value::Object(host.realm.global_object());
      let callback = {
        let mut scope = host.heap.scope();
        scope
          .heap()
          .get_root(callback_root)
          .unwrap_or(Value::Undefined)
      };

      let mut ctx = FastRenderJobContext::new(host);
      let result = ctx.call(host, callback, global_this, &[]);
      host.heap.remove_root(callback_root);
      result.map(|_| ()).map_err(map_vm_error)?;
      Ok(())
    }) {
      Ok(()) => true,
      Err(err) => {
        (*host_ptr).pending_host_error.get_or_insert(err);
        false
      }
    }
  });

  if !queued {
    // The microtask was not enqueued, so the root will never be removed by the task.
    scope.heap_mut().remove_root(callback_root);
  }

  Ok(Value::Undefined)
}

fn native_set_timeout<State: 'static>(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHostHooks,
  _callee: vm_js::GcObject,
  _this: Value,
  args: &[Value],
) -> std::result::Result<Value, VmError> {
  let handler = args.get(0).copied().unwrap_or(Value::Undefined);
  if matches!(handler, Value::String(_)) {
    return Err(throw_type_error(
      scope,
      "setTimeout does not currently support string handlers",
    ));
  }
  if !scope.heap().is_callable(handler)? {
    return Err(throw_type_error(
      scope,
      "setTimeout callback is not callable",
    ));
  }

  let timeout_ms = args.get(1).copied().map(to_timeout_ms).unwrap_or(0);
  let delay_ms = timeout_ms.max(0) as u64;
  let delay = Duration::from_millis(delay_ms);

  let callback_root = scope.heap_mut().add_root(handler)?;
  let mut arg_roots: Vec<RootId> = Vec::new();
  for &v in args.iter().skip(2) {
    match scope.heap_mut().add_root(v) {
      Ok(root) => arg_roots.push(root),
      Err(err) => {
        scope.heap_mut().remove_root(callback_root);
        for root in arg_roots {
          scope.heap_mut().remove_root(root);
        }
        return Err(err);
      }
    }
  }

  let id_cell: Rc<Cell<TimerId>> = Rc::new(Cell::new(0));
  let id_cell_for_cb = Rc::clone(&id_cell);

  ExecCtxGuard::with_current::<State, _>(|host_ptr, event_loop_ptr| unsafe {
    let set_result = (&mut *event_loop_ptr).set_timeout(delay, move |host, event_loop| {
      let _guard = ExecCtxGuard::install(host, event_loop);
      host.reset_budget_for_run();

      let id = id_cell_for_cb.get();
      let Some(entry) = host.timers.remove(&id) else {
        // Cleared between scheduling and firing.
        return Ok(());
      };

      let global_this = Value::Object(host.realm.global_object());
      let (callback, call_args) = {
        let mut scope = host.heap.scope();
        let callback = scope
          .heap()
          .get_root(entry.callback)
          .unwrap_or(Value::Undefined);
        let call_args: Vec<Value> = entry
          .args
          .iter()
          .map(|root| scope.heap().get_root(*root).unwrap_or(Value::Undefined))
          .collect();
        (callback, call_args)
      };

      let mut ctx = FastRenderJobContext::new(host);
      let result = ctx.call(host, callback, global_this, &call_args);

      host.heap.remove_root(entry.callback);
      for root in entry.args {
        host.heap.remove_root(root);
      }

      result.map(|_| ()).map_err(map_vm_error)?;
      Ok(())
    });

    match set_result {
      Ok(id) => {
        id_cell.set(id);
        (*host_ptr).timers.insert(
          id,
          TimerEntry {
            callback: callback_root,
            args: arg_roots.clone(),
          },
        );
      }
      Err(err) => {
        (*host_ptr).pending_host_error.get_or_insert(err);
      }
    }
  });

  let id = id_cell.get();
  if id == 0 {
    // Scheduling failed: remove the roots created above.
    scope.heap_mut().remove_root(callback_root);
    for root in arg_roots {
      scope.heap_mut().remove_root(root);
    }
  }

  Ok(Value::Number(id as f64))
}

fn native_clear_timeout<State: 'static>(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHostHooks,
  _callee: vm_js::GcObject,
  _this: Value,
  args: &[Value],
) -> std::result::Result<Value, VmError> {
  let id = args.get(0).copied().map(to_timer_id).unwrap_or(0);
  ExecCtxGuard::with_current::<State, _>(|host_ptr, event_loop_ptr| unsafe {
    (&mut *event_loop_ptr).clear_timeout(id);
    if let Some(entry) = (*host_ptr).timers.remove(&id) {
      scope.heap_mut().remove_root(entry.callback);
      for root in entry.args {
        scope.heap_mut().remove_root(root);
      }
    }
  });
  Ok(Value::Undefined)
}

fn native_set_interval<State: 'static>(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHostHooks,
  _callee: vm_js::GcObject,
  _this: Value,
  args: &[Value],
) -> std::result::Result<Value, VmError> {
  let handler = args.get(0).copied().unwrap_or(Value::Undefined);
  if matches!(handler, Value::String(_)) {
    return Err(throw_type_error(
      scope,
      "setInterval does not currently support string handlers",
    ));
  }
  if !scope.heap().is_callable(handler)? {
    return Err(throw_type_error(
      scope,
      "setInterval callback is not callable",
    ));
  }

  let timeout_ms = args.get(1).copied().map(to_timeout_ms).unwrap_or(0);
  let interval_ms = timeout_ms.max(0) as u64;
  let interval = Duration::from_millis(interval_ms);

  let callback_root = scope.heap_mut().add_root(handler)?;
  let mut arg_roots: Vec<RootId> = Vec::new();
  for &v in args.iter().skip(2) {
    match scope.heap_mut().add_root(v) {
      Ok(root) => arg_roots.push(root),
      Err(err) => {
        scope.heap_mut().remove_root(callback_root);
        for root in arg_roots {
          scope.heap_mut().remove_root(root);
        }
        return Err(err);
      }
    }
  }

  let id_cell: Rc<Cell<TimerId>> = Rc::new(Cell::new(0));
  let id_cell_for_cb = Rc::clone(&id_cell);

  ExecCtxGuard::with_current::<State, _>(|host_ptr, event_loop_ptr| unsafe {
    let set_result = (&mut *event_loop_ptr).set_interval(interval, move |host, event_loop| {
      let _guard = ExecCtxGuard::install(host, event_loop);
      host.reset_budget_for_run();

      let id = id_cell_for_cb.get();
      let Some(entry) = host.timers.get(&id).cloned() else {
        // Cleared between scheduling and firing.
        return Ok(());
      };

      let global_this = Value::Object(host.realm.global_object());
      let (callback, call_args) = {
        let mut scope = host.heap.scope();
        let callback = scope
          .heap()
          .get_root(entry.callback)
          .unwrap_or(Value::Undefined);
        let call_args: Vec<Value> = entry
          .args
          .iter()
          .map(|root| scope.heap().get_root(*root).unwrap_or(Value::Undefined))
          .collect();
        (callback, call_args)
      };

      let mut ctx = FastRenderJobContext::new(host);
      ctx
        .call(host, callback, global_this, &call_args)
        .map(|_| ())
        .map_err(map_vm_error)?;

      Ok(())
    });

    match set_result {
      Ok(id) => {
        id_cell.set(id);
        (*host_ptr).timers.insert(
          id,
          TimerEntry {
            callback: callback_root,
            args: arg_roots.clone(),
          },
        );
      }
      Err(err) => {
        (*host_ptr).pending_host_error.get_or_insert(err);
      }
    }
  });

  let id = id_cell.get();
  if id == 0 {
    // Scheduling failed: remove the roots created above.
    scope.heap_mut().remove_root(callback_root);
    for root in arg_roots {
      scope.heap_mut().remove_root(root);
    }
  }

  Ok(Value::Number(id as f64))
}

fn native_clear_interval<State: 'static>(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHostHooks,
  _callee: vm_js::GcObject,
  _this: Value,
  args: &[Value],
) -> std::result::Result<Value, VmError> {
  let id = args.get(0).copied().map(to_timer_id).unwrap_or(0);
  ExecCtxGuard::with_current::<State, _>(|host_ptr, event_loop_ptr| unsafe {
    (&mut *event_loop_ptr).clear_interval(id);
    if let Some(entry) = (*host_ptr).timers.remove(&id) {
      scope.heap_mut().remove_root(entry.callback);
      for root in entry.args {
        scope.heap_mut().remove_root(root);
      }
    }
  });
  Ok(Value::Undefined)
}

fn native_promise_resolve<State: 'static>(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHostHooks,
  _callee: vm_js::GcObject,
  _this: Value,
  _args: &[Value],
) -> std::result::Result<Value, VmError> {
  let then_call_id = ExecCtxGuard::with_current::<State, _>(|host_ptr, _| unsafe {
    (*host_ptr).promise_then_call_id
  });

  let then_key_s = scope.alloc_string("then")?;
  scope.push_root(Value::String(then_key_s))?;

  let then_fn = {
    let name_s = scope.alloc_string("then")?;
    scope.push_root(Value::String(name_s))?;
    let func = scope.alloc_native_function(then_call_id, None, name_s, 1)?;
    Value::Object(func)
  };

  // Root `then_fn` across allocating the promise object.
  scope.push_root(then_fn)?;

  let promise_obj = scope.alloc_object()?;
  scope.push_root(Value::Object(promise_obj))?;
  scope.define_property(
    promise_obj,
    vm_js::PropertyKey::String(then_key_s),
    vm_js::PropertyDescriptor {
      enumerable: false,
      configurable: true,
      kind: vm_js::PropertyKind::Data {
        value: then_fn,
        writable: true,
      },
    },
  )?;

  Ok(Value::Object(promise_obj))
}

fn native_promise_then<State: 'static>(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHostHooks,
  _callee: vm_js::GcObject,
  _this: Value,
  args: &[Value],
) -> std::result::Result<Value, VmError> {
  let callback = args.get(0).copied().unwrap_or(Value::Undefined);
  if matches!(callback, Value::String(_)) {
    return Err(throw_type_error(
      scope,
      "Promise.then does not currently support string callbacks",
    ));
  }
  if !scope.heap().is_callable(callback)? {
    return Err(throw_type_error(
      scope,
      "Promise.then callback is not callable",
    ));
  }

  // Root callback until the job runs.
  let callback_root = scope.heap_mut().add_root(callback)?;

  let global_this = ExecCtxGuard::with_current::<State, _>(|host_ptr, _| unsafe {
    Value::Object((*host_ptr).realm.global_object())
  });

  let mut job = Job::new(JobKind::Promise, move |ctx, hooks| {
    ctx.call(hooks, callback, global_this, &[]).map(|_| ())
  });
  job.push_root(callback_root);

  // Enqueue as a FastRender microtask. We intentionally bypass `VmHostHooks::host_enqueue_promise_job`
  // here because this function is invoked via `vm.call(..)` (holding `&mut Vm`/`&mut Heap`), so
  // calling into `&mut EcmaVmRuntime` would violate Rust's aliasing rules.
  //
  // `EcmaVmRuntime` still implements `VmHostHooks` for real `vm-js` Promise integration, but this
  // minimal Promise stub enqueues the job directly.
  let job_cell: Rc<RefCell<Option<Job>>> = Rc::new(RefCell::new(Some(job)));
  let job_cell_for_task = Rc::clone(&job_cell);

  let enqueue_result: std::result::Result<(), Error> =
    ExecCtxGuard::with_current::<State, _>(|_host_ptr, event_loop_ptr| unsafe {
      (&mut *event_loop_ptr).queue_microtask(move |host, event_loop| {
        let job = job_cell_for_task
          .borrow_mut()
          .take()
          .expect("vm-js Job should be present when Promise microtask runs");

        let _guard = ExecCtxGuard::install(host, event_loop);
        host.reset_budget_for_run();

        let mut ctx = FastRenderJobContext::new(host);
        job.run(&mut ctx, host).map_err(map_vm_error)?;
        if let Some(err) = host.pending_host_error.take() {
          return Err(err);
        }
        Ok(())
      })
    });

  if let Err(err) = enqueue_result {
    if let Some(job) = job_cell.borrow_mut().take() {
      let mut ctx = HeapRootContext {
        heap: scope.heap_mut(),
      };
      job.discard(&mut ctx);
    }
    ExecCtxGuard::with_current::<State, _>(|host_ptr, _| unsafe {
      (*host_ptr).pending_host_error.get_or_insert(err);
    });
  }

  Ok(Value::Undefined)
}

// --- Minimal lexical environment ---

#[derive(Debug, Default)]
struct Env {
  var: HashMap<String, RootId>,
}

impl Env {
  fn declare_var(&mut self, heap: &mut Heap, name: &str) -> std::result::Result<(), VmError> {
    if self.var.contains_key(name) {
      return Ok(());
    }
    let root = heap.add_root(Value::Undefined)?;
    if self.var.try_reserve(1).is_err() {
      heap.remove_root(root);
      return Err(VmError::OutOfMemory);
    }
    self.var.insert(name.to_string(), root);
    Ok(())
  }

  fn get(&self, heap: &Heap, name: &str) -> Option<Value> {
    self.var.get(name).and_then(|root| heap.get_root(*root))
  }

  fn set(&mut self, heap: &mut Heap, name: &str, value: Value) -> std::result::Result<(), VmError> {
    let Some(root) = self.var.get(name).copied() else {
      return Err(VmError::Unimplemented("unbound identifier"));
    };
    heap.set_root(root, value);
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::js::clock::VirtualClock;
  use crate::js::event_loop::{RunLimits, RunUntilIdleOutcome};
  use crate::js::ScriptType;
  use std::sync::Arc;

  #[derive(Default)]
  struct TestState {
    log: Vec<&'static str>,
    interval_count: usize,
    interval_id: Option<TimerId>,
  }

  fn classic_spec() -> ScriptElementSpec {
    ScriptElementSpec {
      base_url: None,
      src: None,
      inline_text: String::new(),
      async_attr: false,
      defer_attr: false,
      parser_inserted: true,
      script_type: ScriptType::Classic,
    }
  }

  fn log_sync(
    _vm: &mut Vm,
    _scope: &mut Scope<'_>,
    _host: &mut dyn VmHostHooks,
    _callee: vm_js::GcObject,
    _this: Value,
    _args: &[Value],
  ) -> std::result::Result<Value, VmError> {
    ExecCtxGuard::with_current::<TestState, _>(|host_ptr, _| unsafe {
      (*host_ptr).state.log.push("sync");
    });
    Ok(Value::Undefined)
  }

  fn log_micro(
    _vm: &mut Vm,
    _scope: &mut Scope<'_>,
    _host: &mut dyn VmHostHooks,
    _callee: vm_js::GcObject,
    _this: Value,
    _args: &[Value],
  ) -> std::result::Result<Value, VmError> {
    ExecCtxGuard::with_current::<TestState, _>(|host_ptr, _| unsafe {
      (*host_ptr).state.log.push("micro");
    });
    Ok(Value::Undefined)
  }

  fn log_timeout(
    _vm: &mut Vm,
    _scope: &mut Scope<'_>,
    _host: &mut dyn VmHostHooks,
    _callee: vm_js::GcObject,
    _this: Value,
    _args: &[Value],
  ) -> std::result::Result<Value, VmError> {
    ExecCtxGuard::with_current::<TestState, _>(|host_ptr, _| unsafe {
      (*host_ptr).state.log.push("timeout");
    });
    Ok(Value::Undefined)
  }

  #[test]
  fn promise_then_runs_at_microtask_checkpoint() -> Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<EcmaVmRuntime<TestState>>::with_clock(clock);
    let mut host = EcmaVmRuntime::new(TestState::default(), EcmaVmRuntimeConfig::default())?;

    host.define_global_native_function("__log_sync", 0, log_sync)?;
    host.define_global_native_function("__log_micro", 0, log_micro)?;

    host.execute_classic_script(
      "Promise.resolve().then(__log_micro); __log_sync();",
      &classic_spec(),
      &mut event_loop,
    )?;
    assert_eq!(host.state.log, vec!["sync"]);

    event_loop.perform_microtask_checkpoint(&mut host)?;
    assert_eq!(host.state.log, vec!["sync", "micro"]);
    Ok(())
  }

  #[test]
  fn vm_host_hooks_enqueue_promise_jobs_as_microtasks_in_fifo_order() -> Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<EcmaVmRuntime<TestState>>::with_clock(clock);
    let mut host = EcmaVmRuntime::new(TestState::default(), EcmaVmRuntimeConfig::default())?;

    {
      let _guard = ExecCtxGuard::install(&mut host, &mut event_loop);

      host.host_enqueue_promise_job(
        Job::new(JobKind::Promise, |_ctx, _hooks| {
          ExecCtxGuard::with_current::<TestState, _>(|host_ptr, _| unsafe {
            (*host_ptr).state.log.push("job1");
          });
          Ok(())
        }),
        Some(host.realm_id),
      );

      host.host_enqueue_promise_job(
        Job::new(JobKind::Promise, |_ctx, _hooks| {
          ExecCtxGuard::with_current::<TestState, _>(|host_ptr, _| unsafe {
            (*host_ptr).state.log.push("job2");
          });
          Ok(())
        }),
        Some(host.realm_id),
      );
    }

    event_loop.perform_microtask_checkpoint(&mut host)?;
    assert_eq!(host.state.log, vec!["job1", "job2"]);
    Ok(())
  }

  #[test]
  fn microtask_runs_before_timeout_task() -> Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<EcmaVmRuntime<TestState>>::with_clock(clock);
    let mut host = EcmaVmRuntime::new(TestState::default(), EcmaVmRuntimeConfig::default())?;

    host.define_global_native_function("__log_sync", 0, log_sync)?;
    host.define_global_native_function("__log_micro", 0, log_micro)?;
    host.define_global_native_function("__log_timeout", 0, log_timeout)?;

    host.execute_classic_script(
      "setTimeout(__log_timeout, 0); queueMicrotask(__log_micro); __log_sync();",
      &classic_spec(),
      &mut event_loop,
    )?;

    event_loop.perform_microtask_checkpoint(&mut host)?;

    assert_eq!(host.state.log, vec!["sync", "micro"]);
    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    assert_eq!(host.state.log, vec!["sync", "micro", "timeout"]);
    Ok(())
  }

  #[test]
  fn interval_can_clear_itself_via_clear_interval() -> Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<EcmaVmRuntime<TestState>>::with_clock(clock);
    let mut host = EcmaVmRuntime::new(TestState::default(), EcmaVmRuntimeConfig::default())?;

    fn set_interval_id(
      _vm: &mut Vm,
      _scope: &mut Scope<'_>,
      _host: &mut dyn VmHostHooks,
      _callee: vm_js::GcObject,
      _this: Value,
      args: &[Value],
    ) -> std::result::Result<Value, VmError> {
      let id_value = args.get(0).copied().unwrap_or(Value::Number(0.0));
      let id = to_timer_id(id_value);
      ExecCtxGuard::with_current::<TestState, _>(|host_ptr, _| unsafe {
        (*host_ptr).state.interval_id = Some(id);
      });
      Ok(Value::Undefined)
    }

    fn interval_cb(
      vm: &mut Vm,
      scope: &mut Scope<'_>,
      host: &mut dyn VmHostHooks,
      _callee: vm_js::GcObject,
      _this: Value,
      _args: &[Value],
    ) -> std::result::Result<Value, VmError> {
      let (count, id) = ExecCtxGuard::with_current::<TestState, _>(|host_ptr, _| unsafe {
        (*host_ptr).state.interval_count += 1;
        (
          (*host_ptr).state.interval_count,
          (*host_ptr).state.interval_id,
        )
      });

      if count == 3 {
        let Some(id) = id else {
          return Ok(Value::Undefined);
        };

        // Call the JS global `clearInterval(id)`.
        let global = ExecCtxGuard::with_current::<TestState, _>(|host_ptr, _| unsafe {
          (*host_ptr).realm.global_object()
        });
        let global_value = Value::Object(global);
        scope.push_root(global_value)?;
        let key_s = scope.alloc_string("clearInterval")?;
        scope.push_root(Value::String(key_s))?;
        let func =
          scope.ordinary_get(vm, global, vm_js::PropertyKey::String(key_s), global_value)?;
        vm.call_with_host(scope, host, func, global_value, &[Value::Number(id as f64)])?;
      }

      Ok(Value::Undefined)
    }

    host.define_global_native_function("__set_interval_id", 1, set_interval_id)?;
    host.define_global_native_function("__interval_cb", 0, interval_cb)?;

    host.execute_classic_script(
      "__set_interval_id(setInterval(__interval_cb, 0));",
      &classic_spec(),
      &mut event_loop,
    )?;

    assert_eq!(
      event_loop.run_until_idle(
        &mut host,
        RunLimits {
          max_tasks: 10,
          max_microtasks: 100,
          max_wall_time: None,
        },
      )?,
      RunUntilIdleOutcome::Idle
    );
    assert_eq!(host.state.interval_count, 3);
    Ok(())
  }
}
