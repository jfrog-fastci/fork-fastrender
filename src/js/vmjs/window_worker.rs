//! Dedicated worker (`Worker`) bindings for `vm-js` window realms.
//!
//! This is a minimal, cooperative (single-threaded) implementation intended to unblock scripts
//! that expect:
//! - `new Worker(url)`
//! - `worker.postMessage(...)` / `self.postMessage(...)`
//! - `worker.terminate()` / `close()`
//! - `worker.onmessage` / `worker.onerror`
//! - `onmessage` / `onerror` inside the worker global scope
//!
//! ## Scheduling model
//!
//! `vm-js` is not treated as `Send`/thread-safe in FastRender, so workers are executed
//! cooperatively:
//! - `new Worker(...)` does **not** execute the worker script synchronously.
//! - Messages are delivered by queueing tasks onto the owning window's [`crate::js::EventLoop`].
//! - Each worker "turn" processes at most one inbound message and then yields back to the event
//!   loop, preventing worker work from starving window tasks.
//!
//! ## Structured clone
//!
//! Workers exchange messages using a small structured-clone implementation that supports the value
//! shapes commonly used for messaging:
//! - primitives (`undefined`, `null`, booleans, numbers, strings)
//! - plain arrays/objects
//! - `ArrayBuffer` / `Uint8Array` (by copy; transfer list semantics are currently ignored)
//!
//! This is **not** a full spec implementation; unsupported values throw a `TypeError`.
use crate::error::{Error, Result as FastResult};
use crate::js::url_resolve::resolve_url;
use crate::js::vm_limits;
use crate::js::window_realm::{WindowRealmHost, WindowRealmUserData, EVENT_TARGET_HOST_TAG};
use crate::js::window_indexed_db::INDEXED_DB_SHIM_JS;
use crate::js::window_timers::{event_loop_mut_from_hooks, VmJsEventLoopHooks};
use crate::js::{EventLoop, TaskSource};
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use vm_js::{
  GcObject, Heap, HostSlots, JsRuntime as VmJsRuntime, PropertyDescriptor, PropertyKey, PropertyKind, Realm,
  RootId, Scope, SourceText, Value, Vm, VmError, VmHost, VmHostHooks,
};

const WORKER_MAX_QUEUED_MESSAGES: usize = 1_000;
const WORKER_MAX_QUEUED_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone)]
struct WorkerQueueError(String);

impl std::fmt::Display for WorkerQueueError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(&self.0)
  }
}

#[derive(Debug, Clone)]
enum StructuredCloneData {
  Undefined,
  Null,
  Bool(bool),
  Number(f64),
  String(String),
  Array(Vec<StructuredCloneData>),
  Object(Vec<(String, StructuredCloneData)>),
  ArrayBuffer(Vec<u8>),
  Uint8Array(Vec<u8>),
}

impl StructuredCloneData {
  fn estimated_size_bytes(&self) -> usize {
    match self {
      StructuredCloneData::Undefined | StructuredCloneData::Null => 0,
      StructuredCloneData::Bool(_) => 1,
      StructuredCloneData::Number(_) => 8,
      StructuredCloneData::String(s) => s.len(),
      StructuredCloneData::Array(items) => items.iter().map(Self::estimated_size_bytes).sum(),
      StructuredCloneData::Object(entries) => entries
        .iter()
        .map(|(k, v)| k.len() + v.estimated_size_bytes())
        .sum(),
      StructuredCloneData::ArrayBuffer(bytes) | StructuredCloneData::Uint8Array(bytes) => bytes.len(),
    }
  }
}

#[derive(Debug, Clone)]
struct QueuedMessage {
  data: StructuredCloneData,
  bytes: usize,
}

impl QueuedMessage {
  fn new(data: StructuredCloneData) -> Self {
    let bytes = data.estimated_size_bytes();
    Self { data, bytes }
  }
}

struct WorkerVmUserData {
  inner: Weak<RefCell<WorkerInner>>,
}

struct WorkerInner {
  id: u64,
  script_url: String,
  started: bool,
  terminated: bool,
  scheduled_worker_turn: bool,
  scheduled_window_delivery: bool,
  inbound: VecDeque<QueuedMessage>,
  outbound: VecDeque<QueuedMessage>,
  queued_bytes: usize,
  /// The owning window's JS `Worker` wrapper object.
  window_worker_obj: Option<GcObject>,
  /// Persistent root keeping `window_worker_obj` alive while the worker is active.
  window_worker_root: Option<RootId>,
  runtime: Option<Box<VmJsRuntime>>,
}

impl WorkerInner {
  fn new(id: u64, script_url: String) -> Self {
    Self {
      id,
      script_url,
      started: false,
      terminated: false,
      scheduled_worker_turn: false,
      scheduled_window_delivery: false,
      inbound: VecDeque::new(),
      outbound: VecDeque::new(),
      queued_bytes: 0,
      window_worker_obj: None,
      window_worker_root: None,
      runtime: None,
    }
  }

  fn can_queue_message(&self) -> std::result::Result<(), WorkerQueueError> {
    if self.inbound.len() + self.outbound.len() >= WORKER_MAX_QUEUED_MESSAGES {
      return Err(WorkerQueueError(format!(
        "Worker exceeded max queued messages (limit={WORKER_MAX_QUEUED_MESSAGES})"
      )));
    }
    Ok(())
  }

  fn try_push_inbound(&mut self, msg: QueuedMessage) -> std::result::Result<(), WorkerQueueError> {
    self.can_queue_message()?;
    let next_bytes = self.queued_bytes.saturating_add(msg.bytes);
    if next_bytes > WORKER_MAX_QUEUED_BYTES {
      return Err(WorkerQueueError(format!(
        "Worker exceeded max queued message bytes (next={next_bytes}, limit={WORKER_MAX_QUEUED_BYTES})"
      )));
    }
    self.queued_bytes = next_bytes;
    self.inbound.push_back(msg);
    Ok(())
  }

  fn try_push_outbound(&mut self, msg: QueuedMessage) -> std::result::Result<(), WorkerQueueError> {
    self.can_queue_message()?;
    let next_bytes = self.queued_bytes.saturating_add(msg.bytes);
    if next_bytes > WORKER_MAX_QUEUED_BYTES {
      return Err(WorkerQueueError(format!(
        "Worker exceeded max queued message bytes (next={next_bytes}, limit={WORKER_MAX_QUEUED_BYTES})"
      )));
    }
    self.queued_bytes = next_bytes;
    self.outbound.push_back(msg);
    Ok(())
  }

