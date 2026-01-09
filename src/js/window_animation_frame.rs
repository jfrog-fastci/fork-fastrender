//! `requestAnimationFrame` / `cancelAnimationFrame` bindings for a `Window`-like `vm-js` realm.
//!
//! These are backed by FastRender's [`EventLoop`] animation-frame queue.
//!
//! ## Safety / determinism
//! Like timers, string handlers are rejected to avoid string-eval and keep behavior deterministic.

use crate::js::event_loop::AnimationFrameId;
use crate::js::runtime::{current_event_loop_mut, with_event_loop};
use crate::js::window_realm::WindowRealmHost;
use crate::render_control;
use std::time::Instant;
use vm_js::{
  Budget, ExecutionContext, Heap, Job, JobCallback, PropertyDescriptor, PropertyKey, PropertyKind,
  RealmId, RootId, Scope, Value, Vm, VmError, VmHostHooks, VmJobContext,
};

type VmResult<T> = std::result::Result<T, VmError>;

pub(crate) const REQUEST_ANIMATION_FRAME_STRING_HANDLER_ERROR: &str =
  "requestAnimationFrame does not currently support string callbacks";
pub(crate) const REQUEST_ANIMATION_FRAME_NOT_CALLABLE_ERROR: &str =
  "requestAnimationFrame callback is not callable";

const RAF_REGISTRY_KEY: &str = "__fastrender_animation_frame_registry";

const DEFAULT_CALLBACK_FUEL: u64 = 1_000_000;
const DEFAULT_CHECK_TIME_EVERY: u32 = 100;

// Native slot index on rAF host functions that stores the owning global object.
const RAF_GLOBAL_SLOT: usize = 0;

