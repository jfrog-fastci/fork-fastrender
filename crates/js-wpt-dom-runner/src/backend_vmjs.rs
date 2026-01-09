use crate::backend::{Backend, BackendInit, BackendReport};
use crate::wpt_report::WptReport;
use crate::RunError;
use parse_js::ast::class_or_object::{ClassOrObjKey, ClassOrObjVal, ObjMemberType};
use parse_js::ast::expr::lit::{LitBoolExpr, LitNumExpr, LitObjExpr, LitStrExpr};
use parse_js::ast::expr::{
  BinaryExpr, CallExpr, ComputedMemberExpr, Expr, IdExpr, MemberExpr, UnaryExpr,
};
use parse_js::ast::func::FuncBody;
use parse_js::ast::node::Node;
use parse_js::ast::stmt::decl::{PatDecl, VarDecl, VarDeclMode};
use parse_js::ast::stmt::{
  BlockStmt, CatchBlock, DoWhileStmt, ExprStmt, ForBody, ForTripleStmt, IfStmt, ReturnStmt, Stmt,
  ThrowStmt, TryStmt, WhileStmt,
};
use parse_js::operator::OperatorName;
use parse_js::{parse_with_options, Dialect, ParseOptions, SourceType};
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::time::Duration;
use vm_js::{
  GcObject, GcString, Heap, HeapLimits, PropertyDescriptor, PropertyKey, PropertyKind, Value, Vm,
  VmError, VmOptions,
};

pub(crate) fn is_available() -> bool {
  true
}

pub struct VmJsBackend {
  rt: Option<JsWptRuntime>,
  deadline: Option<Duration>,
  timed_out: bool,
  max_tasks: usize,
  max_microtasks: usize,
  tasks_executed: usize,
  microtasks_executed: usize,
}

impl Default for VmJsBackend {
  fn default() -> Self {
    Self::new()
  }
}

impl VmJsBackend {
  pub fn new() -> Self {
    Self {
      rt: None,
      deadline: None,
      timed_out: false,
      max_tasks: 0,
      max_microtasks: 0,
      tasks_executed: 0,
      microtasks_executed: 0,
    }
  }

  fn rt_mut(&mut self) -> Result<&mut JsWptRuntime, RunError> {
    self
      .rt
      .as_mut()
      .ok_or_else(|| RunError::Js("vm-js backend is not initialized".to_string()))
  }

  fn drain_microtasks_internal(&mut self) -> Result<(), RunError> {
    if self.timed_out {
      return Ok(());
    }
    let max_microtasks = self.max_microtasks;
    let mut executed = self.microtasks_executed;
    let mut hit_limit_or_timeout = false;

    let result = {
      let rt = self.rt_mut()?;
      let mut outcome: Result<(), RunError> = Ok(());

      while let Some((cb, this, args)) = rt.event_loop.drain_microtasks() {
        if executed >= max_microtasks {
          hit_limit_or_timeout = true;
          break;
        }
        executed += 1;

        if let Err(err) = rt.call(cb, this, &args) {
          if err.is_timeout() {
            hit_limit_or_timeout = true;
            break;
          }
          let msg = err.to_message(rt);
          if !rt.reported() {
            rt.set_report_error(&msg);
          }
          outcome = Err(RunError::Js(msg));
          break;
        }
      }
      outcome
    };

    self.microtasks_executed = executed;
    if hit_limit_or_timeout {
      self.timed_out = true;
    }
    result
  }
}

impl Backend for VmJsBackend {
  fn init_realm(&mut self, init: BackendInit) -> Result<(), RunError> {
    // Use a virtual deadline derived from the configured timeout. The backend advances its own
    // virtual clock deterministically when idle (see `idle_wait`) rather than sleeping in real
    // time.
    self.deadline = Some(init.timeout);
    self.timed_out = false;
    self.max_tasks = init.max_tasks;
    self.max_microtasks = init.max_microtasks;
    self.tasks_executed = 0;
    self.microtasks_executed = 0;

    self.rt = Some(JsWptRuntime::new(&init.test_url));
    Ok(())
  }

  fn eval_script(&mut self, source: &str) -> Result<(), RunError> {
    if self.timed_out {
      return Ok(());
    }
    if self.is_timed_out() {
      self.timed_out = true;
      return Ok(());
    }

    if self.rt.as_ref().is_some_and(|rt| rt.reported()) {
      return Ok(());
    }

    let exec_result = {
      let rt = self.rt_mut()?;
      rt.exec_script(source)
    };

    match exec_result {
      Ok(_v) => {
        // Microtask checkpoint after every script evaluation.
        self.drain_microtasks_internal()?;
        Ok(())
      }
      Err(err) => {
        if err.is_timeout() {
          self.timed_out = true;
          return Ok(());
        }
        let msg = {
          let rt = self.rt_mut()?;
          let msg = err.to_message(rt);
          if !rt.reported() {
            rt.set_report_error(&msg);
          }
          msg
        };
        Err(RunError::Js(msg))
      }
    }
  }

  fn drain_microtasks(&mut self) -> Result<(), RunError> {
    self.drain_microtasks_internal()
  }

  fn poll_event_loop(&mut self) -> Result<bool, RunError> {
    if self.timed_out {
      return Ok(false);
    }
    if self.is_timed_out() {
      self.timed_out = true;
      return Ok(false);
    }
    if self.tasks_executed >= self.max_tasks {
      self.timed_out = true;
      return Ok(false);
    }

    let max_tasks = self.max_tasks;
    let mut executed = self.tasks_executed;
    let mut hit_limit_or_timeout = false;

    let result = {
      let rt = self.rt_mut()?;

      rt.event_loop.enqueue_due_timers();
      let mut outcome: Result<bool, RunError> = Ok(false);

      match rt.event_loop.pop_next_task() {
        None => {}
        Some((cb, this, args)) => {
          executed += 1;
          if executed > max_tasks {
            hit_limit_or_timeout = true;
          } else if let Err(err) = rt.call(cb, this, &args) {
            if err.is_timeout() {
              hit_limit_or_timeout = true;
            } else {
              let msg = err.to_message(rt);
              if !rt.reported() {
                rt.set_report_error(&msg);
              }
              outcome = Err(RunError::Js(msg));
            }
          } else {
            outcome = Ok(true);
          }
        }
      }

      outcome
    };

    self.tasks_executed = executed;
    if hit_limit_or_timeout {
      self.timed_out = true;
    }

    let did_work = matches!(result.as_ref(), Ok(true));
    if did_work {
      // Microtask checkpoint after a task.
      self.drain_microtasks_internal()?;
    }

    result
  }

  fn take_report(&mut self) -> Result<Option<BackendReport>, RunError> {
    let rt = self.rt_mut()?;
    Ok(rt.report.take())
  }

  fn is_timed_out(&self) -> bool {
    if self.timed_out {
      return true;
    }
    let Some(deadline) = self.deadline else {
      return true;
    };
    let Some(rt) = self.rt.as_ref() else {
      return true;
    };
    rt.event_loop.now >= deadline
  }

  fn idle_wait(&mut self) {
    // Deterministic virtual time advancement:
    // - If a timer is scheduled in the future, fast-forward to its due time (or the deadline).
    // - If no timers remain, fast-forward to the deadline so the run terminates deterministically.
    if self.timed_out {
      return;
    }
    let Some(deadline) = self.deadline else {
      self.timed_out = true;
      return;
    };
    let Some(rt) = self.rt.as_mut() else {
      self.timed_out = true;
      return;
    };

    let now = rt.event_loop.now;
    let next_due = rt.event_loop.next_timer_due_time();
    let target = match next_due {
      Some(due) if due > now => due.min(deadline),
      Some(_due) => now,
      None => deadline,
    };

    if target > now {
      rt.event_loop.now = target;
    } else if now < deadline {
      // Nothing runnable and nothing to advance to: force progress to the deadline so we don't
      // spin forever.
      rt.event_loop.now = deadline;
    }

    if rt.event_loop.now >= deadline {
      self.timed_out = true;
    }
  }
}

// --- Minimal vm-js backed script evaluator + host shims ---

#[derive(Debug)]
enum JsError {
  Parse(String),
  Vm(VmError),
}

impl JsError {
  fn is_timeout(&self) -> bool {
    match self {
      JsError::Parse(_) => false,
      JsError::Vm(VmError::Termination(term)) => matches!(
        term.reason,
        vm_js::TerminationReason::OutOfFuel
          | vm_js::TerminationReason::DeadlineExceeded
          | vm_js::TerminationReason::Interrupted
      ),
      _ => false,
    }
  }

  fn to_message(&self, rt: &mut JsWptRuntime) -> String {
    match self {
      JsError::Parse(msg) => msg.clone(),
      JsError::Vm(VmError::Throw(value)) => rt.value_to_string_lossy(*value),
      JsError::Vm(other) => other.to_string(),
    }
  }
}

impl From<VmError> for JsError {
  fn from(value: VmError) -> Self {
    Self::Vm(value)
  }
}

#[derive(Debug, Clone, Copy)]
struct CachedKeys {
  href: GcString,
  url: GcString,
  log: GcString,
  file_status: GcString,
  harness_status: GcString,
  message: GcString,
  stack: GcString,
}

impl CachedKeys {
  fn new(heap: &mut Heap) -> Result<Self, JsError> {
    let mut scope = heap.scope();
    Ok(Self {
      href: scope.alloc_string("href")?,
      url: scope.alloc_string("URL")?,
      log: scope.alloc_string("log")?,
      file_status: scope.alloc_string("file_status")?,
      harness_status: scope.alloc_string("harness_status")?,
      message: scope.alloc_string("message")?,
      stack: scope.alloc_string("stack")?,
    })
  }
}

#[derive(Debug)]
enum Callable {
  Native(fn(&mut JsWptRuntime, Value, &[Value]) -> Result<Value, JsError>),
  User(UserFunction),
}

#[derive(Debug)]
struct UserFunction {
  params: Vec<String>,
  body: Vec<Node<Stmt>>,
}

#[derive(Debug, Default, Clone)]
struct EnvFrame {
  var: HashMap<String, Value>,
  lexical: Vec<HashMap<String, Value>>,
}

impl EnvFrame {
  fn new() -> Self {
    Self {
      var: HashMap::new(),
      lexical: vec![HashMap::new()],
    }
  }
}

#[derive(Debug, Default)]
struct Env {
  frames: Vec<EnvFrame>,
}