  fn pop_inbound(&mut self) -> Option<QueuedMessage> {
    let msg = self.inbound.pop_front()?;
    self.queued_bytes = self.queued_bytes.saturating_sub(msg.bytes);
    Some(msg)
  }

  fn pop_outbound(&mut self) -> Option<QueuedMessage> {
    let msg = self.outbound.pop_front()?;
    self.queued_bytes = self.queued_bytes.saturating_sub(msg.bytes);
    Some(msg)
  }

  fn terminate_with_window_heap(&mut self, heap: &mut Heap) {
    self.terminated = true;
    self.inbound.clear();
    self.outbound.clear();
    self.queued_bytes = 0;
    self.runtime.take();
    if let Some(root) = self.window_worker_root.take() {
      heap.remove_root(root);
    }
    self.window_worker_obj = None;
  }
}

#[derive(Default)]
pub(crate) struct WorkerRegistry {
  next_id: u64,
  workers: HashMap<u64, Rc<RefCell<WorkerInner>>>,
}

impl WorkerRegistry {
  pub(crate) fn len(&self) -> usize {
    self.workers.len()
  }

  fn alloc_id(&mut self) -> u64 {
    loop {
      self.next_id = self.next_id.wrapping_add(1);
      if self.next_id == 0 {
        continue;
      }
      if !self.workers.contains_key(&self.next_id) {
        return self.next_id;
      }
    }
  }

  fn insert(&mut self, worker: Rc<RefCell<WorkerInner>>) {
    let id = worker.borrow().id;
    self.workers.insert(id, worker);
  }

  fn get(&self, id: u64) -> Option<Rc<RefCell<WorkerInner>>> {
    self.workers.get(&id).cloned()
  }

  pub(crate) fn terminate_all(&mut self, heap: &mut Heap) {
    for worker in self.workers.values() {
      worker.borrow_mut().terminate_with_window_heap(heap);
    }
    self.workers.clear();
  }
}

fn data_desc(value: Value) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: false,
    configurable: true,
    kind: PropertyKind::Data {
      value,
      writable: true,
    },
  }
}

fn ctor_link_desc(value: Value) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: false,
    configurable: false,
    kind: PropertyKind::Data {
      value,
      writable: false,
    },
  }
}

fn alloc_key(scope: &mut Scope<'_>, name: &str) -> std::result::Result<PropertyKey, VmError> {
  let s = scope.alloc_string(name)?;
  scope.push_root(Value::String(s))?;
  Ok(PropertyKey::from_string(s))
}

fn throw_type_error(vm: &Vm, scope: &mut Scope<'_>, message: &str) -> VmError {
  if let Some(intr) = vm.intrinsics() {
    vm_js::throw_type_error(scope, intr, message)
  } else {
    // Realm initialization failed; fall back to a static message.
    VmError::TypeError("TypeError")
  }
}

fn vm_error_to_host_error(err: VmError) -> Error {
  Error::Other(err.to_string())
}

fn require_worker_id_from_this(
  scope: &mut Scope<'_>,
  this: Value,
) -> std::result::Result<u64, VmError> {
  let Value::Object(obj) = this else {
    return Err(VmError::TypeError("Illegal invocation"));
  };
  let slots = scope.heap().object_host_slots(obj)?;
  let Some(slots) = slots else {
    return Err(VmError::TypeError("Illegal invocation"));
  };
  if slots.a != EVENT_TARGET_HOST_TAG {
    return Err(VmError::TypeError("Illegal invocation"));
  }
  if slots.b == 0 {
    return Err(VmError::TypeError("Illegal invocation"));
  }
  Ok(slots.b)
}

fn structured_clone_from_value(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  value: Value,
  depth: u32,
) -> std::result::Result<StructuredCloneData, VmError> {
  if depth > 256 {
    return Err(VmError::TypeError(
      "structuredClone: object graph too deep",
    ));
  }

  Ok(match value {
    Value::Undefined => StructuredCloneData::Undefined,
    Value::Null => StructuredCloneData::Null,
    Value::Bool(b) => StructuredCloneData::Bool(b),
    Value::Number(n) => StructuredCloneData::Number(n),
    Value::String(s) => StructuredCloneData::String(scope.heap().get_string(s)?.to_utf8_lossy()),
    Value::BigInt(_) => {
      return Err(VmError::TypeError(
        "structuredClone: BigInt is not supported",
      ));
    }
    Value::Symbol(_) => {
      return Err(VmError::TypeError(
        "structuredClone: Symbol is not supported",
      ));
    }
    Value::Object(obj) => {
      // Typed array / ArrayBuffer support used by common messaging patterns.
      if scope.heap().is_array_buffer_object(obj) {
        let bytes = scope.heap().array_buffer_data(obj)?.to_vec();
        StructuredCloneData::ArrayBuffer(bytes)
      } else if scope.heap().is_uint8_array_object(obj) {
        let bytes = scope.heap().uint8_array_data(obj)?.to_vec();
        StructuredCloneData::Uint8Array(bytes)
      } else if scope.heap().object_is_array(obj)? {
        // Clone `Array` by cloning indices `[0..length)`.
        let mut inner_scope = scope.reborrow();
        inner_scope.push_root(Value::Object(obj))?;
        let length_key = alloc_key(&mut inner_scope, "length")?;
        let len_val = vm.get(&mut inner_scope, obj, length_key)?;
        let len = match len_val {
          Value::Number(n) if n.is_finite() && n >= 0.0 => n as usize,
          _ => 0,
        };
        let mut out: Vec<StructuredCloneData> = Vec::new();
        out.try_reserve(len).map_err(|_| VmError::OutOfMemory)?;
        for idx in 0..len {
          // Root the array object while allocating the index key.
          let mut idx_scope = inner_scope.reborrow();
          idx_scope.push_root(Value::Object(obj))?;
          let key = alloc_key(&mut idx_scope, &idx.to_string())?;
          let item = vm.get(&mut idx_scope, obj, key)?;
          out.push(structured_clone_from_value(vm, &mut idx_scope, item, depth + 1)?);
        }
        StructuredCloneData::Array(out)
      } else {
        // Plain object: clone enumerable string-keyed own properties.
        // Root `obj` across property enumeration and value reads: `vm.get` may allocate and trigger
        // GC, and `obj` must remain alive for the duration of cloning.
        let mut obj_scope = scope.reborrow();
        obj_scope.push_root(Value::Object(obj))?;
        let keys = obj_scope.ordinary_own_property_keys(obj)?;
        let mut entries: Vec<(String, StructuredCloneData)> = Vec::new();
        for key in keys {
          let PropertyKey::String(key_s) = key else {
            continue;
          };
          let key = PropertyKey::String(key_s);
          let Some(desc) = obj_scope.heap().object_get_own_property(obj, &key)? else {
            continue;
          };
          if !desc.enumerable {
            continue;
          }
          let name = obj_scope.heap().get_string(key_s)?.to_utf8_lossy();
          let value = vm.get(&mut obj_scope, obj, key)?;
          // Reject callables (common source of DataCloneError).
          if obj_scope.heap().is_callable(value).unwrap_or(false) {
            return Err(VmError::TypeError(
              "structuredClone: function values are not supported",
            ));
          }
          entries.push((
            name,
            structured_clone_from_value(vm, &mut obj_scope, value, depth + 1)?,
          ));
        }
        StructuredCloneData::Object(entries)
      }
    }
  })
}