fn callback_budget_from_render_deadline() -> Budget {
  // Prefer the root (outermost) render deadline so JS does not inherit internal per-stage budgets.
  let deadline = render_control::root_deadline().and_then(|d| d.remaining_timeout());
  let deadline = deadline.and_then(|remaining| Instant::now().checked_add(remaining));

  Budget {
    fuel: Some(DEFAULT_CALLBACK_FUEL),
    deadline,
    check_time_every: DEFAULT_CHECK_TIME_EVERY,
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

fn alloc_key(scope: &mut Scope<'_>, name: &str) -> VmResult<PropertyKey> {
  let s = scope.alloc_string(name)?;
  scope.push_root(Value::String(s))?;
  Ok(PropertyKey::from_string(s))
}

fn throw_type_error(message: &'static str) -> VmError {
  // Use `VmError::TypeError` so the evaluator can construct a real `TypeError` object in the
  // current realm (mirrors how internal VM operations report type errors).
  VmError::TypeError(message)
}

fn throw_error(scope: &mut Scope<'_>, message: &str) -> VmError {
  match scope.alloc_string(message) {
    Ok(s) => VmError::Throw(Value::String(s)),
    Err(_) => VmError::Throw(Value::Undefined),
  }
}

fn is_callable(scope: &Scope<'_>, value: Value) -> bool {
  scope.heap().is_callable(value).unwrap_or(false)
}

fn clear_registry_entry(
  scope: &mut Scope<'_>,
  registry: vm_js::GcObject,
  id: AnimationFrameId,
) -> VmResult<()> {
  let key = alloc_key(scope, &id.to_string())?;
  scope.define_property(registry, key, data_desc(Value::Undefined))?;
  Ok(())
}

fn get_raf_registry(
  scope: &mut Scope<'_>,
  global: vm_js::GcObject,
) -> VmResult<vm_js::GcObject> {
  let key_s = scope.alloc_string(RAF_REGISTRY_KEY)?;
  scope.push_root(Value::String(key_s))?;
  let key = PropertyKey::from_string(key_s);
  match scope
    .heap()
    .object_get_own_data_property_value(global, &key)?
  {
    Some(Value::Object(obj)) => Ok(obj),
    _ => Err(VmError::Unimplemented(
      "animation frame registry missing on global object",
    )),
  }
}

fn store_callback(
  scope: &mut Scope<'_>,
  registry: vm_js::GcObject,
  id: AnimationFrameId,
  callback: Value,
) -> VmResult<()> {
  let key = alloc_key(scope, &id.to_string())?;
  scope.define_property(registry, key, data_desc(callback))?;
  Ok(())
}

fn vm_error_to_event_loop_error(heap: &Heap, err: VmError) -> crate::error::Error {
  match err {
    VmError::Throw(value) => {
      if let Value::String(s) = value {
        if let Ok(js) = heap.get_string(s) {
          return crate::error::Error::Other(js.to_utf8_lossy());
        }
      }
      crate::error::Error::Other("uncaught exception".to_string())
    }
    other => crate::error::Error::Other(other.to_string()),
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
  ) -> Result<Value, VmError> {
    Err(VmError::Unimplemented("HeapRootContext::call"))
  }

  fn construct(
    &mut self,
    _host: &mut dyn VmHostHooks,
    _callee: Value,
    _args: &[Value],
    _new_target: Value,
  ) -> Result<Value, VmError> {
    Err(VmError::Unimplemented("HeapRootContext::construct"))
  }

  fn add_root(&mut self, value: Value) -> Result<RootId, VmError> {
    self.heap.add_root(value)
  }

  fn remove_root(&mut self, id: RootId) {
    self.heap.remove_root(id);
  }
}

struct WindowRealmJobContext<'a> {
  window_realm: &'a mut crate::js::window_realm::WindowRealm,
  realm: Option<RealmId>,
}

impl<'a> WindowRealmJobContext<'a> {
  fn new(window_realm: &'a mut crate::js::window_realm::WindowRealm, realm: Option<RealmId>) -> Self {
    Self { window_realm, realm }
  }
}

impl VmJobContext for WindowRealmJobContext<'_> {
  fn call(
    &mut self,
    host: &mut dyn VmHostHooks,
    callee: Value,
    this: Value,
    args: &[Value],
  ) -> Result<Value, VmError> {
    let (vm, heap) = self.window_realm.vm_and_heap_mut();
    let mut scope = heap.scope();
    if let Some(realm) = self.realm {
      let mut vm = vm.execution_context_guard(ExecutionContext {
        realm,
        script_or_module: None,
      });
      vm.call_with_host(&mut scope, host, callee, this, args)
    } else {
      vm.call_with_host(&mut scope, host, callee, this, args)
    }
  }

  fn construct(
    &mut self,
    host: &mut dyn VmHostHooks,
    callee: Value,
    args: &[Value],
    new_target: Value,
  ) -> Result<Value, VmError> {
    let (vm, heap) = self.window_realm.vm_and_heap_mut();
    let mut scope = heap.scope();
    if let Some(realm) = self.realm {
      let mut vm = vm.execution_context_guard(ExecutionContext {
        realm,
        script_or_module: None,
      });
      vm.construct_with_host(&mut scope, host, callee, args, new_target)
    } else {
      vm.construct_with_host(&mut scope, host, callee, args, new_target)
    }
  }

  fn add_root(&mut self, value: Value) -> Result<RootId, VmError> {
    self.window_realm.heap_mut().add_root(value)
  }

  fn remove_root(&mut self, id: RootId) {
    self.window_realm.heap_mut().remove_root(id);
  }
}

struct VmJsEventLoopHooks<Host: WindowRealmHost + 'static> {
  pending_discard: Vec<Job>,
  enqueue_error: Option<crate::error::Error>,
  _marker: std::marker::PhantomData<fn() -> Host>,
}

impl<Host: WindowRealmHost + 'static> VmJsEventLoopHooks<Host> {
  fn new() -> Self {
    Self {
      pending_discard: Vec::new(),
      enqueue_error: None,
      _marker: std::marker::PhantomData,
    }
  }

  fn finish(mut self, heap: &mut Heap) -> Option<crate::error::Error> {
    if !self.pending_discard.is_empty() {
      let mut ctx = HeapRootContext { heap };
      for job in self.pending_discard.drain(..) {
        job.discard(&mut ctx);
      }
    }
    self.enqueue_error.take()
  }
}

