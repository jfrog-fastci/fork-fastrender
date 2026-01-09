use fastrender::dom::DomNode;
use fastrender::dom2::{Document, NodeId};
use fastrender::js::{EventLoop, RunLimits, RunUntilIdleOutcome, VirtualClock};
use fastrender::{Error, Result};
use rquickjs::{Context, Function, Object, Runtime};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

thread_local! {
  static CURRENT_ENV: RefCell<Vec<EnvPointers>> = const { RefCell::new(Vec::new()) };
}

#[derive(Clone, Copy)]
struct EnvPointers {
  host: *mut HostState,
  event_loop: *mut EventLoop<HostState>,
}

fn with_current_env<R>(
  host: *mut HostState,
  event_loop: *mut EventLoop<HostState>,
  f: impl FnOnce() -> R,
) -> R {
  CURRENT_ENV.with(|stack| {
    stack.borrow_mut().push(EnvPointers { host, event_loop });
    let out = f();
    let popped = stack.borrow_mut().pop();
    debug_assert!(popped.is_some(), "env stack underflow");
    out
  })
}

fn with_env_mut<R>(f: impl FnOnce(&mut HostState, &mut EventLoop<HostState>) -> R) -> R {
  CURRENT_ENV.with(|stack| {
    let env = stack
      .borrow()
      .last()
      .copied()
      .expect("JS harness env not installed");
    // Safety: env pointers are installed by `with_current_env` which guarantees they are valid for
    // the duration of the call.
    let host = unsafe { &mut *env.host };
    let event_loop = unsafe { &mut *env.event_loop };
    f(host, event_loop)
  })
}

fn js_fire_timer(
  host: *mut HostState,
  event_loop: *mut EventLoop<HostState>,
  id: i32,
) -> Result<()> {
  let result = with_current_env(host, event_loop, || unsafe {
    (*host).js_ctx.with(|ctx| -> rquickjs::Result<()> {
      let globals = ctx.globals();
      let func: Function = globals.get("__fastrender_fire_timer")?;
      func.call::<_, ()>((id,))?;
      Ok(())
    })
  });
  result.map_err(|e| Error::Other(format!("JS error: {e}")))?;
  Ok(())
}

fn js_eval(host: *mut HostState, event_loop: *mut EventLoop<HostState>, src: &str) -> Result<()> {
  let result = with_current_env(host, event_loop, || unsafe {
    (*host).js_ctx.with(|ctx| ctx.eval::<(), _>(src))
  });
  result.map_err(|e| Error::Other(format!("JS eval error: {e}")))?;
  Ok(())
}

const JS_BOOTSTRAP: &str = r##"
(function () {
  var g = typeof globalThis !== "undefined" ? globalThis : this;

  // console shim used by Rust-side tests.
  g.console = {
    log: function () {
      var parts = [];
      for (var i = 0; i < arguments.length; i++) parts.push(String(arguments[i]));
      g.__fastrender_console_log(parts.join(" "));
    }
  };

  // Basic deterministic time.
  g.performance = { now: function () { return g.__fastrender_host_performance_now(); } };
  if (g.Date && typeof g.Date.now === "function") {
    g.Date.now = function () { return g.__fastrender_host_date_now(); };
  }

  // Minimal DOM facade.
  function Node(handle) { this.__h = handle; }
  Node.prototype.setAttribute = function (name, value) {
    g.__fastrender_dom_set_attribute(this.__h, String(name), String(value));
  };
  Node.prototype.getAttribute = function (name) {
    return g.__fastrender_dom_get_attribute(this.__h, String(name));
  };
  Node.prototype.appendChild = function (child) {
    g.__fastrender_dom_append_child(this.__h, child.__h);
    return child;
  };

  function wrap(handle) { return handle ? new Node(handle) : null; }

  var document = {
    URL: g.location && g.location.href ? g.location.href : "",
    location: g.location,
    querySelector: function (sel) { return wrap(g.__fastrender_dom_query_selector(String(sel))); },
    getElementById: function (id) { return this.querySelector("#" + String(id)); },
    createElement: function (tag) { return wrap(g.__fastrender_dom_create_element(String(tag))); },
    createTextNode: function (data) { return wrap(g.__fastrender_dom_create_text(String(data))); },
  };
  Object.defineProperty(document, "body", {
    get: function () { return document.querySelector("body"); }
  });

  g.document = document;

  // Timer shims backed by the Rust event loop.
  var callbacks = new Map(); // id -> { cb, interval, microtask }

  g.__fastrender_fire_timer = function (id) {
    var entry = callbacks.get(id);
    if (!entry) return;
    if (!entry.interval || entry.microtask) callbacks.delete(id);
    entry.cb();
  };

  function normalizeDelay(ms) {
    var n = Number(ms);
    if (!isFinite(n) || isNaN(n)) n = 0;
    if (n < 0) n = 0;
    return n;
  }

  g.setTimeout = function (cb, ms) {
    if (typeof cb !== "function") throw new TypeError("setTimeout callback is not callable");
    var id = g.__fastrender_host_set_timeout(normalizeDelay(ms));
    callbacks.set(id, { cb: cb, interval: false });
    return id;
  };
  g.clearTimeout = function (id) {
    id = Number(id);
    g.__fastrender_host_clear_timeout(id);
    callbacks.delete(id);
  };

  g.setInterval = function (cb, ms) {
    if (typeof cb !== "function") throw new TypeError("setInterval callback is not callable");
    var id = g.__fastrender_host_set_interval(normalizeDelay(ms));
    callbacks.set(id, { cb: cb, interval: true });
    return id;
  };
  g.clearInterval = function (id) {
    id = Number(id);
    g.__fastrender_host_clear_interval(id);
    callbacks.delete(id);
  };

  g.queueMicrotask = function (cb) {
    if (typeof cb !== "function") throw new TypeError("queueMicrotask callback is not callable");
    var id = g.__fastrender_host_queue_microtask();
    callbacks.set(id, { cb: cb, microtask: true });
  };
})();
"##;

