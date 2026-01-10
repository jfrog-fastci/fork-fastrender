use crate::backend::{Backend, BackendInit};
use crate::cookie_jar::CookieJar;
use crate::wpt_report::{WptReport, WptSubtest};
use crate::RunError;
use crate::window_or_worker_global_scope::{
  forgiving_base64_decode, forgiving_base64_encode, is_secure_context_for_document_url,
  latin1_encode, serialized_origin_for_document_url,
};
use html5ever::tendril::TendrilSink;
use html5ever::tree_builder::TreeBuilderOpts;
use html5ever::{parse_fragment, ParseOpts};
use markup5ever::{LocalName, Namespace, QualName};
use markup5ever_rcdom::{Handle, NodeData, RcDom};
use parse_js::ast::class_or_object::{ClassOrObjKey, ClassOrObjVal, ObjMemberType};
use parse_js::ast::expr::lit::{LitArrExpr, LitBoolExpr, LitNumExpr, LitObjExpr, LitStrExpr};
use parse_js::ast::expr::{
  ArrowFuncExpr, BinaryExpr, CallExpr, ComputedMemberExpr, Expr, IdExpr, MemberExpr, UnaryExpr,
  UnaryPostfixExpr,
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
use url::{form_urlencoded, Url};
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
  fn init_realm(
    &mut self,
    init: BackendInit,
    _host: Option<&mut dyn crate::engine::HostEnvironment>,
  ) -> Result<(), RunError> {
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

  fn eval_script(&mut self, source: &str, _name: &str) -> Result<(), RunError> {
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

  fn take_report(&mut self) -> Result<Option<WptReport>, RunError> {
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
      JsError::Vm(err) => err
        .thrown_value()
        .map(|value| rt.value_to_string_lossy(value))
        .unwrap_or_else(|| err.to_string()),
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
  subtests: GcString,
  name: GcString,
  status: GcString,
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
      subtests: scope.alloc_string("subtests")?,
      name: scope.alloc_string("name")?,
      status: scope.alloc_string("status")?,
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
  is_async: bool,
  body: UserFunctionBody,
  source: Rc<str>,
  hoisted_functions: Vec<(String, GcObject)>,
}

#[derive(Debug)]
enum UserFunctionBody {
  Block(Vec<Node<Stmt>>),
  Expression(Node<Expr>),
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
enum DomNodeKind {
  Element,
  DocumentFragment,
  Text,
}

impl Default for DomNodeKind {
  fn default() -> Self {
    Self::Element
  }
}

#[derive(Debug, Default)]
struct DomNodeState {
  kind: DomNodeKind,
  tag_name: String,
  /// HTML attributes on this element.
  ///
  /// Stored as lowercase attribute name -> value string handle.
  ///
  /// We store values as `GcString` so getters can return them without allocating fresh strings for
  /// every access, which keeps the per-test heap bounded even with many attribute reads.
  attributes: HashMap<String, GcString>,
  /// Serialized character data for this node.
  ///
  /// For `Text` nodes, this stores the node's `data`/`nodeValue`.
  ///
  /// For element nodes, this may be used as a fallback when the DOM shim chooses to treat markup as
  /// literal text (e.g. unsupported `innerHTML` fragments). In the common cases covered by the
  /// curated smoke corpus we allocate explicit `Text` child nodes so `childNodes` matches browser
  /// semantics.
  text_content: Option<String>,
  parent: Option<GcObject>,
  children: Vec<GcObject>,
  // Children appended to <template> go into an inert subtree and must not be traversed by selector
  // APIs. We model this as a separate list rather than a real DocumentFragment.
  template_content: Vec<GcObject>,
  /// Cached `childNodes` object so stored references behave like a live NodeList.
  child_nodes: Option<GcObject>,
  /// Cached `classList` object so stored references behave like a stable DOMTokenList.
  class_list: Option<GcObject>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DomCombinator {
  Descendant,
  Child,
}

#[derive(Debug, Clone)]
struct DomCompoundSelector {
  tag: Option<String>,
  id: Option<String>,
  classes: Vec<String>,
  is_scope: bool,
}

#[derive(Debug, Clone)]
struct DomSelector {
  compounds: Vec<DomCompoundSelector>,
  combinators: Vec<DomCombinator>,
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

#[derive(Debug, Clone)]
struct UrlState {
  url: Url,
  /// Raw query string without the leading `?`.
  ///
  /// This is preserved for `URL.search` (which should not be rewritten just because
  /// `URL.searchParams` is accessed).
  raw_query: String,
  /// Lazily-created `URLSearchParams` object associated with this `URL`.
  search_params: Option<GcObject>,
}

#[derive(Debug, Clone)]
struct UrlSearchParamsState {
  /// Associated `URL` object if this `URLSearchParams` view is live.
  url: Option<GcObject>,
  pairs: Vec<(String, String)>,
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
  document_url: String,
  cookie_jar: CookieJar,
  callables: HashMap<GcObject, Rc<Callable>>,
  arrays: HashMap<GcObject, Vec<Value>>,
  event_targets: HashMap<GcObject, EventTargetState>,
  events: HashMap<GcObject, EventState>,
  dom_nodes: HashMap<GcObject, DomNodeState>,
  dom_token_lists: HashMap<GcObject, GcObject>,
  urls: HashMap<GcObject, UrlState>,
  url_search_params: HashMap<GcObject, UrlSearchParamsState>,
  dom_element_proto: Option<GcObject>,
  dom_token_list_proto: Option<GcObject>,
  document_fragment_proto: Option<GcObject>,
  text_proto: Option<GcObject>,
  document_body: Option<GcObject>,
  url_proto: Option<GcObject>,
  url_search_params_proto: Option<GcObject>,
  request_proto: Option<GcObject>,
  response_proto: Option<GcObject>,
  promises: HashMap<GcObject, PromiseState>,
  promise_jobs: HashMap<u64, PromiseJob>,
  next_promise_job_id: u64,
  promise_job_runner: Option<GcObject>,
  promise_prototype: Option<GcObject>,
  array_prototype: Option<GcObject>,
  error_prototype: Option<GcObject>,
  type_error_prototype: Option<GcObject>,
  string_char_code_at: Option<GcObject>,
  event_target_proto: Option<GcObject>,
  node_proto: Option<GcObject>,
  global_object: GcObject,
  keys: CachedKeys,
  pub(crate) event_loop: EventLoop,
  report: Option<WptReport>,
  this_binding: Value,
  current_source: Option<Rc<str>>,
}

impl JsWptRuntime {
  fn new(test_url: &str) -> Self {
    let mut vm = Vm::new(VmOptions {
      max_stack_depth: 1024,
      default_fuel: None,
      default_deadline: None,
      check_time_every: 1,
      interrupt_flag: None,
      external_interrupt_flag: None,
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
      document_url: test_url.to_string(),
      cookie_jar: CookieJar::new(),
      callables: HashMap::new(),
      arrays: HashMap::new(),
      event_targets: HashMap::new(),
      events: HashMap::new(),
      dom_nodes: HashMap::new(),
      dom_token_lists: HashMap::new(),
      urls: HashMap::new(),
      url_search_params: HashMap::new(),
      dom_element_proto: None,
      dom_token_list_proto: None,
      document_fragment_proto: None,
      text_proto: None,
      document_body: None,
      url_proto: None,
      url_search_params_proto: None,
      request_proto: None,
      response_proto: None,
      promises: HashMap::new(),
      promise_jobs: HashMap::new(),
      next_promise_job_id: 1,
      promise_job_runner: None,
      promise_prototype: None,
      array_prototype: None,
      error_prototype: None,
      type_error_prototype: None,
      string_char_code_at: None,
      event_target_proto: None,
      node_proto: None,
      global_object,
      keys,
      event_loop: EventLoop::new(),
      report: None,
      this_binding: global_value,
      current_source: None,
    };

    // Bind globalThis/window/self.
    rt.env.set("globalThis", global_value);
    rt.env.set("window", global_value);
    rt.env.set("self", global_value);
    // Fundamental global binding: scripts frequently reference `undefined` as an identifier.
    rt.env.set("undefined", Value::Undefined);

    // `Symbol(...)` is used by some tests for smoke coverage (e.g. `reportError(Symbol("x"))`).
    let symbol = rt.alloc_native_function(native_symbol).expect("alloc Symbol");
    rt.env.set("Symbol", Value::Object(symbol));
    let symbol_key = {
      let mut scope = rt.heap.scope();
      PropertyKey::from_string(scope.alloc_string("Symbol").expect("alloc Symbol key"))
    };
    rt
      .define_data_prop(rt.global_object, symbol_key, Value::Object(symbol))
      .expect("define Symbol");

    // Report hook.
    let report_fn = rt.alloc_native_function(native_wpt_report).expect("alloc report fn");
    rt.env.set("__fastrender_wpt_report", Value::Object(report_fn));
    // Mirror the host hook on `window`/`globalThis` so WPT reporter shims that probe
    // `globalThis.__fastrender_wpt_report` work under the vm-js backend (which otherwise treats
    // global bindings and global object properties as separate namespaces).
    let report_key = {
      let mut scope = rt.heap.scope();
      PropertyKey::from_string(
        scope
          .alloc_string("__fastrender_wpt_report")
          .expect("alloc report key"),
      )
    };
    rt
      .define_data_prop(rt.global_object, report_key, Value::Object(report_fn))
      .expect("define __fastrender_wpt_report");

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

    // Expose timer APIs as `window.*` properties for spec-shaped code (and for harness shims that
    // reach through `globalThis`).
    for (name, value) in [
      ("setTimeout", Value::Object(set_timeout)),
      ("clearTimeout", Value::Object(clear_timeout)),
      ("setInterval", Value::Object(set_interval)),
      ("clearInterval", Value::Object(clear_interval)),
      ("queueMicrotask", Value::Object(queue_microtask)),
    ] {
      let key = {
        let mut scope = rt.heap.scope();
        PropertyKey::from_string(scope.alloc_string(name).expect("alloc global key"))
      };
      rt
        .define_data_prop(rt.global_object, key, value)
        .expect("define timer global");
    }

    rt.install_promise_shim().expect("install Promise");
    rt.install_array_shim().expect("install Array");
    rt.install_error_shim().expect("install Error");
    rt.install_string_shim().expect("install String");
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
      .install_window_or_worker_global_scope_primitives(test_url)
      .expect("install WindowOrWorkerGlobalScope primitives");

    rt.install_url_shims().expect("install URL/URLSearchParams");
    rt.install_fetch_shims().expect("install fetch/Request/Response");

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

    // --- Minimal DOM shims ---
    //
    // The vm-js WPT runner is not a full browser environment; it only needs enough DOM/event
    // surface to execute the curated offline WPT corpus:
    // - `document.body` + `document.createElement(...)`
    // - basic tree mutation (`appendChild` / `removeChild` / `childNodes`)
    // - selector APIs (`matches` / `closest` / `querySelector(All)`)
    //
    // Additionally, we register these DOM objects as `EventTarget`s so future event tests can build
    // a propagation chain rooted at `window` -> `document` -> elements.

    // Register `window` (the global object) as an EventTarget so other nodes can point to it.
    let event_target_proto = self.event_target_proto()?;
    if !self.event_targets.contains_key(&self.global_object) {
      self
        .heap
        .object_set_prototype(self.global_object, Some(event_target_proto))?;
      self.event_targets.insert(
        self.global_object,
        EventTargetState {
          parent: None,
          listeners: HashMap::new(),
        },
      );
    }

    // Node prototype (inherits EventTarget, adds DOM tree mutation APIs).
    let node_proto = self.alloc_object()?;
    self
      .heap
      .object_set_prototype(node_proto, Some(event_target_proto))?;
    let append_child = self.alloc_native_function(native_node_append_child)?;
    let insert_before = self.alloc_native_function(native_node_insert_before)?;
    let contains = self.alloc_native_function(native_node_contains)?;
    let has_child_nodes = self.alloc_native_function(native_node_has_child_nodes)?;
    let remove = self.alloc_native_function(native_node_remove)?;
    let remove_child = self.alloc_native_function(native_dom_remove_child)?;
    let replace_child = self.alloc_native_function(native_node_replace_child)?;
    let append_child_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("appendChild")?)
    };
    let insert_before_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("insertBefore")?)
    };
    let remove_child_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("removeChild")?)
    };
    let replace_child_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("replaceChild")?)
    };
    let contains_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("contains")?)
    };
    let has_child_nodes_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("hasChildNodes")?)
    };
    let remove_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("remove")?)
    };
    self.define_data_prop(node_proto, append_child_key, Value::Object(append_child))?;
    self.define_data_prop(node_proto, insert_before_key, Value::Object(insert_before))?;
    self.define_data_prop(node_proto, contains_key, Value::Object(contains))?;
    self.define_data_prop(node_proto, has_child_nodes_key, Value::Object(has_child_nodes))?;
    self.define_data_prop(node_proto, remove_key, Value::Object(remove))?;
    self.define_data_prop(node_proto, remove_child_key, Value::Object(remove_child))?;
    self.define_data_prop(node_proto, replace_child_key, Value::Object(replace_child))?;

    // Basic Node accessors used by the offline WPT corpus.
    let text_content_get = self.alloc_native_function(native_node_get_text_content)?;
    let text_content_set = self.alloc_native_function(native_node_set_text_content)?;
    let owner_document_get = self.alloc_native_function(native_node_get_owner_document)?;
    let is_connected_get = self.alloc_native_function(native_node_get_is_connected)?;

    let text_content_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("textContent")?)
    };
    let owner_document_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("ownerDocument")?)
    };
    let is_connected_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("isConnected")?)
    };
    self.define_accessor_prop(
      node_proto,
      text_content_key,
      Value::Object(text_content_get),
      Value::Object(text_content_set),
    )?;
    self.define_accessor_prop(
      node_proto,
      owner_document_key,
      Value::Object(owner_document_get),
      Value::Undefined,
    )?;
    self.define_accessor_prop(
      node_proto,
      is_connected_key,
      Value::Object(is_connected_get),
      Value::Undefined,
    )?;
    self.node_proto = Some(node_proto);

    // Element prototype (inherits Node, adds selector + DOMParsing APIs).
    let element_proto = self.alloc_object()?;
    self.heap.object_set_prototype(element_proto, Some(node_proto))?;
    let matches = self.alloc_native_function(native_dom_element_matches)?;
    let closest = self.alloc_native_function(native_dom_element_closest)?;
    let query_selector = self.alloc_native_function(native_dom_element_query_selector)?;
    let query_selector_all = self.alloc_native_function(native_dom_element_query_selector_all)?;

    // Basic attribute APIs (`id`/`className` accessors + getAttribute/setAttribute/removeAttribute).
    let get_attribute = self.alloc_native_function(native_dom_element_get_attribute)?;
    let set_attribute = self.alloc_native_function(native_dom_element_set_attribute)?;
    let remove_attribute = self.alloc_native_function(native_dom_element_remove_attribute)?;
    let id_get = self.alloc_native_function(native_dom_element_get_id)?;
    let id_set = self.alloc_native_function(native_dom_element_set_id)?;
    let class_get = self.alloc_native_function(native_dom_element_get_class_name)?;
    let class_set = self.alloc_native_function(native_dom_element_set_class_name)?;

    let inner_get = self.alloc_native_function(native_dom_element_get_inner_html)?;
    let inner_set = self.alloc_native_function(native_dom_element_set_inner_html)?;
    let outer_get = self.alloc_native_function(native_dom_element_get_outer_html)?;
    let outer_set = self.alloc_native_function(native_dom_element_set_outer_html)?;

    let matches_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("matches")?)
    };
    let closest_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("closest")?)
    };
    let query_selector_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("querySelector")?)
    };
    let query_selector_all_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("querySelectorAll")?)
    };
    self.define_data_prop(element_proto, matches_key, Value::Object(matches))?;
    self.define_data_prop(element_proto, closest_key, Value::Object(closest))?;
    self.define_data_prop(element_proto, query_selector_key, Value::Object(query_selector))?;
    self.define_data_prop(
      element_proto,
      query_selector_all_key,
      Value::Object(query_selector_all),
    )?;
    let inner_html_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("innerHTML")?)
    };
    let outer_html_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("outerHTML")?)
    };
    self.define_accessor_prop(
      element_proto,
      inner_html_key,
      Value::Object(inner_get),
      Value::Object(inner_set),
    )?;
    self.define_accessor_prop(
      element_proto,
      outer_html_key,
      Value::Object(outer_get),
      Value::Object(outer_set),
    )?;

    let get_attribute_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("getAttribute")?)
    };
    let set_attribute_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("setAttribute")?)
    };
    let remove_attribute_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("removeAttribute")?)
    };
    self.define_data_prop(element_proto, get_attribute_key, Value::Object(get_attribute))?;
    self.define_data_prop(element_proto, set_attribute_key, Value::Object(set_attribute))?;
    self.define_data_prop(
      element_proto,
      remove_attribute_key,
      Value::Object(remove_attribute),
    )?;

    let id_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("id")?)
    };
    self.define_accessor_prop(
      element_proto,
      id_key,
      Value::Object(id_get),
      Value::Object(id_set),
    )?;

    let class_name_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("className")?)
    };
    self.define_accessor_prop(
      element_proto,
      class_name_key,
      Value::Object(class_get),
      Value::Object(class_set),
    )?;

    // --- Element.classList / DOMTokenList ---
    let dom_token_list_proto = self.alloc_object()?;
    let token_list_add = self.alloc_native_function(native_dom_token_list_add)?;
    let token_list_remove = self.alloc_native_function(native_dom_token_list_remove)?;
    let token_list_toggle = self.alloc_native_function(native_dom_token_list_toggle)?;
    let token_list_contains = self.alloc_native_function(native_dom_token_list_contains)?;

    let token_list_add_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("add")?)
    };
    let token_list_remove_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("remove")?)
    };
    let token_list_toggle_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("toggle")?)
    };
    let token_list_contains_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("contains")?)
    };
    self.define_data_prop(
      dom_token_list_proto,
      token_list_add_key,
      Value::Object(token_list_add),
    )?;
    self.define_data_prop(
      dom_token_list_proto,
      token_list_remove_key,
      Value::Object(token_list_remove),
    )?;
    self.define_data_prop(
      dom_token_list_proto,
      token_list_toggle_key,
      Value::Object(token_list_toggle),
    )?;
    self.define_data_prop(
      dom_token_list_proto,
      token_list_contains_key,
      Value::Object(token_list_contains),
    )?;
    self.dom_token_list_proto = Some(dom_token_list_proto);

    let class_list_get = self.alloc_native_function(native_dom_element_get_class_list)?;
    let class_list_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("classList")?)
    };
    self.define_accessor_prop(
      element_proto,
      class_list_key,
      Value::Object(class_list_get),
      Value::Undefined,
    )?;

    self.dom_element_proto = Some(element_proto);

    // DocumentFragment prototype (inherits Node, adds NonElementParentNode APIs like getElementById).
    let document_fragment_proto = self.alloc_object()?;
    self
      .heap
      .object_set_prototype(document_fragment_proto, Some(node_proto))?;
    let fragment_get_element_by_id = self.alloc_native_function(native_document_fragment_get_element_by_id)?;
    let fragment_query_selector = self.alloc_native_function(native_document_fragment_query_selector)?;
    let fragment_query_selector_all =
      self.alloc_native_function(native_document_fragment_query_selector_all)?;
    let fragment_get_element_by_id_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("getElementById")?)
    };
    let fragment_query_selector_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("querySelector")?)
    };
    let fragment_query_selector_all_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("querySelectorAll")?)
    };
    self.define_data_prop(
      document_fragment_proto,
      fragment_get_element_by_id_key,
      Value::Object(fragment_get_element_by_id),
    )?;
    self.define_data_prop(
      document_fragment_proto,
      fragment_query_selector_key,
      Value::Object(fragment_query_selector),
    )?;
    self.define_data_prop(
      document_fragment_proto,
      fragment_query_selector_all_key,
      Value::Object(fragment_query_selector_all),
    )?;
    self.document_fragment_proto = Some(document_fragment_proto);

    // Text prototype (inherits Node, exposes `data`).
    let text_proto = self.alloc_object()?;
    self.heap.object_set_prototype(text_proto, Some(node_proto))?;
    let data_get = self.alloc_native_function(native_text_get_data)?;
    let data_set = self.alloc_native_function(native_text_set_data)?;
    let data_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("data")?)
    };
    self.define_accessor_prop(
      text_proto,
      data_key,
      Value::Object(data_get),
      Value::Object(data_set),
    )?;
    self.text_proto = Some(text_proto);

    // `Node` constructor + constants (enough for the offline WPT DOM smoke corpus).
    //
    // Note: `Node` is not constructible in browsers; keep that invariant so tests don't accidentally
    // fabricate hostless nodes that bypass `dom_nodes` bookkeeping.
    let node_ctor = self.alloc_native_function(native_node_ctor)?;
    let prototype_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("prototype")?)
    };
    self.define_data_prop(node_ctor, prototype_key, Value::Object(node_proto))?;
    let constructor_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("constructor")?)
    };
    self.define_data_prop(node_proto, constructor_key, Value::Object(node_ctor))?;

    for (name, value) in [
      ("ELEMENT_NODE", 1.0),
      ("ATTRIBUTE_NODE", 2.0),
      ("TEXT_NODE", 3.0),
      ("CDATA_SECTION_NODE", 4.0),
      ("ENTITY_REFERENCE_NODE", 5.0),
      ("ENTITY_NODE", 6.0),
      ("PROCESSING_INSTRUCTION_NODE", 7.0),
      ("COMMENT_NODE", 8.0),
      ("DOCUMENT_NODE", 9.0),
      ("DOCUMENT_TYPE_NODE", 10.0),
      ("DOCUMENT_FRAGMENT_NODE", 11.0),
      ("NOTATION_NODE", 12.0),
    ] {
      let key = {
        let mut scope = self.heap.scope();
        PropertyKey::from_string(scope.alloc_string(name)?)
      };
      self.define_data_prop(node_ctor, key, Value::Number(value))?;
    }

    self.env.set("Node", Value::Object(node_ctor));
    let node_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("Node")?)
    };
    self.define_data_prop(self.global_object, node_key, Value::Object(node_ctor))?;

    // Document prototype (inherits Node). We expose `Document.prototype.cookie` to back
    // `document.cookie` reads/writes used by many real-world scripts.
    let document_proto = self.alloc_object()?;
    self
      .heap
      .object_set_prototype(document_proto, Some(node_proto))?;
    let cookie_get = self.alloc_native_function(native_document_get_cookie)?;
    let cookie_set = self.alloc_native_function(native_document_set_cookie)?;
    let cookie_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("cookie")?)
    };
    self.define_accessor_prop(
      document_proto,
      cookie_key,
      Value::Object(cookie_get),
      Value::Object(cookie_set),
    )?;

    // ParentNode selector APIs on `Document.prototype`.
    let doc_query_selector = self.alloc_native_function(native_document_query_selector)?;
    let doc_query_selector_all = self.alloc_native_function(native_document_query_selector_all)?;
    let doc_query_selector_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("querySelector")?)
    };
    let doc_query_selector_all_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("querySelectorAll")?)
    };
    self.define_data_prop(
      document_proto,
      doc_query_selector_key,
      Value::Object(doc_query_selector),
    )?;
    self.define_data_prop(
      document_proto,
      doc_query_selector_all_key,
      Value::Object(doc_query_selector_all),
    )?;

    // `document` object: Node + createElement + URL.
    let document = self.alloc_object()?;
    self
      .heap
      .object_set_prototype(document, Some(document_proto))?;
    self.event_targets.insert(
      document,
      EventTargetState {
        parent: Some(self.global_object),
        listeners: HashMap::new(),
      },
    );
    self.define_data_prop(document, PropertyKey::from_string(self.keys.url), href_value)?;

    // Node metadata for `document`.
    let node_type_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("nodeType")?)
    };
    let node_name_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("nodeName")?)
    };
    let node_value_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("nodeValue")?)
    };
    let document_node_name = self.alloc_string_value("#document")?;
    self.define_data_prop(document, node_type_key, Value::Number(9.0))?;
    self.define_data_prop(document, node_name_key, document_node_name)?;
    self.define_data_prop(document, node_value_key, Value::Null)?;

    let create_element = self.alloc_native_function(native_document_create_element)?;
    let create_element_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("createElement")?)
    };
    self.define_data_prop(document, create_element_key, Value::Object(create_element))?;

    let create_document_fragment = self.alloc_native_function(native_document_create_document_fragment)?;
    let create_fragment_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("createDocumentFragment")?)
    };
    self.define_data_prop(
      document,
      create_fragment_key,
      Value::Object(create_document_fragment),
    )?;

    let create_text_node = self.alloc_native_function(native_document_create_text_node)?;
    let create_text_node_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("createTextNode")?)
    };
    self.define_data_prop(document, create_text_node_key, Value::Object(create_text_node))?;

    let get_element_by_id = self.alloc_native_function(native_document_get_element_by_id)?;
    let get_element_by_id_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("getElementById")?)
    };
    self.define_data_prop(document, get_element_by_id_key, Value::Object(get_element_by_id))?;

    // Minimal document structure: <html><head></head><body></body></html>.
    //
    // Only a handful of WPT smoke tests rely on `document.head`/`document.body`/`document.documentElement`;
    // the rest of the vm-js runner uses `document.body` as the root for DOM mutation/selector tests.
    let document_element = self.alloc_dom_element("html")?;
    let head = self.alloc_dom_element("head")?;
    let body = self.alloc_dom_element("body")?;
    self.document_body = Some(body);

    // Wire DOM tree parent/children pointers for selector APIs / `childNodes`.
    if let Some(state) = self.dom_nodes.get_mut(&document_element) {
      state.children.push(head);
      state.children.push(body);
      state.parent = Some(document);
    }
    if let Some(state) = self.dom_nodes.get_mut(&head) {
      state.parent = Some(document_element);
    }
    if let Some(state) = self.dom_nodes.get_mut(&body) {
      state.parent = Some(document_element);
    }
    let parent_node_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("parentNode")?)
    };
    self.define_data_prop(document_element, parent_node_key, Value::Object(document))?;
    self.define_data_prop(head, parent_node_key, Value::Object(document_element))?;
    self.define_data_prop(body, parent_node_key, Value::Object(document_element))?;
    self.update_dom_child_nodes(document_element)?;

    // Wire EventTarget parent pointers so event dispatch can traverse:
    // window -> document -> html -> (head/body) -> descendants.
    if let Some(state) = self.event_targets.get_mut(&document_element) {
      state.parent = Some(document);
    }
    if let Some(state) = self.event_targets.get_mut(&head) {
      state.parent = Some(document_element);
    }
    if let Some(state) = self.event_targets.get_mut(&body) {
      state.parent = Some(document_element);
    }

    let document_element_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("documentElement")?)
    };
    let head_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("head")?)
    };
    let body_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("body")?)
    };
    self.define_data_prop(document, document_element_key, Value::Object(document_element))?;
    self.define_data_prop(document, head_key, Value::Object(head))?;
    self.define_data_prop(document, body_key, Value::Object(body))?;

    self.env.set("document", Value::Object(document));

    // Expose minimal DOM interface constructors required by the curated WPT corpus (notably
    // `DocumentFragment` for `instanceof` checks).
    let prototype_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("prototype")?)
    };

    let document_ctor = self.alloc_native_function(native_illegal_dom_constructor)?;
    self.define_data_prop(document_ctor, prototype_key, Value::Object(document_proto))?;
    self.env.set("Document", Value::Object(document_ctor));
    let document_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("Document")?)
    };
    self.define_data_prop(self.global_object, document_key, Value::Object(document_ctor))?;

    let element_ctor = self.alloc_native_function(native_illegal_dom_constructor)?;
    self.define_data_prop(element_ctor, prototype_key, Value::Object(element_proto))?;
    self.env.set("Element", Value::Object(element_ctor));
    let element_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("Element")?)
    };
    self.define_data_prop(self.global_object, element_key, Value::Object(element_ctor))?;

    let fragment_ctor = self.alloc_native_function(native_illegal_dom_constructor)?;
    self.define_data_prop(
      fragment_ctor,
      prototype_key,
      Value::Object(document_fragment_proto),
    )?;
    self.env.set("DocumentFragment", Value::Object(fragment_ctor));
    let fragment_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("DocumentFragment")?)
    };
    self.define_data_prop(self.global_object, fragment_key, Value::Object(fragment_ctor))?;

    let text_ctor = self.alloc_native_function(native_illegal_dom_constructor)?;
    self.define_data_prop(text_ctor, prototype_key, Value::Object(text_proto))?;
    self.env.set("Text", Value::Object(text_ctor));
    let text_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("Text")?)
    };
    self.define_data_prop(self.global_object, text_key, Value::Object(text_ctor))?;

    let text_ctor = self.alloc_native_function(native_illegal_dom_constructor)?;
    self.define_data_prop(text_ctor, prototype_key, Value::Object(text_proto))?;
    self.env.set("Text", Value::Object(text_ctor));

    // Expose `window.document` and `window.location` for harness code that expects them.
    let document_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("document")?)
    };
    let location_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("location")?)
    };
    self.define_data_prop(
      self.global_object,
      document_key,
      Value::Object(document),
    )?;
    self.define_data_prop(
      self.global_object,
      location_key,
      Value::Object(location),
    )?;

    Ok(())
  }

  fn install_window_or_worker_global_scope_primitives(&mut self, test_url: &str) -> Result<(), JsError> {
    let origin = serialized_origin_for_document_url(test_url);
    let origin_value = self.alloc_string_value(&origin)?;
    let origin_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("origin")?)
    };
    self.define_data_prop(self.global_object, origin_key, origin_value)?;
    self.env.set("origin", origin_value);

    let is_secure_context = Value::Bool(is_secure_context_for_document_url(test_url));
    let is_secure_context_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("isSecureContext")?)
    };
    self.define_data_prop(self.global_object, is_secure_context_key, is_secure_context)?;
    self.env.set("isSecureContext", is_secure_context);

    let cross_origin_isolated = Value::Bool(false);
    let cross_origin_isolated_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("crossOriginIsolated")?)
    };
    self.define_data_prop(self.global_object, cross_origin_isolated_key, cross_origin_isolated)?;
    self.env.set("crossOriginIsolated", cross_origin_isolated);

    let atob_fn = self.alloc_native_function(native_atob)?;
    let atob_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("atob")?)
    };
    self.define_data_prop(self.global_object, atob_key, Value::Object(atob_fn))?;
    self.env.set("atob", Value::Object(atob_fn));

    let btoa_fn = self.alloc_native_function(native_btoa)?;
    let btoa_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("btoa")?)
    };
    self.define_data_prop(self.global_object, btoa_key, Value::Object(btoa_fn))?;
    self.env.set("btoa", Value::Object(btoa_fn));

    let report_error_fn = self.alloc_native_function(native_report_error)?;
    let report_error_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("reportError")?)
    };
    self.define_data_prop(
      self.global_object,
      report_error_key,
      Value::Object(report_error_fn),
    )?;
    self.env.set("reportError", Value::Object(report_error_fn));

    // --- Minimal fetch/URL shims ---
    //
    // These are intentionally tiny, but they allow the smoke corpus to validate URL resolution
    // semantics (`fetch("foo")` should resolve against the document URL).
    let resolve_url_fn = self.alloc_native_function(native_fastrender_resolve_url)?;
    let resolve_url_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("__fastrender_resolve_url")?)
    };
    self.define_data_prop(self.global_object, resolve_url_key, Value::Object(resolve_url_fn))?;
    self.env.set("__fastrender_resolve_url", Value::Object(resolve_url_fn));

    // Request constructor (minimal: stores the resolved request URL as `.url`).
    let request_proto = self.alloc_object()?;
    let request_ctor = self.alloc_native_function(native_request_ctor)?;
    let prototype_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("prototype")?)
    };
    self.define_data_prop(request_ctor, prototype_key, Value::Object(request_proto))?;
    self.env.set("Request", Value::Object(request_ctor));
    let request_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("Request")?)
    };
    self.define_data_prop(self.global_object, request_key, Value::Object(request_ctor))?;

    let fetch_fn = self.alloc_native_function(native_fetch)?;
    let fetch_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("fetch")?)
    };
    self.define_data_prop(self.global_object, fetch_key, Value::Object(fetch_fn))?;
    self.env.set("fetch", Value::Object(fetch_fn));

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

  fn install_array_shim(&mut self) -> Result<(), JsError> {
    let proto = self.alloc_object()?;

    let push_fn = self.alloc_native_function(native_array_push)?;
    let push_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("push")?)
    };
    self.define_data_prop(proto, push_key, Value::Object(push_fn))?;

    let join_fn = self.alloc_native_function(native_array_join)?;
    let join_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("join")?)
    };
    self.define_data_prop(proto, join_key, Value::Object(join_fn))?;

    self.array_prototype = Some(proto);
    Ok(())
  }

  fn install_error_shim(&mut self) -> Result<(), JsError> {
    let proto = self.alloc_object()?;

    // Error constructor.
    let ctor = self.alloc_native_function(native_error_ctor)?;
    let prototype_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("prototype")?)
    };
    self.define_data_prop(ctor, prototype_key, Value::Object(proto))?;
    self.env.set("Error", Value::Object(ctor));
    let error_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("Error")?)
    };
    self.define_data_prop(self.global_object, error_key, Value::Object(ctor))?;

    // TypeError constructor + prototype (inherits Error.prototype).
    let type_proto = self.alloc_object()?;
    self.heap.object_set_prototype(type_proto, Some(proto))?;
    let type_ctor = self.alloc_native_function(native_type_error_ctor)?;
    self.define_data_prop(type_ctor, prototype_key, Value::Object(type_proto))?;
    self.env.set("TypeError", Value::Object(type_ctor));
    let type_error_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("TypeError")?)
    };
    self.define_data_prop(self.global_object, type_error_key, Value::Object(type_ctor))?;

    self.error_prototype = Some(proto);
    self.type_error_prototype = Some(type_proto);
    Ok(())
  }

  fn install_string_shim(&mut self) -> Result<(), JsError> {
    // The runner does not implement the full `String` constructor or `String.prototype`.
    // Instead, we expose a few string primitives required by the curated WPT corpus (e.g.
    // `atob(...).length` and `.charCodeAt(...)`).
    let char_code_at = self.alloc_native_function(native_string_char_code_at)?;
    self.string_char_code_at = Some(char_code_at);
    Ok(())
  }

  fn install_url_shims(&mut self) -> Result<(), JsError> {
    // URLSearchParams prototype.
    let params_proto = self.alloc_object()?;
    let get_fn = self.alloc_native_function(native_urlsearchparams_get)?;
    let append_fn = self.alloc_native_function(native_urlsearchparams_append)?;
    let to_string_fn = self.alloc_native_function(native_urlsearchparams_to_string)?;
    let get_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("get")?)
    };
    let append_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("append")?)
    };
    let to_string_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("toString")?)
    };
    self.define_data_prop(params_proto, get_key, Value::Object(get_fn))?;
    self.define_data_prop(params_proto, append_key, Value::Object(append_fn))?;
    self.define_data_prop(params_proto, to_string_key, Value::Object(to_string_fn))?;
    self.url_search_params_proto = Some(params_proto);

    // URLSearchParams constructor.
    let params_ctor = self.alloc_native_function(native_urlsearchparams_ctor)?;
    let prototype_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("prototype")?)
    };
    self.define_data_prop(params_ctor, prototype_key, Value::Object(params_proto))?;
    self.env.set("URLSearchParams", Value::Object(params_ctor));
    let params_global_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("URLSearchParams")?)
    };
    self.define_data_prop(self.global_object, params_global_key, Value::Object(params_ctor))?;

    // URL prototype.
    let url_proto = self.alloc_object()?;
    let href_get = self.alloc_native_function(native_url_get_href)?;
    let search_get = self.alloc_native_function(native_url_get_search)?;
    let search_set = self.alloc_native_function(native_url_set_search)?;
    let search_params_get = self.alloc_native_function(native_url_get_search_params)?;
    let href_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("href")?)
    };
    let search_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("search")?)
    };
    let search_params_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("searchParams")?)
    };
    self.define_accessor_prop(url_proto, href_key, Value::Object(href_get), Value::Undefined)?;
    self.define_accessor_prop(
      url_proto,
      search_key,
      Value::Object(search_get),
      Value::Object(search_set),
    )?;
    self.define_accessor_prop(
      url_proto,
      search_params_key,
      Value::Object(search_params_get),
      Value::Undefined,
    )?;
    self.url_proto = Some(url_proto);

    // URL constructor.
    let url_ctor = self.alloc_native_function(native_url_ctor)?;
    let prototype_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("prototype")?)
    };
    self.define_data_prop(url_ctor, prototype_key, Value::Object(url_proto))?;
    self.env.set("URL", Value::Object(url_ctor));
    let url_global_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("URL")?)
    };
    self.define_data_prop(self.global_object, url_global_key, Value::Object(url_ctor))?;

    Ok(())
  }

  fn install_fetch_shims(&mut self) -> Result<(), JsError> {
    // __fastrender_resolve_url helper.
    let resolve_fn = self.alloc_native_function(native_fastrender_resolve_url)?;
    self.env.set("__fastrender_resolve_url", Value::Object(resolve_fn));
    let resolve_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("__fastrender_resolve_url")?)
    };
    self.define_data_prop(self.global_object, resolve_key, Value::Object(resolve_fn))?;

    // Request constructor.
    let request_proto = self.alloc_object()?;
    self.request_proto = Some(request_proto);
    let request_ctor = self.alloc_native_function(native_request_ctor)?;
    let prototype_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("prototype")?)
    };
    self.define_data_prop(request_ctor, prototype_key, Value::Object(request_proto))?;
    self.env.set("Request", Value::Object(request_ctor));
    let request_global_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("Request")?)
    };
    self.define_data_prop(self.global_object, request_global_key, Value::Object(request_ctor))?;

    // Response constructor.
    let response_proto = self.alloc_object()?;
    self.response_proto = Some(response_proto);
    let response_ctor = self.alloc_native_function(native_response_ctor)?;
    let prototype_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("prototype")?)
    };
    self.define_data_prop(response_ctor, prototype_key, Value::Object(response_proto))?;
    self.env.set("Response", Value::Object(response_ctor));
    let response_global_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("Response")?)
    };
    self.define_data_prop(
      self.global_object,
      response_global_key,
      Value::Object(response_ctor),
    )?;

    // fetch function.
    let fetch_fn = self.alloc_native_function(native_fetch)?;
    self.env.set("fetch", Value::Object(fetch_fn));
    let fetch_global_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("fetch")?)
    };
    self.define_data_prop(self.global_object, fetch_global_key, Value::Object(fetch_fn))?;

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
    self.event_target_proto = Some(et_proto);

    // Treat the global object as a Window-ish EventTarget so events can bubble through it.
    self.event_targets.insert(
      self.global_object,
      EventTargetState {
        parent: None,
        listeners: HashMap::new(),
      },
    );
    self.heap.object_set_prototype(self.global_object, Some(et_proto))?;

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

  fn define_accessor_prop(
    &mut self,
    obj: GcObject,
    key: PropertyKey,
    get: Value,
    set: Value,
  ) -> Result<(), JsError> {
    let desc = PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: PropertyKind::Accessor { get, set },
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

  fn dom_element_proto(&self) -> Result<GcObject, JsError> {
    self
      .dom_element_proto
      .ok_or_else(|| JsError::Vm(VmError::Unimplemented("DOM Element prototype")))
  }

  fn dom_token_list_proto(&self) -> Result<GcObject, JsError> {
    self
      .dom_token_list_proto
      .ok_or_else(|| JsError::Vm(VmError::Unimplemented("DOMTokenList prototype")))
  }

  fn document_fragment_proto(&self) -> Result<GcObject, JsError> {
    self
      .document_fragment_proto
      .ok_or_else(|| JsError::Vm(VmError::Unimplemented("DOM DocumentFragment prototype")))
  }

  fn text_proto(&self) -> Result<GcObject, JsError> {
    self
      .text_proto
      .ok_or_else(|| JsError::Vm(VmError::Unimplemented("DOM Text prototype")))
  }

  fn alloc_dom_element(&mut self, tag_name: &str) -> Result<GcObject, JsError> {
    let obj = self.alloc_object()?;
    let proto = self.dom_element_proto()?;
    self.heap.object_set_prototype(obj, Some(proto))?;

    let tag_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("tagName")?)
    };
    // HTML `tagName` is ASCII uppercased in browsers.
    let tag_value = self.alloc_string_value(&tag_name.to_ascii_uppercase())?;
    self.define_data_prop(obj, tag_key, tag_value)?;

    // Node metadata.
    let node_type_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("nodeType")?)
    };
    let node_name_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("nodeName")?)
    };
    let node_value_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("nodeValue")?)
    };
    self.define_data_prop(obj, node_type_key, Value::Number(1.0))?;
    self.define_data_prop(obj, node_name_key, tag_value)?;
    self.define_data_prop(obj, node_value_key, Value::Null)?;

    // Minimal `parentNode` bookkeeping as a data property (this harness does not support accessor
    // properties yet).
    let parent_node_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("parentNode")?)
    };
    self.define_data_prop(obj, parent_node_key, Value::Null)?;

    // Treat DOM elements as `EventTarget`s so they can participate in event dispatch paths.
    self.event_targets.insert(
      obj,
      EventTargetState {
        parent: None,
        listeners: HashMap::new(),
      },
    );

    self.dom_nodes.insert(
      obj,
      DomNodeState {
        kind: DomNodeKind::Element,
        tag_name: tag_name.to_ascii_lowercase(),
        attributes: HashMap::new(),
        text_content: None,
        parent: None,
        children: Vec::new(),
        template_content: Vec::new(),
        child_nodes: None,
        class_list: None,
      },
    );

    self.update_dom_child_nodes(obj)?;
    Ok(obj)
  }

  fn alloc_dom_document_fragment(&mut self) -> Result<GcObject, JsError> {
    let obj = self.alloc_object()?;
    // DocumentFragment is a Node but not an Element.
    let proto = self.document_fragment_proto()?;
    self.heap.object_set_prototype(obj, Some(proto))?;

    // Node metadata.
    let node_type_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("nodeType")?)
    };
    let node_name_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("nodeName")?)
    };
    let node_value_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("nodeValue")?)
    };
    let fragment_node_name = self.alloc_string_value("#document-fragment")?;
    self.define_data_prop(obj, node_type_key, Value::Number(11.0))?;
    self.define_data_prop(obj, node_name_key, fragment_node_name)?;
    self.define_data_prop(obj, node_value_key, Value::Null)?;

    // Like other DOM nodes, fragments participate in event dispatch paths, but they start detached.
    self.event_targets.insert(
      obj,
      EventTargetState {
        parent: None,
        listeners: HashMap::new(),
      },
    );

    let parent_node_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("parentNode")?)
    };
    self.define_data_prop(obj, parent_node_key, Value::Null)?;

    self.dom_nodes.insert(
      obj,
      DomNodeState {
        kind: DomNodeKind::DocumentFragment,
        tag_name: String::new(),
        attributes: HashMap::new(),
        text_content: None,
        parent: None,
        children: Vec::new(),
        template_content: Vec::new(),
        child_nodes: None,
        class_list: None,
      },
    );
    self.update_dom_child_nodes(obj)?;
    Ok(obj)
  }

  fn alloc_dom_text_node(&mut self, data: &str) -> Result<GcObject, JsError> {
    let obj = self.alloc_object()?;
    let proto = self.text_proto()?;
    self.heap.object_set_prototype(obj, Some(proto))?;

    // Node metadata.
    let node_type_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("nodeType")?)
    };
    let node_name_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("nodeName")?)
    };
    let node_value_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("nodeValue")?)
    };
    let text_node_name = self.alloc_string_value("#text")?;
    let text_node_value = self.alloc_string_value(data)?;
    self.define_data_prop(obj, node_type_key, Value::Number(3.0))?;
    self.define_data_prop(obj, node_name_key, text_node_name)?;
    // Keep nodeValue in sync with `data` for the minimal harness.
    self.define_data_prop(obj, node_value_key, text_node_value)?;

    // Like other DOM nodes, text nodes participate in event dispatch paths, but start detached.
    self.event_targets.insert(
      obj,
      EventTargetState {
        parent: None,
        listeners: HashMap::new(),
      },
    );

    let parent_node_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("parentNode")?)
    };
    self.define_data_prop(obj, parent_node_key, Value::Null)?;

    self.dom_nodes.insert(
      obj,
      DomNodeState {
        kind: DomNodeKind::Text,
        tag_name: String::new(),
        attributes: HashMap::new(),
        text_content: if data.is_empty() { None } else { Some(data.to_string()) },
        parent: None,
        children: Vec::new(),
        template_content: Vec::new(),
        child_nodes: None,
        class_list: None,
      },
    );
    self.update_dom_child_nodes(obj)?;
    Ok(obj)
  }

  fn make_dom_nodelist(&mut self, nodes: &[GcObject]) -> Result<GcObject, JsError> {
    let list = self.alloc_object()?;

    for (idx, &node) in nodes.iter().enumerate() {
      let key = {
        let mut scope = self.heap.scope();
        PropertyKey::from_string(scope.alloc_string(&idx.to_string())?)
      };
      self.define_data_prop(list, key, Value::Object(node))?;
    }

    let length_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("length")?)
    };
    self.define_data_prop(list, length_key, Value::Number(nodes.len() as f64))?;
    Ok(list)
  }

  fn update_dom_child_nodes(&mut self, node: GcObject) -> Result<(), JsError> {
    let (tag_name, children, cached_list) = match self.dom_nodes.get(&node) {
      None => return Ok(()),
      Some(state) => (state.tag_name.clone(), state.children.clone(), state.child_nodes),
    };

    let visible_children = if tag_name == "template" {
      Vec::new()
    } else {
      children
    };

    let list = match cached_list {
      Some(list) => list,
      None => {
        let list = self.alloc_object()?;
        if let Some(state) = self.dom_nodes.get_mut(&node) {
          state.child_nodes = Some(list);
        }
        list
      }
    };

    let prev_sibling_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("previousSibling")?)
    };
    let next_sibling_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("nextSibling")?)
    };

    // Mutate the cached list in-place so stored references behave like a live NodeList.
    let length_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("length")?)
    };
    let old_len = match self.heap.get_property(list, &length_key)? {
      Some(desc) => match desc.kind {
        PropertyKind::Data { value, .. } => match value {
          Value::Number(n) if n.is_finite() && n.fract() == 0.0 && n >= 0.0 => n as usize,
          _ => 0,
        },
        PropertyKind::Accessor { .. } => 0,
      },
      None => 0,
    };
    for idx in 0..old_len {
      let key = {
        let mut scope = self.heap.scope();
        PropertyKey::from_string(scope.alloc_string(&idx.to_string())?)
      };
      // Clear sibling pointers for nodes that are no longer in the NodeList (or will be re-added
      // with updated neighbors).
      if let Some(desc) = self.heap.get_property(list, &key)? {
        if let PropertyKind::Data { value, .. } = desc.kind {
          if let Value::Object(child) = value {
            if self.dom_nodes.contains_key(&child) {
              // If the node has already been reparented elsewhere, do not touch its sibling
              // pointers. This matters for DocumentFragment insertion where we:
              // 1) move children into the new parent,
              // 2) update the new parent's `childNodes`/siblings,
              // 3) then clear the fragment's `childNodes`.
              //
              // In that sequence the moved children would appear in the fragment's old NodeList,
              // but their siblings should reflect the new parent.
              let current_parent = self.dom_nodes.get(&child).and_then(|s| s.parent);
              if current_parent.is_none() || current_parent == Some(node) {
                self.define_data_prop(child, prev_sibling_key, Value::Null)?;
                self.define_data_prop(child, next_sibling_key, Value::Null)?;
              }
            }
          }
        }
      }
      self.define_data_prop(list, key, Value::Undefined)?;
    }
    for (idx, &child) in visible_children.iter().enumerate() {
      let key = {
        let mut scope = self.heap.scope();
        PropertyKey::from_string(scope.alloc_string(&idx.to_string())?)
      };
      self.define_data_prop(list, key, Value::Object(child))?;
    }
    self.define_data_prop(list, length_key, Value::Number(visible_children.len() as f64))?;

    let child_nodes_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("childNodes")?)
    };
    self.define_data_prop(node, child_nodes_key, Value::Object(list))?;

    let first_child_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("firstChild")?)
    };
    let last_child_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("lastChild")?)
    };
    let first = visible_children.first().copied();
    let last = visible_children.last().copied();
    self.define_data_prop(
      node,
      first_child_key,
      first.map(Value::Object).unwrap_or(Value::Null),
    )?;
    self.define_data_prop(
      node,
      last_child_key,
      last.map(Value::Object).unwrap_or(Value::Null),
    )?;

    // Keep sibling pointers in sync for DOM mutation tests.
    for (idx, &child) in visible_children.iter().enumerate() {
      let prev = idx.checked_sub(1).and_then(|i| visible_children.get(i)).copied();
      let next = visible_children.get(idx + 1).copied();
      self.define_data_prop(
        child,
        prev_sibling_key,
        prev.map(Value::Object).unwrap_or(Value::Null),
      )?;
      self.define_data_prop(
        child,
        next_sibling_key,
        next.map(Value::Object).unwrap_or(Value::Null),
      )?;
    }
    Ok(())
  }

  fn promise_prototype(&self) -> Result<GcObject, JsError> {
    self.promise_prototype.ok_or_else(|| JsError::Vm(VmError::Unimplemented("Promise prototype")))
  }

  fn promise_job_runner(&self) -> Result<GcObject, JsError> {
    self
      .promise_job_runner
      .ok_or_else(|| JsError::Vm(VmError::Unimplemented("Promise job runner")))
  }

  fn event_target_proto(&self) -> Result<GcObject, JsError> {
    self
      .event_target_proto
      .ok_or_else(|| JsError::Vm(VmError::Unimplemented("EventTarget prototype")))
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

  /// Resolves a Promise with `value`, adopting other vm-js Promise objects.
  ///
  /// This implements the bit of the ECMAScript Promise resolution procedure that matters for our
  /// curated WPT corpus: when a `.then` handler returns a Promise, the downstream Promise must
  /// "follow" it instead of being fulfilled with the Promise object itself.
  fn resolve_promise(&mut self, promise: GcObject, value: Value) -> Result<(), JsError> {
    let Some(status) = self.promises.get(&promise).map(|state| state.status) else {
      return Err(JsError::Vm(VmError::Throw(
        self.alloc_string_value("TypeError: not a Promise")?,
      )));
    };
    if status != PromiseStatus::Pending {
      return Ok(());
    }

    if let Value::Object(obj) = value {
      if self.promises.contains_key(&obj) {
        if obj == promise {
          let reason = self.alloc_string_value("TypeError: promise resolved with itself")?;
          return self.settle_promise(promise, PromiseStatus::Rejected, reason);
        }

        let (status, settled_value) = self
          .promises
          .get(&obj)
          .map(|state| (state.status, state.value))
          .expect("promise present");
        let reaction = PromiseReaction {
          on_fulfilled: Value::Undefined,
          on_rejected: Value::Undefined,
          next_promise: promise,
        };
        match status {
          PromiseStatus::Pending => {
            let state = self.promises.get_mut(&obj).expect("promise present");
            state.reactions.push(reaction);
          }
          settled @ (PromiseStatus::Fulfilled | PromiseStatus::Rejected) => {
            self.enqueue_promise_job(PromiseJob {
              status: settled,
              value: settled_value,
              reaction,
            })?;
          }
        }
        return Ok(());
      }
    }

    self.settle_promise(promise, PromiseStatus::Fulfilled, value)
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
    self.current_source = Some(Rc::from(source));
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
    let source = self
      .current_source
      .clone()
      .ok_or_else(|| JsError::Vm(VmError::Unimplemented("missing source text")))?;
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

      let mut body_stmts = body_stmts;
      let hoisted_functions = self.hoist_function_decls(&mut body_stmts, source.clone())?;
      let func_obj = self.alloc_user_function(UserFunction {
        params,
        is_async: func_decl.stx.function.stx.async_,
        body: UserFunctionBody::Block(body_stmts),
        source: source.clone(),
        hoisted_functions,
      })?;
      self.env.set(&name, Value::Object(func_obj));
    }
    Ok(())
  }

  fn hoist_function_decls(
    &mut self,
    stmts: &mut [Node<Stmt>],
    source: Rc<str>,
  ) -> Result<Vec<(String, GcObject)>, JsError> {
    let mut out = Vec::new();
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
        return Err(JsError::Vm(VmError::Unimplemented(
          "arrow bodies not supported",
        )));
      };

      let mut body_stmts = body_stmts;
      let hoisted_functions = self.hoist_function_decls(&mut body_stmts, source.clone())?;
      let func_obj = self.alloc_user_function(UserFunction {
        params,
        is_async: func_decl.stx.function.stx.async_,
        body: UserFunctionBody::Block(body_stmts),
        source: source.clone(),
        hoisted_functions,
      })?;

      out.push((name, func_obj));
    }
    Ok(out)
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

  fn eval_expr_or_throw(&mut self, expr: &Node<Expr>) -> Result<Result<Value, Value>, JsError> {
    match self.eval_expr(expr) {
      Ok(v) => Ok(Ok(v)),
      Err(JsError::Vm(err)) => match err.thrown_value() {
        Some(v) => Ok(Err(v)),
        None => Err(JsError::Vm(err)),
      },
      Err(other) => Err(other),
    }
  }

  fn eval_expr_stmt(&mut self, stmt: &ExprStmt) -> Result<Completion, JsError> {
    match self.eval_expr_or_throw(&stmt.expr)? {
      Ok(value) => Ok(Completion::normal(value)),
      Err(thrown) => Ok(Completion::Throw(thrown)),
    }
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
          let value = match self.eval_expr_or_throw(init)? {
            Ok(v) => v,
            Err(thrown) => return Ok(Completion::Throw(thrown)),
          };
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
            Some(init) => match self.eval_expr_or_throw(init)? {
              Ok(v) => v,
              Err(thrown) => return Ok(Completion::Throw(thrown)),
            },
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
    let test = match self.eval_expr_or_throw(&stmt.test)? {
      Ok(v) => v,
      Err(thrown) => return Ok(Completion::Throw(thrown)),
    };
    if to_boolean(&mut self.heap, test)? {
      self.eval_stmt(&stmt.consequent)
    } else if let Some(alt) = &stmt.alternate {
      self.eval_stmt(alt)
    } else {
      Ok(Completion::empty())
    }
  }

  fn eval_throw(&mut self, stmt: &ThrowStmt) -> Result<Completion, JsError> {
    let value = match self.eval_expr_or_throw(&stmt.value)? {
      Ok(v) => v,
      Err(thrown) => return Ok(Completion::Throw(thrown)),
    };
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
      Some(expr) => match self.eval_expr_or_throw(expr)? {
        Ok(v) => v,
        Err(thrown) => return Ok(Completion::Throw(thrown)),
      },
      None => Value::Undefined,
    };
    Ok(Completion::Return(value))
  }

  fn eval_while(&mut self, stmt: &WhileStmt) -> Result<Completion, JsError> {
    loop {
      self.vm.tick()?;
      let test = match self.eval_expr_or_throw(&stmt.condition)? {
        Ok(v) => v,
        Err(thrown) => return Ok(Completion::Throw(thrown)),
      };
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

      let test = match self.eval_expr_or_throw(&stmt.condition)? {
        Ok(v) => v,
        Err(thrown) => return Ok(Completion::Throw(thrown)),
      };
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
        match self.eval_expr_or_throw(expr)? {
          Ok(_) => {}
          Err(thrown) => return Ok(Completion::Throw(thrown)),
        }
      }
      parse_js::ast::stmt::ForTripleStmtInit::Decl(decl) => {
        let completion = self.eval_var_decl(&decl.stx)?;
        if completion.is_abrupt() {
          return Ok(completion);
        }
      }
    }

    loop {
      self.vm.tick()?;

      if let Some(cond) = &stmt.cond {
        let test = match self.eval_expr_or_throw(cond)? {
          Ok(v) => v,
          Err(thrown) => return Ok(Completion::Throw(thrown)),
        };
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
        match self.eval_expr_or_throw(post)? {
          Ok(_) => {}
          Err(thrown) => return Ok(Completion::Throw(thrown)),
        }
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
      Expr::LitArr(node) => self.eval_lit_arr(&node.stx),
      Expr::LitNum(node) => self.eval_lit_num(&node.stx),
      Expr::LitBool(node) => self.eval_lit_bool(&node.stx),
      Expr::LitNull(_node) => self.eval_lit_null(),
      Expr::This(_node) => Ok(self.this_binding),
      Expr::Id(node) => self.eval_id(&node.stx),
      Expr::Binary(node) => self.eval_binary(&node.stx),
      Expr::Member(node) => self.eval_member(&node.stx),
      Expr::ComputedMember(node) => self.eval_computed_member(&node.stx),
      Expr::Call(node) => self.eval_call(&node.stx),
      Expr::Unary(node) => self.eval_unary(&node.stx),
      Expr::UnaryPostfix(node) => self.eval_unary_postfix(&node.stx),
      Expr::ArrowFunc(node) => self.eval_arrow_func(node),
      Expr::LitObj(node) => self.eval_lit_obj(&node.stx),
      Expr::IdPat(node) => self.eval_id_pat(&node.stx),
      _ => Err(JsError::Vm(VmError::Unimplemented("expression type"))),
    }
  }

  fn eval_lit_arr(&mut self, expr: &LitArrExpr) -> Result<Value, JsError> {
    let proto = self
      .array_prototype
      .ok_or_else(|| JsError::Vm(VmError::Unimplemented("Array prototype")))?;
    let obj = self.alloc_object()?;
    self.heap.object_set_prototype(obj, Some(proto))?;

    let mut elements = Vec::<Value>::with_capacity(expr.elements.len());
    for elem in &expr.elements {
      use parse_js::ast::expr::lit::LitArrElem;
      match elem {
        LitArrElem::Single(value) => elements.push(self.eval_expr(value)?),
        LitArrElem::Empty => elements.push(Value::Undefined),
        LitArrElem::Rest(_) => return Err(JsError::Vm(VmError::Unimplemented("array spread"))),
      }
    }

    // Materialize dense arrays as ordinary objects with numeric string keys plus a `length` data
    // property. The backing vector is used by `push`/`join` for now.
    self.arrays.insert(obj, elements.clone());

    let length_key = {
      let mut scope = self.heap.scope();
      PropertyKey::from_string(scope.alloc_string("length")?)
    };
    self.define_data_prop(obj, length_key, Value::Number(elements.len() as f64))?;

    for (idx, value) in elements.iter().enumerate() {
      let key = {
        let mut scope = self.heap.scope();
        PropertyKey::from_string(scope.alloc_string(&idx.to_string())?)
      };
      self.define_data_prop(obj, key, *value)?;
    }

    Ok(Value::Object(obj))
  }

  fn eval_unary_postfix(&mut self, expr: &UnaryPostfixExpr) -> Result<Value, JsError> {
    match expr.operator {
      OperatorName::PostfixIncrement => {
        let old = self.eval_expr(&expr.argument)?;
        let Value::Number(n) = old else {
          return Err(JsError::Vm(VmError::Unimplemented(
            "postfix ++ only supports numbers",
          )));
        };
        let new = Value::Number(n + 1.0);
        self.assign_to(&expr.argument, new)?;
        Ok(old)
      }
      OperatorName::PostfixDecrement => {
        let old = self.eval_expr(&expr.argument)?;
        let Value::Number(n) = old else {
          return Err(JsError::Vm(VmError::Unimplemented(
            "postfix -- only supports numbers",
          )));
        };
        let new = Value::Number(n - 1.0);
        self.assign_to(&expr.argument, new)?;
        Ok(old)
      }
      _ => Err(JsError::Vm(VmError::Unimplemented("unary postfix operator"))),
    }
  }

  fn eval_arrow_func(&mut self, node: &Node<ArrowFuncExpr>) -> Result<Value, JsError> {
    let source = self
      .current_source
      .as_ref()
      .ok_or_else(|| JsError::Vm(VmError::Unimplemented("missing source text")))?;
    let start = node.loc.0;
    let end = node.loc.1;
    let raw_snippet = source
      .get(start..end)
      .ok_or_else(|| JsError::Vm(VmError::Unimplemented("invalid arrow function span")))?;

    // `parse-js` nodes are not cheaply cloneable, so we re-parse the arrow function expression into
    // an owned AST to store inside the function object.
    let opts = ParseOptions {
      dialect: Dialect::Ecma,
      source_type: SourceType::Script,
    };
    let mut snippet = raw_snippet.trim_end();
    let mut parsed = loop {
      match parse_with_options(snippet, opts) {
        Ok(ast) => break ast,
        Err(err) => {
          // Some parse-js nodes (notably arrow functions in argument position) currently report a
          // span that includes delimiter tokens (e.g. `,)`). Trim common delimiters and retry so we
          // can still materialize the function object.
          let trimmed = snippet.trim_end();
          let last = trimmed.chars().last();
          match last {
            Some(')') | Some(',') | Some(';') => {
              snippet = trimmed
                .get(..trimmed.len().saturating_sub(1))
                .unwrap_or("")
                .trim_end();
              if snippet.is_empty() {
                let mut preview = raw_snippet.replace('\n', "\\n");
                if preview.len() > 120 {
                  preview.truncate(120);
                  preview.push_str("…");
                }
                return Err(JsError::Parse(format!(
                  "arrow function parse failed: {err} (snippet={preview})"
                )));
              }
              continue;
            }
            _ => {
              let mut preview = raw_snippet.replace('\n', "\\n");
              if preview.len() > 120 {
                preview.truncate(120);
                preview.push_str("…");
              }
              return Err(JsError::Parse(format!(
                "arrow function parse failed: {err} (snippet={preview})"
              )));
            }
          }
        }
      }
    };

    if parsed.stx.body.len() != 1 {
      return Err(JsError::Vm(VmError::Unimplemented(
        "arrow function parse produced multiple statements",
      )));
    }

    let stmt = parsed.stx.body.pop().expect("single statement");
    let Stmt::Expr(expr_stmt) = *stmt.stx else {
      return Err(JsError::Vm(VmError::Unimplemented(
        "arrow function parse did not yield an expression statement",
      )));
    };
    let expr = (*expr_stmt.stx).expr;
    let Expr::ArrowFunc(arrow) = *expr.stx else {
      return Err(JsError::Vm(VmError::Unimplemented(
        "arrow function parse did not yield an arrow expression",
      )));
    };

    let mut func = *arrow.stx.func.stx;
    let is_async = func.async_;
    let params = func
      .parameters
      .iter()
      .filter_map(|p| simple_binding_identifier(&p.stx.pattern.stx).ok().flatten())
      .map(|s| s.to_string())
      .collect::<Vec<_>>();
    let Some(body) = func.body.take() else {
      return Err(JsError::Vm(VmError::Unimplemented(
        "arrow function without a body",
      )));
    };
    let source = Rc::<str>::from(snippet);
    let (body, hoisted_functions) = match body {
      FuncBody::Block(mut stmts) => {
        let hoisted = self.hoist_function_decls(&mut stmts, source.clone())?;
        (UserFunctionBody::Block(stmts), hoisted)
      }
      FuncBody::Expression(expr) => (UserFunctionBody::Expression(expr), Vec::new()),
    };

    let func_obj = self.alloc_user_function(UserFunction {
      params,
      is_async,
      body,
      source,
      hoisted_functions,
    })?;

    Ok(Value::Object(func_obj))
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
    let obj_value = self.eval_expr(&expr.left)?;
    match obj_value {
      Value::Object(obj) => {
        let key = {
          let mut scope = self.heap.scope();
          PropertyKey::from_string(scope.alloc_string(&expr.right)?)
        };
        let Some(desc) = self.heap.get_property(obj, &key)? else {
          return Ok(Value::Undefined);
        };
        match desc.kind {
          PropertyKind::Data { value, .. } => Ok(value),
          PropertyKind::Accessor { get, .. } => {
            if matches!(get, Value::Undefined) {
              return Ok(Value::Undefined);
            }
            if !self.is_callable_value(get) {
              return Err(JsError::Vm(VmError::TypeError("accessor getter is not callable")));
            }
            self.call(get, Value::Object(obj), &[])
          }
        }
      }
      Value::String(s) => match expr.right.as_str() {
        "length" => {
          let len = self.heap.get_string(s)?.as_code_units().len();
          Ok(Value::Number(len as f64))
        }
        "charCodeAt" => {
          let Some(func) = self.string_char_code_at else {
            return Err(JsError::Vm(VmError::Unimplemented("String.charCodeAt")));
          };
          Ok(Value::Object(func))
        }
        _ => Ok(Value::Undefined),
      },
      _ => Err(JsError::Vm(VmError::Unimplemented("member access on non-object"))),
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
      PropertyKind::Accessor { get, .. } => {
        if matches!(get, Value::Undefined) {
          return Ok(Value::Undefined);
        }
        if !self.is_callable_value(get) {
          return Err(JsError::Vm(VmError::TypeError("accessor getter is not callable")));
        }
        self.call(get, Value::Object(obj), &[])
      }
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
        match this {
          Value::Object(obj) => {
            let key = {
              let mut scope = self.heap.scope();
              PropertyKey::from_string(scope.alloc_string(&member.stx.right)?)
            };
            let value = match self.heap.get_property(obj, &key)? {
              Some(desc) => match desc.kind {
                PropertyKind::Data { value, .. } => value,
                PropertyKind::Accessor { get, .. } => {
                  if matches!(get, Value::Undefined) {
                    Value::Undefined
                  } else {
                    if !self.is_callable_value(get) {
                      return Err(JsError::Vm(VmError::TypeError("accessor getter is not callable")));
                    }
                    self.call(get, Value::Object(obj), &[])?
                  }
                }
              },
              None => Value::Undefined,
            };
            Ok((value, Value::Object(obj)))
          }
          Value::String(_) => {
            let value = match member.stx.right.as_str() {
              "charCodeAt" => {
                let Some(func) = self.string_char_code_at else {
                  return Err(JsError::Vm(VmError::Unimplemented("String.charCodeAt")));
                };
                Value::Object(func)
              }
              _ => Value::Undefined,
            };
            Ok((value, this))
          }
          _ => Err(JsError::Vm(VmError::NotCallable)),
        }
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
            PropertyKind::Accessor { get, .. } => {
              if matches!(get, Value::Undefined) {
                Value::Undefined
              } else {
                if !self.is_callable_value(get) {
                  return Err(JsError::Vm(VmError::TypeError("accessor getter is not callable")));
                }
                self.call(get, Value::Object(obj), &[])?
              }
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

  fn call_user_function(
    &mut self,
    this: Value,
    args: &[Value],
    func: &UserFunction,
  ) -> Result<Value, JsError> {
    self.env.push_frame();
    let previous_this = self.this_binding;
    self.this_binding = this;
    let previous_source = self.current_source.clone();
    self.current_source = Some(func.source.clone());

    let result = (|| -> Result<Value, JsError> {
      for (idx, name) in func.params.iter().enumerate() {
        self.env.declare_var(name);
        let value = args.get(idx).copied().unwrap_or(Value::Undefined);
        self.env.set(name, value);
      }
      for (name, func_obj) in &func.hoisted_functions {
        self.env.declare_var(name);
        self.env.set(name, Value::Object(*func_obj));
      }
      match &func.body {
        UserFunctionBody::Block(body) => {
          self.hoist_var_decls(body)?;
          self.eval_stmt_list(body)
        }
        UserFunctionBody::Expression(expr) => self.eval_expr(expr),
      }
    })();

    let result = if func.is_async {
      match result {
        Ok(v) => {
          if let Value::Object(obj) = v {
            if self.promises.contains_key(&obj) {
              Ok(Value::Object(obj))
            } else {
              Ok(Value::Object(self.alloc_promise_with_state(PromiseStatus::Fulfilled, v)?))
            }
          } else {
            Ok(Value::Object(self.alloc_promise_with_state(PromiseStatus::Fulfilled, v)?))
          }
        }
        Err(JsError::Vm(VmError::Throw(v))) => {
          Ok(Value::Object(self.alloc_promise_with_state(PromiseStatus::Rejected, v)?))
        }
        Err(other) => Err(other),
      }
    } else {
      result
    };

    self.this_binding = previous_this;
    self.current_source = previous_source;
    self.env.pop_frame();
    result
  }

  fn eval_unary(&mut self, expr: &UnaryExpr) -> Result<Value, JsError> {
    match expr.operator {
      OperatorName::LogicalNot => {
        let arg = self.eval_expr(&expr.argument)?;
        Ok(Value::Bool(!to_boolean(&mut self.heap, arg)?))
      }
      OperatorName::Await => {
        let value = self.eval_expr(&expr.argument)?;
        self.await_value(value)
      }
      OperatorName::Void => {
        // Evaluate operand for side effects.
        let _ = self.eval_expr(&expr.argument)?;
        Ok(Value::Undefined)
      }
      OperatorName::UnaryPlus => {
        let arg = self.eval_expr(&expr.argument)?;
        Ok(Value::Number(self.heap.to_number(arg)?))
      }
      OperatorName::UnaryNegation => {
        let arg = self.eval_expr(&expr.argument)?;
        Ok(Value::Number(-self.heap.to_number(arg)?))
      }
      OperatorName::BitwiseNot => {
        let arg = self.eval_expr(&expr.argument)?;
        let n = self.heap.to_number(arg)?;
        Ok(Value::Number((!to_int32(n)) as f64))
      }
      OperatorName::PrefixIncrement => {
        let old = self.eval_expr(&expr.argument)?;
        let Value::Number(n) = old else {
          return Err(JsError::Vm(VmError::Unimplemented(
            "prefix ++ only supports numbers",
          )));
        };
        let new = Value::Number(n + 1.0);
        self.assign_to(&expr.argument, new)?;
        Ok(new)
      }
      OperatorName::PrefixDecrement => {
        let old = self.eval_expr(&expr.argument)?;
        let Value::Number(n) = old else {
          return Err(JsError::Vm(VmError::Unimplemented(
            "prefix -- only supports numbers",
          )));
        };
        let new = Value::Number(n - 1.0);
        self.assign_to(&expr.argument, new)?;
        Ok(new)
      }
      OperatorName::Delete => self.eval_delete(&expr.argument),
      OperatorName::Typeof => {
        // `typeof` is special: it does not throw for unbound identifiers.
        let value = match &*expr.argument.stx {
          Expr::Id(id) => self.env.get(&id.stx.name).unwrap_or(Value::Undefined),
          _ => self.eval_expr(&expr.argument)?,
        };

        let kind = match value {
          Value::Undefined => "undefined",
          Value::Null => "object",
          Value::Bool(_) => "boolean",
          Value::Number(_) => "number",
          Value::BigInt(_) => "bigint",
          Value::String(_) => "string",
          Value::Symbol(_) => "symbol",
          Value::Object(obj) => {
            if self.callables.contains_key(&obj) {
              "function"
            } else {
              "object"
            }
          }
        };
        self.alloc_string_value(kind)
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

  fn await_value(&mut self, value: Value) -> Result<Value, JsError> {
    let Value::Object(obj) = value else {
      return Ok(value);
    };
    if !self.promises.contains_key(&obj) {
      return Ok(Value::Object(obj));
    }
    self.await_promise(obj)
  }

  fn await_promise(&mut self, promise: GcObject) -> Result<Value, JsError> {
    loop {
      self.vm.tick()?;

      let Some((status, value)) = self.promises.get(&promise).map(|s| (s.status, s.value)) else {
        return Err(JsError::Vm(VmError::Throw(self.alloc_string_value(
          "TypeError: awaited value is not a Promise",
        )?)));
      };

      match status {
        PromiseStatus::Fulfilled => return Ok(value),
        PromiseStatus::Rejected => return Err(JsError::Vm(VmError::Throw(value))),
        PromiseStatus::Pending => {}
      }

      let mut progressed = false;

      // A minimal (blocking) await implementation: keep pumping microtasks/tasks until the promise
      // settles. This is not a full async function suspension model, but it's sufficient for the
      // curated offline WPT DOM smoke tests.
      while let Some((cb, this, args)) = self.event_loop.drain_microtasks() {
        progressed = true;
        self.call(cb, this, &args)?;
      }

      self.event_loop.enqueue_due_timers();
      if let Some((cb, this, args)) = self.event_loop.pop_next_task() {
        progressed = true;
        self.call(cb, this, &args)?;
      }

      if !progressed {
        if let Some(next_due) = self.event_loop.next_timer_due_time() {
          if next_due > self.event_loop.now {
            self.event_loop.now = next_due;
            progressed = true;
          }
        }
      }

      if !progressed {
        return Err(JsError::Vm(VmError::Unimplemented("await pending promise")));
      }
    }
  }

  fn eval_delete(&mut self, operand: &Node<Expr>) -> Result<Value, JsError> {
    match &*operand.stx {
      Expr::Member(member) => {
        let obj_value = self.eval_expr(&member.stx.left)?;
        let Value::Object(obj) = obj_value else {
          return Ok(Value::Bool(true));
        };
        let key = {
          let mut scope = self.heap.scope();
          PropertyKey::from_string(scope.alloc_string(&member.stx.right)?)
        };
        Ok(Value::Bool(self.heap.ordinary_delete(obj, key)?))
      }
      Expr::ComputedMember(member) => {
        let obj_value = self.eval_expr(&member.stx.object)?;
        let Value::Object(obj) = obj_value else {
          return Ok(Value::Bool(true));
        };
        let member_value = self.eval_expr(&member.stx.member)?;
        let key = self.value_to_property_key(member_value)?;
        Ok(Value::Bool(self.heap.ordinary_delete(obj, key)?))
      }
      Expr::Id(_) | Expr::IdPat(_) => Ok(Value::Bool(false)),
      _ => {
        // `delete` on non-reference expressions returns true after evaluating the operand.
        let _ = self.eval_expr(operand)?;
        Ok(Value::Bool(true))
      }
    }
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
      OperatorName::LogicalAnd => {
        let left = self.eval_expr(&expr.left)?;
        if !to_boolean(&mut self.heap, left)? {
          return Ok(left);
        }
        self.eval_expr(&expr.right)
      }
      OperatorName::LogicalOr => {
        let left = self.eval_expr(&expr.left)?;
        if to_boolean(&mut self.heap, left)? {
          return Ok(left);
        }
        self.eval_expr(&expr.right)
      }
      OperatorName::Instanceof => {
        let left = self.eval_expr(&expr.left)?;
        let right = self.eval_expr(&expr.right)?;

        let Value::Object(ctor) = right else {
          return Err(JsError::Vm(VmError::TypeError(
            "right-hand side of 'instanceof' is not an object",
          )));
        };
        if !self.callables.contains_key(&ctor) {
          return Err(JsError::Vm(VmError::NotCallable));
        }

        // OrdinaryHasInstance: look up `ctor.prototype` and walk the prototype chain of `left`.
        let prototype_key = {
          let mut scope = self.heap.scope();
          PropertyKey::from_string(scope.alloc_string("prototype")?)
        };
        let Some(desc) = self.heap.get_property(ctor, &prototype_key)? else {
          return Err(JsError::Vm(VmError::TypeError(
            "'instanceof' constructor has no prototype",
          )));
        };
        let proto_value = match desc.kind {
          PropertyKind::Data { value, .. } => value,
          PropertyKind::Accessor { get, .. } => {
            if matches!(get, Value::Undefined) {
              Value::Undefined
            } else {
              if !self.is_callable_value(get) {
                return Err(JsError::Vm(VmError::TypeError("accessor getter is not callable")));
              }
              self.call(get, Value::Object(ctor), &[])?
            }
          }
        };
        let Value::Object(expected_proto) = proto_value else {
          return Err(JsError::Vm(VmError::TypeError(
            "'instanceof' prototype is not an object",
          )));
        };

        let Value::Object(obj) = left else {
          // Per spec, primitives are never instances of object constructors.
          return Ok(Value::Bool(false));
        };

        let mut current = Some(obj);
        while let Some(cur) = current {
          let proto = self.heap.object_prototype(cur)?;
          if proto == Some(expected_proto) {
            return Ok(Value::Bool(true));
          }
          current = proto;
        }
        Ok(Value::Bool(false))
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
        if let Some(desc) = self.heap.get_property(obj, &key)? {
          if let PropertyKind::Accessor { set, .. } = desc.kind {
            if matches!(set, Value::Undefined) {
              // Non-strict mode: setting a property with no setter is a no-op.
              return Ok(());
            }
            if !self.is_callable_value(set) {
              return Err(JsError::Vm(VmError::TypeError("accessor setter is not callable")));
            }
            let _ = self.call(set, Value::Object(obj), &[value])?;
            return Ok(());
          }
        }

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
        if let Some(desc) = self.heap.get_property(obj, &key)? {
          if let PropertyKind::Accessor { set, .. } = desc.kind {
            if matches!(set, Value::Undefined) {
              return Ok(());
            }
            if !self.is_callable_value(set) {
              return Err(JsError::Vm(VmError::TypeError("accessor setter is not callable")));
            }
            let _ = self.call(set, Value::Object(obj), &[value])?;
            return Ok(());
          }
        }

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
      Value::String(s) => self
        .heap
        .get_string(s)
        .map(|s| s.to_utf8_lossy())
        .unwrap_or_else(|_| "<invalid string>".to_string()),
      Value::Symbol(_) => "[symbol]".to_string(),
      Value::Object(obj) => {
        // Best-effort stringification for thrown objects (e.g. `{name,message}` error-like shapes).
        // Avoid calling full `ToString`/`ToPrimitive` since this mini-runtime does not implement the
        // full object model.
        let formatted = (|| -> Result<String, VmError> {
          let name_key = {
            let mut scope = self.heap.scope();
            PropertyKey::from_string(scope.alloc_string("name")?)
          };
          let message_key = {
            let mut scope = self.heap.scope();
            PropertyKey::from_string(scope.alloc_string("message")?)
          };

          let name = match self.heap.get_property(obj, &name_key)? {
            Some(desc) => match desc.kind {
              PropertyKind::Data { value, .. } => match value {
                Value::String(s) => self.heap.get_string(s)?.to_utf8_lossy(),
                _ => String::new(),
              },
              PropertyKind::Accessor { .. } => String::new(),
            },
            None => String::new(),
          };

          let message = match self.heap.get_property(obj, &message_key)? {
            Some(desc) => match desc.kind {
              PropertyKind::Data { value, .. } => match value {
                Value::String(s) => self.heap.get_string(s)?.to_utf8_lossy(),
                _ => String::new(),
              },
              PropertyKind::Accessor { .. } => String::new(),
            },
            None => String::new(),
          };

          if !name.is_empty() && !message.is_empty() {
            Ok(format!("{name}: {message}"))
          } else if !name.is_empty() {
            Ok(name)
          } else if !message.is_empty() {
            Ok(message)
          } else {
            Ok("[object]".to_string())
          }
        })();

        formatted.unwrap_or_else(|_| "[object]".to_string())
      }
      Value::BigInt(b) => b.to_decimal_string(),
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

fn to_int32(n: f64) -> i32 {
  if !n.is_finite() || n == 0.0 {
    return 0;
  }
  let int = n.trunc();
  let two32 = 4_294_967_296.0_f64;
  let mut int32 = int % two32;
  if int32 < 0.0 {
    int32 += two32;
  }
  if int32 >= 2_147_483_648.0 {
    int32 -= two32;
  }
  int32 as i32
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

fn native_symbol(rt: &mut JsWptRuntime, _this: Value, args: &[Value]) -> Result<Value, JsError> {
  let desc_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let desc = if desc_value == Value::Undefined {
    None
  } else {
    let s = rt.heap.to_string(desc_value)?;
    Some(rt.heap.get_string(s)?.to_utf8_lossy())
  };
  let sym = {
    let mut scope = rt.heap.scope();
    scope.alloc_symbol(desc.as_deref())?
  };
  Ok(Value::Symbol(sym))
}

fn native_report_error(rt: &mut JsWptRuntime, _this: Value, args: &[Value]) -> Result<Value, JsError> {
  let value = args.get(0).copied().unwrap_or(Value::Undefined);
  // `reportError` must never throw. Our vm-js harness does not implement full `ToString`, so we use
  // the runner's best-effort stringification helper.
  let msg = rt.value_to_string_lossy(value);
  eprintln!("[wpt][reportError] {msg}");
  Ok(Value::Undefined)
}

fn parse_urlencoded_pairs(query: &str) -> Vec<(String, String)> {
  form_urlencoded::parse(query.as_bytes()).into_owned().collect()
}

fn serialize_urlencoded_pairs(pairs: &[(String, String)]) -> String {
  let mut serializer = form_urlencoded::Serializer::new(String::new());
  for (k, v) in pairs {
    serializer.append_pair(k, v);
  }
  serializer.finish()
}

fn native_url_ctor(rt: &mut JsWptRuntime, this: Value, args: &[Value]) -> Result<Value, JsError> {
  let Value::Object(url_obj) = this else {
    return dom_throw_named_error(rt, "TypeError", "URL constructor must be called with new");
  };
  let input_value = args.get(0).copied().unwrap_or(Value::Undefined);
  if matches!(input_value, Value::Undefined) {
    return dom_throw_named_error(rt, "TypeError", "URL constructor requires an input");
  }
  let input = rt.heap.to_string(input_value)?;
  let input = rt.heap.get_string(input)?.to_utf8_lossy();

  let url = match args.get(1).copied() {
    None | Some(Value::Undefined) => Url::parse(&input),
    Some(base_value) => {
      let base_s = rt.heap.to_string(base_value)?;
      let base_s = rt.heap.get_string(base_s)?.to_utf8_lossy();
      let base = Url::parse(&base_s);
      base.and_then(|b| b.join(&input))
    }
  };
  let url = match url {
    Ok(url) => url,
    Err(_) => return dom_throw_named_error(rt, "TypeError", "Invalid URL"),
  };

  let raw_query = url.query().unwrap_or("").to_string();
  rt.urls.insert(
    url_obj,
    UrlState {
      url,
      raw_query,
      search_params: None,
    },
  );
  Ok(Value::Undefined)
}

fn native_url_get_href(rt: &mut JsWptRuntime, this: Value, _args: &[Value]) -> Result<Value, JsError> {
  let Value::Object(url_obj) = this else {
    return dom_throw_named_error(rt, "TypeError", "URL.href getter called on non-object");
  };
  let Some(state) = rt.urls.get(&url_obj) else {
    return dom_throw_named_error(rt, "TypeError", "URL.href getter called on non-URL object");
  };
  let href = state.url.as_str().to_string();
  Ok(rt.alloc_string_value(&href)?)
}

fn native_url_get_search(rt: &mut JsWptRuntime, this: Value, _args: &[Value]) -> Result<Value, JsError> {
  let Value::Object(url_obj) = this else {
    return dom_throw_named_error(rt, "TypeError", "URL.search getter called on non-object");
  };
  let Some(state) = rt.urls.get(&url_obj) else {
    return dom_throw_named_error(rt, "TypeError", "URL.search getter called on non-URL object");
  };
  let out = if state.raw_query.is_empty() {
    String::new()
  } else {
    format!("?{}", state.raw_query)
  };
  Ok(rt.alloc_string_value(&out)?)
}

fn native_url_set_search(rt: &mut JsWptRuntime, this: Value, args: &[Value]) -> Result<Value, JsError> {
  let Value::Object(url_obj) = this else {
    return dom_throw_named_error(rt, "TypeError", "URL.search setter called on non-object");
  };
  let value = args.get(0).copied().unwrap_or(Value::Undefined);
  let mut raw_query = if value == Value::Undefined {
    String::new()
  } else {
    let s = rt.heap.to_string(value)?;
    rt.heap.get_string(s)?.to_utf8_lossy()
  };
  if let Some(rest) = raw_query.strip_prefix('?') {
    raw_query = rest.to_string();
  }

  let (params_obj, new_query_for_params) = {
    let Some(state) = rt.urls.get_mut(&url_obj) else {
      return dom_throw_named_error(rt, "TypeError", "URL.search setter called on non-URL object");
    };

    if raw_query.is_empty() {
      state.raw_query.clear();
      state.url.set_query(None);
    } else {
      state.raw_query = raw_query.clone();
      state.url.set_query(Some(&raw_query));
    }

    (state.search_params, state.raw_query.clone())
  };

  if let Some(params_obj) = params_obj {
    let pairs = parse_urlencoded_pairs(&new_query_for_params);
    if let Some(params_state) = rt.url_search_params.get_mut(&params_obj) {
      params_state.pairs = pairs;
    } else {
      rt.url_search_params.insert(
        params_obj,
        UrlSearchParamsState {
          url: Some(url_obj),
          pairs,
        },
      );
    }
  }

  Ok(Value::Undefined)
}

fn native_url_get_search_params(
  rt: &mut JsWptRuntime,
  this: Value,
  _args: &[Value],
) -> Result<Value, JsError> {
  let Value::Object(url_obj) = this else {
    return dom_throw_named_error(rt, "TypeError", "URL.searchParams getter called on non-object");
  };

  let raw_query;
  let existing_params;
  {
    let Some(state) = rt.urls.get(&url_obj) else {
      return dom_throw_named_error(rt, "TypeError", "URL.searchParams getter called on non-URL object");
    };
    raw_query = state.raw_query.clone();
    existing_params = state.search_params;
  }

  if let Some(params) = existing_params {
    return Ok(Value::Object(params));
  }

  let proto = rt
    .url_search_params_proto
    .ok_or_else(|| JsError::Vm(VmError::Unimplemented("URLSearchParams prototype")))?;
  let params_obj = rt.alloc_object()?;
  rt.heap.object_set_prototype(params_obj, Some(proto))?;

  let pairs = parse_urlencoded_pairs(&raw_query);
  rt.url_search_params.insert(
    params_obj,
    UrlSearchParamsState {
      url: Some(url_obj),
      pairs,
    },
  );
  if let Some(state) = rt.urls.get_mut(&url_obj) {
    state.search_params = Some(params_obj);
  }

  Ok(Value::Object(params_obj))
}

fn native_urlsearchparams_ctor(
  rt: &mut JsWptRuntime,
  this: Value,
  args: &[Value],
) -> Result<Value, JsError> {
  let Value::Object(obj) = this else {
    return dom_throw_named_error(
      rt,
      "TypeError",
      "URLSearchParams constructor must be called with new",
    );
  };

  let init = args.get(0).copied().unwrap_or(Value::Undefined);
  let mut input = if init == Value::Undefined {
    String::new()
  } else {
    let s = rt.heap.to_string(init)?;
    rt.heap.get_string(s)?.to_utf8_lossy()
  };
  if let Some(rest) = input.strip_prefix('?') {
    input = rest.to_string();
  }
  let pairs = if input.is_empty() {
    Vec::new()
  } else {
    parse_urlencoded_pairs(&input)
  };

  rt.url_search_params.insert(
    obj,
    UrlSearchParamsState {
      url: None,
      pairs,
    },
  );
  Ok(Value::Undefined)
}

fn native_urlsearchparams_get(rt: &mut JsWptRuntime, this: Value, args: &[Value]) -> Result<Value, JsError> {
  let Value::Object(obj) = this else {
    return dom_throw_named_error(rt, "TypeError", "URLSearchParams.get called on non-object");
  };
  let Some(state) = rt.url_search_params.get(&obj) else {
    return dom_throw_named_error(rt, "TypeError", "URLSearchParams.get called on invalid receiver");
  };
  let name_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let name = rt.heap.to_string(name_value)?;
  let name = rt.heap.get_string(name)?.to_utf8_lossy();
  let found = state
    .pairs
    .iter()
    .find_map(|(k, v)| if k == &name { Some(v.clone()) } else { None });
  match found {
    Some(v) => Ok(rt.alloc_string_value(&v)?),
    None => Ok(Value::Null),
  }
}

fn native_urlsearchparams_append(
  rt: &mut JsWptRuntime,
  this: Value,
  args: &[Value],
) -> Result<Value, JsError> {
  let Value::Object(obj) = this else {
    return dom_throw_named_error(rt, "TypeError", "URLSearchParams.append called on non-object");
  };
  let (linked_url, serialized) = {
    let Some(state) = rt.url_search_params.get_mut(&obj) else {
      return dom_throw_named_error(rt, "TypeError", "URLSearchParams.append called on invalid receiver");
    };
    let name_value = args.get(0).copied().unwrap_or(Value::Undefined);
    let value_value = args.get(1).copied().unwrap_or(Value::Undefined);
    let name = rt.heap.to_string(name_value)?;
    let name = rt.heap.get_string(name)?.to_utf8_lossy();
    let value = rt.heap.to_string(value_value)?;
    let value = rt.heap.get_string(value)?.to_utf8_lossy();
    state.pairs.push((name, value));
    let serialized = serialize_urlencoded_pairs(&state.pairs);
    (state.url, serialized)
  };

  if let Some(url_obj) = linked_url {
    if let Some(url_state) = rt.urls.get_mut(&url_obj) {
      url_state.raw_query = serialized.clone();
      if serialized.is_empty() {
        url_state.url.set_query(None);
      } else {
        url_state.url.set_query(Some(&serialized));
      }
    }
  }

  Ok(Value::Undefined)
}

fn native_urlsearchparams_to_string(
  rt: &mut JsWptRuntime,
  this: Value,
  _args: &[Value],
) -> Result<Value, JsError> {
  let Value::Object(obj) = this else {
    return dom_throw_named_error(rt, "TypeError", "URLSearchParams.toString called on non-object");
  };
  let Some(state) = rt.url_search_params.get(&obj) else {
    return dom_throw_named_error(rt, "TypeError", "URLSearchParams.toString called on invalid receiver");
  };
  let serialized = serialize_urlencoded_pairs(&state.pairs);
  Ok(rt.alloc_string_value(&serialized)?)
}

fn native_fastrender_resolve_url(
  rt: &mut JsWptRuntime,
  _this: Value,
  args: &[Value],
) -> Result<Value, JsError> {
  let input_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let base_value = args.get(1).copied().unwrap_or(Value::Undefined);
  let input = rt.heap.to_string(input_value)?;
  let input = rt.heap.get_string(input)?.to_utf8_lossy();

  // The legacy helper treats `null`/`undefined` as "no base provided" (mirroring the QuickJS
  // backend's shim). When there's no base:
  // - absolute URLs are returned as-is
  // - relative URLs throw TypeError
  if matches!(base_value, Value::Undefined | Value::Null) {
    return match Url::parse(&input) {
      Ok(url) => Ok(rt.alloc_string_value(url.as_str())?),
      Err(url::ParseError::RelativeUrlWithoutBase) => {
        dom_throw_named_error(rt, "TypeError", "relative URL without base")
      }
      Err(_) => dom_throw_named_error(rt, "TypeError", "Invalid URL"),
    };
  }

  let base = rt.heap.to_string(base_value)?;
  let base = rt.heap.get_string(base)?.to_utf8_lossy();

  let resolved = match Url::parse(&base).and_then(|b| b.join(&input)) {
    Ok(url) => url,
    Err(_) => return dom_throw_named_error(rt, "TypeError", "Invalid URL"),
  };
  Ok(rt.alloc_string_value(resolved.as_str())?)
}

fn native_request_ctor(rt: &mut JsWptRuntime, this: Value, args: &[Value]) -> Result<Value, JsError> {
  let Value::Object(obj) = this else {
    return dom_throw_named_error(rt, "TypeError", "Request constructor must be called with new");
  };
  let input_value = args.get(0).copied().unwrap_or(Value::Undefined);

  let url_string = match input_value {
    Value::Object(input_obj) => {
      let url_key = {
        let mut scope = rt.heap.scope();
        PropertyKey::from_string(scope.alloc_string("url")?)
      };
      if let Some(desc) = rt.heap.get_property(input_obj, &url_key)? {
        if let PropertyKind::Data { value, .. } = desc.kind {
          if let Value::String(s) = value {
            Some(rt.heap.get_string(s)?.to_utf8_lossy())
          } else {
            None
          }
        } else {
          None
        }
      } else {
        None
      }
    }
    _ => None,
  };

  let url_string = if let Some(u) = url_string {
    u
  } else {
    let input = rt.heap.to_string(input_value)?;
    let input = rt.heap.get_string(input)?.to_utf8_lossy();
    let base = match Url::parse(&rt.document_url) {
      Ok(base) => base,
      Err(_) => return dom_throw_named_error(rt, "TypeError", "Invalid base URL"),
    };
    let resolved = match base.join(&input) {
      Ok(url) => url,
      Err(_) => return dom_throw_named_error(rt, "TypeError", "Invalid URL"),
    };
    resolved.to_string()
  };

  let url_key = {
    let mut scope = rt.heap.scope();
    PropertyKey::from_string(scope.alloc_string("url")?)
  };
  let url_value = rt.alloc_string_value(&url_string)?;
  rt.define_data_prop(obj, url_key, url_value)?;
  Ok(Value::Undefined)
}

fn native_response_ctor(rt: &mut JsWptRuntime, this: Value, args: &[Value]) -> Result<Value, JsError> {
  let Value::Object(obj) = this else {
    return dom_throw_named_error(rt, "TypeError", "Response constructor must be called with new");
  };
  let url_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let url = rt.heap.to_string(url_value)?;
  let url = rt.heap.get_string(url)?.to_utf8_lossy();
  let url_key = {
    let mut scope = rt.heap.scope();
    PropertyKey::from_string(scope.alloc_string("url")?)
  };
  let url_value = rt.alloc_string_value(&url)?;
  rt.define_data_prop(obj, url_key, url_value)?;
  Ok(Value::Undefined)
}

fn native_fetch(rt: &mut JsWptRuntime, _this: Value, args: &[Value]) -> Result<Value, JsError> {
  let input_value = args.get(0).copied().unwrap_or(Value::Undefined);

  let maybe_url = match input_value {
    Value::Object(obj) => {
      let url_key = {
        let mut scope = rt.heap.scope();
        PropertyKey::from_string(scope.alloc_string("url")?)
      };
      if let Some(desc) = rt.heap.get_property(obj, &url_key)? {
        if let PropertyKind::Data { value, .. } = desc.kind {
          if let Value::String(s) = value {
            Some(rt.heap.get_string(s)?.to_utf8_lossy())
          } else {
            None
          }
        } else {
          None
        }
      } else {
        None
      }
    }
    _ => None,
  };

  let resolved_url = if let Some(url) = maybe_url {
    url
  } else {
    let input = rt.heap.to_string(input_value)?;
    let input = rt.heap.get_string(input)?.to_utf8_lossy();
    let base = match Url::parse(&rt.document_url) {
      Ok(base) => base,
      Err(_) => return dom_throw_named_error(rt, "TypeError", "Invalid base URL"),
    };
    let resolved = match base.join(&input) {
      Ok(url) => url,
      Err(_) => return dom_throw_named_error(rt, "TypeError", "Invalid URL"),
    };
    resolved.to_string()
  };

  let response = rt.alloc_object()?;
  let proto = rt
    .response_proto
    .ok_or_else(|| JsError::Vm(VmError::Unimplemented("Response prototype")))?;
  rt.heap.object_set_prototype(response, Some(proto))?;

  let url_key = {
    let mut scope = rt.heap.scope();
    PropertyKey::from_string(scope.alloc_string("url")?)
  };
  let url_value = rt.alloc_string_value(&resolved_url)?;
  rt.define_data_prop(response, url_key, url_value)?;

  let promise = rt.alloc_promise_with_state(PromiseStatus::Fulfilled, Value::Object(response))?;
  Ok(Value::Object(promise))
}

fn native_btoa(rt: &mut JsWptRuntime, _this: Value, args: &[Value]) -> Result<Value, JsError> {
  let value = args.get(0).copied().unwrap_or(Value::Undefined);
  let s = rt.heap.to_string(value)?;
  let s = rt.heap.get_string(s)?.to_utf8_lossy();

  let bytes = match latin1_encode(&s) {
    Ok(bytes) => bytes,
    Err(_) => {
      return dom_throw_invalid_character_error(
        rt,
        "The string to be encoded contains characters outside of the Latin1 range.",
      );
    }
  };
  let encoded = match forgiving_base64_encode(&bytes) {
    Ok(encoded) => encoded,
    Err(_) => {
      return dom_throw_invalid_character_error(rt, "The string to be encoded is too large.");
    }
  };
  Ok(rt.alloc_string_value(&encoded)?)
}

fn native_atob(rt: &mut JsWptRuntime, _this: Value, args: &[Value]) -> Result<Value, JsError> {
  let value = args.get(0).copied().unwrap_or(Value::Undefined);
  let s = rt.heap.to_string(value)?;
  let s = rt.heap.get_string(s)?.to_utf8_lossy();

  let decoded = match forgiving_base64_decode(&s) {
    Ok(bytes) => bytes,
    Err(_) => {
      return dom_throw_invalid_character_error(
        rt,
        "The string to be decoded is not correctly encoded.",
      );
    }
  };
  let out = decoded.iter().map(|&b| b as char).collect::<String>();
  Ok(rt.alloc_string_value(&out)?)
}

fn native_string_char_code_at(rt: &mut JsWptRuntime, this: Value, args: &[Value]) -> Result<Value, JsError> {
  let index_value = args.get(0).copied().unwrap_or(Value::Number(0.0));
  let mut idx = rt.heap.to_number(index_value)?;
  if !idx.is_finite() {
    idx = 0.0;
  }
  if idx < 0.0 {
    return Ok(Value::Number(f64::NAN));
  }
  let idx = idx.trunc() as usize;

  let s = rt.heap.to_string(this)?;
  let unit = {
    let code_units = rt.heap.get_string(s)?.as_code_units();
    code_units.get(idx).copied()
  };
  let Some(unit) = unit else {
    return Ok(Value::Number(f64::NAN));
  };
  Ok(Value::Number(unit as f64))
}

fn native_error_ctor(rt: &mut JsWptRuntime, _this: Value, args: &[Value]) -> Result<Value, JsError> {
  let proto = rt
    .error_prototype
    .ok_or_else(|| JsError::Vm(VmError::Unimplemented("Error prototype")))?;

  let obj = rt.alloc_object()?;
  rt.heap.object_set_prototype(obj, Some(proto))?;

  let name_key = {
    let mut scope = rt.heap.scope();
    PropertyKey::from_string(scope.alloc_string("name")?)
  };
  let message_key = {
    let mut scope = rt.heap.scope();
    PropertyKey::from_string(scope.alloc_string("message")?)
  };

  let name_value = rt.alloc_string_value("Error")?;
  rt.define_data_prop(obj, name_key, name_value)?;

  let message_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let message_value = if message_value == Value::Undefined {
    rt.alloc_string_value("")?
  } else {
    Value::String(rt.heap.to_string(message_value)?)
  };
  rt.define_data_prop(obj, message_key, message_value)?;

  Ok(Value::Object(obj))
}

fn native_type_error_ctor(rt: &mut JsWptRuntime, _this: Value, args: &[Value]) -> Result<Value, JsError> {
  let proto = rt
    .type_error_prototype
    .ok_or_else(|| JsError::Vm(VmError::Unimplemented("TypeError prototype")))?;

  let obj = rt.alloc_object()?;
  rt.heap.object_set_prototype(obj, Some(proto))?;

  let name_key = {
    let mut scope = rt.heap.scope();
    PropertyKey::from_string(scope.alloc_string("name")?)
  };
  let message_key = {
    let mut scope = rt.heap.scope();
    PropertyKey::from_string(scope.alloc_string("message")?)
  };

  let name_value = rt.alloc_string_value("TypeError")?;
  rt.define_data_prop(obj, name_key, name_value)?;

  let message_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let message_value = if message_value == Value::Undefined {
    rt.alloc_string_value("")?
  } else {
    Value::String(rt.heap.to_string(message_value)?)
  };
  rt.define_data_prop(obj, message_key, message_value)?;

  Ok(Value::Object(obj))
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

fn native_array_push(rt: &mut JsWptRuntime, this: Value, args: &[Value]) -> Result<Value, JsError> {
  let Value::Object(arr) = this else {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
      "TypeError: Array.prototype.push called on non-object",
    )?)));
  };
  let mut appended = Vec::<(usize, Value)>::new();
  let len = {
    let Some(elements) = rt.arrays.get_mut(&arr) else {
      return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
        "TypeError: Array.prototype.push called on non-array",
      )?)));
    };

    for value in args.iter().copied() {
      let idx = elements.len();
      elements.push(value);
      appended.push((idx, value));
    }

    elements.len()
  };

  for (idx, value) in appended {
    let key = {
      let mut scope = rt.heap.scope();
      PropertyKey::from_string(scope.alloc_string(&idx.to_string())?)
    };
    rt.define_data_prop(arr, key, value)?;
  }

  let len = len as f64;
  let length_key = {
    let mut scope = rt.heap.scope();
    PropertyKey::from_string(scope.alloc_string("length")?)
  };
  rt.define_data_prop(arr, length_key, Value::Number(len))?;
  Ok(Value::Number(len))
}

fn native_array_join(rt: &mut JsWptRuntime, this: Value, args: &[Value]) -> Result<Value, JsError> {
  let Value::Object(arr) = this else {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
      "TypeError: Array.prototype.join called on non-object",
    )?)));
  };
  let sep = match args.get(0).copied() {
    None | Some(Value::Undefined) => ",".to_string(),
    Some(v) => string_from_value(rt, v)?,
  };

  let Some(elements) = rt.arrays.get(&arr).cloned() else {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
      "TypeError: Array.prototype.join called on non-array",
    )?)));
  };

  let mut out = String::new();
  for (idx, value) in elements.iter().copied().enumerate() {
    if idx > 0 {
      out.push_str(&sep);
    }
    match value {
      Value::Undefined | Value::Null => {}
      Value::String(s) => out.push_str(&rt.heap.get_string(s)?.to_utf8_lossy()),
      other => out.push_str(&rt.value_to_string_lossy(other)),
    }
  }
  rt.alloc_string_value(&out)
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