impl<Host: WindowRealmHost + 'static> VmHostHooks for VmJsEventLoopHooks<Host> {
  fn host_enqueue_promise_job(&mut self, job: Job, realm: Option<RealmId>) {
    if self.enqueue_error.is_some() {
      self.pending_discard.push(job);
      return;
    }

    let job_cell: std::rc::Rc<std::cell::RefCell<Option<Job>>> =
      std::rc::Rc::new(std::cell::RefCell::new(Some(job)));
    let job_cell_for_closure = std::rc::Rc::clone(&job_cell);

    let enqueue_result: crate::error::Result<()> = (|| {
      let Some(event_loop) = current_event_loop_mut::<Host>() else {
        return Err(crate::error::Error::Other(
          "vm-js Promise job enqueued without an active EventLoop".to_string(),
        ));
      };

      event_loop.queue_microtask(move |host, event_loop| {
        let Some(job) = job_cell_for_closure.borrow_mut().take() else {
          return Ok(());
        };

        let window_realm = host.window_realm();
        window_realm.reset_interrupt();

        with_event_loop(event_loop, || {
          let vm = window_realm.vm_mut();
          vm.set_budget(callback_budget_from_render_deadline());
          let tick_result = vm.tick();

          let mut hooks = VmJsEventLoopHooks::<Host>::new();
          let job_result = tick_result.and_then(|_| {
            let mut ctx = WindowRealmJobContext::new(window_realm, realm);
            job.run(&mut ctx, &mut hooks)
          });

          window_realm
            .vm_mut()
            .set_budget(Budget::unlimited(DEFAULT_CHECK_TIME_EVERY));

          if let Some(err) = hooks.finish(window_realm.heap_mut()) {
            return Err(err);
          }

          job_result
            .map_err(|err| vm_error_to_event_loop_error(window_realm.heap(), err))
            .map(|_| ())
        })
      })
    })();

    if let Err(err) = enqueue_result {
      if let Some(job) = job_cell.borrow_mut().take() {
        self.pending_discard.push(job);
      }
      self.enqueue_error = Some(err);
    }
  }

  fn host_call_job_callback(
    &mut self,
    ctx: &mut dyn VmJobContext,
    callback: &JobCallback,
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
}

fn normalize_animation_frame_id(heap: &mut Heap, value: Value) -> VmResult<AnimationFrameId> {
  let mut n = heap.to_number(value)?;
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

fn raf_global_from_callee(scope: &Scope<'_>, callee: vm_js::GcObject) -> VmResult<vm_js::GcObject> {
  let slot = scope
    .heap()
    .get_function_native_slots(callee)?
    .get(RAF_GLOBAL_SLOT)
    .copied()
    .unwrap_or(Value::Undefined);
  match slot {
    Value::Object(obj) => Ok(obj),
    _ => Err(VmError::Unimplemented(
      "requestAnimationFrame function missing global binding",
    )),
  }
}

fn raf_global_from_this(
  scope: &Scope<'_>,
  callee: vm_js::GcObject,
  this: Value,
  invalid_this_msg: &'static str,
) -> VmResult<vm_js::GcObject> {
  let global = raf_global_from_callee(scope, callee)?;
  match this {
    Value::Undefined | Value::Null => Ok(global),
    Value::Object(obj) if obj == global => Ok(global),
    _ => Err(throw_type_error(invalid_this_msg)),
  }
}

fn request_animation_frame_native<Host: WindowRealmHost + 'static>(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHostHooks,
  callee: vm_js::GcObject,
  this: Value,
  args: &[Value],
) -> VmResult<Value> {
  let callback = args.get(0).copied().unwrap_or(Value::Undefined);
  if matches!(callback, Value::String(_)) {
    return Err(throw_type_error(REQUEST_ANIMATION_FRAME_STRING_HANDLER_ERROR));
  }
  if !is_callable(scope, callback) {
    return Err(throw_type_error(REQUEST_ANIMATION_FRAME_NOT_CALLABLE_ERROR));
  }

  let global_obj = raf_global_from_this(
    scope,
    callee,
    this,
    "requestAnimationFrame called with invalid this value",
  )?;

  let registry = get_raf_registry(scope, global_obj)?;

  let Some(event_loop) = current_event_loop_mut::<Host>() else {
    return Err(throw_type_error(
      "requestAnimationFrame called without an active EventLoop",
    ));
  };

  // Queue the callback first to get the ID, then store the callback in the registry.
  // Use an `Rc<Cell<...>>` to communicate the allocated ID into the queued closure.
  let id_cell: std::rc::Rc<std::cell::Cell<Option<AnimationFrameId>>> =
    std::rc::Rc::new(std::cell::Cell::new(None));
  let id_cell_for_cb = std::rc::Rc::clone(&id_cell);

  let id = event_loop
    .request_animation_frame(move |host, event_loop, ts| {
      let id = id_cell_for_cb
        .get()
        .expect("requestAnimationFrame id should be set");

      let window_realm = host.window_realm();
      window_realm.reset_interrupt();
      let (vm, heap) = window_realm.vm_and_heap_mut();

      let result: crate::error::Result<()> = with_event_loop(event_loop, || {
        vm.set_budget(callback_budget_from_render_deadline());
        let tick_result = vm.tick();

        let mut hooks = VmJsEventLoopHooks::<Host>::new();
        let call_result = tick_result.and_then(|_| {
          let call_result: VmResult<()> = (|| {
            let mut scope = heap.scope();
            let callback_value = {
              let key_s = scope.alloc_string(&id.to_string())?;
              scope.push_root(Value::String(key_s))?;
              let key = PropertyKey::from_string(key_s);
              scope
                .heap()
                .object_get_own_data_property_value(registry, &key)?
                .unwrap_or(Value::Undefined)
            };
            // The callback is invoked with the global object as `this` and the timestamp argument.
            let _ = vm.call_with_host(
              &mut scope,
              &mut hooks,
              callback_value,
              Value::Object(global_obj),
              &[Value::Number(ts)],
            )?;
            Ok(())
        })();
        call_result
      });
        vm.set_budget(Budget::unlimited(DEFAULT_CHECK_TIME_EVERY));

        if let Some(err) = hooks.finish(heap) {
          return Err(err);
        }

        call_result
          .map_err(|err| vm_error_to_event_loop_error(&*heap, err))
          .map(|_| ())
      });

      {
        let mut scope = heap.scope();
        // Always clear the registry entry after the callback runs, even if it throws.
        let _ = clear_registry_entry(&mut scope, registry, id);
      }

      result
    })
    .map_err(|e| throw_error(scope, &format!("{e}")))?;

  id_cell.set(Some(id));

  if let Err(err) = store_callback(scope, registry, id, callback) {
    // If we failed to store the callback for later invocation, cancel the queued animation frame
    // so we don't invoke `undefined`.
    event_loop.cancel_animation_frame(id);
    return Err(err);
  }

  Ok(Value::Number(id as f64))
}

