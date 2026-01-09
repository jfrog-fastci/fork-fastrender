//! `ecma-rs` (`vm-js`) embedding for executing classic scripts with budgeted/interruptible execution.
//!
//! This module intentionally keeps the public API small: it is an adapter layer that the HTML
//! script scheduler/orchestrator can call into without depending directly on `vm-js` internals.
//! The backing engine is currently `vm-js`, but the surface area is "realm-shaped" so a different
//! backend could be swapped in later.

use crate::render_control::RenderDeadline;
use parse_js::ast::expr::lit::{LitNumExpr, LitStrExpr};
use parse_js::ast::expr::{BinaryExpr, CallExpr, Expr, IdExpr};
use parse_js::ast::node::{literal_string_code_units, Node};
use parse_js::ast::stmt::{BlockStmt, Stmt, ThrowStmt, WhileStmt};
use parse_js::{parse_with_options, Dialect, ParseOptions, SourceType};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use vm_js::{
  format_stack_trace, Budget, GcObject, Heap, HeapLimits, InterruptHandle, RootId, Scope, SourceText,
  StackFrame, TerminationReason as VmTerminationReason, Value, Vm, VmError, VmOptions,
};

const DEFAULT_HEAP_MAX_BYTES: usize = 64 * 1024 * 1024;
const MIN_HEAP_MAX_BYTES: usize = 4 * 1024 * 1024;

fn default_heap_limits() -> HeapLimits {
  let mut max = DEFAULT_HEAP_MAX_BYTES;

  // If the process is constrained by `RLIMIT_AS` (typically applied by FastRender CLI flags or
  // an outer `prlimit`/cgroup), keep JS heap usage to a small fraction of that ceiling so other
  // renderer subsystems still have headroom.
  #[cfg(target_os = "linux")]
  {
    if let Ok((cur, _max)) = crate::process_limits::get_address_space_limit_bytes() {
      if cur > 0 && cur < u64::MAX {
        // Stay conservative: scripts are untrusted, and the renderer has many other heaps (DOM,
        // CSS, layout, display list, etc.).
        let suggested = cur / 8;
        if let Ok(suggested) = usize::try_from(suggested) {
          max = max.min(suggested.max(MIN_HEAP_MAX_BYTES));
        }
      }
    }
  }

  // Keep the pre-existing "GC at half the max heap" behavior, but scale it down when the max is
  // derived from process limits.
  let gc_threshold = (max / 2).min(max);
  HeapLimits::new(max, gc_threshold)
}

/// Per-realm configuration for the embedded JS engine.
#[derive(Debug, Clone)]
pub struct ScriptRealmOptions {
  pub heap_limits: HeapLimits,
  /// Default fuel budget to apply when no explicit per-call override is provided.
  ///
  /// Set to `None` only for trusted workloads; for untrusted scripts keep this `Some`.
  pub default_fuel: Option<u64>,
  /// Default wall-time budget to apply when no FastRender deadline is active and no per-call
  /// override is provided.
  pub default_deadline: Option<Duration>,
  /// How frequently `vm-js` should poll wall-clock time (in VM "ticks").
  pub check_time_every: u32,
  pub max_stack_depth: usize,
}

impl Default for ScriptRealmOptions {
  fn default() -> Self {
    // Conservative defaults suitable for running untrusted inline scripts without hanging the host.
    Self {
      heap_limits: default_heap_limits(),
      default_fuel: Some(100_000),
      default_deadline: Some(Duration::from_millis(50)),
      check_time_every: 64,
      max_stack_depth: 1024,
    }
  }
}

/// Optional per-call budget override.
#[derive(Debug, Clone, Default)]
pub struct ScriptBudgetOverride {
  pub fuel: Option<u64>,
  pub wall_time: Option<Duration>,
}

/// A small, engine-agnostic value representation for callers.
#[derive(Debug, Clone, PartialEq)]
pub enum ScriptValue {
  Undefined,
  Null,
  Bool(bool),
  Number(f64),
  /// UTF-8 string value.
  ///
  /// Note: BigInt values are currently surfaced as decimal strings because this shim does not yet
  /// expose a dedicated BigInt variant.
  String(String),
  /// Non-primitive values are currently surfaced as opaque markers.
  Object,
  Symbol,
}

/// Structured reason for terminating script execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ScriptTerminationReason {
  #[error("out of fuel")]
  OutOfFuel,
  #[error("deadline exceeded")]
  DeadlineExceeded,
  #[error("interrupted")]
  Interrupted,
  #[error("out of memory")]
  OutOfMemory,
  #[error("stack overflow")]
  StackOverflow,
}

impl From<VmTerminationReason> for ScriptTerminationReason {
  fn from(value: VmTerminationReason) -> Self {
    match value {
      VmTerminationReason::OutOfFuel => Self::OutOfFuel,
      VmTerminationReason::DeadlineExceeded => Self::DeadlineExceeded,
      VmTerminationReason::Interrupted => Self::Interrupted,
      VmTerminationReason::OutOfMemory => Self::OutOfMemory,
      VmTerminationReason::StackOverflow => Self::StackOverflow,
    }
  }
}

/// Errors surfaced by the script realm.
#[derive(Debug, thiserror::Error)]
pub enum ScriptError {
  #[error("JavaScript syntax error: {message}")]
  Syntax { message: String },

  #[error("JavaScript exception: {message}\n{stack_trace}")]
  Exception {
    message: String,
    stack_trace: String,
  },

  #[error("JavaScript execution terminated: {reason}\n{stack_trace}")]
  Termination {
    reason: ScriptTerminationReason,
    stack_trace: String,
  },

  #[error("JavaScript runtime error: {message}\n{stack_trace}")]
  Runtime {
    message: String,
    stack_trace: String,
  },
}

pub type HostFunction =
  Box<dyn FnMut(&[ScriptValue]) -> Result<ScriptValue, ScriptError> + 'static>;

/// Minimal engine-agnostic realm interface for classic script evaluation.
pub trait ScriptRealm {
  fn eval_script(&mut self, source_name: &str, source: &str) -> Result<ScriptValue, ScriptError>;

  fn eval_script_with_budget(
    &mut self,
    source_name: &str,
    source: &str,
    budget: ScriptBudgetOverride,
  ) -> Result<ScriptValue, ScriptError>;

  fn register_host_function(&mut self, name: &str, func: HostFunction) -> Result<(), ScriptError>;
}