fn native_node_ctor(rt: &mut JsWptRuntime, _this: Value, _args: &[Value]) -> Result<Value, JsError> {
  Err(JsError::Vm(VmError::Throw(
    rt.alloc_string_value("TypeError: Illegal constructor")?,
  )))
}

fn native_document_get_cookie(
  rt: &mut JsWptRuntime,
  _this: Value,
  _args: &[Value],
) -> Result<Value, JsError> {
  let cookie = rt.cookie_jar.cookie_string();
  rt.alloc_string_value(&cookie)
}

fn native_document_set_cookie(
  rt: &mut JsWptRuntime,
  _this: Value,
  args: &[Value],
) -> Result<Value, JsError> {
  let input = string_from_value(rt, args.get(0).copied().unwrap_or(Value::Undefined))?;
  rt.cookie_jar.set_cookie_string(&input);
  Ok(Value::Undefined)
}

fn native_document_create_element(
  rt: &mut JsWptRuntime,
  this: Value,
  args: &[Value],
) -> Result<Value, JsError> {
  let Value::Object(document) = this else {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
      "TypeError: createElement called on non-object",
    )?)));
  };
  if !matches!(rt.env.get("document"), Some(Value::Object(doc)) if doc == document) {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
      "TypeError: createElement called on non-document",
    )?)));
  }

  let tag_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let tag = string_from_value(rt, tag_value)?;
  let elem = rt.alloc_dom_element(&tag)?;
  Ok(Value::Object(elem))
}

