use crate::error::{Error, Result};
use crate::render_control;

use parse_js::error::SyntaxErrorType;

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::marker::PhantomData;
use std::ptr;
use std::rc::Rc;
use std::time::{Duration, Instant};

use vm_js::{
  Budget, Heap, HeapLimits, Job, Realm, RealmId, RootId, Scope, Value, Vm, VmError, VmHost,
  VmHostHooks, VmJobContext, VmOptions,
};
use webidl_vm_js::{WebIdlBindingsHost, WebIdlBindingsHostSlot};

use super::event_loop::{EventLoop, TimerId};
use super::vm_error_format;
use super::ScriptElementSpec;

/// FastRender embedding of `ecma-rs`'s `vm-js` primitives.
///
/// This is an MVP JS host/runtime that:
/// - owns the `vm-js` heap + VM + a single document realm,
/// - installs minimal Window-ish globals (`queueMicrotask` and timers),
/// - integrates ECMAScript Promise jobs with the FastRender [`EventLoop`] microtask queue.
///
/// Note: `vm-js` is still early-stage; this runtime includes a small AST interpreter for a subset
/// of JavaScript needed by FastRender's early scripting and scheduling tests.
pub struct EcmaVmRuntime<State: WebIdlBindingsHost + 'static> {
  pub state: State,
  bindings_host: WebIdlBindingsHostSlot,
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
      heap_limits: super::vm_limits::default_heap_limits(),
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

impl<State: WebIdlBindingsHost + 'static> EcmaVmRuntime<State> {
  pub fn new(state: State, config: EcmaVmRuntimeConfig) -> Result<Self> {
    let heap = Heap::new(config.heap_limits);

    let vm_options = VmOptions {
      default_fuel: config.fuel,
      default_deadline: config.deadline,
      check_time_every: config.check_time_every,
      external_interrupt_flag: Some(render_control::interrupt_flag()),
      ..VmOptions::default()
    };
    let mut vm = Vm::new(vm_options);

    let mut heap = heap;
    let realm = Realm::new(&mut vm, &mut heap).map_err(|err| map_vm_error(&mut heap, err))?;

    let mut rt = Self {
      state,
      bindings_host: WebIdlBindingsHostSlot::default(),
      heap,
      vm,
      realm,
      env: Env::default(),
      timers: HashMap::new(),
      config,
      realm_id: RealmId::from_raw(1),
      pending_host_error: None,
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
      .map_err(|err| map_vm_error(&mut self.heap, err))?;
    self
      .env
      .set(&mut self.heap, "undefined", Value::Undefined)
      .map_err(|err| map_vm_error(&mut self.heap, err))?;

    self
      .env
      .declare_var(&mut self.heap, "globalThis")
      .map_err(|err| map_vm_error(&mut self.heap, err))?;
    self
      .env
      .set(
        &mut self.heap,
        "globalThis",
        Value::Object(self.realm.global_object()),
      )
      .map_err(|err| map_vm_error(&mut self.heap, err))?;

    // queueMicrotask(fn)
    self.define_global_native_function("queueMicrotask", 1, native_queue_microtask::<State>)?;

    // Timers.
    self.define_global_native_function("setTimeout", 1, native_set_timeout::<State>)?;
    self.define_global_native_function("clearTimeout", 1, native_clear_timeout::<State>)?;
    self.define_global_native_function("setInterval", 1, native_set_interval::<State>)?;
    self.define_global_native_function("clearInterval", 1, native_clear_interval::<State>)?;
    Ok(())
  }

  fn alloc_native_function(
    &mut self,
    name: &str,
    length: u32,
    call: vm_js::NativeCall,
  ) -> Result<Value> {
    let call_id = self
      .vm
      .register_native_call(call)
      .map_err(|err| map_vm_error(&mut self.heap, err))?;
    let mut scope = self.heap.scope();
    let name_s = scope
      .alloc_string(name)
      .map_err(|err| map_vm_error(scope.heap_mut(), err))?;
    scope
      .push_root(Value::String(name_s))
      .map_err(|err| map_vm_error(scope.heap_mut(), err))?;
    let func = scope
      .alloc_native_function(call_id, None, name_s, length)
      .map_err(|err| map_vm_error(scope.heap_mut(), err))?;
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
      .map_err(|err| map_vm_error(&mut self.heap, err))?;
    self
      .env
      .set(&mut self.heap, name, value)
      .map_err(|err| map_vm_error(&mut self.heap, err))?;

    // And on the realm global object for spec shape.
    let mut scope = self.heap.scope();
    let global = self.realm.global_object();
    scope
      .push_root(Value::Object(global))
      .map_err(|err| map_vm_error(scope.heap_mut(), err))?;
    scope
      .push_root(value)
      .map_err(|err| map_vm_error(scope.heap_mut(), err))?;

    let key = prop_key(&mut scope, name).map_err(|err| map_vm_error(scope.heap_mut(), err))?;
    define_value(&mut scope, global, key, value)
      .map_err(|err| map_vm_error(scope.heap_mut(), err))?;
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

    // Enforce VM budgets/interrupts during parsing as well as evaluation.
    self
      .vm
      .tick()
      .map_err(|err| map_vm_error(&mut self.heap, err))?;

    let opts = parse_js::ParseOptions {
      dialect: parse_js::Dialect::Ecma,
      source_type: parse_js::SourceType::Script,
    };
    let top = {
      const PARSE_CANCEL_STRIDE: usize = 1024;
      let mut parse_counter = 0usize;
      let mut tick_err: Option<VmError> = None;
      let vm = &mut self.vm;
      match parse_js::parse_with_options_cancellable_by(script_text, opts, || {
        parse_counter = parse_counter.wrapping_add(1);
        if parse_counter % PARSE_CANCEL_STRIDE != 0 {
          return false;
        }
        match vm.tick() {
          Ok(()) => false,
          Err(err) => {
            tick_err = Some(err);
            true
          }
        }
      }) {
        Ok(top) => top,
        Err(err) => {
          if err.typ == SyntaxErrorType::Cancelled {
            if let Some(vm_err) = tick_err.take() {
              return Err(map_vm_error(&mut self.heap, vm_err));
            }
            if let Err(vm_err) = vm.tick() {
              return Err(map_vm_error(&mut self.heap, vm_err));
            }
            return Err(Error::Other("JS parse cancelled".to_string()));
          }
          return Err(Error::Other(format!("JS parse error: {err}")));
        }
      }
    };

    let global_this = self.global_this();
    let intrinsics = *self.realm.intrinsics();
    // `vm-js` Promise built-ins require a host hook implementation to enqueue jobs. We use a small
    // adapter that forwards to `EcmaVmRuntime` while keeping the VM/heap borrowable.
    let mut hooks = RuntimeHostHooks::new(self);

    // `vm-js` native functions can downcast the explicit `VmHost` context passed through
    // `call_with_host_and_hooks` / `construct_with_host_and_hooks`. For this legacy runtime, pass a
    // lightweight host context that points at the embedder `state` (instead of running with the
    // default dummy host).
    let mut host_ctx = EcmaVmHostContext::new(&mut self.state);

    let (vm, env, heap, pending_host_error) = (
      &mut self.vm,
      &mut self.env,
      &mut self.heap,
      &mut self.pending_host_error,
    );
    let mut evaluator = Evaluator {
      vm,
      env,
      host: &mut host_ctx,
      hooks: &mut hooks,
      global_this,
      intrinsics,
    };

    for stmt in &top.stx.body {
      let mut scope = heap.scope();
      evaluator
        .eval_stmt(&mut scope, stmt)
        .map_err(|err| map_vm_error(scope.heap_mut(), err))?;
      if let Some(err) = pending_host_error.take() {
        return Err(err);
      }
    }

    Ok(())
  }
}