impl Env {
  fn new() -> Self {
    Self {
      frames: vec![EnvFrame::new()],
    }
  }

  fn push_frame(&mut self) {
    self.frames.push(EnvFrame::new());
  }

  fn pop_frame(&mut self) {
    debug_assert!(self.frames.len() > 1, "attempted to pop global frame");
    if self.frames.len() > 1 {
      self.frames.pop();
    }
  }

  fn push_lexical(&mut self) {
    let Some(frame) = self.frames.last_mut() else {
      return;
    };
    frame.lexical.push(HashMap::new());
  }

  fn pop_lexical(&mut self) {
    let Some(frame) = self.frames.last_mut() else {
      return;
    };
    debug_assert!(frame.lexical.len() > 1, "attempted to pop global lexical env");
    if frame.lexical.len() > 1 {
      frame.lexical.pop();
    }
  }

  fn declare_var(&mut self, name: &str) {
    let Some(frame) = self.frames.last_mut() else {
      return;
    };
    frame.var.entry(name.to_string()).or_insert(Value::Undefined);
  }

  fn declare_lexical(&mut self, name: &str, value: Value) {
    let Some(frame) = self.frames.last_mut() else {
      return;
    };
    let Some(scope) = frame.lexical.last_mut() else {
      return;
    };
    scope.insert(name.to_string(), value);
  }

  fn get(&self, name: &str) -> Option<Value> {
    for frame in self.frames.iter().rev() {
      for scope in frame.lexical.iter().rev() {
        if let Some(v) = scope.get(name).copied() {
          return Some(v);
        }
      }
      if let Some(v) = frame.var.get(name).copied() {
        return Some(v);
      }
    }
    None
  }

  fn set(&mut self, name: &str, value: Value) {
    for frame in self.frames.iter_mut().rev() {
      for scope in frame.lexical.iter_mut().rev() {
        if scope.contains_key(name) {
          scope.insert(name.to_string(), value);
          return;
        }
      }
      if frame.var.contains_key(name) {
        frame.var.insert(name.to_string(), value);
        return;
      }
    }

    // Sloppy-mode fallback: create a global `var` binding.
    let Some(global) = self.frames.first_mut() else {
      return;
    };
    global.var.insert(name.to_string(), value);
  }
}

struct TimerState {
  kind: TimerKind,
  interval: Option<Duration>,
  callback: Value,
  this: Value,
  args: Vec<Value>,
  schedule_seq: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimerKind {
  Timeout,
  Interval,
}

#[derive(Debug, Clone, Copy)]
struct ListenerOptions {
  capture: bool,
  once: bool,
  passive: bool,
}

#[derive(Debug, Clone)]
struct EventListener {
  callback: Value,
  options: ListenerOptions,
}

#[derive(Debug, Default)]
struct EventTargetState {
  parent: Option<GcObject>,
  // event type -> listeners (in registration order).
  listeners: HashMap<String, Vec<EventListener>>,
}

#[derive(Debug, Clone)]
struct EventState {
  typ: String,
  bubbles: bool,
  cancelable: bool,
  default_prevented: bool,
  propagation_stopped: bool,
  immediate_stopped: bool,
  in_passive_listener: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromiseStatus {
  Pending,
  Fulfilled,
  Rejected,
}

#[derive(Debug, Clone)]
struct PromiseReaction {
  on_fulfilled: Value,
  on_rejected: Value,
  next_promise: GcObject,
}

#[derive(Debug, Clone)]
struct PromiseState {
  status: PromiseStatus,
  value: Value,
  reactions: Vec<PromiseReaction>,
}

#[derive(Debug, Clone)]
struct PromiseJob {
  status: PromiseStatus,
  value: Value,
  reaction: PromiseReaction,
}

#[derive(Default)]
struct EventLoop {
  now: Duration,
  next_timer_id: i32,
  next_timer_seq: u64,
  timers: HashMap<i32, TimerState>,
  timer_queue: BinaryHeap<Reverse<(Duration, u64, i32)>>,
  task_queue: VecDeque<(Value, Value, Vec<Value>)>,
  microtask_queue: VecDeque<(Value, Value, Vec<Value>)>,
}

impl EventLoop {
  fn new() -> Self {
    Self {
      now: Duration::ZERO,
      next_timer_id: 1,
      next_timer_seq: 0,
      timers: HashMap::new(),
      timer_queue: BinaryHeap::new(),
      task_queue: VecDeque::new(),
      microtask_queue: VecDeque::new(),
    }
  }

  fn set_timeout(&mut self, callback: Value, this: Value, delay: Duration, args: Vec<Value>) -> i32 {
    self.add_timer(TimerKind::Timeout, callback, this, delay, None, args)
  }

  fn set_interval(
    &mut self,
    callback: Value,
    this: Value,
    interval: Duration,
    args: Vec<Value>,
  ) -> i32 {
    self.add_timer(
      TimerKind::Interval,
      callback,
      this,
      interval,
      Some(interval),
      args,
    )
  }

  fn add_timer(
    &mut self,
    kind: TimerKind,
    callback: Value,
    this: Value,
    delay: Duration,
    interval: Option<Duration>,
    args: Vec<Value>,
  ) -> i32 {
    let id = self.next_timer_id;
    self.next_timer_id = self.next_timer_id.wrapping_add(1);
    let due = self.now.saturating_add(delay);
    let schedule_seq = self.next_timer_seq;
    self.next_timer_seq = self.next_timer_seq.wrapping_add(1);

    self.timers.insert(
      id,
      TimerState {
        kind,
        interval,
        callback,
        this,
        args,
        schedule_seq,
      },
    );
    self.timer_queue.push(Reverse((due, schedule_seq, id)));
    id
  }

  fn clear_timeout(&mut self, id: i32) {
    self.timers.remove(&id);
  }

  fn queue_microtask(&mut self, callback: Value, this: Value, args: Vec<Value>) {
    self.microtask_queue.push_back((callback, this, args));
  }

  fn drain_microtasks(&mut self) -> Option<(Value, Value, Vec<Value>)> {
    self.microtask_queue.pop_front()
  }

  fn pop_next_task(&mut self) -> Option<(Value, Value, Vec<Value>)> {
    self.task_queue.pop_front()
  }

  fn enqueue_due_timers(&mut self) {
    let now = self.now;
    while let Some(Reverse((due, schedule_seq, id))) = self.timer_queue.peek().copied() {
      if due > now {
        break;
      }
      let _ = self.timer_queue.pop();

      let Some(timer) = self.timers.get(&id) else {
        continue;
      };
      if timer.schedule_seq != schedule_seq {
        continue;
      }

      self.task_queue.push_back((timer.callback, timer.this, timer.args.clone()));

      match timer.kind {
        TimerKind::Timeout => {
          self.timers.remove(&id);
        }
        TimerKind::Interval => {
          // Avoid an infinite loop when the interval is 0: rescheduling a timer with the same
          // `due` timestamp would cause `enqueue_due_timers` to repeatedly dequeue/re-enqueue the
          // same interval in a single pass. A 1ms minimum keeps the runner cooperative while still
          // allowing "as fast as possible" intervals for tests.
          let interval = timer.interval.unwrap_or(Duration::from_millis(0));
          let interval = if interval.is_zero() {
            Duration::from_millis(1)
          } else {
            interval
          };
          let next_due = now + interval;
          let next_seq = self.next_timer_seq;
          self.next_timer_seq = self.next_timer_seq.wrapping_add(1);

          if let Some(timer) = self.timers.get_mut(&id) {
            timer.schedule_seq = next_seq;
          }
          self.timer_queue.push(Reverse((next_due, next_seq, id)));
        }
      }
    }
  }

  fn next_timer_due_time(&mut self) -> Option<Duration> {
    while let Some(Reverse((due, schedule_seq, id))) = self.timer_queue.peek().copied() {
      match self.timers.get(&id) {
        Some(timer) if timer.schedule_seq == schedule_seq => return Some(due),
        _ => {
          // Stale queue entry (cleared timer or rescheduled id).
          let _ = self.timer_queue.pop();
        }
      }
    }
    None
  }
}

struct JsWptRuntime {
  vm: Vm,
  heap: Heap,
  env: Env,
  callables: HashMap<GcObject, Rc<Callable>>,
  event_targets: HashMap<GcObject, EventTargetState>,
  events: HashMap<GcObject, EventState>,
  promises: HashMap<GcObject, PromiseState>,
  promise_jobs: HashMap<u64, PromiseJob>,
  next_promise_job_id: u64,
  promise_job_runner: Option<GcObject>,
  promise_prototype: Option<GcObject>,
  global_object: GcObject,
  keys: CachedKeys,
  pub(crate) event_loop: EventLoop,
  report: Option<WptReport>,
  this_binding: Value,
}

impl JsWptRuntime {
  fn new(test_url: &str) -> Self {
    let mut vm = Vm::new(VmOptions {
      max_stack_depth: 1024,
      default_fuel: None,
      default_deadline: None,
      check_time_every: 1,
      interrupt_flag: None,
    });
    vm.set_budget(vm_js::Budget {
      fuel: Some(50_000_000),
      // The offline runner enforces per-test timeouts using a deterministic virtual clock. We keep
      // vm-js's time-based deadlines disabled to avoid depending on wall-clock time.
      deadline: None,
      check_time_every: 1,
    });

    // Avoid GC during typical smoke test runs; this harness does not model stack/persistent roots
    // for values stored in host-side data structures.
    let mut heap = Heap::new(HeapLimits::new(128 * 1024 * 1024, 128 * 1024 * 1024));
    let keys = CachedKeys::new(&mut heap).expect("cached key allocation");

    let global_object = {
      let mut scope = heap.scope();
      scope.alloc_object().expect("alloc global object")
    };
    let global_value = Value::Object(global_object);

    let mut rt = Self {
      vm,
      heap,
      env: Env::new(),
      callables: HashMap::new(),
      event_targets: HashMap::new(),
      events: HashMap::new(),
      promises: HashMap::new(),
      promise_jobs: HashMap::new(),
      next_promise_job_id: 1,
      promise_job_runner: None,
      promise_prototype: None,
      global_object,
      keys,
      event_loop: EventLoop::new(),
      report: None,
      this_binding: global_value,
    };

    // Bind globalThis/window/self.
    rt.env.set("globalThis", global_value);
    rt.env.set("window", global_value);
    rt.env.set("self", global_value);

    // Report hook.
    let report_fn = rt.alloc_native_function(native_wpt_report).expect("alloc report fn");
    rt.env.set("__fastrender_wpt_report", Value::Object(report_fn));

    // Timers + microtasks.
    let set_timeout = rt.alloc_native_function(native_set_timeout).expect("alloc setTimeout");
    rt.env.set("setTimeout", Value::Object(set_timeout));
    let clear_timeout = rt.alloc_native_function(native_clear_timeout).expect("alloc clearTimeout");
    rt.env.set("clearTimeout", Value::Object(clear_timeout));
    let set_interval = rt.alloc_native_function(native_set_interval).expect("alloc setInterval");
    rt.env.set("setInterval", Value::Object(set_interval));
    let clear_interval = rt
      .alloc_native_function(native_clear_interval)
      .expect("alloc clearInterval");
    rt.env.set("clearInterval", Value::Object(clear_interval));
    let queue_microtask = rt
      .alloc_native_function(native_queue_microtask)
      .expect("alloc queueMicrotask");
    rt.env.set("queueMicrotask", Value::Object(queue_microtask));

    rt.install_promise_shim().expect("install Promise");
    rt
      .install_event_target_and_event()
      .expect("install EventTarget/Event");

    // console.log.
    let console = rt.alloc_object().expect("alloc console");
    let log_fn = rt.alloc_native_function(native_console_log).expect("alloc console.log");
    rt
      .define_data_prop(console, PropertyKey::from_string(rt.keys.log), Value::Object(log_fn))
      .expect("define console.log");
    rt.env.set("console", Value::Object(console));

    rt.install_location_and_document(test_url)
      .expect("install location/document");

    rt
  }

