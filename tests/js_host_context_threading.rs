use fastrender::dom2::Document as Dom2Document;
use fastrender::js::{RunLimits, RunUntilIdleOutcome, TaskSource, VmJsHostContext, WindowHost};
use fastrender::resource::{FetchDestination, FetchRequest, FetchedResource, HttpRequest, ResourceFetcher};
use fastrender::{Error, Result};
use selectors::context::QuirksMode;
use std::sync::Arc;
use vm_js::{PropertyDescriptor, PropertyKey, PropertyKind, Scope, Value, Vm, VmError, VmHost, VmHostHooks};

#[derive(Debug)]
struct StubFetcher;

impl ResourceFetcher for StubFetcher {
  fn fetch(&self, url: &str) -> Result<FetchedResource> {
    let fetch = FetchRequest::new(url, FetchDestination::Fetch);
    self.fetch_http_request(HttpRequest::new(fetch, "GET"))
  }

  fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
    self.fetch_http_request(HttpRequest::new(req, "GET"))
  }

  fn fetch_http_request(&self, req: HttpRequest<'_>) -> Result<FetchedResource> {
    if req.fetch.url != "https://example.com/x" {
      return Err(Error::Other(format!(
        "unexpected fetch url in host context threading test: {}",
        req.fetch.url
      )));
    }
    let mut res = FetchedResource::new(Vec::new(), None);
    res.status = Some(200);
    Ok(res)
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

const HOST_CONTEXT_DOWNCAST_ERROR: &str = "VmHost is not VmJsHostContext";
const HOST_CONTEXT_DOM_MISSING_ERROR: &str = "VmJsHostContext missing dom pointer";

fn host_ctx_tick_native(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: vm_js::GcObject,
  _this: Value,
  _args: &[Value],
) -> std::result::Result<Value, VmError> {
  let Some(ctx) = host.as_any_mut().downcast_mut::<VmJsHostContext>() else {
    return Err(VmError::TypeError(HOST_CONTEXT_DOWNCAST_ERROR));
  };
  if ctx.dom_ptr().is_none() {
    return Err(VmError::TypeError(HOST_CONTEXT_DOM_MISSING_ERROR));
  }
  Ok(Value::Number(1.0))
}

fn install_host_ctx_tick(host: &mut WindowHost) -> Result<()> {
  let window = host.host_mut().window_mut();
  let (vm, realm, heap) = window.vm_realm_and_heap_mut();
  let mut scope = heap.scope();
  let global = realm.global_object();
  scope
    .push_root(Value::Object(global))
    .map_err(|e| Error::Other(e.to_string()))?;

  let id = vm
    .register_native_call(host_ctx_tick_native)
    .map_err(|e| Error::Other(e.to_string()))?;

  let name_s = scope
    .alloc_string("__host_ctx_tick")
    .map_err(|e| Error::Other(e.to_string()))?;
  scope
    .push_root(Value::String(name_s))
    .map_err(|e| Error::Other(e.to_string()))?;
  let func = scope
    .alloc_native_function(id, None, name_s, 0)
    .map_err(|e| Error::Other(e.to_string()))?;
  scope
    .heap_mut()
    .object_set_prototype(func, Some(realm.intrinsics().function_prototype()))
    .map_err(|e| Error::Other(e.to_string()))?;
  scope
    .push_root(Value::Object(func))
    .map_err(|e| Error::Other(e.to_string()))?;

  let key = PropertyKey::from_string(name_s);
  scope
    .define_property(global, key, data_desc(Value::Object(func)))
    .map_err(|e| Error::Other(e.to_string()))?;

  Ok(())
}

#[test]
fn vm_js_host_context_is_threaded_through_window_entry_points() -> Result<()> {
  let fetcher: Arc<dyn ResourceFetcher> = Arc::new(StubFetcher);
  let dom = Dom2Document::new(QuirksMode::NoQuirks);
  let mut host = WindowHost::new_with_fetcher(dom, "https://example.com/", fetcher)?;
  install_host_ctx_tick(&mut host)?;

  host
    .exec_script(
      r#"
globalThis.__count = 0;
function ping() { globalThis.__count += __host_ctx_tick(); }
ping();
"#,
    )
    .map_err(|e| Error::Other(format!("exec_script (bootstrap): {e}")))?;

  host
    .exec_script("Promise.resolve().then(ping);")
    .map_err(|e| Error::Other(format!("exec_script (Promise.then): {e:?}")))?;
  host
    .exec_script("setTimeout(ping, 0);")
    .map_err(|e| Error::Other(format!("exec_script (setTimeout): {e}")))?;
  host
    .exec_script("requestAnimationFrame(ping);")
    .map_err(|e| Error::Other(format!("exec_script (requestAnimationFrame): {e}")))?;
  host
    .exec_script("fetch(\"https://example.com/x\").then(ping);")
    .map_err(|e| Error::Other(format!("exec_script (fetch.then): {e}")))?;

  assert_eq!(
    host
      .run_until_idle(RunLimits::unbounded())
      .map_err(|e| Error::Other(format!("run_until_idle (turn 1): {e}")))?,
    RunUntilIdleOutcome::Idle
  );

  // `run_until_idle` intentionally does not run animation frames. Queue an explicit task that runs
  // one frame turn so the callback fires.
  host
    .queue_task(TaskSource::Script, |host, event_loop| {
      let _ = event_loop.run_animation_frame(host)?;
      Ok(())
    })
    .map_err(|e| Error::Other(format!("queue animation frame task: {e}")))?;

  assert_eq!(
    host
      .run_until_idle(RunLimits::unbounded())
      .map_err(|e| Error::Other(format!("run_until_idle (turn 2): {e}")))?,
    RunUntilIdleOutcome::Idle
  );

  let count = host
    .exec_script("globalThis.__count")
    .map_err(|e| Error::Other(format!("read __count: {e}")))?;
  let Value::Number(n) = count else {
    return Err(Error::Other(format!(
      "expected globalThis.__count to be a number, got {count:?}"
    )));
  };
  assert_eq!(n, 5.0);
  Ok(())
}