impl<State: WebIdlBindingsHost + 'static> WebIdlBindingsHost for EcmaVmRuntime<State> {
  fn call_operation(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    receiver: Option<Value>,
    interface: &'static str,
    operation: &'static str,
    overload: usize,
    args: &[Value],
  ) -> std::result::Result<Value, VmError> {
    self
      .state
      .call_operation(vm, scope, receiver, interface, operation, overload, args)
  }

  fn call_constructor(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    interface: &'static str,
    overload: usize,
    args: &[Value],
    new_target: Value,
  ) -> std::result::Result<Value, VmError> {
    self
      .state
      .call_constructor(vm, scope, interface, overload, args, new_target)
  }
}

struct Evaluator<'a> {
  vm: &'a mut Vm,
  env: &'a mut Env,
  host: &'a mut dyn VmHost,
  hooks: &'a mut dyn VmHostHooks,
  global_this: Value,
  intrinsics: vm_js::Intrinsics,
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
      Expr::LitArr(node) => self.eval_array_literal(scope, &node.stx),
      Expr::Id(node) => {
        if let Some(value) = self.env.get(scope.heap(), &node.stx.name) {
          return Ok(value);
        }

        // Fall back to the global object so realm-provided globals (e.g. `Promise`) are visible to
        // the MVP evaluator without needing to mirror every binding in `Env`.
        let Value::Object(global_obj) = self.global_this else {
          return Err(VmError::Unimplemented("global object is not an object"));
        };

        let mut child = scope.reborrow();
        // Root the receiver and key string across allocation + prototype lookup.
        child.push_root(self.global_this)?;

        let key_s = child.alloc_string(&node.stx.name)?;
        child.push_root(Value::String(key_s))?;
        let key = vm_js::PropertyKey::String(key_s);

        if child.heap().get_property(global_obj, &key)?.is_none() {
          return Err(VmError::Unimplemented("unbound identifier"));
        }

        // Use `ordinary_get_with_host_and_hooks` so accessor getters run via
        // `Vm::call_with_host_and_hooks` and can enqueue Promise jobs through the FastRender host
        // hooks (instead of `vm-js`'s internal microtask queue used by `Vm::call`).
        child.ordinary_get_with_host_and_hooks(
          self.vm,
          self.host,
          self.hooks,
          global_obj,
          key,
          self.global_this,
        )
      }
      Expr::This(_) => Ok(self.global_this),
      Expr::Member(node) => self.eval_member_expr(scope, &node.stx),
      Expr::Call(node) => self.eval_call_expr(scope, &node.stx),
      _ => Err(VmError::Unimplemented("expression type")),
    }
  }

  fn eval_array_literal(
    &mut self,
    scope: &mut Scope<'_>,
    expr: &parse_js::ast::expr::lit::LitArrExpr,
  ) -> std::result::Result<Value, VmError> {
    let len = expr.elements.len();
    let arr = scope.alloc_array(len)?;
    scope.push_root(Value::Object(arr))?;
    scope
      .heap_mut()
      .object_set_prototype(arr, Some(self.intrinsics.array_prototype()))?;

    for (idx, elem) in expr.elements.iter().enumerate() {
      match elem {
        parse_js::ast::expr::lit::LitArrElem::Empty => {}
        parse_js::ast::expr::lit::LitArrElem::Single(value) => {
          let mut child = scope.reborrow();
          child.push_root(Value::Object(arr))?;

          let v = self.eval_expr(&mut child, value)?;
          child.push_root(v)?;

          let key_s = child.alloc_string(&idx.to_string())?;
          child.push_root(Value::String(key_s))?;
          let key = vm_js::PropertyKey::from_string(key_s);

          child.define_property(
            arr,
            key,
            vm_js::PropertyDescriptor {
              enumerable: true,
              configurable: true,
              kind: vm_js::PropertyKind::Data {
                value: v,
                writable: true,
              },
            },
          )?;
        }
        parse_js::ast::expr::lit::LitArrElem::Rest(_) => {
          return Err(VmError::Unimplemented("array literal spread"));
        }
      }
    }

    Ok(Value::Object(arr))
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
    child.ordinary_get_with_host_and_hooks(self.vm, self.host, self.hooks, obj, key, obj_value)
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
        let func = child
          .ordinary_get_with_host_and_hooks(self.vm, self.host, self.hooks, obj, key, obj_value)?;
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

    self
      .vm
      .call_with_host_and_hooks(self.host, &mut call_scope, self.hooks, callee, this, &args)
  }
}

impl<State: WebIdlBindingsHost + 'static> Drop for EcmaVmRuntime<State> {
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
  fn install<State: WebIdlBindingsHost + 'static>(
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

  fn with_current<State: WebIdlBindingsHost + 'static, R>(
    f: impl FnOnce(*mut EcmaVmRuntime<State>, *mut EventLoop<EcmaVmRuntime<State>>) -> R,
  ) -> Option<R> {
    EXEC_CTX.with(|cell| {
      let ctx = cell.get();
      if ctx.host.is_null() || ctx.event_loop.is_null() {
        debug_assert!(
          !ctx.host.is_null() && !ctx.event_loop.is_null(),
          "vm-js host hook called outside of an active JS execution context"
        );
        return None;
      }
      // SAFETY: `ExecCtxGuard` installs pointers that are valid for the duration of script/job
      // execution. This intentionally bypasses Rust's aliasing rules to allow `vm-js` native
      // functions (which do not carry host references) to access the FastRender event loop.
      Some(f(
        ctx.host as *mut EcmaVmRuntime<State>,
        ctx.event_loop as *mut EventLoop<EcmaVmRuntime<State>>,
      ))
    })
  }
}

impl Drop for ExecCtxGuard {
  fn drop(&mut self) {
    let previous = self.previous;
    EXEC_CTX.with(|cell| cell.set(previous));
  }
}

/// A lightweight `VmHost` context for this legacy runtime.
///
/// Host-aware `vm-js` entrypoints accept an explicit `&mut dyn VmHost` so native calls/constructs
/// can downcast to embedder state. The canonical `WindowHost` pipeline passes the real embedder
/// host (e.g. `DocumentHostState`); this legacy runtime passes a small wrapper around a raw
/// pointer to its `state` field.
struct EcmaVmHostContext<State: WebIdlBindingsHost + 'static> {
  state: *mut State,
}