  fn install_location_and_document(&mut self, test_url: &str) -> Result<(), JsError> {
    let href_value = self.alloc_string_value(test_url)?;

    let location = self.alloc_object()?;
    self.define_data_prop(
      location,
      PropertyKey::from_string(self.keys.href),
      href_value,
    )?;
    self.env.set("location", Value::Object(location));

    let document = self.alloc_object()?;
    self.define_data_prop(document, PropertyKey::from_string(self.keys.url), href_value)?;
    self.env.set("document", Value::Object(document));

    Ok(())
  }

  fn install_promise_shim(&mut self) -> Result<(), JsError> {
    let proto = self.alloc_object()?;
    let then_fn = self.alloc_native_function(native_promise_then)?;
    let then_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("then")?)
    };
    self.define_data_prop(proto, then_key, Value::Object(then_fn))?;

    let promise = self.alloc_object()?;
    let resolve_fn = self.alloc_native_function(native_promise_resolve)?;
    let resolve_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("resolve")?)
    };
    self.define_data_prop(promise, resolve_key, Value::Object(resolve_fn))?;

    self.env.set("Promise", Value::Object(promise));

    let job_runner = self.alloc_native_function(native_run_promise_job)?;
    self.promise_job_runner = Some(job_runner);
    self.promise_prototype = Some(proto);
    Ok(())
  }

  fn install_event_target_and_event(&mut self) -> Result<(), JsError> {
    // Event prototype.
    let event_proto = self.alloc_object()?;
    let prevent_default = self.alloc_native_function(native_event_prevent_default)?;
    let stop_propagation = self.alloc_native_function(native_event_stop_propagation)?;
    let stop_immediate = self.alloc_native_function(native_event_stop_immediate_propagation)?;

    let prevent_default_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("preventDefault")?)
    };
    let stop_propagation_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("stopPropagation")?)
    };
    let stop_immediate_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("stopImmediatePropagation")?)
    };
    self.define_data_prop(
      event_proto,
      prevent_default_key,
      Value::Object(prevent_default),
    )?;
    self.define_data_prop(
      event_proto,
      stop_propagation_key,
      Value::Object(stop_propagation),
    )?;
    self.define_data_prop(
      event_proto,
      stop_immediate_key,
      Value::Object(stop_immediate),
    )?;

    // Event constructor.
    let event_ctor = self.alloc_native_function(native_event_ctor)?;
    let prototype_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("prototype")?)
    };
    self.define_data_prop(event_ctor, prototype_key, Value::Object(event_proto))?;
    self.env.set("Event", Value::Object(event_ctor));

    // EventTarget prototype.
    let et_proto = self.alloc_object()?;
    let add_listener = self.alloc_native_function(native_eventtarget_add_event_listener)?;
    let remove_listener = self.alloc_native_function(native_eventtarget_remove_event_listener)?;
    let dispatch = self.alloc_native_function(native_eventtarget_dispatch_event)?;

    let add_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("addEventListener")?)
    };
    let remove_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("removeEventListener")?)
    };
    let dispatch_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("dispatchEvent")?)
    };

    self.define_data_prop(et_proto, add_key, Value::Object(add_listener))?;
    self.define_data_prop(et_proto, remove_key, Value::Object(remove_listener))?;
    self.define_data_prop(et_proto, dispatch_key, Value::Object(dispatch))?;

    // EventTarget constructor.
    let et_ctor = self.alloc_native_function(native_eventtarget_ctor)?;
    let prototype_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("prototype")?)
    };
    self.define_data_prop(et_ctor, prototype_key, Value::Object(et_proto))?;
    self.env.set("EventTarget", Value::Object(et_ctor));

    Ok(())
  }

  fn alloc_object(&mut self) -> Result<GcObject, JsError> {
    let obj = {
      let mut scope = self.heap.scope();
      scope.alloc_object()?
    };
    Ok(obj)
  }

  fn alloc_string_value(&mut self, s: &str) -> Result<Value, JsError> {
    let handle = {
      let mut scope = self.heap.scope();
      scope.alloc_string(s)?
    };
    Ok(Value::String(handle))
  }

  fn define_data_prop(
    &mut self,
    obj: GcObject,
    key: PropertyKey,
    value: Value,
  ) -> Result<(), JsError> {
    let desc = PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: PropertyKind::Data {
        value,
        writable: true,
      },
    };
    let mut scope = self.heap.scope();
    scope.define_property(obj, key, desc)?;
    Ok(())
  }

  fn alloc_native_function(
    &mut self,
    f: fn(&mut JsWptRuntime, Value, &[Value]) -> Result<Value, JsError>,
  ) -> Result<GcObject, JsError> {
    let func_obj = self.alloc_object()?;
    self.callables.insert(func_obj, Rc::new(Callable::Native(f)));
    Ok(func_obj)
  }

  fn alloc_user_function(&mut self, func: UserFunction) -> Result<GcObject, JsError> {
    let func_obj = self.alloc_object()?;
    self.callables.insert(func_obj, Rc::new(Callable::User(func)));
    Ok(func_obj)
  }

  fn is_callable_value(&self, value: Value) -> bool {
    matches!(value, Value::Object(obj) if self.callables.contains_key(&obj))
  }

  fn promise_prototype(&self) -> Result<GcObject, JsError> {
    self.promise_prototype.ok_or_else(|| JsError::Vm(VmError::Unimplemented("Promise prototype")))
  }

  fn promise_job_runner(&self) -> Result<GcObject, JsError> {
    self
      .promise_job_runner
      .ok_or_else(|| JsError::Vm(VmError::Unimplemented("Promise job runner")))
  }

  fn alloc_promise_with_state(&mut self, status: PromiseStatus, value: Value) -> Result<GcObject, JsError> {
    let obj = self.alloc_object()?;
    let proto = self.promise_prototype()?;
    self.heap.object_set_prototype(obj, Some(proto))?;
    self.promises.insert(
      obj,
      PromiseState {
        status,
        value,
        reactions: Vec::new(),
      },
    );
    Ok(obj)
  }

  fn enqueue_promise_job(&mut self, job: PromiseJob) -> Result<(), JsError> {
    let id = self.next_promise_job_id;
    self.next_promise_job_id = self.next_promise_job_id.wrapping_add(1);
    self.promise_jobs.insert(id, job);

    let runner = self.promise_job_runner()?;
    self.event_loop.queue_microtask(
      Value::Object(runner),
      Value::Undefined,
      vec![Value::Number(id as f64)],
    );
    Ok(())
  }

  fn settle_promise(&mut self, promise: GcObject, status: PromiseStatus, value: Value) -> Result<(), JsError> {
    let Some(state) = self.promises.get_mut(&promise) else {
      return Err(JsError::Vm(VmError::Throw(self.alloc_string_value("TypeError: not a Promise")?)));
    };
    if state.status != PromiseStatus::Pending {
      return Ok(());
    }
    state.status = status;
    state.value = value;

    let reactions = std::mem::take(&mut state.reactions);
    for reaction in reactions {
      self.enqueue_promise_job(PromiseJob {
        status,
        value,
        reaction,
      })?;
    }
    Ok(())
  }

  fn reported(&self) -> bool {
    self.report.is_some()
  }

  fn set_report_error(&mut self, message: &str) {
    if self.report.is_some() {
      return;
    }
    self.report = Some(WptReport {
      file_status: "error".to_string(),
      harness_status: "error".to_string(),
      message: Some(message.to_string()),
      stack: None,
      subtests: Vec::new(),
    });
  }

  fn exec_script(&mut self, source: &str) -> Result<Value, JsError> {
    let opts = ParseOptions {
      dialect: Dialect::Ecma,
      source_type: SourceType::Script,
    };
    let mut top =
      parse_with_options(source, opts).map_err(|err| JsError::Parse(err.to_string()))?;

    self.hoist_script_functions(&mut top.stx.body)?;
    self.hoist_var_decls(&top.stx.body)?;
    self.eval_stmt_list(&top.stx.body)
  }

  fn hoist_script_functions(&mut self, stmts: &mut [Node<Stmt>]) -> Result<(), JsError> {
    for stmt in stmts.iter_mut() {
      let Stmt::FunctionDecl(func_decl) = &mut *stmt.stx else {
        continue;
      };

      let Some(name_node) = func_decl.stx.name.as_ref() else {
        continue;
      };
      let name = name_node.stx.name.clone();

      let params = func_decl
        .stx
        .function
        .stx
        .parameters
        .iter()
        .filter_map(|p| simple_binding_identifier(&p.stx.pattern.stx).ok().flatten())
        .map(|s| s.to_string())
        .collect::<Vec<_>>();

      let Some(body) = func_decl.stx.function.stx.body.take() else {
        continue;
      };
      let FuncBody::Block(body_stmts) = body else {
        return Err(JsError::Vm(VmError::Unimplemented("arrow bodies not supported")));
      };

      let func_obj = self.alloc_user_function(UserFunction {
        params,
        body: body_stmts,
      })?;
      self.env.set(&name, Value::Object(func_obj));
    }
    Ok(())
  }

  fn hoist_var_decls(&mut self, stmts: &[Node<Stmt>]) -> Result<(), JsError> {
    let mut names = HashSet::<String>::new();
    for stmt in stmts {
      self.collect_var_names(&stmt.stx, &mut names)?;
    }
    for name in names {
      self.env.declare_var(&name);
    }
    Ok(())
  }

  fn collect_var_names(&self, stmt: &Stmt, out: &mut HashSet<String>) -> Result<(), JsError> {
    match stmt {
      Stmt::VarDecl(var) => {
        if var.stx.mode != VarDeclMode::Var {
          return Ok(());
        }
        for decl in &var.stx.declarators {
          if let Some(name) = simple_binding_identifier(&decl.pattern.stx)? {
            out.insert(name.to_string());
          }
        }
      }
      Stmt::Block(block) => {
        for stmt in &block.stx.body {
          self.collect_var_names(&stmt.stx, out)?;
        }
      }
      Stmt::If(stmt) => {
        self.collect_var_names(&stmt.stx.consequent.stx, out)?;
        if let Some(alt) = &stmt.stx.alternate {
          self.collect_var_names(&alt.stx, out)?;
        }
      }
      Stmt::Try(stmt) => {
        for s in &stmt.stx.wrapped.stx.body {
          self.collect_var_names(&s.stx, out)?;
        }
        if let Some(catch) = &stmt.stx.catch {
          for s in &catch.stx.body {
            self.collect_var_names(&s.stx, out)?;
          }
        }
        if let Some(finally) = &stmt.stx.finally {
          for s in &finally.stx.body {
            self.collect_var_names(&s.stx, out)?;
          }
        }
      }
      Stmt::While(stmt) => self.collect_var_names(&stmt.stx.body.stx, out)?,
      Stmt::DoWhile(stmt) => self.collect_var_names(&stmt.stx.body.stx, out)?,
      Stmt::ForTriple(stmt) => {
        if let parse_js::ast::stmt::ForTripleStmtInit::Decl(decl) = &stmt.stx.init {
          if decl.stx.mode == VarDeclMode::Var {
            for d in &decl.stx.declarators {
              if let Some(name) = simple_binding_identifier(&d.pattern.stx)? {
                out.insert(name.to_string());
              }
            }
          }
        }
        for s in &stmt.stx.body.stx.body {
          self.collect_var_names(&s.stx, out)?;
        }
      }
      Stmt::FunctionDecl(_) => {}
      _ => {}
    }
    Ok(())
  }

  fn eval_stmt_list(&mut self, stmts: &[Node<Stmt>]) -> Result<Value, JsError> {
    let mut last_value = Value::Undefined;
    for stmt in stmts {
      match self.eval_stmt(stmt)? {
        Completion::Normal(v) => {
          if let Some(v) = v {
            last_value = v;
          }
        }
        Completion::Throw(v) => return Err(JsError::Vm(VmError::Throw(v))),
        Completion::Return(v) => return Ok(v),
        Completion::Break(..) => return Err(JsError::Vm(VmError::Unimplemented("break"))),
        Completion::Continue(..) => return Err(JsError::Vm(VmError::Unimplemented("continue"))),
      }
    }
    Ok(last_value)
  }

  fn eval_stmt(&mut self, stmt: &Node<Stmt>) -> Result<Completion, JsError> {
    self.vm.tick()?;

    match &*stmt.stx {
      Stmt::Empty(_) => Ok(Completion::empty()),
      Stmt::Expr(expr_stmt) => self.eval_expr_stmt(&expr_stmt.stx),
      Stmt::VarDecl(var_decl) => self.eval_var_decl(&var_decl.stx),
      Stmt::Block(block) => self.eval_block_stmt(&block.stx),
      Stmt::If(stmt) => self.eval_if(&stmt.stx),
      Stmt::Throw(stmt) => self.eval_throw(&stmt.stx),
      Stmt::Try(stmt) => self.eval_try(&stmt.stx),
      Stmt::Return(stmt) => self.eval_return(&stmt.stx),
      Stmt::While(stmt) => self.eval_while(&stmt.stx),
      Stmt::DoWhile(stmt) => self.eval_do_while(&stmt.stx),
      Stmt::ForTriple(stmt) => self.eval_for_triple(&stmt.stx),
      Stmt::Break(stmt) => {
        if stmt.stx.label.is_some() {
          return Err(JsError::Vm(VmError::Unimplemented("labelled break")));
        }
        Ok(Completion::Break(None, None))
      }
      Stmt::Continue(stmt) => {
        if stmt.stx.label.is_some() {
          return Err(JsError::Vm(VmError::Unimplemented("labelled continue")));
        }
        Ok(Completion::Continue(None, None))
      }
      Stmt::FunctionDecl(_) => Ok(Completion::empty()),
      _ => Err(JsError::Vm(VmError::Unimplemented("statement type"))),
    }
  }

  fn eval_block_stmt(&mut self, block: &BlockStmt) -> Result<Completion, JsError> {
    self.env.push_lexical();
    let result = (|| {
      for stmt in &block.body {
        let completion = self.eval_stmt(stmt)?;
        if completion.is_abrupt() {
          return Ok(completion);
        }
      }
      Ok(Completion::empty())
    })();
    self.env.pop_lexical();
    result
  }

  fn eval_expr_stmt(&mut self, stmt: &ExprStmt) -> Result<Completion, JsError> {
    let value = self.eval_expr(&stmt.expr)?;
    Ok(Completion::normal(value))
  }

  fn eval_var_decl(&mut self, decl: &VarDecl) -> Result<Completion, JsError> {
    match decl.mode {
      VarDeclMode::Var => {
        for declarator in &decl.declarators {
          let Some(init) = &declarator.initializer else {
            continue;
          };
          let Some(name) = simple_binding_identifier(&declarator.pattern.stx)? else {
            continue;
          };
          let value = self.eval_expr(init)?;
          self.env.set(name, value);
        }
        Ok(Completion::empty())
      }
      VarDeclMode::Let | VarDeclMode::Const => {
        for declarator in &decl.declarators {
          let Some(name) = simple_binding_identifier(&declarator.pattern.stx)? else {
            continue;
          };
          let value = match &declarator.initializer {
            Some(init) => self.eval_expr(init)?,
            None => Value::Undefined,
          };
          self.env.declare_lexical(name, value);
        }
        Ok(Completion::empty())
      }
      _ => Err(JsError::Vm(VmError::Unimplemented("var declaration kind"))),
    }
  }

  fn eval_if(&mut self, stmt: &IfStmt) -> Result<Completion, JsError> {
    let test = self.eval_expr(&stmt.test)?;
    if to_boolean(&mut self.heap, test)? {
      self.eval_stmt(&stmt.consequent)
    } else if let Some(alt) = &stmt.alternate {
      self.eval_stmt(alt)
    } else {
      Ok(Completion::empty())
    }
  }

  fn eval_throw(&mut self, stmt: &ThrowStmt) -> Result<Completion, JsError> {
    let value = self.eval_expr(&stmt.value)?;
    Ok(Completion::Throw(value))
  }

  fn eval_try(&mut self, stmt: &TryStmt) -> Result<Completion, JsError> {
    let mut result = self.eval_block_stmt(&stmt.wrapped.stx)?;

    if matches!(result, Completion::Throw(_)) {
      if let Some(catch) = &stmt.catch {
        let thrown = match result {
          Completion::Throw(v) => v,
          _ => unreachable!(),
        };
        result = self.eval_catch(&catch.stx, thrown)?;
      }
    }

    if let Some(finally) = &stmt.finally {
      let finally_result = self.eval_block_stmt(&finally.stx)?;
      if finally_result.is_abrupt() {
        return Ok(finally_result);
      }
    }

    Ok(result)
  }

  fn eval_catch(&mut self, catch: &CatchBlock, thrown: Value) -> Result<Completion, JsError> {
    self.env.push_lexical();
    if let Some(param) = &catch.parameter {
      if let Some(name) = simple_binding_identifier(&param.stx)? {
        self.env.declare_lexical(name, thrown);
      }
    }

    let result = (|| {
      for stmt in &catch.body {
        let completion = self.eval_stmt(stmt)?;
        if completion.is_abrupt() {
          return Ok(completion);
        }
      }
      Ok(Completion::empty())
    })();
    self.env.pop_lexical();
    result
  }

  fn eval_return(&mut self, stmt: &ReturnStmt) -> Result<Completion, JsError> {
    let value = match &stmt.value {
      Some(expr) => self.eval_expr(expr)?,
      None => Value::Undefined,
    };
    Ok(Completion::Return(value))
  }

  fn eval_while(&mut self, stmt: &WhileStmt) -> Result<Completion, JsError> {
    loop {
      self.vm.tick()?;
      let test = self.eval_expr(&stmt.condition)?;
      if !to_boolean(&mut self.heap, test)? {
        break;
      }

      match self.eval_stmt(&stmt.body)? {
        Completion::Normal(_) => {}
        Completion::Continue(None, _) => continue,
        Completion::Break(None, _) => break,
        other => return Ok(other),
      }
    }
    Ok(Completion::empty())
  }

  fn eval_do_while(&mut self, stmt: &DoWhileStmt) -> Result<Completion, JsError> {
    loop {
      self.vm.tick()?;
      match self.eval_stmt(&stmt.body)? {
        Completion::Normal(_) => {}
        Completion::Continue(None, _) => {}
        Completion::Break(None, _) => break,
        other => return Ok(other),
      }

      let test = self.eval_expr(&stmt.condition)?;
      if !to_boolean(&mut self.heap, test)? {
        break;
      }
    }
    Ok(Completion::empty())
  }

  fn eval_for_triple(&mut self, stmt: &ForTripleStmt) -> Result<Completion, JsError> {
    match &stmt.init {
      parse_js::ast::stmt::ForTripleStmtInit::None => {}
      parse_js::ast::stmt::ForTripleStmtInit::Expr(expr) => {
        let _ = self.eval_expr(expr)?;
      }
      parse_js::ast::stmt::ForTripleStmtInit::Decl(decl) => {
        let _ = self.eval_var_decl(&decl.stx)?;
      }
    }

    loop {
      self.vm.tick()?;

      if let Some(cond) = &stmt.cond {
        let test = self.eval_expr(cond)?;
        if !to_boolean(&mut self.heap, test)? {
          break;
        }
      }

      match self.eval_for_body(&stmt.body.stx)? {
        Completion::Normal(_) => {}
        Completion::Continue(None, _) => {}
        Completion::Break(None, _) => break,
        other => return Ok(other),
      }

      if let Some(post) = &stmt.post {
        let _ = self.eval_expr(post)?;
      }
    }

    Ok(Completion::empty())
  }

  fn eval_for_body(&mut self, body: &ForBody) -> Result<Completion, JsError> {
    for stmt in &body.body {
      let completion = self.eval_stmt(stmt)?;
      if completion.is_abrupt() {
        return Ok(completion);
      }
    }
    Ok(Completion::empty())
  }

  fn eval_expr(&mut self, expr: &Node<Expr>) -> Result<Value, JsError> {
    match &*expr.stx {
      Expr::LitStr(node) => self.eval_lit_str(&node.stx),
      Expr::LitNum(node) => self.eval_lit_num(&node.stx),
      Expr::LitBool(node) => self.eval_lit_bool(&node.stx),
      Expr::LitNull(_node) => self.eval_lit_null(),
      Expr::Id(node) => self.eval_id(&node.stx),
      Expr::Binary(node) => self.eval_binary(&node.stx),
      Expr::Member(node) => self.eval_member(&node.stx),
      Expr::ComputedMember(node) => self.eval_computed_member(&node.stx),
      Expr::Call(node) => self.eval_call(&node.stx),
      Expr::Unary(node) => self.eval_unary(&node.stx),
      Expr::LitObj(node) => self.eval_lit_obj(&node.stx),
      Expr::IdPat(node) => self.eval_id_pat(&node.stx),
      _ => Err(JsError::Vm(VmError::Unimplemented("expression type"))),
    }
  }

  fn eval_lit_str(&mut self, expr: &LitStrExpr) -> Result<Value, JsError> {
    self.alloc_string_value(&expr.value)
  }

  fn eval_lit_num(&self, expr: &LitNumExpr) -> Result<Value, JsError> {
    Ok(Value::Number(expr.value.0))
  }

  fn eval_lit_bool(&self, expr: &LitBoolExpr) -> Result<Value, JsError> {
    Ok(Value::Bool(expr.value))
  }

  fn eval_lit_null(&self) -> Result<Value, JsError> {
    Ok(Value::Null)
  }

  fn eval_id(&self, expr: &IdExpr) -> Result<Value, JsError> {
    self
      .env
      .get(&expr.name)
      .ok_or_else(|| JsError::Vm(VmError::Unimplemented("unbound identifier")))
  }

  fn eval_id_pat(&self, expr: &parse_js::ast::expr::pat::IdPat) -> Result<Value, JsError> {
    self
      .env
      .get(&expr.name)
      .ok_or_else(|| JsError::Vm(VmError::Unimplemented("unbound identifier")))
  }

  fn eval_member(&mut self, expr: &MemberExpr) -> Result<Value, JsError> {
    let obj = self.eval_expr(&expr.left)?;
    let Value::Object(obj) = obj else {
      return Err(JsError::Vm(VmError::Unimplemented("member access on non-object")));
    };
    let key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string(&expr.right)?)
    };
    let Some(desc) = self.heap.get_property(obj, &key)? else {
      return Ok(Value::Undefined);
    };
    match desc.kind {
      PropertyKind::Data { value, .. } => Ok(value),
      PropertyKind::Accessor { .. } => Err(JsError::Vm(VmError::Unimplemented("accessor props"))),
    }
  }

  fn eval_computed_member(&mut self, expr: &ComputedMemberExpr) -> Result<Value, JsError> {
    let obj = self.eval_expr(&expr.object)?;
    let Value::Object(obj) = obj else {
      return Err(JsError::Vm(VmError::Unimplemented(
        "computed member access on non-object",
      )));
    };
    let member = self.eval_expr(&expr.member)?;
    let key = self.value_to_property_key(member)?;
    let Some(desc) = self.heap.get_property(obj, &key)? else {
      return Ok(Value::Undefined);
    };
    match desc.kind {
      PropertyKind::Data { value, .. } => Ok(value),
      PropertyKind::Accessor { .. } => Err(JsError::Vm(VmError::Unimplemented("accessor props"))),
    }
  }

  fn eval_call(&mut self, expr: &CallExpr) -> Result<Value, JsError> {
    let mut args = Vec::with_capacity(expr.arguments.len());
    for arg in &expr.arguments {
      if arg.stx.spread {
        return Err(JsError::Vm(VmError::Unimplemented("spread args")));
      }
      args.push(self.eval_expr(&arg.stx.value)?);
    }

    let (callee, this) = self.eval_callee(&expr.callee)?;
    self.call(callee, this, &args)
  }

  fn eval_callee(&mut self, expr: &Node<Expr>) -> Result<(Value, Value), JsError> {
    match &*expr.stx {
      Expr::Member(member) => {
        let this = self.eval_expr(&member.stx.left)?;
        let Value::Object(obj) = this else {
          return Err(JsError::Vm(VmError::NotCallable));
        };
        let key = {
          let mut scope = self.heap.scope();
          PropertyKey::from_string(scope.alloc_string(&member.stx.right)?)
        };
        let value = match self.heap.get_property(obj, &key)? {
          Some(desc) => match desc.kind {
            PropertyKind::Data { value, .. } => value,
            PropertyKind::Accessor { .. } => {
              return Err(JsError::Vm(VmError::Unimplemented("accessor props")))
            }
          },
          None => Value::Undefined,
        };
        Ok((value, Value::Object(obj)))
      }
      Expr::ComputedMember(member) => {
        let this = self.eval_expr(&member.stx.object)?;
        let Value::Object(obj) = this else {
          return Err(JsError::Vm(VmError::NotCallable));
        };
        let member_value = self.eval_expr(&member.stx.member)?;
        let key = self.value_to_property_key(member_value)?;
        let value = match self.heap.get_property(obj, &key)? {
          Some(desc) => match desc.kind {
            PropertyKind::Data { value, .. } => value,
            PropertyKind::Accessor { .. } => {
              return Err(JsError::Vm(VmError::Unimplemented("accessor props")))
            }
          },
          None => Value::Undefined,
        };
        Ok((value, Value::Object(obj)))
      }
      _ => Ok((self.eval_expr(expr)?, self.this_binding)),
    }
  }

  fn call(&mut self, callee: Value, this: Value, args: &[Value]) -> Result<Value, JsError> {
    let Value::Object(obj) = callee else {
      return Err(JsError::Vm(VmError::NotCallable));
    };
    let Some(callable) = self.callables.get(&obj).cloned() else {
      return Err(JsError::Vm(VmError::NotCallable));
    };

    let previous_this = self.this_binding;
    self.this_binding = this;

    let result = match &*callable {
      Callable::Native(f) => f(self, this, args),
      Callable::User(func) => self.call_user_function(this, args, func),
    };

    self.this_binding = previous_this;
    result
  }

  fn call_user_function(&mut self, this: Value, args: &[Value], func: &UserFunction) -> Result<Value, JsError> {
    self.env.push_frame();
    let previous_this = self.this_binding;
    self.this_binding = this;

    let result = (|| -> Result<Value, JsError> {
      for (idx, name) in func.params.iter().enumerate() {
        self.env.declare_var(name);
        let value = args.get(idx).copied().unwrap_or(Value::Undefined);
        self.env.set(name, value);
      }
      self.hoist_var_decls(&func.body)?;
      self.eval_stmt_list(&func.body)
    })();

    self.this_binding = previous_this;
    self.env.pop_frame();
    result
  }

  fn eval_unary(&mut self, expr: &UnaryExpr) -> Result<Value, JsError> {
    match expr.operator {
      OperatorName::LogicalNot => {
        let arg = self.eval_expr(&expr.argument)?;
        Ok(Value::Bool(!to_boolean(&mut self.heap, arg)?))
      }
      OperatorName::New => self.eval_new(&expr.argument),
      _ => Err(JsError::Vm(VmError::Unimplemented("unary operator"))),
    }
  }

  fn eval_new(&mut self, operand: &Node<Expr>) -> Result<Value, JsError> {
    let (ctor, args) = match &*operand.stx {
      Expr::Call(call) => {
        let mut args = Vec::with_capacity(call.stx.arguments.len());
        for arg in &call.stx.arguments {
          if arg.stx.spread {
            return Err(JsError::Vm(VmError::Unimplemented("spread args")));
          }
          args.push(self.eval_expr(&arg.stx.value)?);
        }

        let ctor = self.eval_expr(&call.stx.callee)?;
        (ctor, args)
      }
      _ => (self.eval_expr(operand)?, Vec::new()),
    };

    self.construct(ctor, &args)
  }

  fn construct(&mut self, ctor: Value, args: &[Value]) -> Result<Value, JsError> {
    let Value::Object(ctor_obj) = ctor else {
      return Err(JsError::Vm(VmError::NotCallable));
    };
    if !self.callables.contains_key(&ctor_obj) {
      return Err(JsError::Vm(VmError::NotCallable));
    }

    let instance = self.alloc_object()?;

    let prototype_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("prototype")?)
    };
    if let Some(desc) = self.heap.get_property(ctor_obj, &prototype_key)? {
      if let PropertyKind::Data { value, .. } = desc.kind {
        if let Value::Object(proto_obj) = value {
          self.heap.object_set_prototype(instance, Some(proto_obj))?;
        }
      }
    }

    let result = self.call(ctor, Value::Object(instance), args)?;
    Ok(match result {
      Value::Object(_) => result,
      _ => Value::Object(instance),
    })
  }

  fn eval_lit_obj(&mut self, expr: &LitObjExpr) -> Result<Value, JsError> {
    let obj = self.alloc_object()?;
    for member in &expr.members {
      match &member.stx.typ {
        ObjMemberType::Valued { key, val } => {
          let ClassOrObjKey::Direct(key) = key else {
            return Err(JsError::Vm(VmError::Unimplemented("computed object key")));
          };
          let ClassOrObjVal::Prop(Some(expr)) = val else {
            return Err(JsError::Vm(VmError::Unimplemented("object member kind")));
          };
          let key_str = key.stx.key.as_str();
          let value = self.eval_expr(expr)?;
          let key = {
            let mut scope = self.heap.scope();
            PropertyKey::from_string(scope.alloc_string(key_str)?)
          };
          self.define_data_prop(obj, key, value)?;
        }
        ObjMemberType::Shorthand { .. } => {
          return Err(JsError::Vm(VmError::Unimplemented("object shorthand")))
        }
        ObjMemberType::Rest { .. } => return Err(JsError::Vm(VmError::Unimplemented("object rest"))),
      }
    }
    Ok(Value::Object(obj))
  }

  fn eval_binary(&mut self, expr: &BinaryExpr) -> Result<Value, JsError> {
    match expr.operator {
      OperatorName::StrictEquality => {
        let left = self.eval_expr(&expr.left)?;
        let right = self.eval_expr(&expr.right)?;
        Ok(Value::Bool(strict_equal(&mut self.heap, left, right)?))
      }
      OperatorName::StrictInequality => {
        let left = self.eval_expr(&expr.left)?;
        let right = self.eval_expr(&expr.right)?;
        Ok(Value::Bool(!strict_equal(&mut self.heap, left, right)?))
      }
      OperatorName::Assignment => {
        let value = self.eval_expr(&expr.right)?;
        self.assign_to(&expr.left, value)?;
        Ok(value)
      }
      _ => Err(JsError::Vm(VmError::Unimplemented("binary operator"))),
    }
  }

  fn assign_to(&mut self, target: &Node<Expr>, value: Value) -> Result<(), JsError> {
    match &*target.stx {
      Expr::Id(id) => {
        self.env.set(&id.stx.name, value);
        Ok(())
      }
      Expr::IdPat(id) => {
        self.env.set(&id.stx.name, value);
        Ok(())
      }
      Expr::Member(member) => {
        let obj_value = self.eval_expr(&member.stx.left)?;
        let Value::Object(obj) = obj_value else {
          return Err(JsError::Vm(VmError::Unimplemented(
            "assignment to member of non-object",
          )));
        };
        let key = {
          let mut scope = self.heap.scope();
          PropertyKey::from_string(scope.alloc_string(&member.stx.right)?)
        };
        self.define_data_prop(obj, key, value)?;
        Ok(())
      }
      Expr::ComputedMember(member) => {
        let obj_value = self.eval_expr(&member.stx.object)?;
        let Value::Object(obj) = obj_value else {
          return Err(JsError::Vm(VmError::Unimplemented(
            "assignment to computed member of non-object",
          )));
        };
        let member_value = self.eval_expr(&member.stx.member)?;
        let key = self.value_to_property_key(member_value)?;
        self.define_data_prop(obj, key, value)?;
        Ok(())
      }
      _ => Err(JsError::Vm(VmError::Unimplemented("assignment target"))),
    }
  }

  fn value_to_property_key(&mut self, value: Value) -> Result<PropertyKey, JsError> {
    Ok(self.heap.to_property_key(value)?)
  }

  fn value_to_string_lossy(&mut self, value: Value) -> String {
    match value {
      Value::Symbol(_) => "[symbol]".to_string(),
      Value::Object(_) => "[object]".to_string(),
      _ => {
        self
          .heap
          .to_string(value)
          .and_then(|s| self.heap.get_string(s).map(|s| s.to_utf8_lossy()))
          .unwrap_or_else(|_| "<invalid string>".to_string())
      }
    }
  }
}