#[derive(Debug, Default)]
struct Env {
  globals: HashMap<String, RootId>,
}

impl Env {
  fn is_read_only_global(name: &str) -> bool {
    matches!(name, "undefined" | "NaN" | "Infinity")
  }

  fn get(&self, heap: &Heap, name: &str) -> Option<Value> {
    let root = self.globals.get(name).copied()?;
    heap.get_root(root)
  }

  fn set(&mut self, heap: &mut Heap, name: &str, value: Value) -> Result<(), VmError> {
    // The global properties `undefined`, `NaN`, and `Infinity` are non-writable in the ECMAScript
    // global object. We model that by allowing initialization but ignoring subsequent writes.
    if Self::is_read_only_global(name) && self.globals.contains_key(name) {
      return Ok(());
    }

    match self.globals.get(name).copied() {
      Some(root) => {
        heap.set_root(root, value);
        Ok(())
      }
      None => {
        // Avoid aborting on OOM: pre-allocate the key string + HashMap capacity before rooting.
        let mut owned = String::new();
        owned
          .try_reserve(name.len())
          .map_err(|_| VmError::OutOfMemory)?;
        owned.push_str(name);
        self
          .globals
          .try_reserve(1)
          .map_err(|_| VmError::OutOfMemory)?;
        let root = heap.add_root(value)?;
        self.globals.insert(owned, root);
        Ok(())
      }
    }
  }
}

struct HostFunctionEntry {
  name: Arc<str>,
  func: HostFunction,
}

/// A `vm-js`-backed script realm.
pub struct VmJsScriptRealm {
  options: ScriptRealmOptions,
  vm: Vm,
  heap: Heap,
  env: Env,
  host_functions: HashMap<GcObject, HostFunctionEntry>,
  interrupt_flag: Arc<AtomicBool>,
  interrupt_handle: InterruptHandle,
}

impl VmJsScriptRealm {
  pub fn new(options: ScriptRealmOptions) -> Result<Self, ScriptError> {
    // Use a shared interrupt flag so this realm can be reused across multiple evaluations, while
    // still allowing the active render deadline to interrupt long-running scripts.
    let interrupt_flag = Arc::new(AtomicBool::new(false));
    let vm = Vm::new(VmOptions {
      max_stack_depth: options.max_stack_depth,
      default_fuel: options.default_fuel,
      default_deadline: options.default_deadline,
      check_time_every: options.check_time_every,
      interrupt_flag: Some(Arc::clone(&interrupt_flag)),
    });
    let interrupt_handle = vm.interrupt_handle();
    let mut heap = Heap::new(options.heap_limits);
    let mut env = Env::default();
    // Populate a few well-known global constants so scripts can reference them by identifier.
    // These are non-writable in the real ECMAScript global object; see `Env::set`.
    env
      .set(&mut heap, "undefined", Value::Undefined)
      .map_err(vm_error_to_runtime)?;
    env
      .set(&mut heap, "NaN", Value::Number(f64::NAN))
      .map_err(vm_error_to_runtime)?;
    env
      .set(&mut heap, "Infinity", Value::Number(f64::INFINITY))
      .map_err(vm_error_to_runtime)?;
    Ok(Self {
      options,
      vm,
      heap,
      env,
      host_functions: HashMap::new(),
      interrupt_flag,
      interrupt_handle,
    })
  }

  fn derive_budget(
    &self,
    render_deadline: Option<&RenderDeadline>,
    override_budget: &ScriptBudgetOverride,
  ) -> Budget {
    let fuel = override_budget.fuel.or(self.options.default_fuel);
    let mut wall_time = override_budget.wall_time.or(self.options.default_deadline);

    // Ensure JS cannot exceed the active/root FastRender timeout.
    if let Some(deadline) = render_deadline {
      if deadline.timeout_limit().is_some() {
        // When the deadline is already exceeded, `remaining_timeout()` returns `None`; treat that as
        // an immediate deadline rather than "unlimited".
        let remaining = deadline.remaining_timeout().unwrap_or(Duration::ZERO);
        wall_time = Some(match wall_time {
          Some(existing) => existing.min(remaining),
          None => remaining,
        });
      }
    }

    let deadline = wall_time.and_then(|duration| Instant::now().checked_add(duration));
    Budget {
      fuel,
      deadline,
      check_time_every: self.options.check_time_every,
    }
  }
}

impl super::VmJsEngineHost for VmJsScriptRealm {
  fn vm_js_heap(&self) -> &Heap {
    &self.heap
  }

  fn vm_js_heap_mut(&mut self) -> &mut vm_js::Heap {
    &mut self.heap
  }

  fn vm_js_vm_and_heap_mut(&mut self) -> (&mut Vm, &mut Heap) {
    (&mut self.vm, &mut self.heap)
  }
}

impl ScriptRealm for VmJsScriptRealm {
  fn eval_script(&mut self, source_name: &str, source: &str) -> Result<ScriptValue, ScriptError> {
    self.eval_script_with_budget(source_name, source, ScriptBudgetOverride::default())
  }

  fn eval_script_with_budget(
    &mut self,
    source_name: &str,
    source: &str,
    budget: ScriptBudgetOverride,
  ) -> Result<ScriptValue, ScriptError> {
    // If a previous evaluation was interrupted, the VM interrupt flag stays set until the host
    // resets it. Clear it here so subsequent evaluations can proceed.
    self.interrupt_flag.store(false, Ordering::Relaxed);

    let render_deadline =
      crate::render_control::active_deadline().or_else(crate::render_control::root_deadline);
    let budget = self.derive_budget(render_deadline.as_ref(), &budget);
    self.vm.set_budget(budget);

    let opts = ParseOptions {
      dialect: Dialect::Ecma,
      source_type: SourceType::Script,
    };
    let top = parse_with_options(source, opts).map_err(|err| ScriptError::Syntax {
      message: err.to_string(),
    })?;

    let source_text = SourceText::new(source_name, source);

    // Push a frame so errors/terminations surface at least one stack frame.
    let (line, col) = source_text.line_col(0);
    self
      .vm
      .push_frame(StackFrame {
        function: None,
        source: source_text.name.clone(),
        line,
        col,
      })
      .map_err(|err| match err {
        VmError::Termination(t) => ScriptError::Termination {
          reason: t.reason.into(),
          stack_trace: format_stack_trace(&t.stack),
        },
        other => ScriptError::Runtime {
          message: other.to_string(),
          stack_trace: String::new(),
        },
      })?;

    // Split borrows like `vm-js` does: we want to evaluate using independent mutable references to
    // `vm`, `heap`, and environment state.
    let vm = &mut self.vm;
    let heap = &mut self.heap;
    let env = &mut self.env;
    let host_functions = &mut self.host_functions;
    let interrupt_handle = self.interrupt_handle.clone();

    let mut scope = heap.scope();
    let mut evaluator = Evaluator {
      vm,
      env,
      host_functions,
      interrupt_handle,
      render_deadline,
      source: &source_text,
    };

    let result = evaluator.exec_stmt_list(&mut scope, &top.stx.body);
    evaluator.vm.pop_frame();

    let value = result?;
    Ok(evaluator.value_to_script_value(scope.heap(), value)?)
  }