impl<State: WebIdlBindingsHost + 'static> EcmaVmHostContext<State> {
  fn new(state: &mut State) -> Self {
    Self { state }
  }

  #[allow(dead_code)]
  unsafe fn state_mut(&mut self) -> &mut State {
    &mut *self.state
  }
}

struct RuntimeHostHooks<State: WebIdlBindingsHost + 'static> {
  host: *mut EcmaVmRuntime<State>,
  bindings_host: WebIdlBindingsHostSlot,
}

impl<State: WebIdlBindingsHost + 'static> RuntimeHostHooks<State> {
  fn new(host: &mut EcmaVmRuntime<State>) -> Self {
    Self {
      host: host as *mut _,
      bindings_host: WebIdlBindingsHostSlot::new(host),
    }
  }
}

impl<State: WebIdlBindingsHost + 'static> VmHostHooks for RuntimeHostHooks<State> {
  fn host_enqueue_promise_job(&mut self, job: Job, realm: Option<RealmId>) {
    // SAFETY: `RuntimeHostHooks` is only constructed while an `EcmaVmRuntime` is actively
    // executing a task/microtask. The raw pointer indirection is required because `vm-js` host
    // hooks are invoked from within `Vm::call` without access to the Rust host borrow.
    unsafe { (&mut *self.host).host_enqueue_promise_job(job, realm) }
  }

  fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
    Some(&mut self.bindings_host)
  }

  fn host_call_job_callback(
    &mut self,
    ctx: &mut dyn VmJobContext,
    callback: &vm_js::JobCallback,
    this_argument: Value,
    arguments: &[Value],
  ) -> std::result::Result<Value, VmError> {
    unsafe { (&mut *self.host).host_call_job_callback(ctx, callback, this_argument, arguments) }
  }
}

// --- vm-js host hooks ---

impl<State: WebIdlBindingsHost + 'static> VmHostHooks for EcmaVmRuntime<State> {
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

    let enqueue_result: std::result::Result<(), Error> = ExecCtxGuard::with_current::<State, _>(
      |_host_ptr, event_loop_ptr| unsafe {
        (&mut *event_loop_ptr).queue_microtask(move |host, event_loop| {
          run_vm_js_job_microtask(job_cell_for_task, host, event_loop)
        })
      },
    )
    .unwrap_or_else(|| {
      Err(Error::Other(
        "vm-js Promise job enqueued outside of an active JS execution context".to_string(),
      ))
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
    let mut hooks = RuntimeHostHooks::new(self);
    ctx.call(
      &mut hooks,
      Value::Object(callback.callback_object()),
      this_argument,
      arguments,
    )
  }

  fn as_any_mut(&mut self) -> Option<&mut dyn Any> {
    let (state, slot) = (&mut self.state, &mut self.bindings_host);
    slot.set(state);
    Some(slot)
  }
}

fn run_vm_js_job_microtask<State: WebIdlBindingsHost + 'static>(
  job_cell: Rc<RefCell<Option<Job>>>,
  host: &mut EcmaVmRuntime<State>,
  event_loop: &mut EventLoop<EcmaVmRuntime<State>>,
) -> Result<()> {
  // Jobs are stored in an `Option` so we can `discard` them on enqueue failure (without leaking VM
  // roots). If the job is missing when the microtask runs, treat it as a no-op rather than
  // panicking: this can happen if a microtask runnable was retained while its job was already
  // discarded/consumed.
  let Some(job) = job_cell.borrow_mut().take() else {
    return Ok(());
  };

  let _guard = ExecCtxGuard::install(host, event_loop);
  host.reset_budget_for_run();

  let mut ctx = FastRenderJobContext::new(host);
  let mut hooks = RuntimeHostHooks::new(host);
  job
    .run(&mut ctx, &mut hooks)
    .map_err(|err| map_vm_error(&mut host.heap, err))?;
  if let Some(err) = host.pending_host_error.take() {
    return Err(err);
  }
  Ok(())
}

struct FastRenderJobContext<State: WebIdlBindingsHost + 'static> {
  vm: *mut Vm,
  heap: *mut Heap,
  state: *mut State,
}

impl<State: WebIdlBindingsHost + 'static> FastRenderJobContext<State> {
  fn new(host: &mut EcmaVmRuntime<State>) -> Self {
    Self {
      vm: &mut host.vm as *mut Vm,
      heap: &mut host.heap as *mut Heap,
      state: &mut host.state as *mut State,
    }
  }
}

impl<State: WebIdlBindingsHost + 'static> VmJobContext for FastRenderJobContext<State> {
  fn call(
    &mut self,
    host_hooks: &mut dyn VmHostHooks,
    callee: Value,
    this: Value,
    args: &[Value],
  ) -> std::result::Result<Value, VmError> {
    // SAFETY: `FastRenderJobContext` is only used while `EcmaVmRuntime` is alive. This uses raw
    // pointers to split-borrow `EcmaVmRuntime` so it can be passed to `Job::run` as both a
    // `VmJobContext` (backed by the VM/heap) and a `VmHostHooks` implementation (Promise job
    // scheduling) without violating Rust's aliasing rules.
    unsafe {
      let heap = &mut *self.heap;
      let vm = &mut *self.vm;
      let mut scope = heap.scope();
      let mut host = EcmaVmHostContext::<State> { state: self.state };
      vm.call_with_host_and_hooks(&mut host, &mut scope, host_hooks, callee, this, args)
    }
  }

  fn construct(
    &mut self,
    host_hooks: &mut dyn VmHostHooks,
    callee: Value,
    args: &[Value],
    new_target: Value,
  ) -> std::result::Result<Value, VmError> {
    unsafe {
      let heap = &mut *self.heap;
      let vm = &mut *self.vm;
      let mut scope = heap.scope();
      let mut host = EcmaVmHostContext::<State> { state: self.state };
      vm.construct_with_host_and_hooks(&mut host, &mut scope, host_hooks, callee, args, new_target)
    }
  }

  fn add_root(&mut self, value: Value) -> std::result::Result<RootId, VmError> {
    unsafe { (&mut *self.heap).add_root(value) }
  }

  fn remove_root(&mut self, id: RootId) {
    unsafe { (&mut *self.heap).remove_root(id) }
  }
}
// --- Script execution helpers ---

impl<State: WebIdlBindingsHost + 'static> EcmaVmRuntime<State> {
  pub fn execute_classic_script(
    &mut self,
    script_text: &str,
    _spec: &ScriptElementSpec,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<()> {
    let _guard = ExecCtxGuard::install(self, event_loop);
    self.execute_script_text(script_text)
  }

  pub fn execute_module_script(
    &mut self,
    script_text: &str,
    _spec: &ScriptElementSpec,
    event_loop: &mut EventLoop<Self>,
  ) -> Result<()> {
    // `vm-js` does not yet expose a full module evaluator. For now, treat module scripts as
    // classic-script execution for the purposes of scheduling/orchestration tests.
    //
    // IMPORTANT: module scripts must still be executed from a queued task (enforced by the
    // scheduler), and `Document.currentScript` bookkeeping is handled by the orchestrator.
    let _guard = ExecCtxGuard::install(self, event_loop);
    self.execute_script_text(script_text)
  }
}