fn simple_binding_identifier<'a>(pat_decl: &'a PatDecl) -> Result<Option<&'a str>, JsError> {
  match &*pat_decl.pat.stx {
    parse_js::ast::expr::pat::Pat::Id(id) => Ok(Some(&id.stx.name)),
    _ => Err(JsError::Vm(VmError::Unimplemented(
      "destructuring patterns are not supported",
    ))),
  }
}

fn to_boolean(heap: &mut Heap, value: Value) -> Result<bool, JsError> {
  Ok(match value {
    Value::Undefined | Value::Null => false,
    Value::Bool(b) => b,
    Value::Number(n) => n != 0.0 && !n.is_nan(),
    Value::BigInt(b) => !b.is_zero(),
    Value::String(s) => !heap.get_string(s)?.as_code_units().is_empty(),
    Value::Symbol(_) | Value::Object(_) => true,
  })
}

fn strict_equal(heap: &mut Heap, a: Value, b: Value) -> Result<bool, JsError> {
  Ok(match (a, b) {
    (Value::Undefined, Value::Undefined) => true,
    (Value::Null, Value::Null) => true,
    (Value::Bool(x), Value::Bool(y)) => x == y,
    (Value::Number(x), Value::Number(y)) => x == y,
    (Value::BigInt(x), Value::BigInt(y)) => x == y,
    (Value::String(x), Value::String(y)) => heap.get_string(x)? == heap.get_string(y)?,
    (Value::Symbol(x), Value::Symbol(y)) => x == y,
    (Value::Object(x), Value::Object(y)) => x == y,
    _ => false,
  })
}