fn native_document_create_document_fragment(
  rt: &mut JsWptRuntime,
  this: Value,
  _args: &[Value],
) -> Result<Value, JsError> {
  let Value::Object(document) = this else {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
      "TypeError: createDocumentFragment called on non-object",
    )?)));
  };
  if !matches!(rt.env.get("document"), Some(Value::Object(doc)) if doc == document) {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
      "TypeError: createDocumentFragment called on non-document",
    )?)));
  }

  let frag = rt.alloc_dom_document_fragment()?;
  Ok(Value::Object(frag))
}

fn native_document_create_text_node(
  rt: &mut JsWptRuntime,
  this: Value,
  args: &[Value],
) -> Result<Value, JsError> {
  let Value::Object(document) = this else {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
      "TypeError: createTextNode called on non-object",
    )?)));
  };
  if !matches!(rt.env.get("document"), Some(Value::Object(doc)) if doc == document) {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
      "TypeError: createTextNode called on non-document",
    )?)));
  }

  let data = string_from_value(rt, args.get(0).copied().unwrap_or(Value::Undefined))?;
  let text = rt.alloc_dom_text_node(&data)?;
  Ok(Value::Object(text))
}

fn native_document_get_element_by_id(
  rt: &mut JsWptRuntime,
  this: Value,
  args: &[Value],
) -> Result<Value, JsError> {
  let Value::Object(document) = this else {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
      "TypeError: getElementById called on non-object",
    )?)));
  };

  let env_document = match rt.env.get("document") {
    Some(Value::Object(doc)) => doc,
    _ => {
      return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
        "TypeError: getElementById called without a document",
      )?)));
    }
  };
  if document != env_document {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
      "TypeError: getElementById called on non-document",
    )?)));
  }

  let needle_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let needle = string_from_value(rt, needle_value)?;

  let Some(body) = rt.document_body else {
    return Ok(Value::Null);
  };
  let root = rt
    .dom_nodes
    .get(&body)
    .and_then(|state| state.parent)
    .unwrap_or(body);

  for node in dom_subtree_preorder(rt, root) {
    let Some(state) = rt.dom_nodes.get(&node) else {
      continue;
    };
    let Some(&id) = state.attributes.get("id") else {
      continue;
    };
    let actual = rt.heap.get_string(id)?.to_utf8_lossy();
    if actual == needle {
      return Ok(Value::Object(node));
    }
  }

  Ok(Value::Null)
}