// --- Helpers ---

fn map_vm_error(heap: &mut Heap, err: VmError) -> Error {
  let is_exception = err.thrown_value().is_some();
  let message = vm_error_format::vm_error_to_string(heap, err);
  if is_exception {
    Error::Other(format!("JS exception: {message}"))
  } else {
    Error::Other(format!("JS error: {message}"))
  }
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

fn throw_type_error(vm: &Vm, scope: &mut Scope<'_>, message: &str) -> VmError {
  let Some(intr) = vm.intrinsics() else {
    // This runtime always creates a `vm-js` Realm (which initializes intrinsics), but keep this
    // branch non-panicking so host embeddings can surface a useful error instead of crashing.
    return VmError::Unimplemented("intrinsics not initialized");
  };
  match vm_js::new_error(scope, intr.type_error_prototype(), "TypeError", message) {
    Ok(value) => VmError::Throw(value),
    Err(_) => VmError::Throw(Value::Undefined),
  }
}

fn to_number_or_throw_type_error(
  vm: &Vm,
  scope: &mut Scope<'_>,
  value: Value,
) -> std::result::Result<f64, VmError> {
  match scope.heap_mut().to_number(value) {
    Ok(n) => Ok(n),
    Err(VmError::TypeError(msg)) => Err(throw_type_error(vm, scope, msg)),
    Err(err) => Err(err),
  }
}

fn normalize_delay_ms(
  vm: &Vm,
  scope: &mut Scope<'_>,
  value: Value,
) -> std::result::Result<u64, VmError> {
  let mut n = to_number_or_throw_type_error(vm, scope, value)?;
  if !n.is_finite() || n.is_nan() {
    n = 0.0;
  }
  if n < 0.0 {
    n = 0.0;
  }
  // `ToIntegerOrInfinity` rounds toward zero.
  let n = n.trunc();
  if n >= u64::MAX as f64 {
    Ok(u64::MAX)
  } else {
    Ok(n as u64)
  }
}

fn normalize_timer_id(
  vm: &Vm,
  scope: &mut Scope<'_>,
  value: Value,
) -> std::result::Result<TimerId, VmError> {
  let mut n = to_number_or_throw_type_error(vm, scope, value)?;
  if !n.is_finite() || n.is_nan() {
    n = 0.0;
  }
  let n = n.trunc();
  if n >= i32::MAX as f64 {
    Ok(i32::MAX)
  } else if n <= i32::MIN as f64 {
    Ok(i32::MIN)
  } else {
    Ok(n as i32)
  }
}

// --- Native web API globals ---

fn native_queue_microtask<State: WebIdlBindingsHost + 'static>(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: vm_js::GcObject,
  _this: Value,
  args: &[Value],
) -> std::result::Result<Value, VmError> {
  let callback = args.get(0).copied().unwrap_or(Value::Undefined);
  if matches!(callback, Value::String(_)) {
    return Err(throw_type_error(
      vm,
      scope,
      "queueMicrotask does not currently support string callbacks",
    ));
  }
  if !scope.heap().is_callable(callback)? {
    return Err(throw_type_error(
      vm,
      scope,
      "queueMicrotask callback is not callable",
    ));
  }

  let callback_root = scope.heap_mut().add_root(callback)?;

  let Some(queued) = ExecCtxGuard::with_current::<State, _>(|host_ptr, event_loop_ptr| unsafe {
    match (&mut *event_loop_ptr).queue_microtask(move |host, event_loop| {
      let _guard = ExecCtxGuard::install(host, event_loop);
      host.reset_budget_for_run();

      let callback = host
        .heap
        .get_root(callback_root)
        .unwrap_or(Value::Undefined);

      // HTML `queueMicrotask` invokes callbacks with an `undefined` callback-this value.
      let mut ctx = FastRenderJobContext::new(host);
      let mut hooks = RuntimeHostHooks::new(host);
      let result = ctx.call(&mut hooks, callback, Value::Undefined, &[]);
      host.heap.remove_root(callback_root);
      result
        .map(|_| ())
        .map_err(|err| map_vm_error(&mut host.heap, err))?;
      if let Some(err) = host.pending_host_error.take() {
        return Err(err);
      }
      Ok(())
    }) {
      Ok(()) => true,
      Err(err) => {
        (*host_ptr).pending_host_error.get_or_insert(err);
        false
      }
    }
  }) else {
    // If we somehow reached this native function without an execution context installed, avoid
    // dereferencing null raw pointers and instead behave like a synchronous exception.
    scope.heap_mut().remove_root(callback_root);
    return Err(throw_type_error(
      vm,
      scope,
      "queueMicrotask called outside of an active JS execution context",
    ));
  };

  if !queued {
    // The microtask was not enqueued, so the root will never be removed by the task.
    scope.heap_mut().remove_root(callback_root);
  }

  Ok(Value::Undefined)
}