fn structured_clone_into_realm(
  scope: &mut Scope<'_>,
  realm: &Realm,
  data: &StructuredCloneData,
) -> std::result::Result<Value, VmError> {
  match data {
    StructuredCloneData::Undefined => Ok(Value::Undefined),
    StructuredCloneData::Null => Ok(Value::Null),
    StructuredCloneData::Bool(b) => Ok(Value::Bool(*b)),
    StructuredCloneData::Number(n) => Ok(Value::Number(*n)),
    StructuredCloneData::String(s) => Ok(Value::String(scope.alloc_string(s)?)),
    StructuredCloneData::Array(items) => {
      let intr = realm.intrinsics();
      let arr = scope.alloc_array(items.len())?;
      scope.push_root(Value::Object(arr))?;
      scope
        .heap_mut()
        .object_set_prototype(arr, Some(intr.array_prototype()))?;
      for (idx, item) in items.iter().enumerate() {
        let mut item_scope = scope.reborrow();
        item_scope.push_root(Value::Object(arr))?;
        let key = alloc_key(&mut item_scope, &idx.to_string())?;
        let value = structured_clone_into_realm(&mut item_scope, realm, item)?;
        item_scope.push_root(value)?;
        item_scope.define_property(arr, key, PropertyDescriptor {
          enumerable: true,
          configurable: true,
          kind: PropertyKind::Data { value, writable: true },
        })?;
      }
      Ok(Value::Object(arr))
    }
    StructuredCloneData::Object(entries) => {
      let intr = realm.intrinsics();
      let obj = scope.alloc_object_with_prototype(Some(intr.object_prototype()))?;
      scope.push_root(Value::Object(obj))?;
      for (key_s, value_data) in entries.iter() {
        let mut entry_scope = scope.reborrow();
        entry_scope.push_root(Value::Object(obj))?;
        let key = alloc_key(&mut entry_scope, key_s)?;
        let value = structured_clone_into_realm(&mut entry_scope, realm, value_data)?;
        entry_scope.push_root(value)?;
        entry_scope.define_property(obj, key, PropertyDescriptor {
          enumerable: true,
          configurable: true,
          kind: PropertyKind::Data { value, writable: true },
        })?;
      }
      Ok(Value::Object(obj))
    }
    StructuredCloneData::ArrayBuffer(bytes) => {
      let intr = realm.intrinsics();
      let ab = scope.alloc_array_buffer_from_u8_vec(bytes.clone())?;
      scope.push_root(Value::Object(ab))?;
      scope
        .heap_mut()
        .object_set_prototype(ab, Some(intr.array_buffer_prototype()))?;
      Ok(Value::Object(ab))
    }
    StructuredCloneData::Uint8Array(bytes) => {
      let intr = realm.intrinsics();
      let ab = scope.alloc_array_buffer_from_u8_vec(bytes.clone())?;
      scope.push_root(Value::Object(ab))?;
      scope
        .heap_mut()
        .object_set_prototype(ab, Some(intr.array_buffer_prototype()))?;
      let view = scope.alloc_uint8_array(ab, 0, bytes.len())?;
      scope
        .heap_mut()
        .object_set_prototype(view, Some(intr.uint8_array_prototype()))?;
      Ok(Value::Object(view))
    }
  }
}

fn create_message_event(scope: &mut Scope<'_>, data: Value) -> std::result::Result<Value, VmError> {
  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;
  scope.push_root(data)?;

  let data_key = alloc_key(scope, "data")?;
  scope.define_property(obj, data_key, PropertyDescriptor {
    enumerable: true,
    configurable: true,
    kind: PropertyKind::Data {
      value: data,
      writable: true,
    },
  })?;

  Ok(Value::Object(obj))
}

fn create_error_event(scope: &mut Scope<'_>, message: &str) -> std::result::Result<Value, VmError> {
  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;

  let msg_s = scope.alloc_string(message)?;
  scope.push_root(Value::String(msg_s))?;

  let message_key = alloc_key(scope, "message")?;
  scope.define_property(obj, message_key, PropertyDescriptor {
    enumerable: true,
    configurable: true,
    kind: PropertyKind::Data {
      value: Value::String(msg_s),
      writable: true,
    },
  })?;

  Ok(Value::Object(obj))
}

fn worker_registry_from_vm_mut(vm: &mut Vm) -> std::result::Result<&mut WorkerRegistry, VmError> {
  let data = vm
    .user_data_mut::<WindowRealmUserData>()
    .ok_or(VmError::InvariantViolation("window realm missing user data"))?;
  Ok(&mut data.worker_registry)
}

fn schedule_worker_turn(
  worker: Rc<RefCell<WorkerInner>>,
  event_loop: &mut EventLoop<crate::js::window::WindowHostState>,
) -> FastResult<()> {
  {
    let mut inner = worker.borrow_mut();
    if inner.terminated || inner.scheduled_worker_turn {
      return Ok(());
    }
    inner.scheduled_worker_turn = true;
  }
  let worker_for_task = Rc::clone(&worker);
  event_loop.queue_task(TaskSource::Script, move |host, event_loop| {
    run_worker_turn(host, event_loop, Rc::clone(&worker_for_task))
  })
}

fn schedule_window_delivery(
  worker: Rc<RefCell<WorkerInner>>,
  event_loop: &mut EventLoop<crate::js::window::WindowHostState>,
) -> FastResult<()> {
  {
    let mut inner = worker.borrow_mut();
    if inner.terminated || inner.scheduled_window_delivery || inner.outbound.is_empty() {
      return Ok(());
    }
    inner.scheduled_window_delivery = true;
  }
  let worker_for_task = Rc::clone(&worker);
  event_loop.queue_task(TaskSource::Script, move |host, event_loop| {
    run_window_delivery(host, event_loop, Rc::clone(&worker_for_task))
  })
}