struct ScriptLoaderState {
  sources: HashMap<String, String>,
  next_handle: usize,
  handles_by_url: HashMap<String, usize>,
  completed: VecDeque<(usize, String)>,
}

impl Default for ScriptLoaderState {
  fn default() -> Self {
    Self {
      sources: HashMap::new(),
      next_handle: 0,
      handles_by_url: HashMap::new(),
      completed: VecDeque::new(),
    }
  }
}

struct HostState {
  dom: Document,
  node_handles: Vec<NodeId>,
  microtask_id: i32,
  log: Vec<String>,
  loader: ScriptLoaderState,
  document_url: String,
  js_ctx: Context,
  js_rt: Runtime,
}

impl HostState {
  fn alloc_node_handle(&mut self, node: NodeId) -> u32 {
    self.node_handles.push(node);
    u32::try_from(self.node_handles.len()).unwrap_or(u32::MAX)
  }

  fn resolve_node_handle(&self, handle: u32) -> Option<NodeId> {
    if handle == 0 {
      return None;
    }
    let idx = usize::try_from(handle).ok()?.saturating_sub(1);
    self.node_handles.get(idx).copied()
  }

  fn init_realm(&mut self) -> Result<()> {
    let document_url = self.document_url.clone();
    self
      .js_ctx
      .with(|ctx| -> rquickjs::Result<()> {
        let globals = ctx.globals();

        // window / self should refer to the global object in a window realm.
        globals.set("window", globals.clone())?;
        globals.set("self", globals.clone())?;

        // Minimal location object.
        let location = Object::new(ctx.clone())?;
        location.set("href", document_url)?;
        globals.set("location", location)?;

        // Host hooks.
        globals.set(
          "__fastrender_host_set_timeout",
          Function::new(ctx.clone(), |ms: f64| -> rquickjs::Result<i32> {
            Ok(with_env_mut(|_host, event_loop| {
              let delay_ms = ms.max(0.0) as u64;
              let delay = Duration::from_millis(delay_ms);
              let id_cell: Rc<Cell<i32>> = Rc::new(Cell::new(0));
              let id_cell_for_cb = Rc::clone(&id_cell);
              let id = event_loop
                .set_timeout(delay, move |host, event_loop| {
                  let host_ptr: *mut HostState = host;
                  let loop_ptr: *mut EventLoop<HostState> = event_loop;
                  js_fire_timer(host_ptr, loop_ptr, id_cell_for_cb.get())
                })
                .expect("setTimeout scheduling failed");
              id_cell.set(id);
              id
            }))
          })?,
        )?;

        globals.set(
          "__fastrender_host_clear_timeout",
          Function::new(ctx.clone(), |id: i32| {
            with_env_mut(|_host, event_loop| event_loop.clear_timeout(id));
          })?,
        )?;

        globals.set(
          "__fastrender_host_set_interval",
          Function::new(ctx.clone(), |ms: f64| -> rquickjs::Result<i32> {
            Ok(with_env_mut(|_host, event_loop| {
              let delay_ms = ms.max(0.0) as u64;
              let interval = Duration::from_millis(delay_ms);
              let id_cell: Rc<Cell<i32>> = Rc::new(Cell::new(0));
              let id_cell_for_cb = Rc::clone(&id_cell);
              let id = event_loop
                .set_interval(interval, move |host, event_loop| {
                  let host_ptr: *mut HostState = host;
                  let loop_ptr: *mut EventLoop<HostState> = event_loop;
                  js_fire_timer(host_ptr, loop_ptr, id_cell_for_cb.get())
                })
                .expect("setInterval scheduling failed");
              id_cell.set(id);
              id
            }))
          })?,
        )?;

        globals.set(
          "__fastrender_host_clear_interval",
          Function::new(ctx.clone(), |id: i32| {
            with_env_mut(|_host, event_loop| event_loop.clear_interval(id));
          })?,
        )?;

        globals.set(
          "__fastrender_host_queue_microtask",
          Function::new(ctx.clone(), || -> rquickjs::Result<i32> {
            Ok(with_env_mut(|host, event_loop| {
              let id = host.microtask_id;
              host.microtask_id = host.microtask_id.saturating_sub(1);
              event_loop
                .queue_microtask(move |host, event_loop| {
                  let host_ptr: *mut HostState = host;
                  let loop_ptr: *mut EventLoop<HostState> = event_loop;
                  js_fire_timer(host_ptr, loop_ptr, id)
                })
                .expect("queueMicrotask scheduling failed");
              id
            }))
          })?,
        )?;

        globals.set(
          "__fastrender_host_performance_now",
          Function::new(ctx.clone(), || -> rquickjs::Result<f64> {
            with_env_mut(|_host, event_loop| {
              let now = event_loop.now();
              Ok(now.as_secs_f64() * 1000.0)
            })
          })?,
        )?;

        globals.set(
          "__fastrender_host_date_now",
          Function::new(ctx.clone(), || -> rquickjs::Result<f64> {
            // Fixed, deterministic UNIX ms origin (arbitrary, but stable).
            const ORIGIN_MS: f64 = 1_000.0;
            with_env_mut(|_host, event_loop| {
              let now = event_loop.now();
              Ok(ORIGIN_MS + now.as_secs_f64() * 1000.0)
            })
          })?,
        )?;

        globals.set(
          "__fastrender_console_log",
          Function::new(ctx.clone(), |msg: String| {
            with_env_mut(|host, _event_loop| host.log.push(msg));
          })?,
        )?;

        globals.set(
          "__fastrender_dom_query_selector",
          Function::new(ctx.clone(), |selector: String| -> rquickjs::Result<u32> {
            with_env_mut(|host, _event_loop| {
              let found = host.dom.query_selector(&selector, None).ok().flatten();
              Ok(found.map(|id| host.alloc_node_handle(id)).unwrap_or(0))
            })
          })?,
        )?;

        globals.set(
          "__fastrender_dom_create_element",
          Function::new(ctx.clone(), |tag: String| -> rquickjs::Result<u32> {
            with_env_mut(|host, _event_loop| {
              let id = host.dom.create_element(&tag, "");
              Ok(host.alloc_node_handle(id))
            })
          })?,
        )?;

        globals.set(
          "__fastrender_dom_create_text",
          Function::new(ctx.clone(), |data: String| -> rquickjs::Result<u32> {
            with_env_mut(|host, _event_loop| {
              let id = host.dom.create_text(&data);
              Ok(host.alloc_node_handle(id))
            })
          })?,
        )?;

        globals.set(
          "__fastrender_dom_append_child",
          Function::new(ctx.clone(), |parent: u32, child: u32| {
            with_env_mut(|host, _event_loop| {
              let parent = host
                .resolve_node_handle(parent)
                .expect("invalid parent node handle");
              let child = host
                .resolve_node_handle(child)
                .expect("invalid child node handle");
              host
                .dom
                .append_child(parent, child)
                .expect("appendChild failed");
            })
          })?,
        )?;

        globals.set(
          "__fastrender_dom_set_attribute",
          Function::new(ctx.clone(), |node: u32, name: String, value: String| {
            with_env_mut(|host, _event_loop| {
              let node = host.resolve_node_handle(node).expect("invalid node handle");
              host
                .dom
                .set_attribute(node, &name, &value)
                .expect("setAttribute failed");
            })
          })?,
        )?;

        globals.set(
          "__fastrender_dom_get_attribute",
          Function::new(
            ctx.clone(),
            |node: u32, name: String| -> rquickjs::Result<Option<String>> {
              with_env_mut(|host, _event_loop| {
                let node = host.resolve_node_handle(node).expect("invalid node handle");
                Ok(
                  host
                    .dom
                    .get_attribute(node, &name)
                    .ok()
                    .flatten()
                    .map(|v| v.to_string()),
                )
              })
            },
          )?,
        )?;

        ctx.eval::<(), _>(JS_BOOTSTRAP)?;
        Ok(())
      })
      .map_err(|e| Error::Other(format!("JS init error: {e}")))?;
    Ok(())
  }