  fn register_host_function(&mut self, name: &str, func: HostFunction) -> Result<(), ScriptError> {
    let obj = {
      let mut scope = self.heap.scope();
      scope.alloc_object().map_err(vm_error_to_runtime)?
    };
    let value = Value::Object(obj);
    self
      .env
      .set(&mut self.heap, name, value)
      .map_err(vm_error_to_runtime)?;
    self.host_functions.insert(
      obj,
      HostFunctionEntry {
        name: Arc::from(name),
        func,
      },
    );
    Ok(())
  }
}

fn vm_error_to_runtime(err: VmError) -> ScriptError {
  match err {
    VmError::Termination(term) => ScriptError::Termination {
      reason: term.reason.into(),
      stack_trace: format_stack_trace(&term.stack),
    },
    VmError::OutOfMemory => ScriptError::Termination {
      reason: ScriptTerminationReason::OutOfMemory,
      stack_trace: String::new(),
    },
    other => ScriptError::Runtime {
      message: other.to_string(),
      stack_trace: String::new(),
    },
  }
}

struct Evaluator<'a> {
  vm: &'a mut Vm,
  env: &'a mut Env,
  host_functions: &'a mut HashMap<GcObject, HostFunctionEntry>,
  interrupt_handle: InterruptHandle,
  render_deadline: Option<RenderDeadline>,
  source: &'a SourceText,
}