fn dispatch_worker_error(
  worker: Rc<RefCell<WorkerInner>>,
  event_loop: &mut EventLoop<crate::js::window::WindowHostState>,
  message: String,
) -> FastResult<()> {
  let worker_for_task = Rc::clone(&worker);
  event_loop.queue_task(TaskSource::Script, move |host, event_loop| {
    run_window_error_dispatch(host, event_loop, Rc::clone(&worker_for_task), message)
  })
}

fn ensure_worker_runtime_started(
  host: &mut crate::js::window::WindowHostState,
  event_loop: &mut EventLoop<crate::js::window::WindowHostState>,
  worker: &Rc<RefCell<WorkerInner>>,
) -> FastResult<()> {
  // Fast path: already started (or terminated).
  let script_url = {
    let inner = worker.borrow();
    if inner.terminated || inner.started {
      return Ok(());
    }
    inner.script_url.clone()
  };
  let fetched = match host.fetcher().fetch(&script_url) {
    Ok(res) => res,
    Err(err) => {
      dispatch_worker_error(Rc::clone(worker), event_loop, err.to_string())?;
      // If the script can't be fetched, terminate the worker.
      let mut inner = worker.borrow_mut();
      let window = host.window_mut();
      inner.terminate_with_window_heap(window.heap_mut());
      return Ok(());
    }
  };
  let source = String::from_utf8_lossy(&fetched.bytes).to_string();

  // Create an isolated runtime for the worker.
  let mut vm_options = vm_limits::vm_options_from_js_options(&host.js_execution_options(), None);
  // Keep worker VMs cancellable via the global render interrupt flag.
  vm_options.external_interrupt_flag = Some(crate::render_control::interrupt_flag());
  let vm = Vm::new(vm_options);
  let heap = Heap::new(vm_limits::heap_limits_from_js_options(&host.js_execution_options()));
  let mut runtime = Box::new(
    VmJsRuntime::new(vm, heap).map_err(|err| Error::Other(err.to_string()))?,
  );

  let weak = Rc::downgrade(worker);
  runtime
    .vm
    .set_user_data(WorkerVmUserData { inner: weak });

  install_worker_global_scope(&mut runtime).map_err(|err| Error::Other(err.to_string()))?;

  // Evaluate the worker's classic script.
  {
    let budget = host.js_execution_options().vm_js_budget_now();
    runtime.vm.set_budget(budget);
    let source_text =
      match SourceText::new_charged_arc(&mut runtime.heap, script_url.clone(), source) {
        Ok(source_text) => source_text,
      Err(err) => {
        dispatch_worker_error(Rc::clone(worker), event_loop, err.to_string())?;
        let mut inner = worker.borrow_mut();
        let window = host.window_mut();
        inner.terminate_with_window_heap(window.heap_mut());
        return Ok(());
      }
      };
    let result = runtime.exec_script_source(source_text);
    if let Err(err) = result {
      dispatch_worker_error(Rc::clone(worker), event_loop, err.to_string())?;
      let mut inner = worker.borrow_mut();
      let window = host.window_mut();
      inner.terminate_with_window_heap(window.heap_mut());
      return Ok(());
    }

    if let Err(err) = runtime.vm.perform_microtask_checkpoint(&mut runtime.heap) {
      dispatch_worker_error(Rc::clone(worker), event_loop, err.to_string())?;
      let mut inner = worker.borrow_mut();
      let window = host.window_mut();
      inner.terminate_with_window_heap(window.heap_mut());
      return Ok(());
    }
  }

  // If the worker terminated itself during startup (e.g. `close()`), perform window-side cleanup
  // before registering the runtime.
  if worker.borrow().terminated {
    drop(runtime);
    let mut inner = worker.borrow_mut();
    let window = host.window_mut();
    inner.terminate_with_window_heap(window.heap_mut());
    return Ok(());
  }

  {
    let mut inner = worker.borrow_mut();
    inner.runtime = Some(runtime);
    inner.started = true;
  }

  // If the worker posted any messages during startup, schedule delivery.
  schedule_window_delivery(Rc::clone(worker), event_loop)?;
  Ok(())
}

fn run_worker_turn(
  host: &mut crate::js::window::WindowHostState,
  event_loop: &mut EventLoop<crate::js::window::WindowHostState>,
  worker: Rc<RefCell<WorkerInner>>,
) -> FastResult<()> {
  {
    // Clear the scheduled flag now that the task is running.
    let mut inner = worker.borrow_mut();
    inner.scheduled_worker_turn = false;
    if inner.terminated {
      return Ok(());
    }
  }

  // Ensure the worker script has been fetched + evaluated before delivering messages.
  ensure_worker_runtime_started(host, event_loop, &worker)?;

  // Deliver at most one inbound message per turn.
  let msg = { worker.borrow_mut().pop_inbound() };
  if let Some(msg) = msg {
    let mut had_error = None::<String>;
    let mut runtime = {
      let mut inner = worker.borrow_mut();
      if inner.terminated {
        return Ok(());
      }
      match inner.runtime.take() {
        Some(rt) => rt,
        None => return Ok(()),
      }
    };

    let budget = host.js_execution_options().vm_js_budget_now();
    runtime.vm.set_budget(budget);

    {
      let (vm, realm, heap) = runtime.vm_realm_and_heap_mut();
      let global = realm.global_object();
      {
        let mut scope = heap.scope();
        scope
          .push_root(Value::Object(global))
          .map_err(|err| Error::Other(err.to_string()))?;

        let data_value = structured_clone_into_realm(&mut scope, realm, &msg.data)
          .map_err(|err| Error::Other(err.to_string()))?;
        scope
          .push_root(data_value)
          .map_err(|err| Error::Other(err.to_string()))?;
        let ev = create_message_event(&mut scope, data_value)
          .map_err(|err| Error::Other(err.to_string()))?;
        scope.push_root(ev).map_err(|err| Error::Other(err.to_string()))?;

        let onmessage_key =
          alloc_key(&mut scope, "onmessage").map_err(|err| Error::Other(err.to_string()))?;
        let handler = vm
          .get(&mut scope, global, onmessage_key)
          .map_err(|err| Error::Other(err.to_string()))?;
        if scope.heap().is_callable(handler).unwrap_or(false) {
          if let Err(err) = vm.call_without_host(&mut scope, handler, Value::Object(global), &[ev]) {
            had_error = Some(err.to_string());
          }
        }
      }

      if had_error.is_none() {
        if let Err(err) = vm.perform_microtask_checkpoint(heap) {
          had_error = Some(err.to_string());
        }
      }
    }

    let terminated_after_turn = worker.borrow().terminated;
    if terminated_after_turn {
      let mut inner = worker.borrow_mut();
      let window = host.window_mut();
      inner.terminate_with_window_heap(window.heap_mut());
      return Ok(());
    }

    if let Some(err) = had_error.take() {
      dispatch_worker_error(Rc::clone(&worker), event_loop, err)?;
      let mut inner = worker.borrow_mut();
      let window = host.window_mut();
      inner.terminate_with_window_heap(window.heap_mut());
      return Ok(());
    }

    // Restore the runtime now that JS execution has finished (worker native calls may borrow the
    // worker state while running).
    {
      let mut inner = worker.borrow_mut();
      inner.runtime = Some(runtime);
    }
  }

  // If the worker produced outbound messages, schedule delivery.
  schedule_window_delivery(Rc::clone(&worker), event_loop)?;

  // If there are more inbound messages, schedule another turn.
  let (terminated, has_more_inbound) = {
    let inner = worker.borrow();
    (inner.terminated, !inner.inbound.is_empty())
  };
  if terminated {
    return Ok(());
  }
  if has_more_inbound {
    schedule_worker_turn(worker, event_loop)?;
  }
  Ok(())
}