  fn complete_external_script(&mut self, url: &str) -> Result<()> {
    let handle = *self
      .loader
      .handles_by_url
      .get(url)
      .ok_or_else(|| Error::Other(format!("no pending script load for url={url}")))?;
    let src = self
      .loader
      .sources
      .get(url)
      .cloned()
      .ok_or_else(|| Error::Other(format!("no registered script source for url={url}")))?;
    self.loader.completed.push_back((handle, src));
    Ok(())
  }
}

pub struct Harness {
  clock: Arc<VirtualClock>,
  host: HostState,
  event_loop: EventLoop<HostState>,
}

impl Harness {
  pub fn new(document_url: &str, html: &str) -> Result<Self> {
    let renderer_dom = fastrender::dom::parse_html(html)?;
    let dom = Document::from_renderer_dom(&renderer_dom);

    let clock = Arc::new(VirtualClock::new());
    let event_loop = EventLoop::<HostState>::with_clock(clock.clone());

    let js_rt = Runtime::new().map_err(|e| Error::Other(format!("JS runtime: {e}")))?;
    let js_ctx = Context::full(&js_rt).map_err(|e| Error::Other(format!("JS context: {e}")))?;

    let mut host = HostState {
      dom,
      node_handles: Vec::new(),
      microtask_id: -1,
      log: Vec::new(),
      loader: ScriptLoaderState::default(),
      document_url: document_url.to_string(),
      js_ctx,
      js_rt,
    };

    host.init_realm()?;

    Ok(Self {
      clock,
      host,
      event_loop,
    })
  }