fn native_document_query_selector(
  rt: &mut JsWptRuntime,
  this: Value,
  args: &[Value],
) -> Result<Value, JsError> {
  let Value::Object(document) = this else {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
      "TypeError: querySelector called on non-object",
    )?)));
  };
  let env_document = match rt.env.get("document") {
    Some(Value::Object(doc)) => doc,
    _ => {
      return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
        "TypeError: querySelector called without a document",
      )?)));
    }
  };
  if document != env_document {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
      "TypeError: querySelector called on non-document",
    )?)));
  }

  let selector_text = string_from_value(rt, args.get(0).copied().unwrap_or(Value::Undefined))?;
  let selectors = match parse_dom_selector_list(&selector_text) {
    Ok(selectors) => selectors,
    Err(_) => {
      let message = format!("invalid selector: {selector_text}");
      return dom_throw_syntax_error(rt, &message);
    }
  };

  let Some(body) = rt.document_body else {
    return Ok(Value::Null);
  };
  // `document.querySelector(All)` scopes to the document element in browsers.
  let scope = rt
    .dom_nodes
    .get(&body)
    .and_then(|state| state.parent)
    .unwrap_or(body);

  let nodes = dom_subtree_preorder(rt, scope);
  for node in nodes {
    let is_element = rt
      .dom_nodes
      .get(&node)
      .is_some_and(|state| state.kind == DomNodeKind::Element);
    if !is_element {
      continue;
    }
    if dom_matches_any_selector(rt, node, &selectors, scope, /* allow_self_without_scope */ false)? {
      return Ok(Value::Object(node));
    }
  }
  Ok(Value::Null)
}