impl Evaluator<'_> {
  fn tick(&mut self) -> Result<(), ScriptError> {
    if let Some(deadline) = &self.render_deadline {
      if let Some(cancel) = deadline.cancel_callback() {
        if cancel() {
          self.interrupt_handle.interrupt();
        }
      }
    }
    match self.vm.tick() {
      Ok(()) => Ok(()),
      Err(VmError::Termination(term)) => Err(ScriptError::Termination {
        reason: term.reason.into(),
        stack_trace: format_stack_trace(&term.stack),
      }),
      Err(other) => Err(ScriptError::Runtime {
        message: other.to_string(),
        stack_trace: format_stack_trace(&self.vm.capture_stack()),
      }),
    }
  }

  fn frame_at_loc(&self, loc: parse_js::loc::Loc, function: Option<Arc<str>>) -> StackFrame {
    let start_u32 = (loc.0).min(u32::MAX as usize) as u32;
    let (line, col) = self.source.line_col(start_u32);
    StackFrame {
      function,
      source: self.source.name.clone(),
      line,
      col,
    }
  }

  fn stack_trace_at_loc(&self, loc: parse_js::loc::Loc) -> String {
    let mut frames = self.vm.capture_stack();
    frames.push(self.frame_at_loc(loc, None));
    format_stack_trace(&frames)
  }

  fn vm_error_at_loc(&self, loc: parse_js::loc::Loc, err: VmError) -> ScriptError {
    match err {
      VmError::Termination(term) => ScriptError::Termination {
        reason: term.reason.into(),
        stack_trace: format_stack_trace(&term.stack),
      },
      other => ScriptError::Runtime {
        message: other.to_string(),
        stack_trace: self.stack_trace_at_loc(loc),
      },
    }
  }

  fn value_to_string(&self, heap: &mut Heap, value: Value) -> Result<String, VmError> {
    // Use `vm-js`'s `ToString` for primitives so we match ECMAScript formatting rules (not Rust
    // float formatting). In particular, this keeps `Infinity`/`-Infinity`/`NaN` spelling correct.
    match value {
      // Keep the embedding's current placeholder formatting for unsupported types.
      Value::Symbol(_) => Ok("Symbol".to_string()),
      Value::Object(_) => Ok("[object Object]".to_string()),
      other => {
        let s = heap.to_string(other)?;
        Ok(heap.get_string(s)?.to_utf8_lossy())
      }
    }
  }

  fn value_to_script_value(&self, heap: &Heap, value: Value) -> Result<ScriptValue, ScriptError> {
    match value {
      Value::Undefined => Ok(ScriptValue::Undefined),
      Value::Null => Ok(ScriptValue::Null),
      Value::Bool(b) => Ok(ScriptValue::Bool(b)),
      Value::Number(n) => Ok(ScriptValue::Number(n)),
      // The embedding's stable value type does not model BigInt yet; surface it as a string so
      // callers can still inspect deterministic output.
      Value::BigInt(b) => Ok(ScriptValue::String(b.to_decimal_string())),
      Value::String(s) => Ok(ScriptValue::String(
        heap
          .get_string(s)
          .map_err(vm_error_to_runtime)?
          .to_utf8_lossy(),
      )),
      Value::Symbol(_) => Ok(ScriptValue::Symbol),
      Value::Object(_) => Ok(ScriptValue::Object),
    }
  }

  fn script_value_to_value(
    &self,
    scope: &mut Scope<'_>,
    value: ScriptValue,
  ) -> Result<Value, ScriptError> {
    Ok(match value {
      ScriptValue::Undefined => Value::Undefined,
      ScriptValue::Null => Value::Null,
      ScriptValue::Bool(b) => Value::Bool(b),
      ScriptValue::Number(n) => Value::Number(n),
      ScriptValue::String(s) => {
        let handle = scope.alloc_string(&s).map_err(vm_error_to_runtime)?;
        Value::String(handle)
      }
      ScriptValue::Object | ScriptValue::Symbol => {
        return Err(ScriptError::Runtime {
          message: "cannot marshal non-primitive ScriptValue back into vm-js Value".to_string(),
          stack_trace: format_stack_trace(&self.vm.capture_stack()),
        });
      }
    })
  }

  fn exec_stmt_list(
    &mut self,
    scope: &mut Scope<'_>,
    stmts: &[Node<Stmt>],
  ) -> Result<Value, ScriptError> {
    let last_root = scope
      .heap_mut()
      .add_root(Value::Undefined)
      .map_err(vm_error_to_runtime)?;
    // Ensure we clear the persistent root even if statement execution fails partway through.
    let result = (|| -> Result<Value, ScriptError> {
      let mut last_value = Value::Undefined;
      for stmt in stmts {
        if let Some(v) = self.exec_stmt(scope, stmt)? {
          last_value = v;
          scope.heap_mut().set_root(last_root, v);
        }
      }
      Ok(last_value)
    })();
    scope.heap_mut().remove_root(last_root);
    result
  }

  fn exec_stmt(
    &mut self,
    scope: &mut Scope<'_>,
    stmt: &Node<Stmt>,
  ) -> Result<Option<Value>, ScriptError> {
    self.tick()?;

    match &*stmt.stx {
      Stmt::Empty(_) => Ok(None),
      Stmt::Expr(expr_stmt) => {
        let value = self.eval_expr(scope, &expr_stmt.stx.expr)?;
        Ok(Some(value))
      }
      Stmt::Block(block) => self.exec_block(scope, &block.stx),
      Stmt::While(stmt_while) => {
        self.exec_while(scope, &stmt_while.stx)?;
        Ok(None)
      }
      Stmt::Throw(stmt_throw) => Err(self.exec_throw(scope, stmt, &stmt_throw.stx)?),
      _ => Err(ScriptError::Runtime {
        message: "unimplemented statement type".to_string(),
        stack_trace: self.stack_trace_at_loc(stmt.loc),
      }),
    }
  }

  fn exec_block(
    &mut self,
    scope: &mut Scope<'_>,
    block: &BlockStmt,
  ) -> Result<Option<Value>, ScriptError> {
    let mut last: Option<Value> = None;
    for stmt in &block.body {
      last = self.exec_stmt(scope, stmt)?;
    }
    Ok(last)
  }

  fn exec_while(&mut self, scope: &mut Scope<'_>, stmt: &WhileStmt) -> Result<(), ScriptError> {
    loop {
      self.tick()?;
      let test = self.eval_expr(scope, &stmt.condition)?;
      if !to_boolean(scope.heap(), test).map_err(vm_error_to_runtime)? {
        break;
      }
      let _ = self.exec_stmt(scope, &stmt.body)?;
    }
    Ok(())
  }

  fn exec_throw(
    &mut self,
    scope: &mut Scope<'_>,
    node: &Node<Stmt>,
    stmt: &ThrowStmt,
  ) -> Result<ScriptError, ScriptError> {
    let value = self.eval_expr(scope, &stmt.value)?;
    let message = self
      .value_to_string(scope.heap_mut(), value)
      .map_err(vm_error_to_runtime)?;
    let stack_trace = self.stack_trace_at_loc(node.loc);
    Ok(ScriptError::Exception {
      message,
      stack_trace,
    })
  }

  fn eval_expr(&mut self, scope: &mut Scope<'_>, expr: &Node<Expr>) -> Result<Value, ScriptError> {
    match &*expr.stx {
      Expr::LitNum(node) => self.eval_lit_num(&node.stx),
      Expr::LitBool(node) => Ok(Value::Bool(node.stx.value)),
      Expr::LitNull(_) => Ok(Value::Null),
      Expr::LitStr(node) => self.eval_lit_str(scope, node),
      Expr::Id(node) => self
        .eval_id(scope, &node.stx)
        .map_err(|msg| ScriptError::Runtime {
          message: msg,
          stack_trace: self.stack_trace_at_loc(expr.loc),
        }),
      Expr::Binary(node) => self.eval_binary(scope, expr, &node.stx),
      Expr::Call(node) => self.eval_call(scope, expr, &node.stx),
      _ => Err(ScriptError::Runtime {
        message: "unimplemented expression type".to_string(),
        stack_trace: self.stack_trace_at_loc(expr.loc),
      }),
    }
  }

  fn eval_lit_num(&self, expr: &LitNumExpr) -> Result<Value, ScriptError> {
    Ok(Value::Number(expr.value.0))
  }

  fn eval_lit_str(
    &self,
    scope: &mut Scope<'_>,
    node: &Node<LitStrExpr>,
  ) -> Result<Value, ScriptError> {
    // Prefer parser-recorded UTF-16 code units so lone surrogates survive.
    let s = if let Some(units) = literal_string_code_units(&node.assoc) {
      scope.alloc_string_from_code_units(units)
    } else {
      scope.alloc_string(&node.stx.value)
    }
    .map_err(vm_error_to_runtime)?;
    Ok(Value::String(s))
  }

  fn eval_id(&self, scope: &mut Scope<'_>, expr: &IdExpr) -> Result<Value, String> {
    self
      .env
      .get(scope.heap(), &expr.name)
      .ok_or_else(|| format!("unbound identifier: {}", expr.name))
  }

  fn eval_binary(
    &mut self,
    scope: &mut Scope<'_>,
    node: &Node<Expr>,
    expr: &BinaryExpr,
  ) -> Result<Value, ScriptError> {
    match expr.operator {
      parse_js::operator::OperatorName::Addition => {
        let left = self.eval_expr(scope, &expr.left)?;
        let mut rhs_scope = scope.reborrow();
        rhs_scope
          .push_root(left)
          .map_err(|err| self.vm_error_at_loc(node.loc, err))?;
        let right = self.eval_expr(&mut rhs_scope, &expr.right)?;
        rhs_scope
          .push_root(right)
          .map_err(|err| self.vm_error_at_loc(node.loc, err))?;
        add_operator(&mut rhs_scope, left, right).map_err(|err| self.vm_error_at_loc(node.loc, err))
      }
      parse_js::operator::OperatorName::StrictEquality => {
        let left = self.eval_expr(scope, &expr.left)?;
        let mut rhs_scope = scope.reborrow();
        rhs_scope
          .push_root(left)
          .map_err(|err| ScriptError::Runtime {
            message: err.to_string(),
            stack_trace: self.stack_trace_at_loc(node.loc),
          })?;
        let right = self.eval_expr(&mut rhs_scope, &expr.right)?;
        rhs_scope
          .push_root(right)
          .map_err(|err| ScriptError::Runtime {
            message: err.to_string(),
            stack_trace: self.stack_trace_at_loc(node.loc),
          })?;
        let eq = strict_equality(&mut rhs_scope, left, right).map_err(|err| ScriptError::Runtime {
          message: err.to_string(),
          stack_trace: self.stack_trace_at_loc(node.loc),
        })?;
        Ok(Value::Bool(eq))
      }
      parse_js::operator::OperatorName::StrictInequality => {
        let left = self.eval_expr(scope, &expr.left)?;
        let mut rhs_scope = scope.reborrow();
        rhs_scope
          .push_root(left)
          .map_err(|err| ScriptError::Runtime {
            message: err.to_string(),
            stack_trace: self.stack_trace_at_loc(node.loc),
          })?;
        let right = self.eval_expr(&mut rhs_scope, &expr.right)?;
        rhs_scope
          .push_root(right)
          .map_err(|err| ScriptError::Runtime {
            message: err.to_string(),
            stack_trace: self.stack_trace_at_loc(node.loc),
          })?;
        let eq = strict_equality(&mut rhs_scope, left, right).map_err(|err| ScriptError::Runtime {
          message: err.to_string(),
          stack_trace: self.stack_trace_at_loc(node.loc),
        })?;
        Ok(Value::Bool(!eq))
      }
      parse_js::operator::OperatorName::Equality => {
        let left = self.eval_expr(scope, &expr.left)?;
        let mut rhs_scope = scope.reborrow();
        rhs_scope
          .push_root(left)
          .map_err(|err| ScriptError::Runtime {
            message: err.to_string(),
            stack_trace: self.stack_trace_at_loc(node.loc),
          })?;
        let right = self.eval_expr(&mut rhs_scope, &expr.right)?;
        rhs_scope
          .push_root(right)
          .map_err(|err| ScriptError::Runtime {
            message: err.to_string(),
            stack_trace: self.stack_trace_at_loc(node.loc),
          })?;
        let eq = abstract_equality(&mut rhs_scope, left, right).map_err(|err| ScriptError::Runtime {
          message: err.to_string(),
          stack_trace: self.stack_trace_at_loc(node.loc),
        })?;
        Ok(Value::Bool(eq))
      }
      parse_js::operator::OperatorName::Inequality => {
        let left = self.eval_expr(scope, &expr.left)?;
        let mut rhs_scope = scope.reborrow();
        rhs_scope
          .push_root(left)
          .map_err(|err| ScriptError::Runtime {
            message: err.to_string(),
            stack_trace: self.stack_trace_at_loc(node.loc),
          })?;
        let right = self.eval_expr(&mut rhs_scope, &expr.right)?;
        rhs_scope
          .push_root(right)
          .map_err(|err| ScriptError::Runtime {
            message: err.to_string(),
            stack_trace: self.stack_trace_at_loc(node.loc),
          })?;
        let eq = abstract_equality(&mut rhs_scope, left, right).map_err(|err| ScriptError::Runtime {
          message: err.to_string(),
          stack_trace: self.stack_trace_at_loc(node.loc),
        })?;
        Ok(Value::Bool(!eq))
      }
      parse_js::operator::OperatorName::Assignment => {
        let name = match &*expr.left.stx {
          // `parse-js` models assignment targets as patterns, so `x = 1` uses `IdPat` rather than
          // the expression form `Id`.
          Expr::Id(id) => id.stx.name.as_str(),
          Expr::IdPat(id) => id.stx.name.as_str(),
          _ => {
            return Err(ScriptError::Runtime {
              message: "unimplemented assignment target".to_string(),
              stack_trace: self.stack_trace_at_loc(node.loc),
            });
          }
        };
        let value = self.eval_expr(scope, &expr.right)?;
        self
          .env
          .set(scope.heap_mut(), name, value)
          .map_err(|err| self.vm_error_at_loc(node.loc, err))?;
        Ok(value)
      }
      _ => Err(ScriptError::Runtime {
        message: "unimplemented binary operator".to_string(),
        stack_trace: self.stack_trace_at_loc(node.loc),
      }),
    }
  }

  fn eval_call(
    &mut self,
    scope: &mut Scope<'_>,
    node: &Node<Expr>,
    expr: &CallExpr,
  ) -> Result<Value, ScriptError> {
    let callee = self.eval_expr(scope, &expr.callee)?;
    let mut call_scope = scope.reborrow();
    call_scope
      .push_root(callee)
      .map_err(|err| self.vm_error_at_loc(node.loc, err))?;

    let Value::Object(obj) = callee else {
      return Err(ScriptError::Runtime {
        message: "value is not callable".to_string(),
        stack_trace: self.stack_trace_at_loc(node.loc),
      });
    };
    let Some(function_name) = self
      .host_functions
      .get(&obj)
      .map(|entry| entry.name.clone())
    else {
      return Err(ScriptError::Runtime {
        message: "value is not callable".to_string(),
        stack_trace: self.stack_trace_at_loc(node.loc),
      });
    };

    let mut args = Vec::with_capacity(expr.arguments.len());
    for arg in &expr.arguments {
      if arg.stx.spread {
        return Err(ScriptError::Runtime {
          message: "spread arguments are not supported".to_string(),
          stack_trace: self.stack_trace_at_loc(arg.loc),
        });
      }
      let value = self.eval_expr(&mut call_scope, &arg.stx.value)?;
      call_scope
        .push_root(value)
        .map_err(|err| self.vm_error_at_loc(arg.loc, err))?;
      // Convert into the embedding's stable value representation for the host callback.
      args.push(match value {
        Value::Undefined => ScriptValue::Undefined,
        Value::Null => ScriptValue::Null,
        Value::Bool(b) => ScriptValue::Bool(b),
        Value::Number(n) => ScriptValue::Number(n),
        Value::BigInt(b) => ScriptValue::String(b.to_decimal_string()),
        Value::String(s) => ScriptValue::String(
          call_scope
            .heap()
            .get_string(s)
            .map_err(vm_error_to_runtime)?
            .to_utf8_lossy(),
        ),
        Value::Symbol(_) => ScriptValue::Symbol,
        Value::Object(_) => ScriptValue::Object,
      });
    }

    let frame = self.frame_at_loc(node.loc, Some(function_name));
    self.vm.push_frame(frame).map_err(|err| match err {
      VmError::Termination(term) => ScriptError::Termination {
        reason: term.reason.into(),
        stack_trace: format_stack_trace(&term.stack),
      },
      other => ScriptError::Runtime {
        message: other.to_string(),
        stack_trace: format_stack_trace(&self.vm.capture_stack()),
      },
    })?;

    let result = {
      // Re-borrow after argument evaluation so we don't hold a mutable reference across
      // `eval_expr` calls (which also borrow `&mut self`).
      let entry = self
        .host_functions
        .get_mut(&obj)
        .expect("checked host function exists above");
      (entry.func)(&args)
    };
    self.vm.pop_frame();

    let result = result?;
    self.script_value_to_value(&mut call_scope, result)
  }
}