  pub fn exec_script(&mut self, src: &str) -> Result<()> {
    let host_ptr: *mut HostState = &mut self.host;
    let loop_ptr: *mut EventLoop<HostState> = &mut self.event_loop;
    js_eval(host_ptr, loop_ptr, src)
  }

  pub fn advance_time(&mut self, ms: u64) {
    self.clock.advance(Duration::from_millis(ms));
  }

  pub fn run_until_idle(&mut self, limits: RunLimits) -> Result<RunUntilIdleOutcome> {
    self.event_loop.run_until_idle(&mut self.host, limits)
  }

  pub fn snapshot_dom(&self) -> DomNode {
    self.host.dom.to_renderer_dom()
  }

  pub fn take_log(&mut self) -> Vec<String> {
    std::mem::take(&mut self.host.log)
  }

  pub fn set_external_script_sources(&mut self, sources: HashMap<String, String>) {
    self.host.loader.sources = sources;
  }

  pub fn load_external_script_blocking(&mut self, url: &str) -> Result<String> {
    self
      .host
      .loader
      .sources
      .get(url)
      .cloned()
      .ok_or_else(|| Error::Other(format!("no registered script source for url={url}")))
  }

  /// Start a deterministic "async" external script load.
  ///
  /// Call [`Harness::complete_external_script`] to resolve it, and
  /// [`Harness::poll_external_script_completion`] to consume completions in the chosen order.
  pub fn start_external_script_load(&mut self, url: &str) -> Result<usize> {
    let handle = self.host.loader.next_handle;
    self.host.loader.next_handle += 1;
    self
      .host
      .loader
      .handles_by_url
      .insert(url.to_string(), handle);
    Ok(handle)
  }

  pub fn poll_external_script_completion(&mut self) -> Result<Option<(usize, String)>> {
    Ok(self.host.loader.completed.pop_front())
  }

  pub fn complete_external_script(&mut self, url: &str) -> Result<()> {
    self.host.complete_external_script(url)
  }
}