fn native_document_query_selector_all(
  rt: &mut JsWptRuntime,
  this: Value,
  args: &[Value],
) -> Result<Value, JsError> {
  let Value::Object(document) = this else {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
      "TypeError: querySelectorAll called on non-object",
    )?)));
  };
  let env_document = match rt.env.get("document") {
    Some(Value::Object(doc)) => doc,
    _ => {
      return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
        "TypeError: querySelectorAll called without a document",
      )?)));
    }
  };
  if document != env_document {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
      "TypeError: querySelectorAll called on non-document",
    )?)));
  }

  let selector_text = string_from_value(rt, args.get(0).copied().unwrap_or(Value::Undefined))?;
  let selectors = match parse_dom_selector_list(&selector_text) {
    Ok(selectors) => selectors,
    Err(_) => {
      let message = format!("invalid selector: {selector_text}");
      return dom_throw_syntax_error(rt, &message);
    }
  };

  let Some(body) = rt.document_body else {
    return Ok(Value::Object(rt.make_dom_nodelist(&[])?));
  };
  let scope = rt
    .dom_nodes
    .get(&body)
    .and_then(|state| state.parent)
    .unwrap_or(body);

  let nodes = dom_subtree_preorder(rt, scope);
  let mut matches = Vec::new();
  for node in nodes {
    let is_element = rt
      .dom_nodes
      .get(&node)
      .is_some_and(|state| state.kind == DomNodeKind::Element);
    if !is_element {
      continue;
    }
    if dom_matches_any_selector(rt, node, &selectors, scope, /* allow_self_without_scope */ false)? {
      matches.push(node);
    }
  }
  Ok(Value::Object(rt.make_dom_nodelist(&matches)?))
}

fn native_document_fragment_query_selector(
  rt: &mut JsWptRuntime,
  this: Value,
  args: &[Value],
) -> Result<Value, JsError> {
  let Value::Object(fragment) = this else {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
      "TypeError: querySelector called on non-object",
    )?)));
  };
  let Some(state) = rt.dom_nodes.get(&fragment) else {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
      "TypeError: querySelector called on non-DocumentFragment",
    )?)));
  };
  if state.kind != DomNodeKind::DocumentFragment {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
      "TypeError: querySelector called on non-DocumentFragment",
    )?)));
  }

  let selector_text = string_from_value(rt, args.get(0).copied().unwrap_or(Value::Undefined))?;
  let selectors = match parse_dom_selector_list(&selector_text) {
    Ok(selectors) => selectors,
    Err(_) => {
      let message = format!("invalid selector: {selector_text}");
      return dom_throw_syntax_error(rt, &message);
    }
  };

  let nodes = dom_subtree_preorder(rt, fragment);
  for node in nodes {
    let is_element = rt
      .dom_nodes
      .get(&node)
      .is_some_and(|state| state.kind == DomNodeKind::Element);
    if !is_element {
      continue;
    }
    if dom_matches_any_selector(rt, node, &selectors, fragment, /* allow_self_without_scope */ false)?
    {
      return Ok(Value::Object(node));
    }
  }
  Ok(Value::Null)
}

fn native_document_fragment_query_selector_all(
  rt: &mut JsWptRuntime,
  this: Value,
  args: &[Value],
) -> Result<Value, JsError> {
  let Value::Object(fragment) = this else {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
      "TypeError: querySelectorAll called on non-object",
    )?)));
  };
  let Some(state) = rt.dom_nodes.get(&fragment) else {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
      "TypeError: querySelectorAll called on non-DocumentFragment",
    )?)));
  };
  if state.kind != DomNodeKind::DocumentFragment {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
      "TypeError: querySelectorAll called on non-DocumentFragment",
    )?)));
  }

  let selector_text = string_from_value(rt, args.get(0).copied().unwrap_or(Value::Undefined))?;
  let selectors = match parse_dom_selector_list(&selector_text) {
    Ok(selectors) => selectors,
    Err(_) => {
      let message = format!("invalid selector: {selector_text}");
      return dom_throw_syntax_error(rt, &message);
    }
  };

  let nodes = dom_subtree_preorder(rt, fragment);
  let mut matches = Vec::new();
  for node in nodes {
    let is_element = rt
      .dom_nodes
      .get(&node)
      .is_some_and(|state| state.kind == DomNodeKind::Element);
    if !is_element {
      continue;
    }
    if dom_matches_any_selector(rt, node, &selectors, fragment, /* allow_self_without_scope */ false)?
    {
      matches.push(node);
    }
  }
  Ok(Value::Object(rt.make_dom_nodelist(&matches)?))
}

fn native_document_fragment_get_element_by_id(
  rt: &mut JsWptRuntime,
  this: Value,
  args: &[Value],
) -> Result<Value, JsError> {
  let Value::Object(fragment) = this else {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
      "TypeError: getElementById called on non-object",
    )?)));
  };
  let Some(state) = rt.dom_nodes.get(&fragment) else {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
      "TypeError: getElementById called on non-DocumentFragment",
    )?)));
  };
  if state.kind != DomNodeKind::DocumentFragment {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
      "TypeError: getElementById called on non-DocumentFragment",
    )?)));
  }

  let needle_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let needle = string_from_value(rt, needle_value)?;

  for node in dom_subtree_preorder(rt, fragment) {
    let Some(state) = rt.dom_nodes.get(&node) else {
      continue;
    };
    if state.kind != DomNodeKind::Element {
      continue;
    }
    let Some(&id) = state.attributes.get("id") else {
      continue;
    };
    let actual = rt.heap.get_string(id)?.to_utf8_lossy();
    if actual == needle {
      return Ok(Value::Object(node));
    }
  }

  Ok(Value::Null)
}

fn native_node_append_child(rt: &mut JsWptRuntime, this: Value, args: &[Value]) -> Result<Value, JsError> {
  let Value::Object(parent) = this else {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
      "TypeError: appendChild called on non-object",
    )?)));
  };
  if !rt.event_targets.contains_key(&parent) {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
      "TypeError: appendChild called on non-Node",
    )?)));
  }

  let Value::Object(child) = args.get(0).copied().unwrap_or(Value::Undefined) else {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
      "TypeError: appendChild requires a Node",
    )?)));
  };
  if !rt.event_targets.contains_key(&child) {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
      "TypeError: appendChild requires a Node",
    )?)));
  };

  let child_is_fragment = rt
    .dom_nodes
    .get(&child)
    .is_some_and(|state| state.kind == DomNodeKind::DocumentFragment);

  if child_is_fragment {
    // DocumentFragment insertion: move children into the parent (in order) and leave the fragment
    // detached/empty.
    if rt.dom_nodes.contains_key(&parent) && rt.dom_nodes.contains_key(&child) {
      let parent_tag = rt
        .dom_nodes
        .get(&parent)
        .map(|s| s.tag_name.clone())
        .unwrap_or_default();
      let parent_is_template = parent_tag == "template";

      let fragment_children = rt
        .dom_nodes
        .get(&child)
        .map(|s| s.children.clone())
        .unwrap_or_default();

      // Clear the fragment container first so the mutation is atomic from the perspective of the
      // cached `childNodes` list.
      if let Some(state) = rt.dom_nodes.get_mut(&child) {
        state.children.clear();
        state.template_content.clear();
        state.parent = None;
      }
      let parent_node_key = {
        let mut scope = rt.heap.scope();
        PropertyKey::from_string(scope.alloc_string("parentNode")?)
      };
      rt.define_data_prop(child, parent_node_key, Value::Null)?;

      for moved in fragment_children {
        // Update the EventTarget propagation parent pointer for moved nodes.
        if let Some(moved_state) = rt.event_targets.get_mut(&moved) {
          moved_state.parent = Some(parent);
        }

        if let Some(state) = rt.dom_nodes.get_mut(&parent) {
          state.text_content = None;
          if parent_is_template {
            state.template_content.push(moved);
          } else {
            state.children.push(moved);
          }
        }
        if let Some(state) = rt.dom_nodes.get_mut(&moved) {
          state.parent = Some(parent);
        }
        rt.define_data_prop(moved, parent_node_key, Value::Object(parent))?;
      }

      rt.update_dom_child_nodes(parent)?;
      rt.update_dom_child_nodes(child)?;
    }

    // Spec: Node.appendChild(DocumentFragment) returns the fragment itself.
    return Ok(Value::Object(child));
  }

  // Update the EventTarget propagation parent pointer.
  if let Some(child_state) = rt.event_targets.get_mut(&child) {
    child_state.parent = Some(parent);
  }

  // Update the DOM tree and `childNodes` view when both nodes participate in the DOM shim.
  if rt.dom_nodes.contains_key(&parent) && rt.dom_nodes.contains_key(&child) {
    let old_parent = rt.dom_nodes.get(&child).and_then(|s| s.parent);
    let parent_tag = rt
      .dom_nodes
      .get(&parent)
      .map(|s| s.tag_name.clone())
      .unwrap_or_default();
    let parent_is_template = parent_tag == "template";

    if let Some(old_parent) = old_parent {
      let old_tag = rt
        .dom_nodes
        .get(&old_parent)
        .map(|s| s.tag_name.clone())
        .unwrap_or_default();
      let old_is_template = old_tag == "template";

      if let Some(state) = rt.dom_nodes.get_mut(&old_parent) {
        if old_is_template {
          state.template_content.retain(|&c| c != child);
        } else {
          state.children.retain(|&c| c != child);
        }
      }
      rt.update_dom_child_nodes(old_parent)?;
    }

    if let Some(state) = rt.dom_nodes.get_mut(&parent) {
      state.text_content = None;
      if parent_is_template {
        state.template_content.push(child);
      } else {
        state.children.push(child);
      }
    }

    if let Some(state) = rt.dom_nodes.get_mut(&child) {
      state.parent = Some(parent);
    }
    let parent_node_key = {
      let mut scope = rt.heap.scope();
      PropertyKey::from_string(scope.alloc_string("parentNode")?)
    };
    rt.define_data_prop(child, parent_node_key, Value::Object(parent))?;

    rt.update_dom_child_nodes(parent)?;
  }
  Ok(Value::Object(child))
}

fn native_node_insert_before(rt: &mut JsWptRuntime, this: Value, args: &[Value]) -> Result<Value, JsError> {
  let parent = dom_require_node(rt, this, "insertBefore")?;

  let Value::Object(child) = args.get(0).copied().unwrap_or(Value::Undefined) else {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
      "TypeError: insertBefore requires a Node",
    )?)));
  };
  if !dom_is_node(rt, child) {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
      "TypeError: insertBefore requires a Node",
    )?)));
  }

  let reference_value = args.get(1).copied().unwrap_or(Value::Undefined);
  let mut reference = match reference_value {
    Value::Undefined | Value::Null => None,
    Value::Object(obj) if dom_is_node(rt, obj) => Some(obj),
    _ => {
      return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
        "TypeError: insertBefore requires a Node",
      )?)))
    }
  };

  let child_is_fragment = rt
    .dom_nodes
    .get(&child)
    .is_some_and(|state| state.kind == DomNodeKind::DocumentFragment);

  if child_is_fragment {
    // DocumentFragment insertion: insert the fragment's children before the reference node.
    if rt.dom_nodes.contains_key(&parent) && rt.dom_nodes.contains_key(&child) {
      let parent_tag = rt
        .dom_nodes
        .get(&parent)
        .map(|s| s.tag_name.clone())
        .unwrap_or_default();
      let parent_is_template = parent_tag == "template";

      let siblings = if parent_is_template {
        rt.dom_nodes
          .get(&parent)
          .map(|s| s.template_content.clone())
          .unwrap_or_default()
      } else {
        rt.dom_nodes
          .get(&parent)
          .map(|s| s.children.clone())
          .unwrap_or_default()
      };

      let insert_idx = match reference {
        None => siblings.len(),
        Some(node) => match siblings.iter().position(|&n| n == node) {
          Some(idx) => idx,
          None => return dom_throw_dom_exception(rt, "NotFoundError"),
        },
      };

      let fragment_children = rt
        .dom_nodes
        .get(&child)
        .map(|s| s.children.clone())
        .unwrap_or_default();

      // Clear the fragment before mutation so `childNodes` observes the update atomically.
      if let Some(state) = rt.dom_nodes.get_mut(&child) {
        state.children.clear();
        state.template_content.clear();
        state.parent = None;
      }
      let parent_node_key = {
        let mut scope = rt.heap.scope();
        PropertyKey::from_string(scope.alloc_string("parentNode")?)
      };
      rt.define_data_prop(child, parent_node_key, Value::Null)?;

      for (offset, moved) in fragment_children.into_iter().enumerate() {
        if let Some(moved_state) = rt.event_targets.get_mut(&moved) {
          moved_state.parent = Some(parent);
        }

        if let Some(state) = rt.dom_nodes.get_mut(&parent) {
          state.text_content = None;
          if parent_is_template {
            state.template_content.insert(insert_idx + offset, moved);
          } else {
            state.children.insert(insert_idx + offset, moved);
          }
        }
        if let Some(state) = rt.dom_nodes.get_mut(&moved) {
          state.parent = Some(parent);
        }
        rt.define_data_prop(moved, parent_node_key, Value::Object(parent))?;
      }

      rt.update_dom_child_nodes(parent)?;
      rt.update_dom_child_nodes(child)?;
    }

    return Ok(Value::Object(child));
  }

  // Update the EventTarget propagation parent pointer.
  if let Some(child_state) = rt.event_targets.get_mut(&child) {
    child_state.parent = Some(parent);
  }

  if rt.dom_nodes.contains_key(&parent) && rt.dom_nodes.contains_key(&child) {
    let parent_tag = rt
      .dom_nodes
      .get(&parent)
      .map(|s| s.tag_name.clone())
      .unwrap_or_default();
    let parent_is_template = parent_tag == "template";

    let parent_node_key = {
      let mut scope = rt.heap.scope();
      PropertyKey::from_string(scope.alloc_string("parentNode")?)
    };

    // Inserting a node before itself is a no-op.
    if reference == Some(child) {
      let siblings = if parent_is_template {
        rt.dom_nodes
          .get(&parent)
          .map(|s| s.template_content.clone())
          .unwrap_or_default()
      } else {
        rt.dom_nodes
          .get(&parent)
          .map(|s| s.children.clone())
          .unwrap_or_default()
      };
      if let Some(idx) = siblings.iter().position(|&n| n == child) {
        reference = siblings.get(idx + 1).copied();
      } else {
        return dom_throw_dom_exception(rt, "NotFoundError");
      }
    }

    let siblings = if parent_is_template {
      rt.dom_nodes
        .get(&parent)
        .map(|s| s.template_content.clone())
        .unwrap_or_default()
    } else {
      rt.dom_nodes
        .get(&parent)
        .map(|s| s.children.clone())
        .unwrap_or_default()
    };

    let mut insert_idx = match reference {
      None => siblings.len(),
      Some(node) => match siblings.iter().position(|&n| n == node) {
        Some(idx) => idx,
        None => return dom_throw_dom_exception(rt, "NotFoundError"),
      },
    };

    let old_parent = rt.dom_nodes.get(&child).and_then(|s| s.parent);
    if let Some(old_parent) = old_parent {
      if old_parent == parent {
        if let Some(child_idx) = siblings.iter().position(|&n| n == child) {
          if child_idx < insert_idx {
            insert_idx = insert_idx.saturating_sub(1);
          }
        }
      }

      let old_tag = rt
        .dom_nodes
        .get(&old_parent)
        .map(|s| s.tag_name.clone())
        .unwrap_or_default();
      let old_is_template = old_tag == "template";
      if let Some(state) = rt.dom_nodes.get_mut(&old_parent) {
        if old_is_template {
          state.template_content.retain(|&c| c != child);
        } else {
          state.children.retain(|&c| c != child);
        }
      }
      rt.update_dom_child_nodes(old_parent)?;
    }

    if let Some(state) = rt.dom_nodes.get_mut(&parent) {
      state.text_content = None;
      if parent_is_template {
        state.template_content.insert(insert_idx, child);
      } else {
        state.children.insert(insert_idx, child);
      }
    }
    if let Some(state) = rt.dom_nodes.get_mut(&child) {
      state.parent = Some(parent);
    }
    rt.define_data_prop(child, parent_node_key, Value::Object(parent))?;
    rt.update_dom_child_nodes(parent)?;
  }

  Ok(Value::Object(child))
}