fn cancel_animation_frame_native<Host: WindowRealmHost + 'static>(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHostHooks,
  callee: vm_js::GcObject,
  this: Value,
  args: &[Value],
) -> VmResult<Value> {
  let global_obj = raf_global_from_this(
    scope,
    callee,
    this,
    "cancelAnimationFrame called with invalid this value",
  )?;
  let registry = get_raf_registry(scope, global_obj)?;

  let id_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let id = normalize_animation_frame_id(scope.heap_mut(), id_value)?;

  if let Some(event_loop) = current_event_loop_mut::<Host>() {
    event_loop.cancel_animation_frame(id);
  }

  // Best-effort cleanup even if there is no current event loop.
  clear_registry_entry(scope, registry, id)?;

  Ok(Value::Undefined)
}

/// Install `requestAnimationFrame` / `cancelAnimationFrame` on the JS global.
///
/// This should be installed on a `Window`-like realm (i.e. where `this` in these host functions
/// corresponds to the global object).
pub fn install_window_animation_frame_bindings<Host: WindowRealmHost + 'static>(
  vm: &mut Vm,
  realm: &vm_js::Realm,
  heap: &mut Heap,
) -> VmResult<()> {
  let mut scope = heap.scope();
  let global = realm.global_object();
  scope.push_root(Value::Object(global))?;

  let registry = scope.alloc_object()?;
  scope.push_root(Value::Object(registry))?;
  let registry_key = alloc_key(&mut scope, RAF_REGISTRY_KEY)?;
  scope.define_property(global, registry_key, data_desc(Value::Object(registry)))?;

  let global_slots = [Value::Object(global)];

  let raf_id = vm.register_native_call(request_animation_frame_native::<Host>)?;
  let raf_name = scope.alloc_string("requestAnimationFrame")?;
  scope.push_root(Value::String(raf_name))?;
  let raf = scope.alloc_native_function_with_slots(raf_id, None, raf_name, 1, &global_slots)?;
  scope
    .heap_mut()
    .object_set_prototype(raf, Some(realm.intrinsics().function_prototype()))?;
  scope.push_root(Value::Object(raf))?;

  let cancel_id = vm.register_native_call(cancel_animation_frame_native::<Host>)?;
  let cancel_name = scope.alloc_string("cancelAnimationFrame")?;
  scope.push_root(Value::String(cancel_name))?;
  let cancel =
    scope.alloc_native_function_with_slots(cancel_id, None, cancel_name, 1, &global_slots)?;
  scope.heap_mut().object_set_prototype(
    cancel,
    Some(realm.intrinsics().function_prototype()),
  )?;
  scope.push_root(Value::Object(cancel))?;

  let raf_key = alloc_key(&mut scope, "requestAnimationFrame")?;
  let cancel_key = alloc_key(&mut scope, "cancelAnimationFrame")?;

  scope.define_property(global, raf_key, data_desc(Value::Object(raf)))?;
  scope.define_property(global, cancel_key, data_desc(Value::Object(cancel)))?;

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::error::{Error, Result as RenderResult};
  use crate::js::clock::VirtualClock;
  use crate::js::event_loop::{EventLoop, RunLimits, RunUntilIdleOutcome, TaskSource};
  use crate::js::window_realm::{WindowRealm, WindowRealmConfig};
  use std::sync::Arc;
  use std::time::Duration;
  use vm_js::{PropertyDescriptor, PropertyKey, PropertyKind};

  const CALLBACK_GLOBAL_KEY: &str = "__test_global";
  const CALLBACK_JOB_KEY: &str = "__test_job_cb";

  struct Host {
    window: WindowRealm,
  }

  impl Host {
    fn new() -> Self {
      let window = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/")).unwrap();
      Self { window }
    }
  }

  impl WindowRealmHost for Host {
    fn window_realm(&mut self) -> &mut WindowRealm {
      &mut self.window
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

  fn get_prop(scope: &mut Scope<'_>, obj: vm_js::GcObject, name: &str) -> Value {
    let key_s = scope.alloc_string(name).unwrap();
    let key = PropertyKey::from_string(key_s);
    scope
      .heap()
      .object_get_own_data_property_value(obj, &key)
      .unwrap()
      .unwrap_or(Value::Undefined)
  }

  fn set_prop(scope: &mut Scope<'_>, obj: vm_js::GcObject, name: &str, value: Value) {
    let key_s = scope.alloc_string(name).unwrap();
    scope
      .push_root(Value::String(key_s))
      .expect("push root key string");
    let key = PropertyKey::from_string(key_s);
    // Root `value` before defining the property in case it triggers an allocation/GC.
    scope.push_root(value).expect("push root value");
    scope.define_property(obj, key, data_desc(value)).unwrap();
  }

  fn init_log(scope: &mut Scope<'_>, global: vm_js::GcObject) {
    let log_obj = scope.alloc_object().unwrap();
    scope
      .push_root(Value::Object(log_obj))
      .expect("push root log object");
    set_prop(scope, global, "__log_obj", Value::Object(log_obj));
    set_prop(scope, global, "__log_len", Value::Number(0.0));
  }

  fn push_log(scope: &mut Scope<'_>, global: vm_js::GcObject, label: &str) {
    let log_obj = match get_prop(scope, global, "__log_obj") {
      Value::Object(o) => o,
      _ => panic!("missing __log_obj"),
    };
    let len = match get_prop(scope, global, "__log_len") {
      Value::Number(n) => n as u32,
      _ => panic!("missing __log_len"),
    };
    let key_s = scope.alloc_string(&len.to_string()).unwrap();
    scope
      .push_root(Value::String(key_s))
      .expect("push root log key");
    let key = PropertyKey::from_string(key_s);
    let label_s = scope.alloc_string(label).unwrap();
    scope
      .push_root(Value::String(label_s))
      .expect("push root log label");
    scope
      .define_property(log_obj, key, data_desc(Value::String(label_s)))
      .unwrap();
    set_prop(scope, global, "__log_len", Value::Number((len + 1) as f64));
  }

  fn read_log(heap: &mut Heap, realm: &vm_js::Realm) -> Vec<String> {
    let mut scope = heap.scope();
    let global = realm.global_object();
    scope
      .push_root(Value::Object(global))
      .expect("push root global");
    let log_obj = match get_prop(&mut scope, global, "__log_obj") {
      Value::Object(o) => o,
      _ => panic!("missing __log_obj"),
    };
    let len = match get_prop(&mut scope, global, "__log_len") {
      Value::Number(n) => n as u32,
      _ => panic!("missing __log_len"),
    };
    let mut out = Vec::new();
    for i in 0..len {
      let key_s = scope.alloc_string(&i.to_string()).unwrap();
      let key = PropertyKey::from_string(key_s);
      let v = scope
        .heap()
        .object_get_own_data_property_value(log_obj, &key)
        .unwrap()
        .unwrap_or(Value::Undefined);
      let Value::String(s) = v else {
        panic!("expected string log entry");
      };
      out.push(scope.heap().get_string(s).unwrap().to_utf8_lossy());
    }
    out
  }

  fn make_callback(
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    realm: &vm_js::Realm,
    global: vm_js::GcObject,
    name: &str,
    cb: vm_js::NativeCall,
  ) -> vm_js::GcObject {
    let call_id = vm.register_native_call(cb).unwrap();
    let name_s = scope.alloc_string(name).unwrap();
    scope.push_root(Value::String(name_s)).unwrap();
    let func = scope.alloc_native_function(call_id, None, name_s, 1).unwrap();
    scope
      .heap_mut()
      .object_set_prototype(func, Some(realm.intrinsics().function_prototype()))
      .unwrap();
    scope.push_root(Value::Object(func)).unwrap();

    set_prop(scope, func, CALLBACK_GLOBAL_KEY, Value::Object(global));
    func
  }

  fn cb_raf(
    _vm: &mut Vm,
    scope: &mut Scope<'_>,
    _host: &mut dyn VmHostHooks,
    callee: vm_js::GcObject,
    this: Value,
    args: &[Value],
  ) -> VmResult<Value> {
    let Value::Object(global) = get_prop(scope, callee, CALLBACK_GLOBAL_KEY) else {
      return Ok(Value::Undefined);
    };
    set_prop(
      scope,
      global,
      "__raf_this_is_global",
      Value::Bool(this == Value::Object(global)),
    );
    let ts = args.get(0).copied().unwrap_or(Value::Undefined);
    set_prop(scope, global, "__raf_ts", ts);
    push_log(scope, global, "raf");
    Ok(Value::Undefined)
  }

  fn cb_job(
    _vm: &mut Vm,
    scope: &mut Scope<'_>,
    _host: &mut dyn VmHostHooks,
    callee: vm_js::GcObject,
    _this: Value,
    _args: &[Value],
  ) -> VmResult<Value> {
    let Value::Object(global) = get_prop(scope, callee, CALLBACK_GLOBAL_KEY) else {
      return Ok(Value::Undefined);
    };
    push_log(scope, global, "job");
    Ok(Value::Undefined)
  }

  fn cb_raf_enqueue_job(
    _vm: &mut Vm,
    scope: &mut Scope<'_>,
    host: &mut dyn VmHostHooks,
    callee: vm_js::GcObject,
    _this: Value,
    _args: &[Value],
  ) -> VmResult<Value> {
    let Value::Object(global) = get_prop(scope, callee, CALLBACK_GLOBAL_KEY) else {
      return Ok(Value::Undefined);
    };
    push_log(scope, global, "raf");

    let Value::Object(job_cb) = get_prop(scope, callee, CALLBACK_JOB_KEY) else {
      return Ok(Value::Undefined);
    };

    // Simulate a Promise job by directly enqueueing a `vm-js` job via the host hooks. This is
    // sufficient to validate that requestAnimationFrame callbacks are invoked with the correct
    // host hook implementation (so Promise jobs are routed into the FastRender event loop).
    let job = vm_js::Job::new(vm_js::JobKind::Promise, move |ctx, hooks| -> vm_js::JobResult {
      let _ = ctx.call(hooks, Value::Object(job_cb), Value::Object(global), &[])?;
      Ok(())
    });
    host.host_enqueue_promise_job(job, None);

    Ok(Value::Undefined)
  }

  #[test]
  fn request_animation_frame_runs_after_task_and_receives_timestamp() -> RenderResult<()> {
    let clock = Arc::new(VirtualClock::new());
    clock.set_now(Duration::from_millis(10));
    let clock_for_loop: Arc<dyn crate::js::Clock> = clock.clone();
    let mut event_loop = EventLoop::<Host>::with_clock(clock_for_loop);
    let mut host = Host::new();

    {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_animation_frame_bindings::<Host>(vm, realm, heap)
        .map_err(|e| Error::Other(e.to_string()))?;
      let mut scope = heap.scope();
      let global = realm.global_object();
      init_log(&mut scope, global);
      set_prop(&mut scope, global, "__raf_ts", Value::Undefined);
      set_prop(&mut scope, global, "__raf_this_is_global", Value::Bool(false));
    }

    event_loop.queue_task(TaskSource::Script, |host, event_loop| {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      let global = realm.global_object();
      with_event_loop(event_loop, || -> RenderResult<()> {
        let mut scope = heap.scope();
        let raf = get_prop(&mut scope, global, "requestAnimationFrame");
        let cb = make_callback(vm, &mut scope, realm, global, "raf_cb", cb_raf);
        vm.call(&mut scope, raf, Value::Object(global), &[Value::Object(cb)])
          .map_err(|e| Error::Other(e.to_string()))?;
        push_log(&mut scope, global, "sync");
        Ok(())
      })
    })?;

    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    let log = {
      let (_, realm, heap) = host.window.vm_realm_and_heap_mut();
      read_log(heap, realm)
    };
    assert_eq!(log, vec!["sync"]);

    assert_eq!(
      event_loop.run_animation_frame(&mut host)?,
      crate::js::RunAnimationFrameOutcome::Ran { callbacks: 1 }
    );

    let (raf_ts, raf_this_is_global) = {
      let (_, realm, heap) = host.window.vm_realm_and_heap_mut();
      let mut scope = heap.scope();
      let global = realm.global_object();
      let ts = get_prop(&mut scope, global, "__raf_ts");
      let this_ok = get_prop(&mut scope, global, "__raf_this_is_global");
      (ts, this_ok)
    };
    let log = {
      let (_, realm, heap) = host.window.vm_realm_and_heap_mut();
      read_log(heap, realm)
    };

    assert_eq!(log, vec!["sync", "raf"]);
    assert_eq!(raf_ts, Value::Number(10.0));
    assert_eq!(raf_this_is_global, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn request_animation_frame_can_be_called_as_identifier_in_scripts() -> RenderResult<()> {
    let clock = Arc::new(VirtualClock::new());
    clock.set_now(Duration::from_millis(10));
    let clock_for_loop: Arc<dyn crate::js::Clock> = clock.clone();
    let mut event_loop = EventLoop::<Host>::with_clock(clock_for_loop);
    let mut host = Host::new();

    {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_animation_frame_bindings::<Host>(vm, realm, heap)
        .map_err(|e| Error::Other(e.to_string()))?;
    }

    event_loop.queue_task(TaskSource::Script, |host, event_loop| {
      with_event_loop(event_loop, || -> RenderResult<()> {
        host
          .window
          .exec_script(
            "globalThis.__raf_called = false;\n\
             globalThis.__raf_ts = undefined;\n\
             requestAnimationFrame(function(ts){ globalThis.__raf_called = true; globalThis.__raf_ts = ts; });\n",
          )
          .map_err(|e| Error::Other(e.to_string()))?;
        Ok(())
      })
    })?;

    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    assert_eq!(
      event_loop.run_animation_frame(&mut host)?,
      crate::js::RunAnimationFrameOutcome::Ran { callbacks: 1 }
    );

    let (called, ts) = {
      let (_, realm, heap) = host.window.vm_realm_and_heap_mut();
      let mut scope = heap.scope();
      let global = realm.global_object();
      (get_prop(&mut scope, global, "__raf_called"), get_prop(&mut scope, global, "__raf_ts"))
    };
    assert_eq!(called, Value::Bool(true));
    assert_eq!(ts, Value::Number(10.0));
    Ok(())
  }

  #[test]
  fn cancel_animation_frame_prevents_callback() -> RenderResult<()> {
    let clock = Arc::new(VirtualClock::new());
    clock.set_now(Duration::from_millis(1));
    let clock_for_loop: Arc<dyn crate::js::Clock> = clock.clone();
    let mut event_loop = EventLoop::<Host>::with_clock(clock_for_loop);
    let mut host = Host::new();

    {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_animation_frame_bindings::<Host>(vm, realm, heap)
        .map_err(|e| Error::Other(e.to_string()))?;
      let mut scope = heap.scope();
      let global = realm.global_object();
      init_log(&mut scope, global);
    }

    event_loop.queue_task(TaskSource::Script, |host, event_loop| {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      let global = realm.global_object();
      with_event_loop(event_loop, || -> RenderResult<()> {
        let mut scope = heap.scope();
        let raf = get_prop(&mut scope, global, "requestAnimationFrame");
        let cancel = get_prop(&mut scope, global, "cancelAnimationFrame");
        let cb = make_callback(vm, &mut scope, realm, global, "raf_cb", cb_raf);
        let id = vm
          .call(&mut scope, raf, Value::Object(global), &[Value::Object(cb)])
          .map_err(|e| Error::Other(e.to_string()))?;
        vm.call(&mut scope, cancel, Value::Object(global), &[id])
          .map_err(|e| Error::Other(e.to_string()))?;
        push_log(&mut scope, global, "sync");
        Ok(())
      })
    })?;

    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    assert_eq!(event_loop.run_animation_frame(&mut host)?, crate::js::RunAnimationFrameOutcome::Idle);

    let log = {
      let (_, realm, heap) = host.window.vm_realm_and_heap_mut();
      read_log(heap, realm)
    };
    assert_eq!(log, vec!["sync"]);
    Ok(())
  }

  #[test]
  fn request_animation_frame_can_enqueue_promise_jobs() -> RenderResult<()> {
    let clock = Arc::new(VirtualClock::new());
    clock.set_now(Duration::from_millis(1));
    let clock_for_loop: Arc<dyn crate::js::Clock> = clock.clone();
    let mut event_loop = EventLoop::<Host>::with_clock(clock_for_loop);
    let mut host = Host::new();

    {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      install_window_animation_frame_bindings::<Host>(vm, realm, heap)
        .map_err(|e| Error::Other(e.to_string()))?;
      let mut scope = heap.scope();
      let global = realm.global_object();
      init_log(&mut scope, global);
    }

    event_loop.queue_task(TaskSource::Script, |host, event_loop| {
      let (vm, realm, heap) = host.window.vm_realm_and_heap_mut();
      let global = realm.global_object();
      with_event_loop(event_loop, || -> RenderResult<()> {
        let mut scope = heap.scope();
        let raf = get_prop(&mut scope, global, "requestAnimationFrame");
        let job_cb = make_callback(vm, &mut scope, realm, global, "job_cb", cb_job);
        let raf_cb = make_callback(vm, &mut scope, realm, global, "raf_cb", cb_raf_enqueue_job);
        set_prop(&mut scope, raf_cb, CALLBACK_JOB_KEY, Value::Object(job_cb));
        vm.call(&mut scope, raf, Value::Object(global), &[Value::Object(raf_cb)])
          .map_err(|e| Error::Other(e.to_string()))?;
        push_log(&mut scope, global, "sync");
        Ok(())
      })
    })?;

    assert_eq!(
      event_loop.run_until_idle(&mut host, RunLimits::unbounded())?,
      RunUntilIdleOutcome::Idle
    );
    let log = {
      let (_, realm, heap) = host.window.vm_realm_and_heap_mut();
      read_log(heap, realm)
    };
    assert_eq!(log, vec!["sync"]);

    assert_eq!(
      event_loop.run_animation_frame(&mut host)?,
      crate::js::RunAnimationFrameOutcome::Ran { callbacks: 1 }
    );

    let log = {
      let (_, realm, heap) = host.window.vm_realm_and_heap_mut();
      read_log(heap, realm)
    };
    assert_eq!(log, vec!["sync", "raf"]);

    // Promise jobs are queued into the FastRender microtask queue; draining it should run the job.
    event_loop.perform_microtask_checkpoint(&mut host)?;
    let log = {
      let (_, realm, heap) = host.window.vm_realm_and_heap_mut();
      read_log(heap, realm)
    };
    assert_eq!(log, vec!["sync", "raf", "job"]);
    Ok(())
  }
}