fn to_boolean(heap: &Heap, value: Value) -> Result<bool, VmError> {
  Ok(match value {
    Value::Undefined | Value::Null => false,
    Value::Bool(b) => b,
    Value::Number(n) => n != 0.0 && !n.is_nan(),
    Value::BigInt(n) => !n.is_zero(),
    Value::String(s) => !heap.get_string(s)?.as_code_units().is_empty(),
    Value::Symbol(_) | Value::Object(_) => true,
  })
}

fn to_primitive(scope: &mut Scope<'_>, value: Value) -> Result<Value, VmError> {
  match value {
    Value::Object(obj) => {
      // Placeholder `ToPrimitive` implementation matching `vm-js`'s current `ops::to_primitive`:
      // return the same value `Object.prototype.toString` would produce for ordinary objects.
      //
      // This keeps arithmetic/object conversions behaving in a JS-like way without requiring the
      // full @@toPrimitive / valueOf / toString machinery yet.
      let mut scope = scope.reborrow();
      scope.push_root(Value::Object(obj))?;
      let s = scope.alloc_string("[object Object]")?;
      Ok(Value::String(s))
    }
    other => Ok(other),
  }
}

fn to_number(scope: &mut Scope<'_>, value: Value) -> Result<f64, VmError> {
  match value {
    Value::Symbol(_) => Err(VmError::TypeError("Cannot convert a Symbol value to a number")),
    Value::Object(_) => {
      // Per spec: ToPrimitive, then ToNumber.
      let prim = to_primitive(scope, value)?;
      to_number(scope, prim)
    }
    other => scope.heap_mut().to_number(other),
  }
}