#[derive(Clone, Debug, PartialEq)]
enum Completion {
  Normal(Option<Value>),
  Throw(Value),
  Return(Value),
  Break(Option<String>, Option<Value>),
  Continue(Option<String>, Option<Value>),
}

impl Completion {
  fn empty() -> Self {
    Completion::Normal(None)
  }

  fn normal(value: Value) -> Self {
    Completion::Normal(Some(value))
  }

  fn is_abrupt(&self) -> bool {
    !matches!(self, Completion::Normal(_))
  }
}

fn native_console_log(rt: &mut JsWptRuntime, _this: Value, args: &[Value]) -> Result<Value, JsError> {
  let parts = args
    .iter()
    .copied()
    .map(|v| rt.value_to_string_lossy(v))
    .collect::<Vec<_>>();
  eprintln!("[wpt] {}", parts.join(" "));
  Ok(Value::Undefined)
}

fn native_set_timeout(rt: &mut JsWptRuntime, _this: Value, args: &[Value]) -> Result<Value, JsError> {
  let callback = args.get(0).copied().unwrap_or(Value::Undefined);
  let delay_value = args.get(1).copied().unwrap_or(Value::Number(0.0));
  let mut delay = rt.heap.to_number(delay_value)?;
  if !delay.is_finite() {
    delay = 0.0;
  }
  if delay < 0.0 {
    delay = 0.0;
  }
  let delay_ms = delay.trunc() as u64;

  let Value::Object(obj) = callback else {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
      "TypeError: setTimeout callback is not callable",
    )?)));
  };
  if !rt.callables.contains_key(&obj) {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
      "TypeError: setTimeout callback is not callable",
    )?)));
  }

  let extra_args = args.get(2..).unwrap_or(&[]).to_vec();
  let id = rt.event_loop.set_timeout(
    callback,
    Value::Object(rt.global_object),
    Duration::from_millis(delay_ms),
    extra_args,
  );
  Ok(Value::Number(id as f64))
}