fn native_set_timeout<State: WebIdlBindingsHost + 'static>(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: vm_js::GcObject,
  _this: Value,
  args: &[Value],
) -> std::result::Result<Value, VmError> {
  let handler = args.get(0).copied().unwrap_or(Value::Undefined);
  if matches!(handler, Value::String(_)) {
    return Err(throw_type_error(
      vm,
      scope,
      "setTimeout does not currently support string handlers",
    ));
  }
  if !scope.heap().is_callable(handler)? {
    return Err(throw_type_error(
      vm,
      scope,
      "setTimeout callback is not callable",
    ));
  }

  // Root the handler across delay conversion (which may allocate/GC for object delays).
  let handler = scope.push_root(handler)?;

  let delay_value = args.get(1).copied().unwrap_or(Value::Undefined);
  let delay_ms = normalize_delay_ms(vm, scope, delay_value)?;
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

  let _ = ExecCtxGuard::with_current::<State, _>(|host_ptr, event_loop_ptr| unsafe {
    let set_result = (&mut *event_loop_ptr).set_timeout(delay, move |host, event_loop| {
      let _guard = ExecCtxGuard::install(host, event_loop);
      host.reset_budget_for_run();

      let id = id_cell_for_cb.get();
      let Some(entry) = host.timers.remove(&id) else {
        // Cleared between scheduling and firing.
        return Ok(());
      };

      let callback = host
        .heap
        .get_root(entry.callback)
        .unwrap_or(Value::Undefined);
      let call_args: Vec<Value> = entry
        .args
        .iter()
        .map(|root| host.heap.get_root(*root).unwrap_or(Value::Undefined))
        .collect();

      let global_this = Value::Object(host.realm.global_object());
      let mut ctx = FastRenderJobContext::new(host);
      let mut hooks = RuntimeHostHooks::new(host);
      let result = ctx.call(&mut hooks, callback, global_this, &call_args);

      host.heap.remove_root(entry.callback);
      for root in entry.args {
        host.heap.remove_root(root);
      }

      result
        .map(|_| ())
        .map_err(|err| map_vm_error(&mut host.heap, err))?;
      if let Some(err) = host.pending_host_error.take() {
        return Err(err);
      }
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

fn native_clear_timeout<State: WebIdlBindingsHost + 'static>(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: vm_js::GcObject,
  _this: Value,
  args: &[Value],
) -> std::result::Result<Value, VmError> {
  let id_value = args.get(0).copied().unwrap_or(Value::Number(0.0));
  let id = normalize_timer_id(vm, scope, id_value)?;
  let _ = ExecCtxGuard::with_current::<State, _>(|host_ptr, event_loop_ptr| unsafe {
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

fn native_set_interval<State: WebIdlBindingsHost + 'static>(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: vm_js::GcObject,
  _this: Value,
  args: &[Value],
) -> std::result::Result<Value, VmError> {
  let handler = args.get(0).copied().unwrap_or(Value::Undefined);
  if matches!(handler, Value::String(_)) {
    return Err(throw_type_error(
      vm,
      scope,
      "setInterval does not currently support string handlers",
    ));
  }
  if !scope.heap().is_callable(handler)? {
    return Err(throw_type_error(
      vm,
      scope,
      "setInterval callback is not callable",
    ));
  }

  // Root the handler across delay conversion (which may allocate/GC for object delays).
  let handler = scope.push_root(handler)?;

  let interval_value = args.get(1).copied().unwrap_or(Value::Undefined);
  let interval_ms = normalize_delay_ms(vm, scope, interval_value)?;
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

  let _ = ExecCtxGuard::with_current::<State, _>(|host_ptr, event_loop_ptr| unsafe {
    let set_result = (&mut *event_loop_ptr).set_interval(interval, move |host, event_loop| {
      let _guard = ExecCtxGuard::install(host, event_loop);
      host.reset_budget_for_run();

      let id = id_cell_for_cb.get();
      let Some(entry) = host.timers.get(&id).cloned() else {
        // Cleared between scheduling and firing.
        return Ok(());
      };

      let callback = host
        .heap
        .get_root(entry.callback)
        .unwrap_or(Value::Undefined);
      let call_args: Vec<Value> = entry
        .args
        .iter()
        .map(|root| host.heap.get_root(*root).unwrap_or(Value::Undefined))
        .collect();

      let global_this = Value::Object(host.realm.global_object());
      let mut ctx = FastRenderJobContext::new(host);
      ctx
        .call(host, callback, global_this, &call_args)
        .map(|_| ())
        .map_err(|err| map_vm_error(&mut host.heap, err))?;
      if let Some(err) = host.pending_host_error.take() {
        return Err(err);
      }

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

fn native_clear_interval<State: WebIdlBindingsHost + 'static>(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: vm_js::GcObject,
  _this: Value,
  args: &[Value],
) -> std::result::Result<Value, VmError> {
  let id_value = args.get(0).copied().unwrap_or(Value::Number(0.0));
  let id = normalize_timer_id(vm, scope, id_value)?;
  let _ = ExecCtxGuard::with_current::<State, _>(|host_ptr, event_loop_ptr| unsafe {
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
  use crate::clock::VirtualClock;
  use crate::js::event_loop::{RunLimits, RunUntilIdleOutcome};
  use crate::js::ScriptType;
  use std::sync::Arc;
  use vm_js::{PropertyKey, PropertyKind, StackFrame};

  #[derive(Default)]
  struct TestState {
    log: Vec<&'static str>,
    interval_count: usize,
    interval_id: Option<TimerId>,
  }

  impl WebIdlBindingsHost for TestState {
    fn call_operation(
      &mut self,
      _vm: &mut Vm,
      _scope: &mut Scope<'_>,
      _receiver: Option<Value>,
      _interface: &'static str,
      _operation: &'static str,
      _overload: usize,
      _args: &[Value],
    ) -> std::result::Result<Value, VmError> {
      self.log.push("downcast");
      Ok(Value::Undefined)
    }

    fn call_constructor(
      &mut self,
      _vm: &mut Vm,
      _scope: &mut Scope<'_>,
      _interface: &'static str,
      _overload: usize,
      _args: &[Value],
      _new_target: Value,
    ) -> std::result::Result<Value, VmError> {
      Err(VmError::Unimplemented(
        "WebIDL bindings host not implemented for TestState",
      ))
    }
  }

  fn classic_spec() -> ScriptElementSpec {
    ScriptElementSpec {
      base_url: None,
      src: None,
      src_attr_present: false,
      inline_text: String::new(),
      async_attr: false,
      force_async: false,
      defer_attr: false,
      nomodule_attr: false,
      crossorigin: None,
      integrity_attr_present: false,
      integrity: None,
      referrer_policy: None,
      fetch_priority: None,
      parser_inserted: true,
      node_id: None,
      script_type: ScriptType::Classic,
    }
  }

  #[test]
  fn parse_respects_fuel_budget() -> Result<()> {
    let mut host = EcmaVmRuntime::new(
      TestState::default(),
      EcmaVmRuntimeConfig {
        fuel: Some(0),
        deadline: None,
        check_time_every: 1,
        ..EcmaVmRuntimeConfig::default()
      },
    )?;

    let err = host.execute_script_text("").unwrap_err();
    let msg = err.to_string();
    assert!(
      msg.contains("out of fuel"),
      "expected out-of-fuel error, got: {msg}"
    );
    assert!(
      !msg.contains("JS parse error"),
      "expected termination error (not a parse error), got: {msg}"
    );
    Ok(())
  }

  #[test]
  fn parse_cancellation_is_mapped_to_vm_error() -> Result<()> {
    // Exercise the `SyntaxErrorType::Cancelled` → stored `VmError` mapping.
    //
    // Use `fuel=1` so the pre-parse tick succeeds (consuming the only fuel), then force a long
    // parse so the cancellable parse callback ticks again and runs out of fuel during parsing.
    let mut host = EcmaVmRuntime::new(
      TestState::default(),
      EcmaVmRuntimeConfig {
        fuel: Some(1),
        deadline: None,
        check_time_every: 1,
        ..EcmaVmRuntimeConfig::default()
      },
    )?;

    let source = ";".repeat(2048);
    let err = host.execute_script_text(&source).unwrap_err();
    let msg = err.to_string();
    assert!(
      msg.contains("out of fuel"),
      "expected out-of-fuel error, got: {msg}"
    );
    assert!(
      !msg.contains("JS parse error"),
      "expected parse cancellation to surface as VM error (not syntax), got: {msg}"
    );
    Ok(())
  }

  fn log_downcast_via_hooks(
    _vm: &mut Vm,
    _scope: &mut Scope<'_>,
    _host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    _callee: vm_js::GcObject,
    _this: Value,
    _args: &[Value],
  ) -> std::result::Result<Value, VmError> {
    let host = webidl_vm_js::host_from_hooks(hooks)?;
    let _ = host.call_operation(_vm, _scope, None, "Test", "log_downcast_via_hooks", 0, &[])?;
    Ok(Value::Undefined)
  }

  fn log_sync(
    _vm: &mut Vm,
    _scope: &mut Scope<'_>,
    _host: &mut dyn VmHost,
    _hooks: &mut dyn VmHostHooks,
    _callee: vm_js::GcObject,
    _this: Value,
    _args: &[Value],
  ) -> std::result::Result<Value, VmError> {
    let _ = ExecCtxGuard::with_current::<TestState, _>(|host_ptr, _| unsafe {
      (*host_ptr).state.log.push("sync");
    });
    Ok(Value::Undefined)
  }

  fn log_micro(
    _vm: &mut Vm,
    _scope: &mut Scope<'_>,
    _host: &mut dyn VmHost,
    _hooks: &mut dyn VmHostHooks,
    _callee: vm_js::GcObject,
    _this: Value,
    _args: &[Value],
  ) -> std::result::Result<Value, VmError> {
    let _ = ExecCtxGuard::with_current::<TestState, _>(|host_ptr, _| unsafe {
      (*host_ptr).state.log.push("micro");
    });
    Ok(Value::Undefined)
  }

  fn enqueue_nested_microtask(
    _vm: &mut Vm,
    _scope: &mut Scope<'_>,
    _host: &mut dyn VmHost,
    _hooks: &mut dyn VmHostHooks,
    _callee: vm_js::GcObject,
    _this: Value,
    _args: &[Value],
  ) -> std::result::Result<Value, VmError> {
    let _ = ExecCtxGuard::with_current::<TestState, _>(|host_ptr, event_loop_ptr| unsafe {
      (*host_ptr).state.log.push("then");
      let event_loop = &mut *event_loop_ptr;
      event_loop
        .queue_microtask(|host, event_loop| {
          let _guard = ExecCtxGuard::install(host, event_loop);
          host.state.log.push("nested");
          Ok(())
        })
        .expect("queue nested microtask");
    });
    Ok(Value::Undefined)
  }

  fn log_timeout(
    _vm: &mut Vm,
    _scope: &mut Scope<'_>,
    _host: &mut dyn VmHost,
    _hooks: &mut dyn VmHostHooks,
    _callee: vm_js::GcObject,
    _this: Value,
    _args: &[Value],
  ) -> std::result::Result<Value, VmError> {
    let _ = ExecCtxGuard::with_current::<TestState, _>(|host_ptr, _| unsafe {
      (*host_ptr).state.log.push("timeout");
    });
    Ok(Value::Undefined)
  }

  fn log_number_arg(
    _vm: &mut Vm,
    _scope: &mut Scope<'_>,
    _host: &mut dyn VmHost,
    _hooks: &mut dyn VmHostHooks,
    _callee: vm_js::GcObject,
    _this: Value,
    args: &[Value],
  ) -> std::result::Result<Value, VmError> {
    let value = args.get(0).copied().unwrap_or(Value::Undefined);
    let tag = match value {
      Value::Number(n) if n == 1.0 => "1",
      Value::Number(n) if n == 2.0 => "2",
      Value::Undefined => "undefined",
      _ => "other",
    };
    let _ = ExecCtxGuard::with_current::<TestState, _>(|host_ptr, _| unsafe {
      (*host_ptr).state.log.push(tag);
    });
    Ok(Value::Undefined)
  }

  fn thenable_then(
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    _host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    _callee: vm_js::GcObject,
    _this: Value,
    args: &[Value],
  ) -> std::result::Result<Value, VmError> {
    let _ = ExecCtxGuard::with_current::<TestState, _>(|host_ptr, _| unsafe {
      (*host_ptr).state.log.push("thenable_then");
    });

    let resolve = args.get(0).copied().unwrap_or(Value::Undefined);
    vm.call_with_host(scope, hooks, resolve, Value::Undefined, &[])?;
    Ok(Value::Undefined)
  }

  fn make_thenable(
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    _host: &mut dyn VmHost,
    _hooks: &mut dyn VmHostHooks,
    _callee: vm_js::GcObject,
    _this: Value,
    _args: &[Value],
  ) -> std::result::Result<Value, VmError> {
    let call_id = vm.register_native_call(thenable_then)?;

    let then_name = scope.alloc_string("then")?;
    scope.push_root(Value::String(then_name))?;
    let then_func = scope.alloc_native_function(call_id, None, then_name, 2)?;
    scope.push_root(Value::Object(then_func))?;

    let obj = scope.alloc_object()?;
    scope.push_root(Value::Object(obj))?;

    scope.define_property(
      obj,
      vm_js::PropertyKey::String(then_name),
      vm_js::PropertyDescriptor {
        enumerable: false,
        configurable: true,
        kind: vm_js::PropertyKind::Data {
          value: Value::Object(then_func),
          writable: true,
        },
      },
    )?;

    Ok(Value::Object(obj))
  }

  #[test]
  fn hooks_as_any_mut_downcasts_to_runtime_in_evaluator() -> Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<EcmaVmRuntime<TestState>>::with_clock(clock);
    let mut host = EcmaVmRuntime::new(TestState::default(), EcmaVmRuntimeConfig::default())?;
    host.define_global_native_function("__log_downcast", 0, log_downcast_via_hooks)?;
    host.execute_classic_script("__log_downcast();", &classic_spec(), &mut event_loop)?;
    assert_eq!(host.state.log, vec!["downcast"]);
    Ok(())
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
  fn promise_any_accepts_array_literal_iterable() -> Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<EcmaVmRuntime<TestState>>::with_clock(clock);
    let mut host = EcmaVmRuntime::new(TestState::default(), EcmaVmRuntimeConfig::default())?;

    host.define_global_native_function("__log", 1, log_number_arg)?;

    host.execute_classic_script(
      "Promise.any([1, 2]).then(__log);",
      &classic_spec(),
      &mut event_loop,
    )?;
    assert_eq!(host.state.log, Vec::<&'static str>::new());

    event_loop.perform_microtask_checkpoint(&mut host)?;
    assert_eq!(host.state.log, vec!["1"]);
    Ok(())
  }

  #[test]
  fn microtask_enqueued_by_promise_then_runs_in_same_checkpoint() -> Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<EcmaVmRuntime<TestState>>::with_clock(clock);
    let mut host = EcmaVmRuntime::new(TestState::default(), EcmaVmRuntimeConfig::default())?;

    host.define_global_native_function("__log_sync", 0, log_sync)?;
    host.define_global_native_function("__enqueue_nested", 0, enqueue_nested_microtask)?;

    host.execute_classic_script(
      "Promise.resolve().then(__enqueue_nested); __log_sync();",
      &classic_spec(),
      &mut event_loop,
    )?;
    assert_eq!(host.state.log, vec!["sync"]);

    event_loop.perform_microtask_checkpoint(&mut host)?;
    assert_eq!(host.state.log, vec!["sync", "then", "nested"]);
    Ok(())
  }

  #[test]
  fn promise_thenable_jobs_are_drained_in_the_same_microtask_checkpoint() -> Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<EcmaVmRuntime<TestState>>::with_clock(clock);
    let mut host = EcmaVmRuntime::new(TestState::default(), EcmaVmRuntimeConfig::default())?;

    host.define_global_native_function("__log_sync", 0, log_sync)?;
    host.define_global_native_function("__log_micro", 0, log_micro)?;
    host.define_global_native_function("__make_thenable", 0, make_thenable)?;

    host.execute_classic_script(
      "Promise.resolve(__make_thenable()).then(__log_micro); __log_sync();",
      &classic_spec(),
      &mut event_loop,
    )?;
    assert_eq!(host.state.log, vec!["sync"]);

    event_loop.perform_microtask_checkpoint(&mut host)?;
    assert_eq!(host.state.log, vec!["sync", "thenable_then", "micro"]);
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
        Job::new(vm_js::JobKind::Promise, |_ctx, _hooks| {
          let _ = ExecCtxGuard::with_current::<TestState, _>(|host_ptr, _| unsafe {
            (*host_ptr).state.log.push("job1");
          });
          Ok(())
        })
        .unwrap(),
        Some(host.realm_id),
      );

      host.host_enqueue_promise_job(
        Job::new(vm_js::JobKind::Promise, |_ctx, _hooks| {
          let _ = ExecCtxGuard::with_current::<TestState, _>(|host_ptr, _| unsafe {
            (*host_ptr).state.log.push("job2");
          });
          Ok(())
        })
        .unwrap(),
        Some(host.realm_id),
      );
    }

    event_loop.perform_microtask_checkpoint(&mut host)?;
    assert_eq!(host.state.log, vec!["job1", "job2"]);
    Ok(())
  }

  #[test]
  fn missing_vm_js_job_in_microtask_is_noop_and_does_not_panic() -> Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<EcmaVmRuntime<TestState>>::with_clock(clock);
    let mut host = EcmaVmRuntime::new(TestState::default(), EcmaVmRuntimeConfig::default())?;

    // Simulate a microtask that was queued with a missing/discarded `vm-js` `Job`.
    let job_cell: Rc<RefCell<Option<Job>>> = Rc::new(RefCell::new(None));
    event_loop.queue_microtask(move |host, event_loop| {
      run_vm_js_job_microtask(job_cell, host, event_loop)
    })?;
    assert_eq!(event_loop.pending_microtask_count(), 1);

    event_loop.perform_microtask_checkpoint(&mut host)?;
    assert_eq!(event_loop.pending_microtask_count(), 0);
    assert_eq!(host.state.log, Vec::<&'static str>::new());
    Ok(())
  }

  #[test]
  fn accessor_getters_run_with_runtime_host_hooks() -> Result<()> {
    fn getter_enqueues_microtask(
      _vm: &mut Vm,
      _scope: &mut Scope<'_>,
      _host: &mut dyn VmHost,
      hooks: &mut dyn VmHostHooks,
      _callee: vm_js::GcObject,
      _this: Value,
      _args: &[Value],
    ) -> std::result::Result<Value, VmError> {
      let realm_id = ExecCtxGuard::with_current::<TestState, _>(|host_ptr, _| unsafe {
        (*host_ptr).state.log.push("getter");
        (*host_ptr).realm_id
      })
      .unwrap_or(RealmId::from_raw(0));

      let job = Job::new(vm_js::JobKind::Promise, |_ctx, _hooks| {
        let _ = ExecCtxGuard::with_current::<TestState, _>(|host_ptr, _| unsafe {
          (*host_ptr).state.log.push("micro");
        });
        Ok(())
      })?;
      hooks.host_enqueue_promise_job(job, Some(realm_id));

      Ok(Value::Undefined)
    }

    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<EcmaVmRuntime<TestState>>::with_clock(clock);
    let mut host = EcmaVmRuntime::new(TestState::default(), EcmaVmRuntimeConfig::default())?;

    // Define an accessor on the global object whose getter enqueues a Promise job via host hooks.
    {
      let call_id = host
        .vm
        .register_native_call(getter_enqueues_microtask)
        .map_err(|err| map_vm_error(&mut host.heap, err))?;

      let mut scope = host.heap.scope();

      let name_s = scope
        .alloc_string("get_p")
        .map_err(|err| map_vm_error(scope.heap_mut(), err))?;
      scope
        .push_root(Value::String(name_s))
        .map_err(|err| map_vm_error(scope.heap_mut(), err))?;
      let getter = scope
        .alloc_native_function(call_id, None, name_s, 0)
        .map_err(|err| map_vm_error(scope.heap_mut(), err))?;
      scope
        .push_root(Value::Object(getter))
        .map_err(|err| map_vm_error(scope.heap_mut(), err))?;

      let global = host.realm.global_object();
      scope
        .push_root(Value::Object(global))
        .map_err(|err| map_vm_error(scope.heap_mut(), err))?;

      let key_s = scope
        .alloc_string("p")
        .map_err(|err| map_vm_error(scope.heap_mut(), err))?;
      scope
        .push_root(Value::String(key_s))
        .map_err(|err| map_vm_error(scope.heap_mut(), err))?;
      let key = PropertyKey::from_string(key_s);

      scope
        .define_property(
          global,
          key,
          vm_js::PropertyDescriptor {
            enumerable: true,
            configurable: true,
            kind: PropertyKind::Accessor {
              get: Value::Object(getter),
              set: Value::Undefined,
            },
          },
        )
        .map_err(|err| map_vm_error(scope.heap_mut(), err))?;
    }

    host.execute_classic_script("p;", &classic_spec(), &mut event_loop)?;
    assert_eq!(host.state.log, vec!["getter"]);

    event_loop.perform_microtask_checkpoint(&mut host)?;
    assert_eq!(host.state.log, vec!["getter", "micro"]);
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
      vm: &mut Vm,
      scope: &mut Scope<'_>,
      _host: &mut dyn VmHost,
      _hooks: &mut dyn VmHostHooks,
      _callee: vm_js::GcObject,
      _this: Value,
      args: &[Value],
    ) -> std::result::Result<Value, VmError> {
      let id_value = args.get(0).copied().unwrap_or(Value::Number(0.0));
      let id = normalize_timer_id(vm, scope, id_value)?;
      let _ = ExecCtxGuard::with_current::<TestState, _>(|host_ptr, _| unsafe {
        (*host_ptr).state.interval_id = Some(id);
      });
      Ok(Value::Undefined)
    }

    fn interval_cb(
      vm: &mut Vm,
      scope: &mut Scope<'_>,
      _host: &mut dyn VmHost,
      hooks: &mut dyn VmHostHooks,
      _callee: vm_js::GcObject,
      _this: Value,
      _args: &[Value],
    ) -> std::result::Result<Value, VmError> {
      let Some((count, id)) = ExecCtxGuard::with_current::<TestState, _>(|host_ptr, _| unsafe {
        (*host_ptr).state.interval_count += 1;
        (
          (*host_ptr).state.interval_count,
          (*host_ptr).state.interval_id,
        )
      }) else {
        return Ok(Value::Undefined);
      };

      if count == 3 {
        let Some(id) = id else {
          return Ok(Value::Undefined);
        };

        // Call the JS global `clearInterval(id)`.
        let Some(global) = ExecCtxGuard::with_current::<TestState, _>(|host_ptr, _| unsafe {
          (*host_ptr).realm.global_object()
        }) else {
          return Ok(Value::Undefined);
        };
        let global_value = Value::Object(global);
        scope.push_root(global_value)?;
        let key_s = scope.alloc_string("clearInterval")?;
        scope.push_root(Value::String(key_s))?;
        let func =
          scope.ordinary_get(vm, global, vm_js::PropertyKey::String(key_s), global_value)?;
        vm.call_with_host(
          scope,
          hooks,
          func,
          global_value,
          &[Value::Number(id as f64)],
        )?;
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

  fn assert_type_error_object(
    scope: &mut Scope<'_>,
    intrinsics: vm_js::Intrinsics,
    value: Value,
    expected_message: &str,
  ) -> std::result::Result<(), VmError> {
    let Value::Object(obj) = value else {
      panic!("expected thrown object, got {value:?}");
    };
    scope.push_root(Value::Object(obj))?;

    assert_eq!(
      scope.object_get_prototype(obj)?,
      Some(intrinsics.type_error_prototype())
    );

    let name_key_s = scope.alloc_string("name")?;
    scope.push_root(Value::String(name_key_s))?;
    let name_key = PropertyKey::from_string(name_key_s);
    let name_desc = scope
      .ordinary_get_own_property(obj, name_key)?
      .expect("missing name property");
    assert!(!name_desc.enumerable);
    assert!(name_desc.configurable);
    let PropertyKind::Data {
      value: name_value, ..
    } = name_desc.kind
    else {
      panic!("name is not a data property");
    };
    let Value::String(name_s) = name_value else {
      panic!("name is not a string: {name_value:?}");
    };
    assert_eq!(
      scope.heap().get_string(name_s)?.to_utf8_lossy(),
      "TypeError"
    );

    let msg_key_s = scope.alloc_string("message")?;
    scope.push_root(Value::String(msg_key_s))?;
    let msg_key = PropertyKey::from_string(msg_key_s);
    let msg_desc = scope
      .ordinary_get_own_property(obj, msg_key)?
      .expect("missing message property");
    assert!(!msg_desc.enumerable);
    assert!(msg_desc.configurable);
    let PropertyKind::Data {
      value: msg_value, ..
    } = msg_desc.kind
    else {
      panic!("message is not a data property");
    };
    let Value::String(msg_s) = msg_value else {
      panic!("message is not a string: {msg_value:?}");
    };
    assert_eq!(
      scope.heap().get_string(msg_s)?.to_utf8_lossy(),
      expected_message
    );

    Ok(())
  }

  #[test]
  fn set_timeout_parses_string_delay_and_respects_virtual_clock() -> Result<()> {
    let clock = Arc::new(VirtualClock::new());
    let mut event_loop = EventLoop::<EcmaVmRuntime<TestState>>::with_clock(clock.clone());
    let mut host = EcmaVmRuntime::new(TestState::default(), EcmaVmRuntimeConfig::default())?;
    host.define_global_native_function("__log_timeout", 0, log_timeout)?;

    host.execute_classic_script(
      "setTimeout(__log_timeout, '10');",
      &classic_spec(),
      &mut event_loop,
    )?;

    assert_eq!(host.state.log, Vec::<&'static str>::new());
    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    assert_eq!(host.state.log, Vec::<&'static str>::new());

    clock.advance(Duration::from_millis(9));
    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    assert_eq!(host.state.log, Vec::<&'static str>::new());

    clock.advance(Duration::from_millis(1));
    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    assert_eq!(host.state.log, vec!["timeout"]);
    Ok(())
  }

  #[test]
  fn set_timeout_throws_type_error_object_for_non_callable_callback() -> Result<()> {
    let mut host = EcmaVmRuntime::new(TestState::default(), EcmaVmRuntimeConfig::default())?;
    let intr = *host.realm.intrinsics();
    let mut hooks = RuntimeHostHooks::new(&mut host);
    let mut dummy_host = ();

    let mut scope = host.heap.scope();
    let callee = scope
      .alloc_object()
      .map_err(|err| map_vm_error(scope.heap_mut(), err))?;

    let err = native_set_timeout::<TestState>(
      &mut host.vm,
      &mut scope,
      &mut dummy_host,
      &mut hooks,
      callee,
      Value::Undefined,
      &[Value::Number(0.0), Value::Number(0.0)],
    )
    .expect_err("expected setTimeout to throw");

    let Some(value) = err.thrown_value() else {
      panic!("expected thrown error, got {err:?}");
    };
    assert_type_error_object(
      &mut scope,
      intr,
      value,
      "setTimeout callback is not callable",
    )
    .map_err(|err| map_vm_error(scope.heap_mut(), err))?;
    Ok(())
  }

  #[test]
  fn clear_timeout_throws_type_error_object_for_symbol_handle() -> Result<()> {
    let mut host = EcmaVmRuntime::new(TestState::default(), EcmaVmRuntimeConfig::default())?;
    let intr = *host.realm.intrinsics();
    let mut hooks = RuntimeHostHooks::new(&mut host);
    let mut dummy_host = ();

    let mut scope = host.heap.scope();
    let callee = scope
      .alloc_object()
      .map_err(|err| map_vm_error(scope.heap_mut(), err))?;
    let sym = scope
      .alloc_symbol(Some("t"))
      .map_err(|err| map_vm_error(scope.heap_mut(), err))?;

    let err = native_clear_timeout::<TestState>(
      &mut host.vm,
      &mut scope,
      &mut dummy_host,
      &mut hooks,
      callee,
      Value::Undefined,
      &[Value::Symbol(sym)],
    )
    .expect_err("expected clearTimeout to throw");

    let Some(value) = err.thrown_value() else {
      panic!("expected thrown error, got {err:?}");
    };
    assert_type_error_object(
      &mut scope,
      intr,
      value,
      "Cannot convert a Symbol value to a number",
    )
    .map_err(|err| map_vm_error(scope.heap_mut(), err))?;
    Ok(())
  }

  #[test]
  fn map_vm_error_includes_stack_trace_for_throw_with_stack() {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut scope = heap.scope();
    let msg_s = scope.alloc_string("boom").expect("alloc thrown string");
    let _root = scope
      .push_root(Value::String(msg_s))
      .expect("root thrown string");
    let err = VmError::ThrowWithStack {
      value: Value::String(msg_s),
      stack: vec![StackFrame {
        function: Some(Arc::<str>::from("f")),
        source: Arc::<str>::from("<test>"),
        line: 1,
        col: 2,
      }],
    };

    let Error::Other(msg) = map_vm_error(scope.heap_mut(), err) else {
      panic!("expected Error::Other");
    };
    assert!(
      msg.contains("JS exception: boom"),
      "expected thrown string to appear in message, got {msg:?}"
    );
    assert!(
      msg.contains("at f (<test>:1:2)"),
      "expected stack trace frame to appear in message, got {msg:?}"
    );
  }
}