fn run_window_delivery(
  host: &mut crate::js::window::WindowHostState,
  event_loop: &mut EventLoop<crate::js::window::WindowHostState>,
  worker: Rc<RefCell<WorkerInner>>,
) -> FastResult<()> {
  {
    let mut inner = worker.borrow_mut();
    inner.scheduled_window_delivery = false;
    if inner.terminated {
      return Ok(());
    }
  }

  let msg = { worker.borrow_mut().pop_outbound() };
  let Some(msg) = msg else {
    return Ok(());
  };

  // Dispatch to `worker.onmessage` in the owning window realm.
  let mut hooks = VmJsEventLoopHooks::<crate::js::window::WindowHostState>::new_with_host(host)?;
  hooks.set_event_loop(event_loop);
  let (vm_host, window_realm) = host.vm_host_and_window_realm()?;
  let (vm, realm, heap) = window_realm.vm_realm_and_heap_mut();

  {
    let mut scope = heap.scope();

    let worker_obj = match worker.borrow().window_worker_obj {
      Some(obj) => obj,
      None => return Ok(()),
    };

    // Root receiver while allocating key + cloning payload.
    scope
      .push_root(Value::Object(worker_obj))
      .map_err(vm_error_to_host_error)?;
    let data_value = structured_clone_into_realm(&mut scope, realm, &msg.data)
      .map_err(vm_error_to_host_error)?;
    scope
      .push_root(data_value)
      .map_err(vm_error_to_host_error)?;
    let ev = create_message_event(&mut scope, data_value).map_err(vm_error_to_host_error)?;
    scope.push_root(ev).map_err(vm_error_to_host_error)?;

    let onmessage_key = alloc_key(&mut scope, "onmessage").map_err(vm_error_to_host_error)?;
    let handler = vm
      .get_with_host_and_hooks(vm_host, &mut scope, &mut hooks, worker_obj, onmessage_key)
      .map_err(vm_error_to_host_error)?;
    if scope.heap().is_callable(handler).unwrap_or(false) {
      let _ = vm.call_with_host_and_hooks(
        vm_host,
        &mut scope,
        &mut hooks,
        handler,
        Value::Object(worker_obj),
        &[ev],
      );
    }
  }

  if let Some(err) = hooks.finish(heap) {
    return Err(err);
  }

  // If more outbound messages remain, schedule another delivery turn.
  let (terminated, has_more_outbound) = {
    let inner = worker.borrow();
    (inner.terminated, !inner.outbound.is_empty())
  };
  if !terminated && has_more_outbound {
    schedule_window_delivery(worker, event_loop)?;
  }
  Ok(())
}

fn run_window_error_dispatch(
  host: &mut crate::js::window::WindowHostState,
  event_loop: &mut EventLoop<crate::js::window::WindowHostState>,
  worker: Rc<RefCell<WorkerInner>>,
  message: String,
) -> FastResult<()> {
  let mut hooks = VmJsEventLoopHooks::<crate::js::window::WindowHostState>::new_with_host(host)?;
  hooks.set_event_loop(event_loop);
  let (vm_host, window_realm) = host.vm_host_and_window_realm()?;
  let (vm, _realm, heap) = window_realm.vm_realm_and_heap_mut();

  {
    let mut scope = heap.scope();

    let worker_obj = match worker.borrow().window_worker_obj {
      Some(obj) => obj,
      None => return Ok(()),
    };

    scope
      .push_root(Value::Object(worker_obj))
      .map_err(vm_error_to_host_error)?;
    let ev = create_error_event(&mut scope, &message).map_err(vm_error_to_host_error)?;
    scope.push_root(ev).map_err(vm_error_to_host_error)?;

    let onerror_key = alloc_key(&mut scope, "onerror").map_err(vm_error_to_host_error)?;
    let handler = vm
      .get_with_host_and_hooks(vm_host, &mut scope, &mut hooks, worker_obj, onerror_key)
      .map_err(vm_error_to_host_error)?;
    if scope.heap().is_callable(handler).unwrap_or(false) {
      let _ = vm.call_with_host_and_hooks(
        vm_host,
        &mut scope,
        &mut hooks,
        handler,
        Value::Object(worker_obj),
        &[ev],
      );
    }
  }

  if let Some(err) = hooks.finish(heap) {
    return Err(err);
  }
  Ok(())
}