fn add_operator(scope: &mut Scope<'_>, a: Value, b: Value) -> Result<Value, VmError> {
  // Root inputs and any intermediate heap values for the duration of the operation: `+` may
  // allocate (string concatenation) and thus trigger GC.
  scope.push_root(a)?;
  scope.push_root(b)?;

  let a = to_primitive(scope, a)?;
  scope.push_root(a)?;
  let b = to_primitive(scope, b)?;
  scope.push_root(b)?;

  let should_concat = matches!(a, Value::String(_)) || matches!(b, Value::String(_));
  if should_concat {
    let a_str = scope.heap_mut().to_string(a)?;
    scope.push_root(Value::String(a_str))?;
    let b_str = scope.heap_mut().to_string(b)?;
    scope.push_root(Value::String(b_str))?;

    let a_units = scope.heap().get_string(a_str)?.as_code_units();
    let b_units = scope.heap().get_string(b_str)?.as_code_units();
    let mut combined: Vec<u16> = Vec::new();
    combined
      .try_reserve_exact(a_units.len() + b_units.len())
      .map_err(|_| VmError::OutOfMemory)?;
    combined.extend_from_slice(a_units);
    combined.extend_from_slice(b_units);

    let s = scope.alloc_string_from_code_units(&combined)?;
    Ok(Value::String(s))
  } else {
    let an = to_number(scope, a)?;
    let bn = to_number(scope, b)?;
    Ok(Value::Number(an + bn))
  }
}

fn strict_equality(scope: &mut Scope<'_>, a: Value, b: Value) -> Result<bool, VmError> {
  use Value::*;
  match (a, b) {
    (Undefined, Undefined) => Ok(true),
    (Null, Null) => Ok(true),
    (Bool(x), Bool(y)) => Ok(x == y),
    (Number(x), Number(y)) => Ok(x == y),
    (BigInt(x), BigInt(y)) => Ok(x == y),
    (String(x), String(y)) => Ok(scope.heap().get_string(x)? == scope.heap().get_string(y)?),
    (Symbol(x), Symbol(y)) => Ok(x == y),
    (Object(x), Object(y)) => Ok(x == y),
    _ => Ok(false),
  }
}