fn native_set_interval(rt: &mut JsWptRuntime, _this: Value, args: &[Value]) -> Result<Value, JsError> {
  let callback = args.get(0).copied().unwrap_or(Value::Undefined);
  let interval_value = args.get(1).copied().unwrap_or(Value::Number(0.0));
  let mut interval = rt.heap.to_number(interval_value)?;
  if !interval.is_finite() {
    interval = 0.0;
  }
  if interval < 0.0 {
    interval = 0.0;
  }
  let interval_ms = interval.trunc() as u64;

  let Value::Object(obj) = callback else {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
      "TypeError: setInterval callback is not callable",
    )?)));
  };
  if !rt.callables.contains_key(&obj) {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
      "TypeError: setInterval callback is not callable",
    )?)));
  }

  let extra_args = args.get(2..).unwrap_or(&[]).to_vec();
  let id = rt.event_loop.set_interval(
    callback,
    Value::Object(rt.global_object),
    Duration::from_millis(interval_ms),
    extra_args,
  );
  Ok(Value::Number(id as f64))
}

fn native_clear_timeout(rt: &mut JsWptRuntime, _this: Value, args: &[Value]) -> Result<Value, JsError> {
  let id_value = args.get(0).copied().unwrap_or(Value::Number(0.0));
  let mut id = rt.heap.to_number(id_value)?;
  if !id.is_finite() {
    id = 0.0;
  }
  let id = id.trunc() as i32;
  rt.event_loop.clear_timeout(id);
  Ok(Value::Undefined)
}

fn native_clear_interval(rt: &mut JsWptRuntime, this: Value, args: &[Value]) -> Result<Value, JsError> {
  // In HTML, `clearInterval` and `clearTimeout` share the same timer ID space.
  native_clear_timeout(rt, this, args)
}

fn string_from_value(rt: &mut JsWptRuntime, value: Value) -> Result<String, JsError> {
  match value {
    Value::String(s) => Ok(rt.heap.get_string(s)?.to_utf8_lossy()),
    Value::Undefined => Ok("undefined".to_string()),
    Value::Null => Ok("null".to_string()),
    Value::Bool(true) => Ok("true".to_string()),
    Value::Bool(false) => Ok("false".to_string()),
    Value::Number(n) => Ok(n.to_string()),
    Value::BigInt(b) => Ok(b.to_decimal_string()),
    Value::Symbol(_) => Ok("[symbol]".to_string()),
    Value::Object(_) => Ok("[object]".to_string()),
  }
}

fn get_bool_option(rt: &mut JsWptRuntime, obj: GcObject, name: &str) -> Result<bool, JsError> {
  let key = {
    let mut scope = rt.heap.scope();
    PropertyKey::from_string(scope.alloc_string(name)?)
  };
  let Some(desc) = rt.heap.get_property(obj, &key)? else {
    return Ok(false);
  };
  match desc.kind {
    PropertyKind::Data { value, .. } => Ok(to_boolean(&mut rt.heap, value)?),
    PropertyKind::Accessor { .. } => Err(JsError::Vm(VmError::Unimplemented("accessor props"))),
  }
}

fn parse_listener_options(rt: &mut JsWptRuntime, value: Value) -> Result<ListenerOptions, JsError> {
  Ok(match value {
    Value::Bool(capture) => ListenerOptions {
      capture,
      once: false,
      passive: false,
    },
    Value::Object(obj) => ListenerOptions {
      capture: get_bool_option(rt, obj, "capture")?,
      once: get_bool_option(rt, obj, "once")?,
      passive: get_bool_option(rt, obj, "passive")?,
    },
    _ => ListenerOptions {
      capture: false,
      once: false,
      passive: false,
    },
  })
}