fn native_node_replace_child(rt: &mut JsWptRuntime, this: Value, args: &[Value]) -> Result<Value, JsError> {
  let parent = dom_require_node(rt, this, "replaceChild")?;

  let Value::Object(new_child) = args.get(0).copied().unwrap_or(Value::Undefined) else {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
      "TypeError: replaceChild requires a Node",
    )?)));
  };
  if !dom_is_node(rt, new_child) {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
      "TypeError: replaceChild requires a Node",
    )?)));
  }

  let Value::Object(old_child) = args.get(1).copied().unwrap_or(Value::Undefined) else {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
      "TypeError: replaceChild requires a Node",
    )?)));
  };
  if !dom_is_node(rt, old_child) {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
      "TypeError: replaceChild requires a Node",
    )?)));
  }

  if new_child == old_child {
    return Ok(Value::Object(old_child));
  }

  let new_is_fragment = rt
    .dom_nodes
    .get(&new_child)
    .is_some_and(|state| state.kind == DomNodeKind::DocumentFragment);

  if new_is_fragment {
    if rt.dom_nodes.contains_key(&parent) && rt.dom_nodes.contains_key(&new_child) && rt.dom_nodes.contains_key(&old_child) {
      let parent_tag = rt
        .dom_nodes
        .get(&parent)
        .map(|s| s.tag_name.clone())
        .unwrap_or_default();
      let parent_is_template = parent_tag == "template";

      let siblings = if parent_is_template {
        rt.dom_nodes
          .get(&parent)
          .map(|s| s.template_content.clone())
          .unwrap_or_default()
      } else {
        rt.dom_nodes
          .get(&parent)
          .map(|s| s.children.clone())
          .unwrap_or_default()
      };

      let replace_idx = match siblings.iter().position(|&n| n == old_child) {
        Some(idx) => idx,
        None => return dom_throw_dom_exception(rt, "NotFoundError"),
      };

      let fragment_children = rt
        .dom_nodes
        .get(&new_child)
        .map(|s| s.children.clone())
        .unwrap_or_default();

      if let Some(state) = rt.dom_nodes.get_mut(&new_child) {
        state.children.clear();
        state.template_content.clear();
        state.parent = None;
      }

      let parent_node_key = {
        let mut scope = rt.heap.scope();
        PropertyKey::from_string(scope.alloc_string("parentNode")?)
      };
      rt.define_data_prop(new_child, parent_node_key, Value::Null)?;

      // Remove the replaced node from the parent's list.
      if let Some(state) = rt.dom_nodes.get_mut(&parent) {
        if parent_is_template {
          if replace_idx < state.template_content.len() {
            state.template_content.remove(replace_idx);
          }
        } else if replace_idx < state.children.len() {
          state.children.remove(replace_idx);
        }
      }

      if let Some(state) = rt.dom_nodes.get_mut(&old_child) {
        if state.parent == Some(parent) {
          state.parent = None;
        }
      }
      rt.define_data_prop(old_child, parent_node_key, Value::Null)?;
      if let Some(state) = rt.event_targets.get_mut(&old_child) {
        if state.parent == Some(parent) {
          state.parent = None;
        }
      }

      for (offset, moved) in fragment_children.into_iter().enumerate() {
        if let Some(moved_state) = rt.event_targets.get_mut(&moved) {
          moved_state.parent = Some(parent);
        }

        if let Some(state) = rt.dom_nodes.get_mut(&parent) {
          state.text_content = None;
          if parent_is_template {
            state.template_content.insert(replace_idx + offset, moved);
          } else {
            state.children.insert(replace_idx + offset, moved);
          }
        }
        if let Some(state) = rt.dom_nodes.get_mut(&moved) {
          state.parent = Some(parent);
        }
        rt.define_data_prop(moved, parent_node_key, Value::Object(parent))?;
      }

      rt.update_dom_child_nodes(parent)?;
      rt.update_dom_child_nodes(new_child)?;
    }

    return Ok(Value::Object(old_child));
  }

  // Update the EventTarget propagation parent pointer.
  if let Some(state) = rt.event_targets.get_mut(&new_child) {
    state.parent = Some(parent);
  }

  if rt.dom_nodes.contains_key(&parent) && rt.dom_nodes.contains_key(&new_child) && rt.dom_nodes.contains_key(&old_child) {
    let parent_tag = rt
      .dom_nodes
      .get(&parent)
      .map(|s| s.tag_name.clone())
      .unwrap_or_default();
    let parent_is_template = parent_tag == "template";

    let siblings = if parent_is_template {
      rt.dom_nodes
        .get(&parent)
        .map(|s| s.template_content.clone())
        .unwrap_or_default()
    } else {
      rt.dom_nodes
        .get(&parent)
        .map(|s| s.children.clone())
        .unwrap_or_default()
    };

    let mut replace_idx = match siblings.iter().position(|&n| n == old_child) {
      Some(idx) => idx,
      None => return dom_throw_dom_exception(rt, "NotFoundError"),
    };

    let old_parent = rt.dom_nodes.get(&new_child).and_then(|s| s.parent);
    if let Some(old_parent) = old_parent {
      if old_parent == parent {
        if let Some(idx) = siblings.iter().position(|&n| n == new_child) {
          if idx < replace_idx {
            replace_idx = replace_idx.saturating_sub(1);
          }
        }
      }

      let old_tag = rt
        .dom_nodes
        .get(&old_parent)
        .map(|s| s.tag_name.clone())
        .unwrap_or_default();
      let old_is_template = old_tag == "template";
      if let Some(state) = rt.dom_nodes.get_mut(&old_parent) {
        if old_is_template {
          state.template_content.retain(|&c| c != new_child);
        } else {
          state.children.retain(|&c| c != new_child);
        }
      }
      rt.update_dom_child_nodes(old_parent)?;
    }

    // Replace the old child in-place.
    if let Some(state) = rt.dom_nodes.get_mut(&parent) {
      state.text_content = None;
      if parent_is_template {
        if replace_idx < state.template_content.len() {
          state.template_content[replace_idx] = new_child;
        }
      } else if replace_idx < state.children.len() {
        state.children[replace_idx] = new_child;
      }
    }

    let parent_node_key = {
      let mut scope = rt.heap.scope();
      PropertyKey::from_string(scope.alloc_string("parentNode")?)
    };

    if let Some(state) = rt.dom_nodes.get_mut(&new_child) {
      state.parent = Some(parent);
    }
    rt.define_data_prop(new_child, parent_node_key, Value::Object(parent))?;

    if let Some(state) = rt.dom_nodes.get_mut(&old_child) {
      if state.parent == Some(parent) {
        state.parent = None;
      }
    }
    rt.define_data_prop(old_child, parent_node_key, Value::Null)?;
    if let Some(state) = rt.event_targets.get_mut(&old_child) {
      if state.parent == Some(parent) {
        state.parent = None;
      }
    }

    rt.update_dom_child_nodes(parent)?;
  }

  Ok(Value::Object(old_child))
}

fn native_node_contains(rt: &mut JsWptRuntime, this: Value, args: &[Value]) -> Result<Value, JsError> {
  let this_node = dom_require_node(rt, this, "contains")?;

  let other_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let other = match other_value {
    Value::Undefined | Value::Null => return Ok(Value::Bool(false)),
    Value::Object(obj) if dom_is_node(rt, obj) => obj,
    _ => {
      return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
        "TypeError: contains requires a Node",
      )?)))
    }
  };

  let mut current = Some(other);
  while let Some(node) = current {
    if node == this_node {
      return Ok(Value::Bool(true));
    }
    current = dom_parent_for_contains(rt, node);
  }
  Ok(Value::Bool(false))
}

fn native_node_remove(rt: &mut JsWptRuntime, this: Value, _args: &[Value]) -> Result<Value, JsError> {
  let node = dom_require_node(rt, this, "remove")?;

  let parent = rt.dom_nodes.get(&node).and_then(|s| s.parent);
  let Some(parent) = parent else {
    return Ok(Value::Undefined);
  };

  let parent_tag = rt
    .dom_nodes
    .get(&parent)
    .map(|s| s.tag_name.clone())
    .unwrap_or_default();
  let parent_is_template = parent_tag == "template";

  if let Some(state) = rt.dom_nodes.get_mut(&parent) {
    if parent_is_template {
      state.template_content.retain(|&c| c != node);
    } else {
      state.children.retain(|&c| c != node);
    }
  }

  if let Some(state) = rt.dom_nodes.get_mut(&node) {
    if state.parent == Some(parent) {
      state.parent = None;
    }
  }
  let parent_node_key = {
    let mut scope = rt.heap.scope();
    PropertyKey::from_string(scope.alloc_string("parentNode")?)
  };
  rt.define_data_prop(node, parent_node_key, Value::Null)?;
  if let Some(state) = rt.event_targets.get_mut(&node) {
    if state.parent == Some(parent) {
      state.parent = None;
    }
  }
  rt.update_dom_child_nodes(parent)?;
  Ok(Value::Undefined)
}

fn dom_collect_text_content(rt: &JsWptRuntime, node: GcObject, out: &mut String) {
  let Some(state) = rt.dom_nodes.get(&node) else {
    return;
  };

  match state.kind {
    DomNodeKind::Text => {
      if let Some(text) = &state.text_content {
        out.push_str(text);
      }
    }
    DomNodeKind::DocumentFragment => {
      for &child in dom_element_children_for_serialization(state) {
        dom_collect_text_content(rt, child, out);
      }
    }
    DomNodeKind::Element => {
      let children = dom_element_children_for_serialization(state);
      if !children.is_empty() {
        for &child in children {
          dom_collect_text_content(rt, child, out);
        }
      } else if let Some(text) = &state.text_content {
        out.push_str(text);
      }
    }
  }
}

fn native_node_get_text_content(
  rt: &mut JsWptRuntime,
  this: Value,
  _args: &[Value],
) -> Result<Value, JsError> {
  let node = dom_require_node(rt, this, "textContent")?;
  let document = match rt.env.get("document") {
    Some(Value::Object(doc)) => doc,
    _ => {
      return Err(JsError::Vm(VmError::Unimplemented(
        "document global missing",
      )))
    }
  };

  if node == document {
    return Ok(Value::Null);
  }

  let mut out = String::new();
  dom_collect_text_content(rt, node, &mut out);
  rt.alloc_string_value(&out)
}

fn native_node_set_text_content(rt: &mut JsWptRuntime, this: Value, args: &[Value]) -> Result<Value, JsError> {
  let node = dom_require_node(rt, this, "textContent")?;
  let document = match rt.env.get("document") {
    Some(Value::Object(doc)) => doc,
    _ => {
      return Err(JsError::Vm(VmError::Unimplemented(
        "document global missing",
      )))
    }
  };

  // Spec-ish: treat null as the empty string; otherwise use ToString.
  let raw_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let text = match raw_value {
    Value::Null => String::new(),
    v => string_from_value(rt, v)?,
  };

  // Document.textContent is null and ignores writes.
  if node == document {
    return Ok(Value::Undefined);
  }

  // Text node: update its data in-place.
  if let Some(state) = rt.dom_nodes.get(&node) {
    if state.kind == DomNodeKind::Text {
      if let Some(state) = rt.dom_nodes.get_mut(&node) {
        state.text_content = if text.is_empty() { None } else { Some(text.clone()) };
      }
      let node_value_key = {
        let mut scope = rt.heap.scope();
        PropertyKey::from_string(scope.alloc_string("nodeValue")?)
      };
      let node_value = rt.alloc_string_value(&text)?;
      rt.define_data_prop(node, node_value_key, node_value)?;
      return Ok(Value::Undefined);
    }
  }

  // Elements / fragments: clear children and replace with a single text node.
  dom_detach_children(rt, node)?;
  if !text.is_empty() && rt.dom_nodes.contains_key(&node) {
    let parent_tag = rt
      .dom_nodes
      .get(&node)
      .map(|s| s.tag_name.clone())
      .unwrap_or_default();
    let parent_is_template = parent_tag == "template";
    let parent_node_key = {
      let mut scope = rt.heap.scope();
      PropertyKey::from_string(scope.alloc_string("parentNode")?)
    };

    let text_node = rt.alloc_dom_text_node(&text)?;
    if let Some(state) = rt.dom_nodes.get_mut(&text_node) {
      state.parent = Some(node);
    }
    if let Some(et) = rt.event_targets.get_mut(&text_node) {
      et.parent = Some(node);
    }
    rt.define_data_prop(text_node, parent_node_key, Value::Object(node))?;

    if let Some(state) = rt.dom_nodes.get_mut(&node) {
      if parent_is_template {
        state.template_content.push(text_node);
      } else {
        state.children.push(text_node);
      }
    }
    rt.update_dom_child_nodes(node)?;
  }
  Ok(Value::Undefined)
}

fn native_node_get_owner_document(
  rt: &mut JsWptRuntime,
  this: Value,
  _args: &[Value],
) -> Result<Value, JsError> {
  let node = dom_require_node(rt, this, "ownerDocument")?;
  let document = match rt.env.get("document") {
    Some(Value::Object(doc)) => doc,
    _ => {
      return Err(JsError::Vm(VmError::Unimplemented(
        "document global missing",
      )))
    }
  };

  if node == document {
    return Ok(Value::Null);
  }
  Ok(Value::Object(document))
}

fn native_node_get_is_connected(
  rt: &mut JsWptRuntime,
  this: Value,
  _args: &[Value],
) -> Result<Value, JsError> {
  let node = dom_require_node(rt, this, "isConnected")?;
  let document = match rt.env.get("document") {
    Some(Value::Object(doc)) => doc,
    _ => {
      return Err(JsError::Vm(VmError::Unimplemented(
        "document global missing",
      )))
    }
  };

  if node == document {
    return Ok(Value::Bool(true));
  }

  let mut current = Some(node);
  while let Some(cur) = current {
    if cur == document {
      return Ok(Value::Bool(true));
    }
    current = dom_parent_for_contains(rt, cur);
  }
  Ok(Value::Bool(false))
}

fn native_node_has_child_nodes(
  rt: &mut JsWptRuntime,
  this: Value,
  _args: &[Value],
) -> Result<Value, JsError> {
  let node = dom_require_node(rt, this, "hasChildNodes")?;

  // The vm-js DOM shim does not track the document in `dom_nodes`, but the runner always builds a
  // document with a `<html>` element.
  if matches!(rt.env.get("document"), Some(Value::Object(doc)) if doc == node) {
    return Ok(Value::Bool(true));
  }

  let Some(state) = rt.dom_nodes.get(&node) else {
    return Ok(Value::Bool(false));
  };
  Ok(Value::Bool(match state.kind {
    DomNodeKind::Text => false,
    DomNodeKind::DocumentFragment => !state.children.is_empty(),
    DomNodeKind::Element => {
      !dom_element_children_for_serialization(state).is_empty() || state.text_content.is_some()
    }
  }))
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

fn dom_throw_named_error(rt: &mut JsWptRuntime, name: &str, message: &str) -> Result<Value, JsError> {
  let obj = rt.alloc_object()?;

  // Best-effort prototype assignment so JS `instanceof TypeError` works in smoke tests. The runner
  // does not fully implement the built-in Error hierarchy, but we at least thread TypeError through
  // `TypeError.prototype` when available.
  if name == "TypeError" {
    if let Some(proto) = rt
      .type_error_prototype
      .or(rt.error_prototype)
    {
      rt.heap.object_set_prototype(obj, Some(proto))?;
    }
  } else if name.ends_with("Error") {
    if let Some(proto) = rt.error_prototype {
      rt.heap.object_set_prototype(obj, Some(proto))?;
    }
  }

  let name_key = {
    let mut scope = rt.heap.scope();
    PropertyKey::from_string(scope.alloc_string("name")?)
  };
  let message_key = {
    let mut scope = rt.heap.scope();
    PropertyKey::from_string(scope.alloc_string("message")?)
  };
  let name_value = rt.alloc_string_value(name)?;
  let message_value = rt.alloc_string_value(message)?;
  rt.define_data_prop(obj, name_key, name_value)?;
  rt.define_data_prop(obj, message_key, message_value)?;
  Err(JsError::Vm(VmError::Throw(Value::Object(obj))))
}

fn dom_throw_syntax_error(rt: &mut JsWptRuntime, message: &str) -> Result<Value, JsError> {
  dom_throw_named_error(rt, "SyntaxError", message)
}

fn dom_throw_invalid_character_error(rt: &mut JsWptRuntime, message: &str) -> Result<Value, JsError> {
  dom_throw_named_error(rt, "InvalidCharacterError", message)
}

fn dom_throw_dom_exception(rt: &mut JsWptRuntime, name: &str) -> Result<Value, JsError> {
  dom_throw_named_error(rt, name, "")
}

fn parse_dom_selector_list(input: &str) -> Result<Vec<DomSelector>, ()> {
  let input = input.trim();
  if input.is_empty() {
    return Err(());
  }
  if input.as_bytes().iter().any(|&b| matches!(b, b'[' | b']')) {
    return Err(());
  }

  let mut selectors = Vec::new();
  for part in input.split(',') {
    let part = part.trim();
    if part.is_empty() {
      return Err(());
    }
    selectors.push(parse_dom_selector(part)?);
  }
  Ok(selectors)
}

fn parse_dom_selector(input: &str) -> Result<DomSelector, ()> {
  let bytes = input.as_bytes();
  let mut pos = 0usize;
  let mut compounds = Vec::new();
  let mut combinators = Vec::new();

  skip_ascii_whitespace(bytes, &mut pos);
  let first = parse_dom_compound(bytes, &mut pos)?;
  compounds.push(first);

  loop {
    let had_ws = skip_ascii_whitespace(bytes, &mut pos);
    if pos >= bytes.len() {
      break;
    }
    if bytes[pos] == b'>' {
      pos += 1;
      combinators.push(DomCombinator::Child);
      skip_ascii_whitespace(bytes, &mut pos);
      let next = parse_dom_compound(bytes, &mut pos)?;
      compounds.push(next);
      continue;
    }
    if had_ws {
      combinators.push(DomCombinator::Descendant);
      let next = parse_dom_compound(bytes, &mut pos)?;
      compounds.push(next);
      continue;
    }
    return Err(());
  }

  if compounds.is_empty() {
    return Err(());
  }
  if combinators.len() + 1 != compounds.len() {
    return Err(());
  }

  Ok(DomSelector {
    compounds,
    combinators,
  })
}

fn skip_ascii_whitespace(bytes: &[u8], pos: &mut usize) -> bool {
  let start = *pos;
  while *pos < bytes.len() && bytes[*pos].is_ascii_whitespace() {
    *pos += 1;
  }
  *pos != start
}

fn parse_dom_compound(bytes: &[u8], pos: &mut usize) -> Result<DomCompoundSelector, ()> {
  let mut compound = DomCompoundSelector {
    tag: None,
    id: None,
    classes: Vec::new(),
    is_scope: false,
  };

  let mut saw_any = false;

  while *pos < bytes.len() {
    match bytes[*pos] {
      b' ' | b'\t' | b'\n' | b'\r' | b'>' => break,
      b'#' => {
        *pos += 1;
        let ident = parse_dom_ident(bytes, pos)?;
        compound.id = Some(ident);
        saw_any = true;
      }
      b'.' => {
        *pos += 1;
        let ident = parse_dom_ident(bytes, pos)?;
        compound.classes.push(ident);
        saw_any = true;
      }
      b':' => {
        *pos += 1;
        let ident = parse_dom_ident(bytes, pos)?;
        if ident != "scope" {
          return Err(());
        }
        compound.is_scope = true;
        saw_any = true;
      }
      b => {
        if compound.tag.is_some() {
          return Err(());
        }
        if !(b as char).is_ascii_alphabetic() {
          return Err(());
        }
        let ident = parse_dom_ident(bytes, pos)?;
        compound.tag = Some(ident.to_ascii_lowercase());
        saw_any = true;
      }
    }
  }

  if !saw_any {
    return Err(());
  }
  Ok(compound)
}

fn parse_dom_ident(bytes: &[u8], pos: &mut usize) -> Result<String, ()> {
  let start = *pos;
  while *pos < bytes.len() {
    let b = bytes[*pos];
    if (b as char).is_ascii_alphanumeric() || b == b'_' || b == b'-' {
      *pos += 1;
    } else {
      break;
    }
  }
  if *pos == start {
    return Err(());
  }
  Ok(String::from_utf8(bytes[start..*pos].to_vec()).map_err(|_| ())?)
}

fn dom_require_element(rt: &mut JsWptRuntime, value: Value, method: &str) -> Result<GcObject, JsError> {
  let Value::Object(obj) = value else {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(&format!(
      "TypeError: {method} called on non-Element"
    ))?)));
  };
  if !rt
    .dom_nodes
    .get(&obj)
    .is_some_and(|state| state.kind == DomNodeKind::Element)
  {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(&format!(
      "TypeError: {method} called on non-Element"
    ))?)));
  }
  Ok(obj)
}

fn dom_get_string_prop(rt: &mut JsWptRuntime, obj: GcObject, name: &str) -> Result<String, JsError> {
  let key = {
    let mut scope = rt.heap.scope();
    PropertyKey::from_string(scope.alloc_string(name)?)
  };
  let Some(desc) = rt.heap.get_property(obj, &key)? else {
    return Ok(String::new());
  };
  match desc.kind {
    PropertyKind::Data { value, .. } => match value {
      Value::String(s) => Ok(rt.heap.get_string(s)?.to_utf8_lossy()),
      _ => Ok(String::new()),
    },
    PropertyKind::Accessor { get, .. } => {
      if matches!(get, Value::Undefined) {
        return Ok(String::new());
      }
      if !rt.is_callable_value(get) {
        return Err(JsError::Vm(VmError::TypeError("accessor getter is not callable")));
      }
      let value = rt.call(get, Value::Object(obj), &[])?;
      match value {
        Value::String(s) => Ok(rt.heap.get_string(s)?.to_utf8_lossy()),
        _ => Ok(String::new()),
      }
    }
  }
}

fn dom_compound_matches(
  rt: &mut JsWptRuntime,
  element: GcObject,
  compound: &DomCompoundSelector,
  scope: GcObject,
) -> Result<bool, JsError> {
  if compound.is_scope && element != scope {
    return Ok(false);
  }

  if let Some(tag) = &compound.tag {
    let Some(state) = rt.dom_nodes.get(&element) else {
      return Ok(false);
    };
    if state.tag_name != *tag {
      return Ok(false);
    }
  }

  if let Some(id) = &compound.id {
    let actual = dom_get_string_prop(rt, element, "id")?;
    if actual != *id {
      return Ok(false);
    }
  }

  if !compound.classes.is_empty() {
    let class_name = dom_get_string_prop(rt, element, "className")?;
    for required in &compound.classes {
      let mut has = false;
      for part in class_name.split_whitespace() {
        if part == required {
          has = true;
          break;
        }
      }
      if !has {
        return Ok(false);
      }
    }
  }

  Ok(true)
}