fn abstract_equality(scope: &mut Scope<'_>, a: Value, b: Value) -> Result<bool, VmError> {
  use Value::*;

  // `==` can allocate when converting objects to primitives (via `ToPrimitive`), so root operands
  // for the duration of the comparison.
  let mut scope = scope.reborrow();
  let mut a = scope.push_root(a)?;
  let mut b = scope.push_root(b)?;

  loop {
    match (a, b) {
      // Same-type comparisons use Strict Equality Comparison.
      (Undefined, Undefined) => return Ok(true),
      (Null, Null) => return Ok(true),
      (Bool(x), Bool(y)) => return Ok(x == y),
      (Number(x), Number(y)) => return Ok(x == y),
      (BigInt(x), BigInt(y)) => return Ok(x == y),
      (String(x), String(y)) => return Ok(scope.heap().get_string(x)? == scope.heap().get_string(y)?),
      (Symbol(x), Symbol(y)) => return Ok(x == y),
      (Object(x), Object(y)) => return Ok(x == y),

      // `null == undefined`
      (Undefined, Null) | (Null, Undefined) => return Ok(true),

      // Number/string conversions.
      (Number(_), String(_)) => {
        let bn = to_number(&mut scope, b)?;
        b = scope.push_root(Number(bn))?;
      }
      (String(_), Number(_)) => {
        let an = to_number(&mut scope, a)?;
        a = scope.push_root(Number(an))?;
      }

      // Boolean conversions.
      (Bool(_), _) => {
        let an = to_number(&mut scope, a)?;
        a = scope.push_root(Number(an))?;
      }
      (_, Bool(_)) => {
        let bn = to_number(&mut scope, b)?;
        b = scope.push_root(Number(bn))?;
      }

      // Object-to-primitive conversions.
      (Object(_), String(_) | Number(_) | BigInt(_) | Symbol(_)) => {
        let prim = to_primitive(&mut scope, a)?;
        a = scope.push_root(prim)?;
      }
      (String(_) | Number(_) | BigInt(_) | Symbol(_), Object(_)) => {
        let prim = to_primitive(&mut scope, b)?;
        b = scope.push_root(prim)?;
      }

      _ => return Ok(false),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::render_control::RenderDeadline;

  fn eval_raw_value(
    realm: &mut VmJsScriptRealm,
    source_name: &str,
    source: &str,
  ) -> Result<Value, ScriptError> {
    // Mirror `VmJsScriptRealm::eval_script_with_budget`, but return the raw vm-js `Value` so tests
    // can inspect heap-backed strings without lossy UTF-8 conversion.
    realm.interrupt_flag.store(false, Ordering::Relaxed);
    let budget = realm.derive_budget(None, &ScriptBudgetOverride::default());
    realm.vm.set_budget(budget);

    let opts = ParseOptions {
      dialect: Dialect::Ecma,
      source_type: SourceType::Script,
    };
    let top = parse_with_options(source, opts).map_err(|err| ScriptError::Syntax {
      message: err.to_string(),
    })?;

    let source_text = SourceText::new(source_name, source);

    // Push a frame so errors/terminations surface at least one stack frame.
    let (line, col) = source_text.line_col(0);
    realm
      .vm
      .push_frame(StackFrame {
        function: None,
        source: source_text.name.clone(),
        line,
        col,
      })
      .map_err(|err| match err {
        VmError::Termination(t) => ScriptError::Termination {
          reason: t.reason.into(),
          stack_trace: format_stack_trace(&t.stack),
        },
        other => ScriptError::Runtime {
          message: other.to_string(),
          stack_trace: String::new(),
        },
      })?;

    let vm = &mut realm.vm;
    let heap = &mut realm.heap;
    let env = &mut realm.env;
    let host_functions = &mut realm.host_functions;
    let interrupt_handle = realm.interrupt_handle.clone();

    let mut scope = heap.scope();
    let mut evaluator = Evaluator {
      vm,
      env,
      host_functions,
      interrupt_handle,
      render_deadline: None,
      source: &source_text,
    };
    let result = evaluator.exec_stmt_list(&mut scope, &top.stx.body);
    evaluator.vm.pop_frame();
    result
  }

  #[test]
  fn evals_trivial_expression() {
    let mut realm = VmJsScriptRealm::new(ScriptRealmOptions {
      heap_limits: HeapLimits::new(4 * 1024 * 1024, 2 * 1024 * 1024),
      default_fuel: Some(10_000),
      default_deadline: None,
      check_time_every: 1,
      max_stack_depth: 1024,
    })
    .unwrap();
    let value = realm.eval_script("test.js", "1+2").unwrap();
    assert_eq!(value, ScriptValue::Number(3.0));
  }

  #[test]
  fn string_concatenation_uses_ecmascript_number_formatting() {
    let mut realm = VmJsScriptRealm::new(ScriptRealmOptions {
      heap_limits: HeapLimits::new(4 * 1024 * 1024, 2 * 1024 * 1024),
      default_fuel: Some(10_000),
      default_deadline: None,
      check_time_every: 1,
      max_stack_depth: 1024,
    })
    .unwrap();

    let value = realm.eval_script("num.js", "1e999 + ''").unwrap();
    assert_eq!(value, ScriptValue::String("Infinity".to_string()));
  }

  #[test]
  fn addition_coerces_booleans_and_null_to_numbers() {
    let mut realm = VmJsScriptRealm::new(ScriptRealmOptions {
      heap_limits: HeapLimits::new(4 * 1024 * 1024, 2 * 1024 * 1024),
      default_fuel: Some(10_000),
      default_deadline: None,
      check_time_every: 1,
      max_stack_depth: 1024,
    })
    .unwrap();

    assert_eq!(
      realm.eval_script("add.js", "1 + true").unwrap(),
      ScriptValue::Number(2.0)
    );
    assert_eq!(
      realm.eval_script("add.js", "1 + null").unwrap(),
      ScriptValue::Number(1.0)
    );
    assert_eq!(
      realm.eval_script("add.js", "'1' + true").unwrap(),
      ScriptValue::String("1true".to_string())
    );
  }

  #[test]
  fn equality_operators_follow_ecmascript_coercions() {
    let mut realm = VmJsScriptRealm::new(ScriptRealmOptions {
      heap_limits: HeapLimits::new(4 * 1024 * 1024, 2 * 1024 * 1024),
      default_fuel: Some(10_000),
      default_deadline: None,
      check_time_every: 1,
      max_stack_depth: 1024,
    })
    .unwrap();

    // Abstract equality (`==`) performs type coercion.
    assert_eq!(
      realm.eval_script("eq.js", "0 == false").unwrap(),
      ScriptValue::Bool(true)
    );
    assert_eq!(
      realm.eval_script("eq.js", "'0' == 0").unwrap(),
      ScriptValue::Bool(true)
    );
    assert_eq!(
      realm.eval_script("eq.js", "null == undefined").unwrap(),
      ScriptValue::Bool(true)
    );
    // `NaN` is never equal to itself, even with `==`.
    assert_eq!(
      realm.eval_script("eq.js", "NaN == NaN").unwrap(),
      ScriptValue::Bool(false)
    );

    // Strict equality (`===`) does not coerce.
    assert_eq!(
      realm.eval_script("eq.js", "0 === false").unwrap(),
      ScriptValue::Bool(false)
    );
    assert_eq!(
      realm.eval_script("eq.js", "'0' === 0").unwrap(),
      ScriptValue::Bool(false)
    );
    assert_eq!(
      realm.eval_script("eq.js", "null === undefined").unwrap(),
      ScriptValue::Bool(false)
    );
    assert_eq!(
      realm.eval_script("eq.js", "NaN === NaN").unwrap(),
      ScriptValue::Bool(false)
    );

    // Inequality operators are the negation of the corresponding equality.
    assert_eq!(
      realm.eval_script("eq.js", "1 != '1'").unwrap(),
      ScriptValue::Bool(false)
    );
    assert_eq!(
      realm.eval_script("eq.js", "1 !== '1'").unwrap(),
      ScriptValue::Bool(true)
    );
  }

  #[test]
  fn read_only_globals_cannot_be_overwritten() {
    let mut realm = VmJsScriptRealm::new(ScriptRealmOptions {
      heap_limits: HeapLimits::new(4 * 1024 * 1024, 2 * 1024 * 1024),
      default_fuel: Some(10_000),
      default_deadline: None,
      check_time_every: 1,
      max_stack_depth: 1024,
    })
    .unwrap();

    // In sloppy mode, assignments to non-writable globals fail silently.
    assert_eq!(
      realm.eval_script("ro.js", "undefined = 1; undefined").unwrap(),
      ScriptValue::Undefined
    );
    assert_eq!(
      realm.eval_script("ro.js", "NaN = 1; NaN === NaN").unwrap(),
      ScriptValue::Bool(false)
    );
    assert_eq!(
      realm.eval_script("ro.js", "Infinity = 1; Infinity").unwrap(),
      ScriptValue::Number(f64::INFINITY)
    );
  }

  #[test]
  fn infinite_loop_is_interrupted_by_fuel_budget() {
    let mut realm = VmJsScriptRealm::new(ScriptRealmOptions {
      heap_limits: HeapLimits::new(4 * 1024 * 1024, 2 * 1024 * 1024),
      default_fuel: Some(1_000_000),
      default_deadline: None,
      check_time_every: 1,
      max_stack_depth: 1024,
    })
    .unwrap();

    let err = realm
      .eval_script_with_budget(
        "loop.js",
        "while(true){}",
        ScriptBudgetOverride {
          fuel: Some(32),
          wall_time: None,
        },
      )
      .unwrap_err();

    assert!(matches!(
      err,
      ScriptError::Termination {
        reason: ScriptTerminationReason::OutOfFuel,
        ..
      }
    ));
  }

  #[test]
  fn uncaught_throw_includes_stack_trace() {
    let mut realm = VmJsScriptRealm::new(ScriptRealmOptions {
      heap_limits: HeapLimits::new(4 * 1024 * 1024, 2 * 1024 * 1024),
      default_fuel: Some(10_000),
      default_deadline: None,
      check_time_every: 1,
      max_stack_depth: 1024,
    })
    .unwrap();

    let err = realm.eval_script("boom.js", "throw 'boom';").unwrap_err();

    match err {
      ScriptError::Exception {
        message,
        stack_trace,
      } => {
        assert_eq!(message, "boom");
        assert!(stack_trace.contains("boom.js"));
        assert!(stack_trace.contains("boom.js:1:1"));
      }
      other => panic!("expected ScriptError::Exception, got {other:?}"),
    }
  }

  #[test]
  fn derives_deadline_from_render_control() {
    let deadline = RenderDeadline::new(Some(Duration::from_millis(0)), None);
    let mut realm = VmJsScriptRealm::new(ScriptRealmOptions {
      heap_limits: HeapLimits::new(4 * 1024 * 1024, 2 * 1024 * 1024),
      default_fuel: None, // ensure we terminate due to deadline, not fuel
      default_deadline: None,
      check_time_every: 1,
      max_stack_depth: 1024,
    })
    .unwrap();

    let err = crate::render_control::with_deadline(Some(&deadline), || {
      realm.eval_script("deadline.js", "while(true){}")
    })
    .unwrap_err();

    assert!(matches!(
      err,
      ScriptError::Termination {
        reason: ScriptTerminationReason::DeadlineExceeded,
        ..
      }
    ));
  }

  #[test]
  fn host_function_can_be_called() {
    let mut realm = VmJsScriptRealm::new(ScriptRealmOptions {
      heap_limits: HeapLimits::new(4 * 1024 * 1024, 2 * 1024 * 1024),
      default_fuel: Some(10_000),
      default_deadline: None,
      check_time_every: 1,
      max_stack_depth: 1024,
    })
    .unwrap();

    realm
      .register_host_function(
        "add",
        Box::new(|args| match args {
          [ScriptValue::Number(a), ScriptValue::Number(b)] => Ok(ScriptValue::Number(a + b)),
          other => Err(ScriptError::Runtime {
            message: format!("unexpected args: {other:?}"),
            stack_trace: String::new(),
          }),
        }),
      )
      .unwrap();

    let value = realm.eval_script("host.js", "add(1, 2)").unwrap();
    assert_eq!(value, ScriptValue::Number(3.0));
  }

  #[test]
  fn string_literals_preserve_utf16_code_units() {
    let mut realm = VmJsScriptRealm::new(ScriptRealmOptions {
      heap_limits: HeapLimits::new(4 * 1024 * 1024, 2 * 1024 * 1024),
      default_fuel: Some(10_000),
      default_deadline: None,
      check_time_every: 1,
      max_stack_depth: 1024,
    })
    .unwrap();

    let value = eval_raw_value(&mut realm, "string.js", r#""\uD800""#).unwrap();
    let Value::String(s) = value else {
      panic!("expected string value, got {value:?}");
    };
    assert_eq!(realm.heap.get_string(s).unwrap().as_code_units(), &[0xD800]);

    let value = eval_raw_value(&mut realm, "string.js", r#""a\uD800b""#).unwrap();
    let Value::String(s) = value else {
      panic!("expected string value, got {value:?}");
    };
    assert_eq!(
      realm.heap.get_string(s).unwrap().as_code_units(),
      &[0x0061, 0xD800, 0x0062]
    );
  }

  #[test]
  fn interrupt_is_reset_between_evaluations() {
    let deadline = RenderDeadline::new(None, Some(Arc::new(|| true)));
    let mut realm = VmJsScriptRealm::new(ScriptRealmOptions {
      heap_limits: HeapLimits::new(4 * 1024 * 1024, 2 * 1024 * 1024),
      default_fuel: Some(10_000),
      default_deadline: None,
      check_time_every: 1,
      max_stack_depth: 1024,
    })
    .unwrap();

    let err = crate::render_control::with_deadline(Some(&deadline), || {
      realm.eval_script("interrupt.js", "1+2")
    })
    .unwrap_err();
    assert!(matches!(
      err,
      ScriptError::Termination {
        reason: ScriptTerminationReason::Interrupted,
        ..
      }
    ));

    let value = realm.eval_script("ok.js", "1+2").unwrap();
    assert_eq!(value, ScriptValue::Number(3.0));
  }

  #[test]
  fn out_of_memory_is_reported_as_termination() {
    let mut realm = VmJsScriptRealm::new(ScriptRealmOptions {
      heap_limits: HeapLimits::new(32 * 1024, 16 * 1024),
      default_fuel: Some(10_000),
      default_deadline: None,
      check_time_every: 1,
      max_stack_depth: 1024,
    })
    .unwrap();

    let source = format!("\"{}\"", "a".repeat(100_000));
    let err = realm.eval_script("oom.js", &source).unwrap_err();
    assert!(matches!(
      err,
      ScriptError::Termination {
        reason: ScriptTerminationReason::OutOfMemory,
        ..
      }
    ));
  }
}