fn native_eventtarget_ctor(rt: &mut JsWptRuntime, this: Value, args: &[Value]) -> Result<Value, JsError> {
  let Value::Object(obj) = this else {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
      "TypeError: EventTarget constructor called without a valid this",
    )?)));
  };

  let parent = match args.get(0).copied().unwrap_or(Value::Undefined) {
    Value::Object(parent) if rt.event_targets.contains_key(&parent) => Some(parent),
    _ => None,
  };

  rt.event_targets.insert(
    obj,
    EventTargetState {
      parent,
      listeners: HashMap::new(),
    },
  );

  Ok(Value::Undefined)
}

fn native_eventtarget_add_event_listener(
  rt: &mut JsWptRuntime,
  this: Value,
  args: &[Value],
) -> Result<Value, JsError> {
  let Value::Object(target) = this else {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
      "TypeError: addEventListener called on non-EventTarget",
    )?)));
  };

  let typ = string_from_value(rt, args.get(0).copied().unwrap_or(Value::Undefined))?;
  let callback = args.get(1).copied().unwrap_or(Value::Undefined);
  if matches!(callback, Value::Undefined | Value::Null) {
    return Ok(Value::Undefined);
  }
  if !rt.is_callable_value(callback) {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
      "TypeError: addEventListener callback is not callable",
    )?)));
  }

  let options = parse_listener_options(rt, args.get(2).copied().unwrap_or(Value::Undefined))?;

  let Some(state) = rt.event_targets.get_mut(&target) else {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
      "TypeError: addEventListener called on non-EventTarget",
    )?)));
  };
  let list = state.listeners.entry(typ).or_default();
  if list.iter().any(|rec| rec.callback == callback && rec.options.capture == options.capture) {
    return Ok(Value::Undefined);
  }
  list.push(EventListener { callback, options });
  Ok(Value::Undefined)
}

fn native_eventtarget_remove_event_listener(
  rt: &mut JsWptRuntime,
  this: Value,
  args: &[Value],
) -> Result<Value, JsError> {
  let Value::Object(target) = this else {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
      "TypeError: removeEventListener called on non-EventTarget",
    )?)));
  };
  let typ = string_from_value(rt, args.get(0).copied().unwrap_or(Value::Undefined))?;
  let callback = args.get(1).copied().unwrap_or(Value::Undefined);
  if matches!(callback, Value::Undefined | Value::Null) {
    return Ok(Value::Undefined);
  }

  let options = parse_listener_options(rt, args.get(2).copied().unwrap_or(Value::Undefined))?;

  let Some(state) = rt.event_targets.get_mut(&target) else {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
      "TypeError: removeEventListener called on non-EventTarget",
    )?)));
  };
  if let Some(list) = state.listeners.get_mut(&typ) {
    list.retain(|rec| !(rec.callback == callback && rec.options.capture == options.capture));
    if list.is_empty() {
      state.listeners.remove(&typ);
    }
  }

  Ok(Value::Undefined)
}

fn native_event_ctor(rt: &mut JsWptRuntime, this: Value, args: &[Value]) -> Result<Value, JsError> {
  let Value::Object(obj) = this else {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
      "TypeError: Event constructor called without a valid this",
    )?)));
  };

  let type_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let typ = string_from_value(rt, type_value)?;

  let mut bubbles = false;
  let mut cancelable = false;
  if let Some(Value::Object(init)) = args.get(1).copied() {
    bubbles = get_bool_option(rt, init, "bubbles")?;
    cancelable = get_bool_option(rt, init, "cancelable")?;
  }

  rt.events.insert(
    obj,
    EventState {
      typ,
      bubbles,
      cancelable,
      default_prevented: false,
      propagation_stopped: false,
      immediate_stopped: false,
      in_passive_listener: false,
    },
  );

  // Initialize common properties.
  let type_key = {
    let mut scope = rt.heap.scope();
    PropertyKey::from_string(scope.alloc_string("type")?)
  };
  rt.define_data_prop(obj, type_key, type_value)?;

  let bubbles_key = {
    let mut scope = rt.heap.scope();
    PropertyKey::from_string(scope.alloc_string("bubbles")?)
  };
  rt.define_data_prop(obj, bubbles_key, Value::Bool(bubbles))?;

  let cancelable_key = {
    let mut scope = rt.heap.scope();
    PropertyKey::from_string(scope.alloc_string("cancelable")?)
  };
  rt.define_data_prop(obj, cancelable_key, Value::Bool(cancelable))?;

  let default_prevented_key = {
    let mut scope = rt.heap.scope();
    PropertyKey::from_string(scope.alloc_string("defaultPrevented")?)
  };
  rt.define_data_prop(obj, default_prevented_key, Value::Bool(false))?;

  let target_key = {
    let mut scope = rt.heap.scope();
    PropertyKey::from_string(scope.alloc_string("target")?)
  };
  rt.define_data_prop(obj, target_key, Value::Null)?;

  let current_target_key = {
    let mut scope = rt.heap.scope();
    PropertyKey::from_string(scope.alloc_string("currentTarget")?)
  };
  rt.define_data_prop(obj, current_target_key, Value::Null)?;

  Ok(Value::Undefined)
}

fn native_event_prevent_default(
  rt: &mut JsWptRuntime,
  this: Value,
  _args: &[Value],
) -> Result<Value, JsError> {
  let Value::Object(obj) = this else {
    return Ok(Value::Undefined);
  };
  let Some(state) = rt.events.get_mut(&obj) else {
    return Ok(Value::Undefined);
  };
  if !state.cancelable || state.in_passive_listener {
    return Ok(Value::Undefined);
  }

  state.default_prevented = true;
  let key = {
    let mut scope = rt.heap.scope();
    PropertyKey::from_string(scope.alloc_string("defaultPrevented")?)
  };
  rt.define_data_prop(obj, key, Value::Bool(true))?;
  Ok(Value::Undefined)
}

fn native_event_stop_propagation(
  rt: &mut JsWptRuntime,
  this: Value,
  _args: &[Value],
) -> Result<Value, JsError> {
  let Value::Object(obj) = this else {
    return Ok(Value::Undefined);
  };
  if let Some(state) = rt.events.get_mut(&obj) {
    state.propagation_stopped = true;
  }
  Ok(Value::Undefined)
}

fn native_event_stop_immediate_propagation(
  rt: &mut JsWptRuntime,
  this: Value,
  _args: &[Value],
) -> Result<Value, JsError> {
  let Value::Object(obj) = this else {
    return Ok(Value::Undefined);
  };
  if let Some(state) = rt.events.get_mut(&obj) {
    state.immediate_stopped = true;
    state.propagation_stopped = true;
  }
  Ok(Value::Undefined)
}

fn native_eventtarget_dispatch_event(
  rt: &mut JsWptRuntime,
  this: Value,
  args: &[Value],
) -> Result<Value, JsError> {
  let Value::Object(target) = this else {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
      "TypeError: dispatchEvent called on non-EventTarget",
    )?)));
  };
  if !rt.event_targets.contains_key(&target) {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
      "TypeError: dispatchEvent called on non-EventTarget",
    )?)));
  }

  let Value::Object(event_obj) = args.get(0).copied().unwrap_or(Value::Undefined) else {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
      "TypeError: dispatchEvent requires an Event object",
    )?)));
  };

  // Snapshot event info before dispatch so we can drop borrows while calling back into JS.
  let (event_type, bubbles) = {
    let Some(state) = rt.events.get_mut(&event_obj) else {
      return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
        "TypeError: dispatchEvent requires an Event created by this runtime",
      )?)));
    };
    state.propagation_stopped = false;
    state.immediate_stopped = false;
    state.in_passive_listener = false;
    (state.typ.clone(), state.bubbles)
  };

  // Set `event.target = this`.
  let target_key = {
    let mut scope = rt.heap.scope();
    PropertyKey::from_string(scope.alloc_string("target")?)
  };
  rt.define_data_prop(event_obj, target_key, Value::Object(target))?;

  // Build propagation path: target -> ... -> root.
  let mut path: Vec<GcObject> = Vec::new();
  let mut visited: HashSet<GcObject> = HashSet::new();
  let mut current = Some(target);
  while let Some(obj) = current {
    if !visited.insert(obj) {
      break;
    }
    path.push(obj);
    current = rt.event_targets.get(&obj).and_then(|state| state.parent);
  }

  let mut reached_target = false;

  // Capture phase: root -> target.
  for &current_target in path.iter().rev() {
    reached_target = reached_target || current_target == target;

    dispatch_listeners(rt, current_target, event_obj, &event_type, true)?;

    let (prop_stop, imm_stop) = match rt.events.get(&event_obj) {
      Some(state) => (state.propagation_stopped, state.immediate_stopped),
      None => (false, false),
    };

    if imm_stop {
      break;
    }
    if prop_stop && current_target != target {
      reached_target = false;
      break;
    }
  }

  if reached_target {
    // At-target bubble listeners always run.
    dispatch_listeners(rt, target, event_obj, &event_type, false)?;

    let (prop_stop, imm_stop) = match rt.events.get(&event_obj) {
      Some(state) => (state.propagation_stopped, state.immediate_stopped),
      None => (false, false),
    };

    if bubbles && !imm_stop && !prop_stop {
      for &current_target in path.iter().skip(1) {
        dispatch_listeners(rt, current_target, event_obj, &event_type, false)?;
        let (prop_stop, imm_stop) = match rt.events.get(&event_obj) {
          Some(state) => (state.propagation_stopped, state.immediate_stopped),
          None => (false, false),
        };
        if imm_stop || prop_stop {
          break;
        }
      }
    }
  }

  // Clear `currentTarget` when dispatch completes.
  let current_target_key = {
    let mut scope = rt.heap.scope();
    PropertyKey::from_string(scope.alloc_string("currentTarget")?)
  };
  rt.define_data_prop(event_obj, current_target_key, Value::Null)?;

  let default_prevented = rt
    .events
    .get(&event_obj)
    .map(|s| s.default_prevented)
    .unwrap_or(false);
  Ok(Value::Bool(!default_prevented))
}