fn dom_matches_selector(
  rt: &mut JsWptRuntime,
  element: GcObject,
  selector: &DomSelector,
  scope: GcObject,
  allow_self_without_scope: bool,
) -> Result<bool, JsError> {
  if selector.compounds.is_empty() {
    return Ok(false);
  }

  let mut current = element;
  let right = selector.compounds.last().expect("non-empty compounds");
  if !dom_compound_matches(rt, current, right, scope)? {
    return Ok(false);
  }
  if !allow_self_without_scope && element == scope && !right.is_scope {
    return Ok(false);
  }

  // Walk combinators from right-to-left.
  for idx in (0..selector.combinators.len()).rev() {
    let combinator = selector.combinators[idx];
    let left = &selector.compounds[idx];

    match combinator {
      DomCombinator::Child => {
        let parent = rt.dom_nodes.get(&current).and_then(|s| s.parent);
        let Some(parent) = parent else {
          return Ok(false);
        };
        if !dom_compound_matches(rt, parent, left, scope)? {
          return Ok(false);
        }
        current = parent;
      }
      DomCombinator::Descendant => {
        let mut parent = rt.dom_nodes.get(&current).and_then(|s| s.parent);
        let mut found = None;
        while let Some(p) = parent {
          if dom_compound_matches(rt, p, left, scope)? {
            found = Some(p);
            break;
          }
          parent = rt.dom_nodes.get(&p).and_then(|s| s.parent);
        }
        let Some(found) = found else {
          return Ok(false);
        };
        current = found;
      }
    }
  }

  Ok(true)
}

fn dom_matches_any_selector(
  rt: &mut JsWptRuntime,
  element: GcObject,
  selectors: &[DomSelector],
  scope: GcObject,
  allow_self_without_scope: bool,
) -> Result<bool, JsError> {
  for sel in selectors {
    if dom_matches_selector(rt, element, sel, scope, allow_self_without_scope)? {
      return Ok(true);
    }
  }
  Ok(false)
}

fn dom_subtree_preorder(rt: &JsWptRuntime, root: GcObject) -> Vec<GcObject> {
  let mut out = Vec::new();
  let mut stack = vec![root];
  while let Some(node) = stack.pop() {
    out.push(node);
    let Some(state) = rt.dom_nodes.get(&node) else {
      continue;
    };
    for &child in state.children.iter().rev() {
      stack.push(child);
    }
  }
  out
}

fn native_dom_remove_child(rt: &mut JsWptRuntime, this: Value, args: &[Value]) -> Result<Value, JsError> {
  let parent = dom_require_node(rt, this, "removeChild")?;
  let child = dom_require_node(
    rt,
    args.get(0).copied().unwrap_or(Value::Undefined),
    "removeChild",
  )?;

  if !rt.dom_nodes.contains_key(&parent) {
    return dom_throw_dom_exception(rt, "NotFoundError");
  }
  if !rt.dom_nodes.contains_key(&child) {
    return dom_throw_dom_exception(rt, "NotFoundError");
  }

  let parent_tag = rt
    .dom_nodes
    .get(&parent)
    .map(|s| s.tag_name.clone())
    .unwrap_or_default();
  let parent_is_template = parent_tag == "template";

  // Per spec, throw if `child` is not actually a child of `parent`.
  let siblings = if parent_is_template {
    rt.dom_nodes
      .get(&parent)
      .map(|s| s.template_content.clone())
      .unwrap_or_default()
  } else {
    rt.dom_nodes
      .get(&parent)
      .map(|s| s.children.clone())
      .unwrap_or_default()
  };
  if !siblings.iter().any(|&n| n == child) {
    return dom_throw_dom_exception(rt, "NotFoundError");
  }

  if let Some(state) = rt.dom_nodes.get_mut(&parent) {
    match state.kind {
      DomNodeKind::Text => return dom_throw_dom_exception(rt, "NotFoundError"),
      DomNodeKind::DocumentFragment => {
        state.children.retain(|&c| c != child);
      }
      DomNodeKind::Element => {
        if state.tag_name == "template" {
          state.template_content.retain(|&c| c != child);
        } else {
          state.children.retain(|&c| c != child);
        }
      }
    }
  }
  if let Some(state) = rt.dom_nodes.get_mut(&child) {
    if state.parent == Some(parent) {
      state.parent = None;
    }
  }
  let parent_node_key = {
    let mut scope = rt.heap.scope();
    PropertyKey::from_string(scope.alloc_string("parentNode")?)
  };
  rt.define_data_prop(child, parent_node_key, Value::Null)?;
  if let Some(state) = rt.event_targets.get_mut(&child) {
    if state.parent == Some(parent) {
      state.parent = None;
    }
  }
  rt.update_dom_child_nodes(parent)?;
  Ok(Value::Object(child))
}

fn dom_is_node(rt: &JsWptRuntime, obj: GcObject) -> bool {
  if rt.dom_nodes.contains_key(&obj) {
    return true;
  }
  matches!(rt.env.get("document"), Some(Value::Object(doc)) if doc == obj)
}

fn dom_parent_for_contains(rt: &JsWptRuntime, node: GcObject) -> Option<GcObject> {
  let parent = rt.dom_nodes.get(&node).and_then(|s| s.parent)?;
  let Some(parent_state) = rt.dom_nodes.get(&parent) else {
    return Some(parent);
  };
  if parent_state.tag_name == "template" && parent_state.template_content.contains(&node) {
    return None;
  }
  Some(parent)
}

fn dom_require_node(rt: &mut JsWptRuntime, value: Value, method: &str) -> Result<GcObject, JsError> {
  let Value::Object(obj) = value else {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(&format!(
      "TypeError: {method} called on non-Node"
    ))?)));
  };
  if dom_is_node(rt, obj) {
    return Ok(obj);
  }
  Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(&format!(
    "TypeError: {method} called on non-Node"
  ))?)))
}

fn dom_require_text(rt: &mut JsWptRuntime, value: Value, method: &str) -> Result<GcObject, JsError> {
  let Value::Object(obj) = value else {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(&format!(
      "TypeError: {method} called on non-Text"
    ))?)));
  };
  let is_text = rt
    .dom_nodes
    .get(&obj)
    .is_some_and(|state| state.kind == DomNodeKind::Text);
  if is_text {
    return Ok(obj);
  }
  Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(&format!(
    "TypeError: {method} called on non-Text"
  ))?)))
}

fn native_illegal_dom_constructor(
  rt: &mut JsWptRuntime,
  _this: Value,
  _args: &[Value],
) -> Result<Value, JsError> {
  Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(
    "TypeError: Illegal constructor",
  )?)))
}

fn dom_escape_text(out: &mut String, text: &str) {
  for ch in text.chars() {
    match ch {
      '&' => out.push_str("&amp;"),
      '<' => out.push_str("&lt;"),
      '>' => out.push_str("&gt;"),
      _ => out.push(ch),
    }
  }
}

fn dom_escape_attr_value(out: &mut String, text: &str) {
  for ch in text.chars() {
    match ch {
      '&' => out.push_str("&amp;"),
      '<' => out.push_str("&lt;"),
      '>' => out.push_str("&gt;"),
      '"' => out.push_str("&quot;"),
      _ => out.push(ch),
    }
  }
}

fn dom_is_void_html_element(tag_name: &str) -> bool {
  // https://html.spec.whatwg.org/#void-elements
  matches!(
    tag_name,
    "area"
      | "base"
      | "br"
      | "col"
      | "embed"
      | "hr"
      | "img"
      | "input"
      | "link"
      | "meta"
      | "param"
      | "source"
      | "track"
      | "wbr"
  )
}

fn dom_element_children_for_serialization(state: &DomNodeState) -> &[GcObject] {
  if state.tag_name == "template" {
    &state.template_content
  } else {
    &state.children
  }
}

fn dom_serialize_attributes(rt: &JsWptRuntime, state: &DomNodeState, out: &mut String) -> Result<(), JsError> {
  if state.attributes.is_empty() {
    return Ok(());
  }

  let mut push_attr = |name: &str, value: GcString| -> Result<(), JsError> {
    out.push(' ');
    out.push_str(name);
    out.push_str("=\"");
    let value = rt.heap.get_string(value)?.to_utf8_lossy();
    dom_escape_attr_value(out, &value);
    out.push('"');
    Ok(())
  };

  // Emit commonly-used HTML attributes in a stable order.
  if let Some(&value) = state.attributes.get("id") {
    push_attr("id", value)?;
  }
  if let Some(&value) = state.attributes.get("class") {
    push_attr("class", value)?;
  }

  // Emit any remaining attributes in a deterministic order.
  let mut remaining = state
    .attributes
    .iter()
    .filter_map(|(name, &value)| {
      if name == "id" || name == "class" {
        None
      } else {
        Some((name.as_str(), value))
      }
    })
    .collect::<Vec<_>>();
  remaining.sort_by(|(a, _), (b, _)| a.cmp(b));
  for (name, value) in remaining {
    push_attr(name, value)?;
  }

  Ok(())
}

fn dom_serialize_node(rt: &JsWptRuntime, node: GcObject, out: &mut String) -> Result<(), JsError> {
  let Some(state) = rt.dom_nodes.get(&node) else {
    return Ok(());
  };

  match state.kind {
    DomNodeKind::DocumentFragment => {
      for &child in dom_element_children_for_serialization(state) {
        dom_serialize_node(rt, child, out)?;
      }
      Ok(())
    }
    DomNodeKind::Element => {
      out.push('<');
      out.push_str(&state.tag_name);
      dom_serialize_attributes(rt, state, out)?;
      out.push('>');

      if !dom_is_void_html_element(&state.tag_name) {
        let children = dom_element_children_for_serialization(state);
        if !children.is_empty() {
          for &child in children {
            dom_serialize_node(rt, child, out)?;
          }
        } else if let Some(text) = &state.text_content {
          dom_escape_text(out, text);
        }

        out.push_str("</");
        out.push_str(&state.tag_name);
        out.push('>');
      }
      Ok(())
    }
    DomNodeKind::Text => {
      if let Some(text) = &state.text_content {
        dom_escape_text(out, text);
      }
      Ok(())
    }
  }
}

const HTML_NAMESPACE: &str = "http://www.w3.org/1999/xhtml";

fn handle_children(handle: &Handle) -> Vec<Handle> {
  handle.children.borrow().iter().cloned().collect()
}

fn fragment_children_from_rcdom(rcdom: &RcDom) -> Vec<Handle> {
  let children = handle_children(&rcdom.document);
  let significant: Vec<Handle> = children
    .iter()
    .filter(|handle| !matches!(handle.data, NodeData::Doctype { .. } | NodeData::Comment { .. }))
    .cloned()
    .collect();

  // `html5ever`'s fragment parsing may return a synthetic `<html>` wrapper; strip it so callers can
  // insert the nodes directly (matching `innerHTML`/`outerHTML` semantics).
  if significant.len() == 1 {
    if let NodeData::Element { name, .. } = &significant[0].data {
      if name.ns.to_string() == HTML_NAMESPACE && name.local.as_ref().eq_ignore_ascii_case("html") {
        return handle_children(&significant[0]);
      }
    }
  }

  significant
}

fn dom_parse_html_fragment(
  rt: &mut JsWptRuntime,
  context_tag: &str,
  html: &str,
) -> Result<Vec<GcObject>, JsError> {
  let context_tag = if context_tag.is_empty() {
    "div"
  } else {
    context_tag
  };

  let context = QualName::new(
    None,
    Namespace::from(HTML_NAMESPACE),
    LocalName::from(context_tag.to_ascii_lowercase()),
  );

  let opts = ParseOpts {
    tree_builder: TreeBuilderOpts {
      scripting_enabled: true,
      ..Default::default()
    },
    ..Default::default()
  };

  // `html5ever::parse_fragment` takes `context_element_allows_scripting` as a separate boolean flag
  // (it only affects the tokenizer initial state when the context element is `<noscript>`). Our
  // harness assumes JS-enabled parsing semantics, so keep it enabled.
  let rcdom: RcDom = parse_fragment(RcDom::default(), opts, context, Vec::new(), true).one(html);

  #[derive(Clone)]
  struct WorkItem {
    parent: Option<GcObject>,
    handle: Handle,
  }

  let mut roots: Vec<GcObject> = Vec::new();
  let mut stack: Vec<WorkItem> = fragment_children_from_rcdom(&rcdom)
    .into_iter()
    .rev()
    .map(|handle| WorkItem { parent: None, handle })
    .collect();

  while let Some(item) = stack.pop() {
    match &item.handle.data {
      NodeData::Document => {
        for child in handle_children(&item.handle).into_iter().rev() {
          stack.push(WorkItem {
            parent: item.parent,
            handle: child,
          });
        }
      }
      NodeData::Text { contents } => {
        let content = contents.borrow().to_string();
        let id = rt.alloc_dom_text_node(&content)?;
        if let Some(parent) = item.parent {
          let _ = native_node_append_child(rt, Value::Object(parent), &[Value::Object(id)])?;
        } else {
          roots.push(id);
        }
      }
      NodeData::Element {
        name,
        attrs,
        template_contents,
        ..
      } => {
        let id = rt.alloc_dom_element(&name.local.to_string())?;

        let attrs_ref = attrs.borrow();
        for attr in attrs_ref.iter() {
          let attr_name = attr.name.local.to_string().to_ascii_lowercase();
          let value = attr.value.to_string();
          let value_handle = {
            let mut scope = rt.heap.scope();
            scope.alloc_string(&value)?
          };
          if let Some(state) = rt.dom_nodes.get_mut(&id) {
            state.attributes.insert(attr_name, value_handle);
          }
        }

        if let Some(parent) = item.parent {
          let _ = native_node_append_child(rt, Value::Object(parent), &[Value::Object(id)])?;
        } else {
          roots.push(id);
        }

        let is_template = name.local.as_ref().eq_ignore_ascii_case("template");
        let children = if is_template {
          template_contents
            .borrow()
            .as_ref()
            .map(handle_children)
            .unwrap_or_else(|| handle_children(&item.handle))
        } else {
          handle_children(&item.handle)
        };

        for child in children.into_iter().rev() {
          stack.push(WorkItem {
            parent: Some(id),
            handle: child,
          });
        }
      }
      _ => {}
    }
  }

  Ok(roots)
}

fn native_dom_element_get_attribute(
  rt: &mut JsWptRuntime,
  this: Value,
  args: &[Value],
) -> Result<Value, JsError> {
  let element = dom_require_element(rt, this, "getAttribute")?;
  let name = string_from_value(rt, args.get(0).copied().unwrap_or(Value::Undefined))?;
  let name = name.to_ascii_lowercase();
  let Some(state) = rt.dom_nodes.get(&element) else {
    return Ok(Value::Null);
  };
  Ok(match state.attributes.get(&name) {
    Some(&value) => Value::String(value),
    None => Value::Null,
  })
}

fn native_dom_element_set_attribute(
  rt: &mut JsWptRuntime,
  this: Value,
  args: &[Value],
) -> Result<Value, JsError> {
  let element = dom_require_element(rt, this, "setAttribute")?;
  let name = string_from_value(rt, args.get(0).copied().unwrap_or(Value::Undefined))?;
  let name = name.to_ascii_lowercase();
  let value = string_from_value(rt, args.get(1).copied().unwrap_or(Value::Undefined))?;
  let Value::String(handle) = rt.alloc_string_value(&value)? else {
    return Ok(Value::Undefined);
  };
  if let Some(state) = rt.dom_nodes.get_mut(&element) {
    state.attributes.insert(name, handle);
  }
  Ok(Value::Undefined)
}

fn native_dom_element_remove_attribute(
  rt: &mut JsWptRuntime,
  this: Value,
  args: &[Value],
) -> Result<Value, JsError> {
  let element = dom_require_element(rt, this, "removeAttribute")?;
  let name = string_from_value(rt, args.get(0).copied().unwrap_or(Value::Undefined))?;
  let name = name.to_ascii_lowercase();
  if let Some(state) = rt.dom_nodes.get_mut(&element) {
    state.attributes.remove(&name);
  }
  Ok(Value::Undefined)
}

fn native_dom_element_get_id(rt: &mut JsWptRuntime, this: Value, _args: &[Value]) -> Result<Value, JsError> {
  let element = dom_require_element(rt, this, "id")?;
  let Some(state) = rt.dom_nodes.get(&element) else {
    return rt.alloc_string_value("");
  };
  match state.attributes.get("id") {
    Some(&value) => Ok(Value::String(value)),
    None => rt.alloc_string_value(""),
  }
}

fn native_dom_element_set_id(rt: &mut JsWptRuntime, this: Value, args: &[Value]) -> Result<Value, JsError> {
  let element = dom_require_element(rt, this, "id")?;
  let value = string_from_value(rt, args.get(0).copied().unwrap_or(Value::Undefined))?;
  let Value::String(handle) = rt.alloc_string_value(&value)? else {
    return Ok(Value::Undefined);
  };
  if let Some(state) = rt.dom_nodes.get_mut(&element) {
    state.attributes.insert("id".to_string(), handle);
  }
  Ok(Value::Undefined)
}

fn native_dom_element_get_class_name(
  rt: &mut JsWptRuntime,
  this: Value,
  _args: &[Value],
) -> Result<Value, JsError> {
  let element = dom_require_element(rt, this, "className")?;
  let Some(state) = rt.dom_nodes.get(&element) else {
    return rt.alloc_string_value("");
  };
  match state.attributes.get("class") {
    Some(&value) => Ok(Value::String(value)),
    None => rt.alloc_string_value(""),
  }
}

fn native_dom_element_set_class_name(
  rt: &mut JsWptRuntime,
  this: Value,
  args: &[Value],
) -> Result<Value, JsError> {
  let element = dom_require_element(rt, this, "className")?;
  let value = string_from_value(rt, args.get(0).copied().unwrap_or(Value::Undefined))?;
  let Value::String(handle) = rt.alloc_string_value(&value)? else {
    return Ok(Value::Undefined);
  };
  if let Some(state) = rt.dom_nodes.get_mut(&element) {
    state.attributes.insert("class".to_string(), handle);
  }
  Ok(Value::Undefined)
}

fn dom_token_list_validate_token(rt: &mut JsWptRuntime, token: &str) -> Result<(), JsError> {
  if token.is_empty() {
    return dom_throw_syntax_error(rt, "DOMTokenList token must not be empty").map(|_| ());
  }
  if token
    .as_bytes()
    .iter()
    .any(|&b| matches!(b, b'\t' | b'\n' | 0x0C | b'\r' | b' '))
  {
    return dom_throw_invalid_character_error(rt, "DOMTokenList token must not contain whitespace")
      .map(|_| ());
  }
  Ok(())
}

fn dom_token_list_tokens_from_element(rt: &JsWptRuntime, element: GcObject) -> Result<Vec<String>, JsError> {
  let Some(state) = rt.dom_nodes.get(&element) else {
    return Ok(Vec::new());
  };
  let class_value = state
    .attributes
    .get("class")
    .copied()
    .and_then(|s| rt.heap.get_string(s).ok())
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_default();

  let mut out = Vec::new();
  let mut seen: HashSet<String> = HashSet::new();
  for token in class_value
    .split(|ch| matches!(ch, '\t' | '\n' | '\u{000c}' | '\r' | ' '))
    .filter(|t| !t.is_empty())
  {
    if seen.insert(token.to_string()) {
      out.push(token.to_string());
    }
  }
  Ok(out)
}

fn dom_token_list_write_tokens_to_element(
  rt: &mut JsWptRuntime,
  element: GcObject,
  tokens: &[String],
) -> Result<(), JsError> {
  let class_value = tokens.join(" ");
  let Value::String(handle) = rt.alloc_string_value(&class_value)? else {
    return Ok(());
  };
  if let Some(state) = rt.dom_nodes.get_mut(&element) {
    state.attributes.insert("class".to_string(), handle);
  }
  Ok(())
}

fn dom_require_dom_token_list(
  rt: &mut JsWptRuntime,
  value: Value,
  method: &str,
) -> Result<(GcObject, GcObject), JsError> {
  let Value::Object(obj) = value else {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(&format!(
      "TypeError: DOMTokenList.{method} called on non-object"
    ))?)));
  };
  let Some(&element) = rt.dom_token_lists.get(&obj) else {
    return Err(JsError::Vm(VmError::Throw(rt.alloc_string_value(&format!(
      "TypeError: DOMTokenList.{method} called on non-DOMTokenList"
    ))?)));
  };
  Ok((obj, element))
}

fn native_dom_element_get_class_list(rt: &mut JsWptRuntime, this: Value, _args: &[Value]) -> Result<Value, JsError> {
  let element = dom_require_element(rt, this, "classList")?;
  if let Some(existing) = rt.dom_nodes.get(&element).and_then(|s| s.class_list) {
    return Ok(Value::Object(existing));
  }

  let obj = rt.alloc_object()?;
  let proto = rt.dom_token_list_proto()?;
  rt.heap.object_set_prototype(obj, Some(proto))?;
  rt.dom_token_lists.insert(obj, element);
  if let Some(state) = rt.dom_nodes.get_mut(&element) {
    state.class_list = Some(obj);
  }
  Ok(Value::Object(obj))
}

fn native_dom_token_list_contains(rt: &mut JsWptRuntime, this: Value, args: &[Value]) -> Result<Value, JsError> {
  let (_list, element) = dom_require_dom_token_list(rt, this, "contains")?;
  let token = string_from_value(rt, args.get(0).copied().unwrap_or(Value::Undefined))?;
  dom_token_list_validate_token(rt, &token)?;

  let tokens = dom_token_list_tokens_from_element(rt, element)?;
  Ok(Value::Bool(tokens.iter().any(|t| t == &token)))
}

fn native_dom_token_list_add(rt: &mut JsWptRuntime, this: Value, args: &[Value]) -> Result<Value, JsError> {
  let (_list, element) = dom_require_dom_token_list(rt, this, "add")?;
  let mut tokens = dom_token_list_tokens_from_element(rt, element)?;
  let mut seen: HashSet<String> = tokens.iter().cloned().collect();

  for arg in args.iter().copied() {
    let token = string_from_value(rt, arg)?;
    dom_token_list_validate_token(rt, &token)?;
    if seen.insert(token.clone()) {
      tokens.push(token);
    }
  }

  dom_token_list_write_tokens_to_element(rt, element, &tokens)?;
  Ok(Value::Undefined)
}

fn native_dom_token_list_remove(rt: &mut JsWptRuntime, this: Value, args: &[Value]) -> Result<Value, JsError> {
  let (_list, element) = dom_require_dom_token_list(rt, this, "remove")?;
  let mut to_remove: HashSet<String> = HashSet::new();
  for arg in args.iter().copied() {
    let token = string_from_value(rt, arg)?;
    dom_token_list_validate_token(rt, &token)?;
    to_remove.insert(token);
  }

  let mut tokens = dom_token_list_tokens_from_element(rt, element)?;
  tokens.retain(|t| !to_remove.contains(t));
  dom_token_list_write_tokens_to_element(rt, element, &tokens)?;
  Ok(Value::Undefined)
}

fn native_dom_token_list_toggle(rt: &mut JsWptRuntime, this: Value, args: &[Value]) -> Result<Value, JsError> {
  let (_list, element) = dom_require_dom_token_list(rt, this, "toggle")?;
  let token = string_from_value(rt, args.get(0).copied().unwrap_or(Value::Undefined))?;
  dom_token_list_validate_token(rt, &token)?;

  let force = match args.get(1).copied() {
    None | Some(Value::Undefined) => None,
    Some(v) => Some(to_boolean(&mut rt.heap, v)?),
  };

  let mut tokens = dom_token_list_tokens_from_element(rt, element)?;
  let has = tokens.iter().any(|t| t == &token);
  let should_add = match force {
    Some(true) => true,
    Some(false) => false,
    None => !has,
  };

  let new_has = if should_add {
    if !has {
      tokens.push(token);
    }
    true
  } else {
    tokens.retain(|t| t != &token);
    false
  };

  dom_token_list_write_tokens_to_element(rt, element, &tokens)?;
  Ok(Value::Bool(new_has))
}

fn native_text_get_data(rt: &mut JsWptRuntime, this: Value, _args: &[Value]) -> Result<Value, JsError> {
  let node = dom_require_text(rt, this, "data")?;
  let data = rt
    .dom_nodes
    .get(&node)
    .and_then(|s| s.text_content.clone())
    .unwrap_or_default();
  rt.alloc_string_value(&data)
}