fn worker_ctor_call(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> std::result::Result<Value, VmError> {
  Err(VmError::TypeError("Worker constructor cannot be invoked without 'new'"))
}

fn worker_ctor_construct(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  args: &[Value],
  new_target: Value,
) -> std::result::Result<Value, VmError> {
  // Require an active window event loop so worker turns can be scheduled.
  let Some(event_loop) = event_loop_mut_from_hooks::<crate::js::window::WindowHostState>(hooks) else {
    return Err(VmError::TypeError(
      "Worker constructor called without an active EventLoop",
    ));
  };

  // --- Resolve script URL ---
  let script_url_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let script_url_string = match script_url_value {
    Value::String(s) => scope.heap().get_string(s)?.to_utf8_lossy(),
    other => {
      let s = scope.heap_mut().to_string(other)?;
      scope.heap().get_string(s)?.to_utf8_lossy()
    }
  };

  let base_url = vm
    .user_data::<WindowRealmUserData>()
    .and_then(|data| data.base_url.as_deref().or(Some(data.document_url())))
    .map(|s| s.to_string());
  let resolved = resolve_url(&script_url_string, base_url.as_deref()).map_err(|err| {
    VmError::TypeError(match err {
      crate::js::UrlResolveError::RelativeUrlWithoutBase => "Worker script URL is relative without a base URL",
      crate::js::UrlResolveError::Url(_) => "Worker script URL is invalid",
    })
  })?;

  // --- Allocate worker wrapper object ---
  let ctor_obj = match new_target {
    Value::Object(obj) => obj,
    _ => callee,
  };

  let prototype_key = alloc_key(scope, "prototype")?;
  let proto = scope
    .heap()
    .object_get_own_data_property_value(ctor_obj, &prototype_key)?
    .and_then(|v| match v {
      Value::Object(obj) => Some(obj),
      _ => None,
    });

  let worker_obj = match proto {
    Some(proto_obj) => scope.alloc_object_with_prototype(Some(proto_obj))?,
    None => scope.alloc_object()?,
  };
  scope.push_root(Value::Object(worker_obj))?;

  // Allocate worker ID + register in the per-window registry.
  let worker_rc = {
    let registry = worker_registry_from_vm_mut(vm)?;
    let id = registry.alloc_id();
    let worker = Rc::new(RefCell::new(WorkerInner::new(id, resolved)));
    registry.insert(Rc::clone(&worker));
    worker
  };

  // Brand the JS wrapper via host slots.
  let id = worker_rc.borrow().id;
  scope
    .heap_mut()
    .object_set_host_slots(worker_obj, HostSlots {
      a: EVENT_TARGET_HOST_TAG,
      b: id,
    })?;

  // Initialize `onmessage`/`onerror` attributes.
  for name in ["onmessage", "onerror"] {
    let key = alloc_key(scope, name)?;
    scope.define_property(worker_obj, key, data_desc(Value::Null))?;
  }

  // Root the wrapper so tasks can call handlers even if user code drops references.
  let root = scope.heap_mut().add_root(Value::Object(worker_obj))?;
  {
    let mut inner = worker_rc.borrow_mut();
    inner.window_worker_obj = Some(worker_obj);
    inner.window_worker_root = Some(root);
  }

  // Schedule initial worker startup and message processing.
  schedule_worker_turn(worker_rc, event_loop)
    .map_err(|err| throw_type_error(vm, scope, &err.to_string()))?;

  Ok(Value::Object(worker_obj))
}

fn worker_post_message_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> std::result::Result<Value, VmError> {
  let id = require_worker_id_from_this(scope, this)?;
  let Some(worker_rc) = worker_registry_from_vm_mut(vm)?.get(id) else {
    return Err(VmError::TypeError("Illegal invocation"));
  };
  if worker_rc.borrow().terminated {
    return Ok(Value::Undefined);
  }

  let message = args.get(0).copied().unwrap_or(Value::Undefined);
  let data = structured_clone_from_value(vm, scope, message, 0)?;
  let msg = QueuedMessage::new(data);
  worker_rc
    .borrow_mut()
    .try_push_inbound(msg)
    .map_err(|err| throw_type_error(vm, scope, &err.0))?;

  let Some(event_loop) = event_loop_mut_from_hooks::<crate::js::window::WindowHostState>(hooks) else {
    return Err(VmError::TypeError(
      "Worker.postMessage called without an active EventLoop",
    ));
  };
  schedule_worker_turn(worker_rc, event_loop)
    .map_err(|err| throw_type_error(vm, scope, &err.to_string()))?;

  Ok(Value::Undefined)
}

fn worker_terminate_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> std::result::Result<Value, VmError> {
  let id = require_worker_id_from_this(scope, this)?;
  let Some(worker_rc) = worker_registry_from_vm_mut(vm)?.get(id) else {
    return Err(VmError::TypeError("Illegal invocation"));
  };
  if worker_rc.borrow().terminated {
    return Ok(Value::Undefined);
  }
  worker_rc
    .borrow_mut()
    .terminate_with_window_heap(scope.heap_mut());
  Ok(Value::Undefined)
}

fn worker_global_post_message_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> std::result::Result<Value, VmError> {
  let Some(data) = vm.user_data_mut::<WorkerVmUserData>() else {
    return Err(VmError::InvariantViolation(
      "worker postMessage missing WorkerVmUserData",
    ));
  };
  let Some(worker_rc) = data.inner.upgrade() else {
    return Ok(Value::Undefined);
  };
  if worker_rc.borrow().terminated {
    return Ok(Value::Undefined);
  }

  let message = args.get(0).copied().unwrap_or(Value::Undefined);
  let cloned = structured_clone_from_value(vm, scope, message, 0)?;
  let msg = QueuedMessage::new(cloned);
  worker_rc
    .borrow_mut()
    .try_push_outbound(msg)
    .map_err(|err| throw_type_error(vm, scope, &err.0))?;
  Ok(Value::Undefined)
}

fn worker_global_close_native(
  vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> std::result::Result<Value, VmError> {
  let Some(data) = vm.user_data_mut::<WorkerVmUserData>() else {
    return Err(VmError::InvariantViolation(
      "worker close missing WorkerVmUserData",
    ));
  };
  let Some(worker_rc) = data.inner.upgrade() else {
    return Ok(Value::Undefined);
  };
  worker_rc.borrow_mut().terminated = true;
  Ok(Value::Undefined)
}

fn install_worker_global_scope(runtime: &mut VmJsRuntime) -> std::result::Result<(), VmError> {
  let mut install_indexeddb = false;
  {
    let (vm, realm, heap) = runtime.vm_realm_and_heap_mut();
    let global = realm.global_object();
    let intr = realm.intrinsics();
    let func_proto = intr.function_prototype();

    let mut scope = heap.scope();
    scope.push_root(Value::Object(global))?;

    // self === globalThis
    {
      let self_key = alloc_key(&mut scope, "self")?;
      scope.define_property(global, self_key, data_desc(Value::Object(global)))?;
    }

    // structuredClone(value[, options])
    crate::js::window_structured_clone::install_window_structured_clone(vm, &mut scope, realm, global)?;

    // onmessage / onerror placeholders
    for name in ["onmessage", "onerror"] {
      let key = alloc_key(&mut scope, name)?;
      scope.define_property(global, key, data_desc(Value::Null))?;
    }

    // postMessage
    {
      let id = vm.register_native_call(worker_global_post_message_native)?;
      let name = scope.alloc_string("postMessage")?;
      scope.push_root(Value::String(name))?;
      let func = scope.alloc_native_function(id, None, name, 1)?;
      scope.heap_mut().object_set_prototype(func, Some(func_proto))?;
      scope.push_root(Value::Object(func))?;
      let key = alloc_key(&mut scope, "postMessage")?;
      scope.define_property(global, key, data_desc(Value::Object(func)))?;
    }

    // close
    {
      let id = vm.register_native_call(worker_global_close_native)?;
      let name = scope.alloc_string("close")?;
      scope.push_root(Value::String(name))?;
      let func = scope.alloc_native_function(id, None, name, 0)?;
      scope.heap_mut().object_set_prototype(func, Some(func_proto))?;
      scope.push_root(Value::Object(func))?;
      let key = alloc_key(&mut scope, "close")?;
      scope.define_property(global, key, data_desc(Value::Object(func)))?;
    }

    // indexedDB shim (idempotent: don't overwrite if installed by a future worker runtime).
    let indexeddb_key = alloc_key(&mut scope, "indexedDB")?;
    install_indexeddb = scope
      .heap()
      .object_get_own_property(global, &indexeddb_key)?
      .is_none();
  }

  if install_indexeddb {
    // Avoid `Arc::new`, which can abort the process on allocator OOM.
    let source = SourceText::new_charged_arc(
      &mut runtime.heap,
      "fastrender_indexeddb_shim.js",
      INDEXED_DB_SHIM_JS,
    )?;
    runtime.exec_script_source(source)?;
  }

  Ok(())
}

/// Install `Worker` onto the global object of a `vm-js` Window realm.
pub fn install_window_worker_bindings(
  vm: &mut Vm,
  realm: &Realm,
  heap: &mut Heap,
) -> std::result::Result<(), VmError> {
  let mut scope = heap.scope();
  let global = realm.global_object();
  scope.push_root(Value::Object(global))?;

  // Idempotency: do not clobber if a different bindings layer already installed Worker.
  let worker_key = alloc_key(&mut scope, "Worker")?;
  if scope.heap().object_get_own_property(global, &worker_key)?.is_some() {
    return Ok(());
  }

  // `Worker.prototype` should inherit from `EventTarget.prototype` when available.
  let event_target_proto = (|| {
    let event_target_key = alloc_key(&mut scope, "EventTarget").ok()?;
    let ctor = scope
      .heap()
      .object_get_own_data_property_value(global, &event_target_key)
      .ok()?
      .and_then(|v| match v {
        Value::Object(obj) => Some(obj),
        _ => None,
      })?;
    let proto_key = alloc_key(&mut scope, "prototype").ok()?;
    scope
      .heap()
      .object_get_own_data_property_value(ctor, &proto_key)
      .ok()?
      .and_then(|v| match v {
        Value::Object(obj) => Some(obj),
        _ => None,
      })
  })()
  .unwrap_or_else(|| realm.intrinsics().object_prototype());

  let worker_proto = scope.alloc_object_with_prototype(Some(event_target_proto))?;
  scope.push_root(Value::Object(worker_proto))?;

  let func_proto = realm.intrinsics().function_prototype();

  // Worker.prototype.postMessage
  {
    let call_id = vm.register_native_call(worker_post_message_native)?;
    let name = scope.alloc_string("postMessage")?;
    scope.push_root(Value::String(name))?;
    let func = scope.alloc_native_function(call_id, None, name, 1)?;
    scope.heap_mut().object_set_prototype(func, Some(func_proto))?;
    scope.push_root(Value::Object(func))?;
    let key = alloc_key(&mut scope, "postMessage")?;
    scope.define_property(worker_proto, key, data_desc(Value::Object(func)))?;
  }

  // Worker.prototype.terminate
  {
    let call_id = vm.register_native_call(worker_terminate_native)?;
    let name = scope.alloc_string("terminate")?;
    scope.push_root(Value::String(name))?;
    let func = scope.alloc_native_function(call_id, None, name, 0)?;
    scope.heap_mut().object_set_prototype(func, Some(func_proto))?;
    scope.push_root(Value::Object(func))?;
    let key = alloc_key(&mut scope, "terminate")?;
    scope.define_property(worker_proto, key, data_desc(Value::Object(func)))?;
  }

  // Worker constructor
  let ctor_call_id = vm.register_native_call(worker_ctor_call)?;
  let ctor_construct_id = vm.register_native_construct(worker_ctor_construct)?;
  let worker_name = scope.alloc_string("Worker")?;
  scope.push_root(Value::String(worker_name))?;
  let ctor = scope.alloc_native_function(ctor_call_id, Some(ctor_construct_id), worker_name, 1)?;
  scope.heap_mut().object_set_prototype(ctor, Some(func_proto))?;
  scope.push_root(Value::Object(ctor))?;

  // Link constructor <-> prototype.
  let prototype_key = alloc_key(&mut scope, "prototype")?;
  let constructor_key = alloc_key(&mut scope, "constructor")?;
  scope.define_property(
    ctor,
    prototype_key,
    ctor_link_desc(Value::Object(worker_proto)),
  )?;
  scope.define_property(
    worker_proto,
    constructor_key,
    ctor_link_desc(Value::Object(ctor)),
  )?;

  // Expose global Worker.
  scope.define_property(global, worker_key, data_desc(Value::Object(ctor)))?;
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::dom2;
  use crate::js::{RunLimits, WindowHost};
  use crate::resource::{FetchedResource, ResourceFetcher};
  use selectors::context::QuirksMode;
  use std::sync::Arc;
  use vm_js::{PropertyKey, Value};

  #[derive(Debug, Default)]
  struct NoFetchResourceFetcher;

  impl ResourceFetcher for NoFetchResourceFetcher {
    fn fetch(&self, url: &str) -> FastResult<FetchedResource> {
      Err(Error::Other(format!(
        "NoFetchResourceFetcher does not support fetch: {url}"
      )))
    }
  }

  fn make_host(dom: dom2::Document, document_url: impl Into<String>) -> FastResult<WindowHost> {
    WindowHost::new_with_fetcher(dom, document_url, Arc::new(NoFetchResourceFetcher))
  }

  fn get_global_number(host: &mut WindowHost, name: &str) -> Option<f64> {
    let window = host.host_mut().window_mut();
    let (_vm, realm, heap) = window.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    let global = realm.global_object();
    scope.push_root(Value::Object(global)).unwrap();
    let key_s = scope.alloc_string(name).unwrap();
    scope.push_root(Value::String(key_s)).unwrap();
    let key = PropertyKey::from_string(key_s);
    match scope
      .heap()
      .object_get_own_data_property_value(global, &key)
      .unwrap()
      .unwrap_or(Value::Undefined)
    {
      Value::Number(n) => Some(n),
      _ => None,
    }
  }

  fn get_global_string(host: &mut WindowHost, name: &str) -> Option<String> {
    let window = host.host_mut().window_mut();
    let (_vm, realm, heap) = window.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    let global = realm.global_object();
    scope.push_root(Value::Object(global)).unwrap();
    let key_s = scope.alloc_string(name).unwrap();
    scope.push_root(Value::String(key_s)).unwrap();
    let key = PropertyKey::from_string(key_s);
    match scope
      .heap()
      .object_get_own_data_property_value(global, &key)
      .unwrap()
      .unwrap_or(Value::Undefined)
    {
      Value::String(s) => Some(scope.heap().get_string(s).unwrap().to_utf8_lossy()),
      _ => None,
    }
  }

  fn get_global_len(host: &mut WindowHost, array_name: &str) -> Option<usize> {
    let window = host.host_mut().window_mut();
    let (_vm, realm, heap) = window.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    let global = realm.global_object();
    scope.push_root(Value::Object(global)).unwrap();
    let key_s = scope.alloc_string(array_name).unwrap();
    scope.push_root(Value::String(key_s)).unwrap();
    let key = PropertyKey::from_string(key_s);
    let arr = scope
      .heap()
      .object_get_own_data_property_value(global, &key)
      .unwrap()
      .unwrap_or(Value::Undefined);
    let Value::Object(arr_obj) = arr else {
      return None;
    };
    scope.push_root(Value::Object(arr_obj)).unwrap();
    let len_key_s = scope.alloc_string("length").unwrap();
    scope.push_root(Value::String(len_key_s)).unwrap();
    let len_key = PropertyKey::from_string(len_key_s);
    match scope.heap().get(arr_obj, &len_key).unwrap() {
      Value::Number(n) if n.is_finite() && n >= 0.0 => Some(n as usize),
      _ => None,
    }
  }

  #[test]
  fn worker_echoes_message() -> FastResult<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = make_host(dom, "https://example.invalid/")?;

    host.exec_script(
      r#"
      globalThis.__result = null;
      const w = new Worker('data:text/javascript,onmessage=e=>postMessage(e.data+1)');
      w.onmessage = e => { globalThis.__result = e.data; };
      w.postMessage(1);
      "#,
    )?;

    host.run_until_idle(RunLimits {
      max_tasks: 50,
      max_microtasks: 100,
      max_wall_time: None,
    })?;

    assert_eq!(get_global_number(&mut host, "__result"), Some(2.0));
    Ok(())
  }

  #[test]
  fn worker_terminate_prevents_further_messages() -> FastResult<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = make_host(dom, "https://example.invalid/")?;

    host.exec_script(
      r#"
      globalThis.__msgs = [];
      globalThis.__w = new Worker('data:text/javascript,onmessage=e=>postMessage(e.data+1)');
      __w.onmessage = e => { __msgs.push(e.data); };
      __w.postMessage(1);
      "#,
    )?;

    host.run_until_idle(RunLimits {
      max_tasks: 50,
      max_microtasks: 100,
      max_wall_time: None,
    })?;

    assert_eq!(get_global_len(&mut host, "__msgs"), Some(1));

    host.exec_script(
      r#"
      __w.terminate();
      __w.postMessage(2);
      "#,
    )?;

    host.run_until_idle(RunLimits {
      max_tasks: 50,
      max_microtasks: 100,
      max_wall_time: None,
    })?;

    assert_eq!(get_global_len(&mut host, "__msgs"), Some(1));
    Ok(())
  }

  #[test]
  fn worker_can_access_structured_clone() -> FastResult<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = make_host(dom, "https://example.invalid/")?;

    host.exec_script(
      r#"
      globalThis.__result = null;
      const w = new Worker('data:text/javascript,postMessage(typeof structuredClone)');
      w.onmessage = e => { globalThis.__result = e.data; };
      "#,
    )?;

    host.run_until_idle(RunLimits {
      max_tasks: 50,
      max_microtasks: 100,
      max_wall_time: None,
    })?;

    assert_eq!(
      get_global_string(&mut host, "__result").as_deref(),
      Some("function")
    );
    Ok(())
  }

  #[test]
  fn worker_structured_clone_deep_clones_object() -> FastResult<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = make_host(dom, "https://example.invalid/")?;

    host.exec_script(
      r#"
      globalThis.__result = null;
      const w = new Worker('data:text/javascript,postMessage(structuredClone({a:1}).a)');
      w.onmessage = e => { globalThis.__result = e.data; };
      "#,
    )?;

    host.run_until_idle(RunLimits {
      max_tasks: 50,
      max_microtasks: 100,
      max_wall_time: None,
    })?;

    assert_eq!(get_global_number(&mut host, "__result"), Some(1.0));
    Ok(())
  }

  #[test]
  fn worker_is_event_target() -> FastResult<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = make_host(dom, "https://example.invalid/")?;

    host.exec_script(
      r#"
      globalThis.__count = 0;
      const w = new Worker('data:text/javascript,0');
      w.addEventListener('x', () => { __count++; });
      w.dispatchEvent(new Event('x'));
      "#,
    )?;

    host.run_until_idle(RunLimits {
      max_tasks: 50,
      max_microtasks: 100,
      max_wall_time: None,
    })?;

    assert_eq!(get_global_number(&mut host, "__count"), Some(1.0));
    Ok(())
  }

  #[test]
  fn worker_can_access_indexeddb_shim() -> FastResult<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = make_host(dom, "https://example.invalid/")?;

    host.exec_script(
      r#"
      globalThis.__result = null;
      const w = new Worker("data:text/javascript,(()=>{if(typeof indexedDB!=='object'||!indexedDB)throw Error('noindexeddb');if(typeof IDBKeyRange!=='function')throw Error('noidbkeyrange');if(typeof webkitIndexedDB!=='object'||webkitIndexedDB!==indexedDB)throw Error('novendor');const req=indexedDB.open('x');if(typeof req!=='object'||!req)throw Error('noreq');if(typeof req.addEventListener!=='function')throw Error('nolistener');req.onerror=()=>postMessage(req.error&&req.error.name||null);})()");
      w.onmessage = e => { globalThis.__result = e.data; };
      "#,
    )?;

    host.run_until_idle(RunLimits {
      max_tasks: 50,
      max_microtasks: 100,
      max_wall_time: None,
    })?;

    assert_eq!(
      get_global_string(&mut host, "__result").as_deref(),
      Some("NotSupportedError")
    );
    Ok(())
  }
}