fn dispatch_listeners(
  rt: &mut JsWptRuntime,
  target: GcObject,
  event_obj: GcObject,
  event_type: &str,
  capture: bool,
) -> Result<(), JsError> {
  let snapshot: Vec<EventListener> = rt
    .event_targets
    .get(&target)
    .and_then(|state| state.listeners.get(event_type))
    .cloned()
    .unwrap_or_default();

  if snapshot.is_empty() {
    return Ok(());
  }

  for listener in snapshot {
    if listener.options.capture != capture {
      continue;
    }

    let imm_stop = rt
      .events
      .get(&event_obj)
      .is_some_and(|state| state.immediate_stopped);
    if imm_stop {
      break;
    }

    // Set `currentTarget`.
    let current_target_key = {
      let mut scope = rt.heap.scope();
      PropertyKey::from_string(scope.alloc_string("currentTarget")?)
    };
    rt.define_data_prop(event_obj, current_target_key, Value::Object(target))?;

    if let Some(state) = rt.events.get_mut(&event_obj) {
      state.in_passive_listener = listener.options.passive;
    }

    rt.call(listener.callback, Value::Object(target), &[Value::Object(event_obj)])?;

    if let Some(state) = rt.events.get_mut(&event_obj) {
      state.in_passive_listener = false;
    }

    if listener.options.once {
      if let Some(state) = rt.event_targets.get_mut(&target) {
        if let Some(list) = state.listeners.get_mut(event_type) {
          list.retain(|rec| !(rec.callback == listener.callback && rec.options.capture == capture));
          if list.is_empty() {
            state.listeners.remove(event_type);
          }
        }
      }
    }

    let imm_stop = rt
      .events
      .get(&event_obj)
      .is_some_and(|state| state.immediate_stopped);
    if imm_stop {
      break;
    }
  }

  Ok(())
}

fn native_queue_microtask(rt: &mut JsWptRuntime, _this: Value, args: &[Value]) -> Result<Value, JsError> {
  let callback = args.get(0).copied().unwrap_or(Value::Undefined);
  let Value::Object(obj) = callback else {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
      "TypeError: queueMicrotask callback is not callable",
    )?)));
  };
  if !rt.callables.contains_key(&obj) {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
      "TypeError: queueMicrotask callback is not callable",
    )?)));
  }
  rt
    .event_loop
    .queue_microtask(callback, Value::Object(rt.global_object), vec![]);
  Ok(Value::Undefined)
}

fn native_promise_resolve(rt: &mut JsWptRuntime, _this: Value, args: &[Value]) -> Result<Value, JsError> {
  let value = args.get(0).copied().unwrap_or(Value::Undefined);
  if let Value::Object(obj) = value {
    if rt.promises.contains_key(&obj) {
      return Ok(Value::Object(obj));
    }
  }
  let promise = rt.alloc_promise_with_state(PromiseStatus::Fulfilled, value)?;
  Ok(Value::Object(promise))
}

fn native_promise_then(rt: &mut JsWptRuntime, this: Value, args: &[Value]) -> Result<Value, JsError> {
  let Value::Object(promise) = this else {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
      "TypeError: Promise.prototype.then called on non-object",
    )?)));
  };

  let Some((status, settled_value)) = rt
    .promises
    .get(&promise)
    .map(|state| (state.status, state.value))
  else {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
      "TypeError: Promise.prototype.then called on non-Promise",
    )?)));
  };

  let on_fulfilled = args.get(0).copied().unwrap_or(Value::Undefined);
  let on_rejected = args.get(1).copied().unwrap_or(Value::Undefined);
  let on_fulfilled = if rt.is_callable_value(on_fulfilled) {
    on_fulfilled
  } else {
    Value::Undefined
  };
  let on_rejected = if rt.is_callable_value(on_rejected) {
    on_rejected
  } else {
    Value::Undefined
  };

  let next = rt.alloc_promise_with_state(PromiseStatus::Pending, Value::Undefined)?;
  let reaction = PromiseReaction {
    on_fulfilled,
    on_rejected,
    next_promise: next,
  };

  match status {
    PromiseStatus::Pending => {
      let state = rt
        .promises
        .get_mut(&promise)
        .expect("promise was present in map");
      state.reactions.push(reaction);
    }
    settled @ (PromiseStatus::Fulfilled | PromiseStatus::Rejected) => {
      rt.enqueue_promise_job(PromiseJob {
        status: settled,
        value: settled_value,
        reaction,
      })?;
    }
  }

  Ok(Value::Object(next))
}

fn native_run_promise_job(rt: &mut JsWptRuntime, _this: Value, args: &[Value]) -> Result<Value, JsError> {
  rt.vm.tick()?;

  let id = match args.get(0).copied().unwrap_or(Value::Number(0.0)) {
    Value::Number(n) => n as u64,
    _ => 0,
  };
  let Some(job) = rt.promise_jobs.remove(&id) else {
    return Ok(Value::Undefined);
  };

  let next = job.reaction.next_promise;
  let handler = match job.status {
    PromiseStatus::Fulfilled => job.reaction.on_fulfilled,
    PromiseStatus::Rejected => job.reaction.on_rejected,
    PromiseStatus::Pending => Value::Undefined,
  };

  if rt.is_callable_value(handler) {
    match rt.call(handler, Value::Undefined, &[job.value]) {
      Ok(v) => rt.settle_promise(next, PromiseStatus::Fulfilled, v)?,
      Err(JsError::Vm(VmError::Throw(reason))) => {
        rt.settle_promise(next, PromiseStatus::Rejected, reason)?
      }
      Err(other) => return Err(other),
    }
  } else {
    match job.status {
      PromiseStatus::Fulfilled => rt.settle_promise(next, PromiseStatus::Fulfilled, job.value)?,
      PromiseStatus::Rejected => rt.settle_promise(next, PromiseStatus::Rejected, job.value)?,
      PromiseStatus::Pending => {}
    }
  }

  Ok(Value::Undefined)
}

fn native_wpt_report(rt: &mut JsWptRuntime, _this: Value, args: &[Value]) -> Result<Value, JsError> {
  let payload = args.get(0).copied().unwrap_or(Value::Undefined);

  let mut file_status: Option<String> = None;
  let mut harness_status: Option<String> = None;
  let mut message: Option<String> = None;
  let mut stack: Option<String> = None;

  if let Value::Object(obj) = payload {
    let file_status_key = PropertyKey::from_string(rt.keys.file_status);
    let harness_status_key = PropertyKey::from_string(rt.keys.harness_status);
    let message_key = PropertyKey::from_string(rt.keys.message);
    let stack_key = PropertyKey::from_string(rt.keys.stack);

    if let Some(desc) = rt.heap.get_property(obj, &file_status_key)? {
      if let PropertyKind::Data { value, .. } = desc.kind {
        if !matches!(value, Value::Undefined | Value::Null) {
          file_status = Some(rt.value_to_string_lossy(value));
        }
      }
    }
    if let Some(desc) = rt.heap.get_property(obj, &harness_status_key)? {
      if let PropertyKind::Data { value, .. } = desc.kind {
        if !matches!(value, Value::Undefined | Value::Null) {
          harness_status = Some(rt.value_to_string_lossy(value));
        }
      }
    }
    if let Some(desc) = rt.heap.get_property(obj, &message_key)? {
      if let PropertyKind::Data { value, .. } = desc.kind {
        if !matches!(value, Value::Undefined | Value::Null) {
          message = Some(rt.value_to_string_lossy(value));
        }
      }
    }
    if let Some(desc) = rt.heap.get_property(obj, &stack_key)? {
      if let PropertyKind::Data { value, .. } = desc.kind {
        if !matches!(value, Value::Undefined | Value::Null) {
          stack = Some(rt.value_to_string_lossy(value));
        }
      }
    }
  } else if matches!(payload, Value::String(_)) {
    file_status = Some(rt.value_to_string_lossy(payload));
  }

  let report = WptReport {
    file_status: file_status.unwrap_or_else(|| "error".to_string()),
    harness_status: harness_status.unwrap_or_else(|| "ok".to_string()),
    message,
    stack,
    subtests: Vec::new(),
  };
  rt.report = Some(report);

  Ok(Value::Undefined)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn noop_native(_rt: &mut JsWptRuntime, _this: Value, _args: &[Value]) -> Result<Value, JsError> {
    Ok(Value::Undefined)
  }

  #[test]
  fn value_to_property_key_supports_bigint() {
    let mut rt = JsWptRuntime::new("https://example.com/");

    let key = rt
      .value_to_property_key(Value::BigInt(vm_js::JsBigInt::from_u128(42)))
      .expect("ToPropertyKey(BigInt) should succeed");

    let PropertyKey::String(s) = key else {
      panic!("expected string property key");
    };
    let rendered = rt.heap.get_string(s).expect("property key string");
    assert_eq!(rendered.to_utf8_lossy(), "42");
  }

  #[test]
  fn value_to_property_key_supports_fractional_numbers() {
    let mut rt = JsWptRuntime::new("https://example.com/");

    let key = rt
      .value_to_property_key(Value::Number(1.5))
      .expect("ToPropertyKey(Number) should succeed");

    let PropertyKey::String(s) = key else {
      panic!("expected string property key");
    };
    let rendered = rt.heap.get_string(s).expect("property key string");
    assert_eq!(rendered.to_utf8_lossy(), "1.5");
  }

  #[test]
  fn value_to_string_lossy_formats_numbers_like_ecmascript() {
    let mut rt = JsWptRuntime::new("https://example.com/");

    assert_eq!(rt.value_to_string_lossy(Value::Number(f64::INFINITY)), "Infinity");
    assert_eq!(
      rt.value_to_string_lossy(Value::Number(f64::NEG_INFINITY)),
      "-Infinity"
    );
    // ECMAScript `ToString(-0)` is `"0"`.
    assert_eq!(rt.value_to_string_lossy(Value::Number(-0.0)), "0");
  }

  #[test]
  fn set_timeout_delay_uses_tonumber_and_treats_infinity_as_zero() {
    let mut rt = JsWptRuntime::new("https://example.com/");

    let cb = rt.alloc_native_function(noop_native).expect("alloc callback");
    native_set_timeout(
      &mut rt,
      Value::Undefined,
      &[Value::Object(cb), Value::Number(f64::INFINITY)],
    )
    .expect("setTimeout should accept Infinity delay");

    // Infinity should be treated like 0 in WebIDL long conversion, making the timer immediately due.
    rt.event_loop.enqueue_due_timers();
    assert!(rt.event_loop.pop_next_task().is_some());
  }

  #[test]
  fn set_timeout_delay_throws_on_bigint() {
    let mut rt = JsWptRuntime::new("https://example.com/");

    let cb = rt.alloc_native_function(noop_native).expect("alloc callback");
    let err = native_set_timeout(
      &mut rt,
      Value::Undefined,
      &[Value::Object(cb), Value::BigInt(vm_js::JsBigInt::from_u128(1))],
    )
    .unwrap_err();
    assert!(matches!(err, JsError::Vm(VmError::TypeError(_))));
  }
}