fn native_text_set_data(rt: &mut JsWptRuntime, this: Value, args: &[Value]) -> Result<Value, JsError> {
  let node = dom_require_text(rt, this, "data")?;
  let data = string_from_value(rt, args.get(0).copied().unwrap_or(Value::Undefined))?;

  if let Some(state) = rt.dom_nodes.get_mut(&node) {
    state.text_content = if data.is_empty() { None } else { Some(data.clone()) };
  }

  // Keep `nodeValue` in sync with `data`.
  let node_value_key = {
    let mut scope = rt.heap.scope();
    PropertyKey::from_string(scope.alloc_string("nodeValue")?)
  };
  let node_value = rt.alloc_string_value(&data)?;
  rt.define_data_prop(node, node_value_key, node_value)?;
  Ok(Value::Undefined)
}

fn native_dom_element_get_inner_html(
  rt: &mut JsWptRuntime,
  this: Value,
  _args: &[Value],
) -> Result<Value, JsError> {
  let element = dom_require_element(rt, this, "innerHTML")?;
  let Some(state) = rt.dom_nodes.get(&element) else {
    return rt.alloc_string_value("");
  };

  let children = dom_element_children_for_serialization(state);
  let mut out = String::new();
  if !children.is_empty() {
    for &child in children {
      dom_serialize_node(rt, child, &mut out)?;
    }
  } else if let Some(text) = &state.text_content {
    dom_escape_text(&mut out, text);
  }
  rt.alloc_string_value(&out)
}

fn dom_detach_children(rt: &mut JsWptRuntime, parent: GcObject) -> Result<(), JsError> {
  let (children, template_children) = rt
    .dom_nodes
    .get(&parent)
    .map(|s| (s.children.clone(), s.template_content.clone()))
    .unwrap_or_default();
  let parent_node_key = {
    let mut scope = rt.heap.scope();
    PropertyKey::from_string(scope.alloc_string("parentNode")?)
  };

  for child in children.into_iter().chain(template_children.into_iter()) {
    if let Some(state) = rt.dom_nodes.get_mut(&child) {
      state.parent = None;
    }
    if let Some(et) = rt.event_targets.get_mut(&child) {
      et.parent = None;
    }
    rt.define_data_prop(child, parent_node_key, Value::Null)?;
  }

  if let Some(state) = rt.dom_nodes.get_mut(&parent) {
    state.children.clear();
    state.template_content.clear();
    state.text_content = None;
  }
  rt.update_dom_child_nodes(parent)?;
  Ok(())
}

fn native_dom_element_set_inner_html(
  rt: &mut JsWptRuntime,
  this: Value,
  args: &[Value],
) -> Result<Value, JsError> {
  let element = dom_require_element(rt, this, "innerHTML")?;
  let html = string_from_value(rt, args.get(0).copied().unwrap_or(Value::Undefined))?;

  dom_detach_children(rt, element)?;

  let context_tag = rt
    .dom_nodes
    .get(&element)
    .map(|s| s.tag_name.clone())
    .unwrap_or_else(|| "div".to_string());
  let new_nodes = dom_parse_html_fragment(rt, &context_tag, &html)?;
  for node in new_nodes {
    let args = [Value::Object(node)];
    let _ = native_node_append_child(rt, Value::Object(element), &args)?;
  }
  Ok(Value::Undefined)
}

fn native_dom_element_get_outer_html(
  rt: &mut JsWptRuntime,
  this: Value,
  _args: &[Value],
) -> Result<Value, JsError> {
  let element = dom_require_element(rt, this, "outerHTML")?;
  let mut out = String::new();
  dom_serialize_node(rt, element, &mut out)?;
  rt.alloc_string_value(&out)
}

fn native_dom_element_set_outer_html(
  rt: &mut JsWptRuntime,
  this: Value,
  args: &[Value],
) -> Result<Value, JsError> {
  let element = dom_require_element(rt, this, "outerHTML")?;
  let html = string_from_value(rt, args.get(0).copied().unwrap_or(Value::Undefined))?;

  let parent = rt.dom_nodes.get(&element).and_then(|s| s.parent);
  let Some(parent) = parent else {
    // Spec: if the element has no parent, setting outerHTML is a no-op.
    return Ok(Value::Undefined);
  };

  let parent_tag = rt
    .dom_nodes
    .get(&parent)
    .map(|s| s.tag_name.clone())
    .unwrap_or_default();
  let parent_is_template = parent_tag == "template";
  let parent_node_key = {
    let mut scope = rt.heap.scope();
    PropertyKey::from_string(scope.alloc_string("parentNode")?)
  };

  let context_tag = if parent_tag.is_empty() {
    "div".to_string()
  } else {
    parent_tag.clone()
  };
  let new_nodes = dom_parse_html_fragment(rt, &context_tag, &html)?;

  // Detach the replaced element.
  if let Some(state) = rt.dom_nodes.get_mut(&element) {
    state.parent = None;
  }
  if let Some(et) = rt.event_targets.get_mut(&element) {
    et.parent = None;
  }
  rt.define_data_prop(element, parent_node_key, Value::Null)?;

  // Replace in parent's child list.
  let replacement_idx = rt
    .dom_nodes
    .get(&parent)
    .and_then(|s| {
      let list = if parent_is_template {
        &s.template_content
      } else {
        &s.children
      };
      list.iter().position(|&id| id == element)
    })
    .ok_or_else(|| JsError::Vm(VmError::TypeError("outerHTML target is not a child of its parent")))?;

  if let Some(state) = rt.dom_nodes.get_mut(&parent) {
    state.text_content = None;
    let list = if parent_is_template {
      &mut state.template_content
    } else {
      &mut state.children
    };
    list.splice(
      replacement_idx..replacement_idx + 1,
      new_nodes.iter().copied(),
    );
  }

  for &node in &new_nodes {
    if let Some(state) = rt.dom_nodes.get_mut(&node) {
      state.parent = Some(parent);
    }
    if let Some(et) = rt.event_targets.get_mut(&node) {
      et.parent = Some(parent);
    }
    rt.define_data_prop(node, parent_node_key, Value::Object(parent))?;
  }

  rt.update_dom_child_nodes(parent)?;
  Ok(Value::Undefined)
}

fn native_dom_element_matches(rt: &mut JsWptRuntime, this: Value, args: &[Value]) -> Result<Value, JsError> {
  let element = dom_require_element(rt, this, "matches")?;
  let selector_text = string_from_value(rt, args.get(0).copied().unwrap_or(Value::Undefined))?;
  let selectors = match parse_dom_selector_list(&selector_text) {
    Ok(selectors) => selectors,
    Err(_) => {
      let message = format!("invalid selector: {selector_text}");
      return dom_throw_syntax_error(rt, &message);
    }
  };
  let ok = dom_matches_any_selector(rt, element, &selectors, element, /* allow_self_without_scope */ true)?;
  Ok(Value::Bool(ok))
}

fn native_dom_element_closest(rt: &mut JsWptRuntime, this: Value, args: &[Value]) -> Result<Value, JsError> {
  let element = dom_require_element(rt, this, "closest")?;
  let selector_text = string_from_value(rt, args.get(0).copied().unwrap_or(Value::Undefined))?;
  let selectors = match parse_dom_selector_list(&selector_text) {
    Ok(selectors) => selectors,
    Err(_) => {
      let message = format!("invalid selector: {selector_text}");
      return dom_throw_syntax_error(rt, &message);
    }
  };

  let mut current = Some(element);
  while let Some(node) = current {
    if dom_matches_any_selector(rt, node, &selectors, element, /* allow_self_without_scope */ true)? {
      return Ok(Value::Object(node));
    }
    current = rt.dom_nodes.get(&node).and_then(|s| s.parent);
  }
  Ok(Value::Null)
}

fn native_dom_element_query_selector(
  rt: &mut JsWptRuntime,
  this: Value,
  args: &[Value],
) -> Result<Value, JsError> {
  let scope = dom_require_element(rt, this, "querySelector")?;
  let selector_text = string_from_value(rt, args.get(0).copied().unwrap_or(Value::Undefined))?;
  let selectors = match parse_dom_selector_list(&selector_text) {
    Ok(selectors) => selectors,
    Err(_) => {
      let message = format!("invalid selector: {selector_text}");
      return dom_throw_syntax_error(rt, &message);
    }
  };

  let nodes = dom_subtree_preorder(rt, scope);
  for node in nodes {
    if dom_matches_any_selector(rt, node, &selectors, scope, /* allow_self_without_scope */ false)? {
      return Ok(Value::Object(node));
    }
  }
  Ok(Value::Null)
}

fn native_dom_element_query_selector_all(
  rt: &mut JsWptRuntime,
  this: Value,
  args: &[Value],
) -> Result<Value, JsError> {
  let scope = dom_require_element(rt, this, "querySelectorAll")?;
  let selector_text = string_from_value(rt, args.get(0).copied().unwrap_or(Value::Undefined))?;
  let selectors = match parse_dom_selector_list(&selector_text) {
    Ok(selectors) => selectors,
    Err(_) => {
      let message = format!("invalid selector: {selector_text}");
      return dom_throw_syntax_error(rt, &message);
    }
  };

  let nodes = dom_subtree_preorder(rt, scope);
  let mut matches = Vec::new();
  for node in nodes {
    if dom_matches_any_selector(rt, node, &selectors, scope, /* allow_self_without_scope */ false)? {
      matches.push(node);
    }
  }

  let list = rt.make_dom_nodelist(&matches)?;
  Ok(Value::Object(list))
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
      Ok(v) => rt.resolve_promise(next, v)?,
      Err(JsError::Vm(err)) => match err.thrown_value() {
        Some(reason) => rt.settle_promise(next, PromiseStatus::Rejected, reason)?,
        None => return Err(JsError::Vm(err)),
      },
      Err(other) => return Err(other),
    }
  } else {
    match job.status {
      PromiseStatus::Fulfilled => rt.resolve_promise(next, job.value)?,
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
  let mut subtests: Vec<WptSubtest> = Vec::new();

  if let Value::Object(obj) = payload {
    let file_status_key = PropertyKey::from_string(rt.keys.file_status);
    let harness_status_key = PropertyKey::from_string(rt.keys.harness_status);
    let message_key = PropertyKey::from_string(rt.keys.message);
    let stack_key = PropertyKey::from_string(rt.keys.stack);
    let subtests_key = PropertyKey::from_string(rt.keys.subtests);
    let name_key = PropertyKey::from_string(rt.keys.name);
    let status_key = PropertyKey::from_string(rt.keys.status);

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

    // Subtests array (optional).
    if let Some(desc) = rt.heap.get_property(obj, &subtests_key)? {
      if let PropertyKind::Data { value, .. } = desc.kind {
        if let Value::Object(arr) = value {
          // The vm-js runtime models arrays as ordinary objects plus an internal backing vector
          // stored in `rt.arrays`.
          if let Some(elements) = rt.arrays.get(&arr).cloned() {
            for elem in elements {
              let Value::Object(st_obj) = elem else {
                continue;
              };

              let mut st_name: Option<String> = None;
              let mut st_status: Option<String> = None;
              let mut st_message: Option<String> = None;
              let mut st_stack: Option<String> = None;

              if let Some(desc) = rt.heap.get_property(st_obj, &name_key)? {
                if let PropertyKind::Data { value, .. } = desc.kind {
                  if !matches!(value, Value::Undefined | Value::Null) {
                    st_name = Some(rt.value_to_string_lossy(value));
                  }
                }
              }
              if let Some(desc) = rt.heap.get_property(st_obj, &status_key)? {
                if let PropertyKind::Data { value, .. } = desc.kind {
                  if !matches!(value, Value::Undefined | Value::Null) {
                    st_status = Some(rt.value_to_string_lossy(value));
                  }
                }
              }
              if let Some(desc) = rt.heap.get_property(st_obj, &message_key)? {
                if let PropertyKind::Data { value, .. } = desc.kind {
                  if !matches!(value, Value::Undefined | Value::Null) {
                    st_message = Some(rt.value_to_string_lossy(value));
                  }
                }
              }
              if let Some(desc) = rt.heap.get_property(st_obj, &stack_key)? {
                if let PropertyKind::Data { value, .. } = desc.kind {
                  if !matches!(value, Value::Undefined | Value::Null) {
                    st_stack = Some(rt.value_to_string_lossy(value));
                  }
                }
              }

              subtests.push(WptSubtest {
                name: st_name.unwrap_or_else(|| "<unnamed subtest>".to_string()),
                status: st_status.unwrap_or_else(|| "error".to_string()),
                message: st_message,
                stack: st_stack,
              });
            }
          }
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
    subtests,
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

  fn get_data_prop(rt: &mut JsWptRuntime, obj: GcObject, name: &str) -> Value {
    let key = {
      let mut scope = rt.heap.scope();
      PropertyKey::from_string(scope.alloc_string(name).expect("alloc key"))
    };
    let desc = rt
      .heap
      .get_property(obj, &key)
      .expect("get property")
      .unwrap_or_else(|| panic!("missing property {name}"));
    match desc.kind {
      PropertyKind::Data { value, .. } => value,
      PropertyKind::Accessor { .. } => panic!("unexpected accessor for {name}"),
    }
  }

  #[test]
  fn node_constants_and_metadata_exist() {
    let mut rt = JsWptRuntime::new("https://example.com/");

    let Value::Object(node_ctor) = rt.env.get("Node").expect("Node global") else {
      panic!("Node binding should be an object");
    };

    assert_eq!(get_data_prop(&mut rt, node_ctor, "ELEMENT_NODE"), Value::Number(1.0));
    assert_eq!(get_data_prop(&mut rt, node_ctor, "TEXT_NODE"), Value::Number(3.0));
    assert_eq!(get_data_prop(&mut rt, node_ctor, "DOCUMENT_NODE"), Value::Number(9.0));
    assert_eq!(
      get_data_prop(&mut rt, node_ctor, "DOCUMENT_FRAGMENT_NODE"),
      Value::Number(11.0)
    );

    // `window.Node` should reference the same constructor object.
    let global_object = rt.global_object;
    let window_node = get_data_prop(&mut rt, global_object, "Node");
    assert_eq!(window_node, Value::Object(node_ctor));

    // `Node.prototype` should be the shared DOM node prototype.
    let node_proto_value = get_data_prop(&mut rt, node_ctor, "prototype");
    assert_eq!(node_proto_value, Value::Object(rt.node_proto.expect("Node prototype")));

    // Document node metadata.
    let Value::Object(document) = rt.env.get("document").expect("document global") else {
      panic!("document binding should be an object");
    };
    assert_eq!(get_data_prop(&mut rt, document, "nodeType"), Value::Number(9.0));
    let node_name = get_data_prop(&mut rt, document, "nodeName");
    assert_eq!(rt.value_to_string_lossy(node_name), "#document");
    assert_eq!(get_data_prop(&mut rt, document, "nodeValue"), Value::Null);

    // Element metadata (`document.body`).
    let Value::Object(body) = get_data_prop(&mut rt, document, "body") else {
      panic!("document.body should be an object");
    };
    assert_eq!(get_data_prop(&mut rt, body, "nodeType"), Value::Number(1.0));
    assert_eq!(get_data_prop(&mut rt, body, "nodeName"), get_data_prop(&mut rt, body, "tagName"));
    assert_eq!(get_data_prop(&mut rt, body, "nodeValue"), Value::Null);

    // DocumentFragment metadata.
    let Value::Object(fragment) =
      native_document_create_document_fragment(&mut rt, Value::Object(document), &[])
        .expect("createDocumentFragment")
    else {
      panic!("expected fragment object");
    };
    assert_eq!(get_data_prop(&mut rt, fragment, "nodeType"), Value::Number(11.0));
    let fragment_name = get_data_prop(&mut rt, fragment, "nodeName");
    assert_eq!(rt.value_to_string_lossy(fragment_name), "#document-fragment");
    assert_eq!(get_data_prop(&mut rt, fragment, "nodeValue"), Value::Null);
  }

  #[test]
  fn document_cookie_round_trips_and_ignores_attributes() {
    let mut rt = JsWptRuntime::new("https://example.com/");

    let Value::Object(arr) = rt
      .exec_script(
        r#"
        const initial = document.cookie;
        document.cookie = "a=b";
        const after_set = document.cookie;
        document.cookie = "a=c";
        const after_overwrite = document.cookie;
        document.cookie = "b=d; Path=/; Expires=Wed, 21 Oct 2015 07:28:00 GMT";
        const after_second = document.cookie;
        [initial, after_set, after_overwrite, after_second];
      "#,
      )
      .expect("exec script")
    else {
      panic!("expected array return value");
    };

    let initial = get_data_prop(&mut rt, arr, "0");
    assert_eq!(rt.value_to_string_lossy(initial), "");
    let after_set = get_data_prop(&mut rt, arr, "1");
    assert_eq!(rt.value_to_string_lossy(after_set), "a=b");
    let after_overwrite = get_data_prop(&mut rt, arr, "2");
    assert_eq!(rt.value_to_string_lossy(after_overwrite), "a=c");
    let after_second = get_data_prop(&mut rt, arr, "3");
    assert_eq!(rt.value_to_string_lossy(after_second), "a=c; b=d");
  }

  #[test]
  fn element_attributes_reflect_in_getattribute_and_outerhtml() {
    let mut rt = JsWptRuntime::new("https://example.com/");

    let Value::Object(arr) = rt
      .exec_script(
        r#"
        const el = document.createElement("div");
        el.id = "root";
        el.className = "a b";
        [
          el.id,
          el.getAttribute("id"),
          el.getAttribute("class"),
          el.outerHTML,
        ];
      "#,
      )
      .expect("exec script")
    else {
      panic!("expected array return value");
    };

    let el_id = get_data_prop(&mut rt, arr, "0");
    assert_eq!(rt.value_to_string_lossy(el_id), "root");
    let attr_id = get_data_prop(&mut rt, arr, "1");
    assert_eq!(rt.value_to_string_lossy(attr_id), "root");
    let attr_class = get_data_prop(&mut rt, arr, "2");
    assert_eq!(rt.value_to_string_lossy(attr_class), "a b");
    let outer_html = get_data_prop(&mut rt, arr, "3");
    assert_eq!(
      rt.value_to_string_lossy(outer_html),
      r#"<div id="root" class="a b"></div>"#
    );
  }

  #[test]
  fn element_remove_attribute_clears_id_and_serialization() {
    let mut rt = JsWptRuntime::new("https://example.com/");

    let Value::Object(arr) = rt
      .exec_script(
        r#"
        const el = document.createElement("div");
        el.id = "root";
        el.removeAttribute("id");
        [el.id, el.getAttribute("id"), el.outerHTML];
      "#,
      )
      .expect("exec script")
    else {
      panic!("expected array return value");
    };

    let el_id = get_data_prop(&mut rt, arr, "0");
    assert_eq!(rt.value_to_string_lossy(el_id), "");
    assert_eq!(get_data_prop(&mut rt, arr, "1"), Value::Null);
    let outer_html = get_data_prop(&mut rt, arr, "2");
    assert_eq!(rt.value_to_string_lossy(outer_html), "<div></div>");
  }

  #[test]
  fn inner_html_populates_child_nodes_including_text() {
    let mut rt = JsWptRuntime::new("https://example.com/");

    let Value::Object(arr) = rt
      .exec_script(
        r#"
        const el = document.createElement("div");
        el.innerHTML = '<span id="x" class="y">hi</span>';
        const span = el.childNodes[0];
        const text = span.childNodes[0];
        [
          span.childNodes.length,
          text instanceof Text,
          text.data,
          text.nodeType,
          span.firstChild === text,
          span.lastChild === text,
          text.previousSibling,
          text.nextSibling,
          el.innerHTML,
          el.outerHTML,
        ];
      "#,
      )
      .expect("exec script")
    else {
      panic!("expected array return value");
    };

    assert_eq!(get_data_prop(&mut rt, arr, "0"), Value::Number(1.0));
    assert_eq!(get_data_prop(&mut rt, arr, "1"), Value::Bool(true));
    let data = get_data_prop(&mut rt, arr, "2");
    assert_eq!(rt.value_to_string_lossy(data), "hi");
    assert_eq!(get_data_prop(&mut rt, arr, "3"), Value::Number(3.0));
    assert_eq!(get_data_prop(&mut rt, arr, "4"), Value::Bool(true));
    assert_eq!(get_data_prop(&mut rt, arr, "5"), Value::Bool(true));
    assert_eq!(get_data_prop(&mut rt, arr, "6"), Value::Null);
    assert_eq!(get_data_prop(&mut rt, arr, "7"), Value::Null);
    let inner_html = get_data_prop(&mut rt, arr, "8");
    let outer_html = get_data_prop(&mut rt, arr, "9");
    assert_eq!(
      rt.value_to_string_lossy(inner_html),
      r#"<span id="x" class="y">hi</span>"#
    );
    assert_eq!(
      rt.value_to_string_lossy(outer_html),
      r#"<div><span id="x" class="y">hi</span></div>"#
    );
  }

  #[test]
  fn url_search_params_is_live_and_updates_search_and_href() {
    let mut rt = JsWptRuntime::new("https://example.com/");

    let Value::Object(arr) = rt
      .exec_script(
        r#"
        const url = new URL("https://example.com/?a=b%20~");
        const params = url.searchParams;
        const before = url.search;
        const a = params.get("a");
        const normalized = params.toString();
        const still = url.search;
        params.append("c", "d");
        [before, a, normalized, still, url.search, url.href];
      "#,
      )
      .expect("exec script")
    else {
      panic!("expected array return value");
    };

    let v0 = get_data_prop(&mut rt, arr, "0");
    let v1 = get_data_prop(&mut rt, arr, "1");
    let v2 = get_data_prop(&mut rt, arr, "2");
    let v3 = get_data_prop(&mut rt, arr, "3");
    let v4 = get_data_prop(&mut rt, arr, "4");
    let v5 = get_data_prop(&mut rt, arr, "5");

    assert_eq!(rt.value_to_string_lossy(v0), "?a=b%20~");
    assert_eq!(rt.value_to_string_lossy(v1), "b ~");
    assert_eq!(rt.value_to_string_lossy(v2), "a=b+%7E");
    assert_eq!(rt.value_to_string_lossy(v3), "?a=b%20~");
    assert_eq!(rt.value_to_string_lossy(v4), "?a=b+%7E&c=d");
    assert_eq!(rt.value_to_string_lossy(v5), "https://example.com/?a=b+%7E&c=d");
  }

  #[test]
  fn setting_url_search_updates_search_params_view() {
    let mut rt = JsWptRuntime::new("https://example.com/");

    let Value::Object(arr) = rt
      .exec_script(
        r#"
        const url = new URL("https://example.com/");
        const params = url.searchParams;
        url.search = "?q=a+b";
        [url.search, params.get("q"), params.toString()];
      "#,
      )
      .expect("exec script")
    else {
      panic!("expected array return value");
    };

    let v0 = get_data_prop(&mut rt, arr, "0");
    let v1 = get_data_prop(&mut rt, arr, "1");
    let v2 = get_data_prop(&mut rt, arr, "2");
    assert_eq!(rt.value_to_string_lossy(v0), "?q=a+b");
    assert_eq!(rt.value_to_string_lossy(v1), "a b");
    assert_eq!(rt.value_to_string_lossy(v2), "q=a+b");
  }

  #[test]
  fn fetch_resolves_relative_urls_and_supports_async_await() {
    let mut rt = JsWptRuntime::new("https://web-platform.test/smoke/fetch_relative.window.js");

    let Value::Object(promise) = rt
      .exec_script(
        r#"
        const run = async () => {
          const resp = await fetch("/x");
          const rel = await fetch("foo");
          const req = new Request("/y");
          const resp2 = await fetch(req);
          return [resp.url, rel.url, resp2.url];
        };
        run();
      "#,
      )
      .expect("exec script")
    else {
      panic!("expected Promise return value");
    };

    let (status, value) = {
      let state = rt.promises.get(&promise).expect("promise state");
      (state.status, state.value)
    };
    assert_eq!(status, PromiseStatus::Fulfilled);
    let Value::Object(arr) = value else {
      panic!("expected fulfilled value to be an array object");
    };

    let v0 = get_data_prop(&mut rt, arr, "0");
    let v1 = get_data_prop(&mut rt, arr, "1");
    let v2 = get_data_prop(&mut rt, arr, "2");
    assert_eq!(rt.value_to_string_lossy(v0), "https://web-platform.test/x");
    assert_eq!(rt.value_to_string_lossy(v1), "https://web-platform.test/smoke/foo");
    assert_eq!(rt.value_to_string_lossy(v2), "https://web-platform.test/y");
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

    assert_eq!(
      rt.value_to_string_lossy(Value::Number(f64::INFINITY)),
      "Infinity"
    );
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
