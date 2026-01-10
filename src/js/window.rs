use crate::dom2;
use crate::error::{Error, Result};
use crate::js::host_document::DocumentHostState;
use crate::js::orchestrator::CurrentScriptHost;
use crate::js::runtime::with_event_loop;
use crate::js::window_realm::{
  register_dom_source, unregister_dom_source, WindowRealm, WindowRealmConfig, WindowRealmHost,
};
use crate::js::{
  install_window_animation_frame_bindings, install_window_fetch_bindings_with_guard,
  install_window_timers_bindings, DomHost, EventLoop, RunLimits, RunUntilIdleOutcome, TaskSource,
  WindowFetchBindings, WindowFetchEnv,
};
use crate::js::vm_error_format;
use crate::resource::{HttpFetcher, ResourceFetcher};
use std::ptr::NonNull;
use std::sync::Arc;

/// Host-owned "window" state for executing scripts against a single DOM document.
///
/// This is a convenience composition type that bundles:
/// - a mutable `dom2::Document` (via [`DocumentHostState`]),
/// - a `vm-js` realm with Window-like globals (`window`/`self`/`document`/`location`) via [`WindowRealm`],
/// - and an HTML-like event loop (`setTimeout`/microtasks) via [`EventLoop`].
///
/// The JS realm is configured with a clone of the document's [`CurrentScriptHost`] handle so
/// `document.currentScript` is observable during script execution.
pub struct WindowHost {
  host: WindowHostState,
  event_loop: EventLoop<WindowHostState>,
}

impl WindowHost {
  pub fn new(dom: dom2::Document, document_url: impl Into<String>) -> Result<Self> {
    Self::new_with_fetcher(dom, document_url, Arc::new(HttpFetcher::new()))
  }

  pub fn new_with_fetcher(
    dom: dom2::Document,
    document_url: impl Into<String>,
    fetcher: Arc<dyn ResourceFetcher>,
  ) -> Result<Self> {
    let host = WindowHostState::new_with_fetcher(dom, document_url, fetcher)?;
    let event_loop = EventLoop::new();
    Ok(Self { host, event_loop })
  }

  pub fn from_renderer_dom(root: &crate::dom::DomNode, document_url: impl Into<String>) -> Result<Self> {
    Self::new(dom2::Document::from_renderer_dom(root), document_url)
  }

  pub fn from_renderer_dom_with_fetcher(
    root: &crate::dom::DomNode,
    document_url: impl Into<String>,
    fetcher: Arc<dyn ResourceFetcher>,
  ) -> Result<Self> {
    Self::new_with_fetcher(dom2::Document::from_renderer_dom(root), document_url, fetcher)
  }

  pub fn host(&self) -> &WindowHostState {
    &self.host
  }

  pub fn host_mut(&mut self) -> &mut WindowHostState {
    &mut self.host
  }

  pub fn event_loop(&self) -> &EventLoop<WindowHostState> {
    &self.event_loop
  }

  pub fn event_loop_mut(&mut self) -> &mut EventLoop<WindowHostState> {
    &mut self.event_loop
  }

  pub fn queue_task<F>(&mut self, source: TaskSource, runnable: F) -> Result<()>
  where
    F: FnOnce(&mut WindowHostState, &mut EventLoop<WindowHostState>) -> Result<()> + 'static,
  {
    self.event_loop.queue_task(source, runnable)
  }

  pub fn perform_microtask_checkpoint(&mut self) -> Result<()> {
    self.event_loop.perform_microtask_checkpoint(&mut self.host)
  }

  pub fn run_until_idle(&mut self, limits: RunLimits) -> Result<RunUntilIdleOutcome> {
    self.event_loop.run_until_idle(&mut self.host, limits)
  }

  /// Execute a classic script in this window's JS realm.
  ///
  /// This installs the accompanying [`EventLoop`] as the thread-local "current event loop" so Web
  /// APIs like `queueMicrotask`, `setTimeout`, and `requestAnimationFrame` can schedule work.
  ///
  /// Note: this does **not** automatically run a microtask checkpoint. Call
  /// [`WindowHost::perform_microtask_checkpoint`] or drive the event loop as needed.
  pub fn exec_script(&mut self, source: &str) -> Result<vm_js::Value> {
    use crate::js::window_timers::VmJsEventLoopHooks;

    let (host, event_loop) = (&mut self.host, &mut self.event_loop);
    with_event_loop(event_loop, || {
      let window = host.window_mut();
      let mut hooks = VmJsEventLoopHooks::<WindowHostState>::new();
      let result = window.exec_script_with_hooks(&mut hooks, source);

      if let Some(err) = hooks.finish(window.heap_mut()) {
        return Err(err);
      }

      match result {
        Ok(value) => Ok(value),
        Err(err) => Err(vm_error_format::vm_error_to_error(window.heap_mut(), err)),
      }
    })
  }
}

/// Host state used by [`WindowHost`]'s event loop.
pub struct WindowHostState {
  pub document_url: String,
  /// Current document base URL used for resolving relative URLs.
  ///
  /// This is a host-level concept (HTML `Document.baseURI`) and is not stored in `dom2`.
  pub base_url: Option<String>,
  dom_source_id: Option<u64>,
  document: Box<DocumentHostState>,
  window: WindowRealm,
  _fetch_bindings: WindowFetchBindings,
}

impl WindowHostState {
  pub fn new(dom: dom2::Document, document_url: impl Into<String>) -> Result<Self> {
    Self::new_with_fetcher(dom, document_url, Arc::new(HttpFetcher::new()))
  }

  pub fn new_with_fetcher(
    dom: dom2::Document,
    document_url: impl Into<String>,
    fetcher: Arc<dyn ResourceFetcher>,
  ) -> Result<Self> {
    let document_url = document_url.into();
    // The JS bindings store a `dom_source_id` that resolves to a raw pointer in a thread-local
    // registry. That pointer must remain stable for the lifetime of this host, so keep the
    // `DocumentHostState` on the heap.
    let mut document = Box::new(DocumentHostState::new(dom));
    let dom_source_id = register_dom_source(NonNull::from(document.dom_mut()));
    let mut window = match WindowRealm::new(
      WindowRealmConfig::new(document_url.clone())
        .with_dom_source_id(dom_source_id)
        .with_current_script_state(document.current_script_state().clone()),
    ) {
      Ok(window) => window,
      Err(err) => {
        unregister_dom_source(dom_source_id);
        return Err(Error::Other(err.to_string()));
      }
    };
    window.set_cookie_fetcher(fetcher.clone());

    // Install timer bindings (`setTimeout`, `setInterval`, `queueMicrotask`) so scripts executed in
    // this host can schedule work onto the accompanying `EventLoop`.
    let fetch_bindings = {
      let (vm, realm, heap) = window.vm_realm_and_heap_mut();
      if let Err(err) = install_window_timers_bindings::<WindowHostState>(vm, realm, heap) {
        unregister_dom_source(dom_source_id);
        return Err(Error::Other(err.to_string()));
      }
      if let Err(err) = install_window_animation_frame_bindings::<WindowHostState>(vm, realm, heap)
      {
        unregister_dom_source(dom_source_id);
        return Err(Error::Other(err.to_string()));
      }
      match install_window_fetch_bindings_with_guard::<WindowHostState>(
        vm,
        realm,
        heap,
        WindowFetchEnv::for_document(fetcher, Some(document_url.clone())),
      ) {
        Ok(bindings) => bindings,
        Err(err) => {
          unregister_dom_source(dom_source_id);
          return Err(Error::Other(err.to_string()));
        }
      }
    };

    Ok(Self {
      base_url: Some(document_url.clone()),
      document_url,
      dom_source_id: Some(dom_source_id),
      document,
      window,
      _fetch_bindings: fetch_bindings,
    })
  }

  pub fn from_renderer_dom(root: &crate::dom::DomNode, document_url: impl Into<String>) -> Result<Self> {
    Self::new(dom2::Document::from_renderer_dom(root), document_url)
  }

  pub fn from_renderer_dom_with_fetcher(
    root: &crate::dom::DomNode,
    document_url: impl Into<String>,
    fetcher: Arc<dyn ResourceFetcher>,
  ) -> Result<Self> {
    Self::new_with_fetcher(dom2::Document::from_renderer_dom(root), document_url, fetcher)
  }

  pub fn dom(&self) -> &dom2::Document {
    self.document.dom()
  }

  pub fn dom_mut(&mut self) -> &mut dom2::Document {
    self.document.dom_mut()
  }

  pub fn document_host(&self) -> &DocumentHostState {
    &self.document
  }

  pub fn document_host_mut(&mut self) -> &mut DocumentHostState {
    &mut self.document
  }

  pub fn window(&self) -> &WindowRealm {
    &self.window
  }

  pub fn window_mut(&mut self) -> &mut WindowRealm {
    &mut self.window
  }
}

impl Drop for WindowHostState {
  fn drop(&mut self) {
    if let Some(id) = self.dom_source_id.take() {
      unregister_dom_source(id);
    }
  }
}

impl DomHost for WindowHostState {
  fn with_dom<R, F>(&self, f: F) -> R
  where
    F: FnOnce(&dom2::Document) -> R,
  {
    self.document.with_dom(f)
  }

  fn mutate_dom<R, F>(&mut self, f: F) -> R
  where
    F: FnOnce(&mut dom2::Document) -> (R, bool),
  {
    self.document.mutate_dom(f)
  }
}

impl CurrentScriptHost for WindowHostState {
  fn current_script_state(&self) -> &crate::js::CurrentScriptStateHandle {
    self.document.current_script_state()
  }
}

impl WindowRealmHost for WindowHostState {
  fn window_realm(&mut self) -> &mut WindowRealm {
    &mut self.window
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  use crate::resource::FetchedResource;
  use selectors::context::QuirksMode;
  use std::sync::Mutex;
  use vm_js::{PropertyKey, Value};

  fn get_global_prop(host: &mut WindowHost, name: &str) -> Value {
    let window = host.host_mut().window_mut();
    let (_vm, realm, heap) = window.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    let global = realm.global_object();
    scope
      .push_root(Value::Object(global))
      .expect("push root global");
    let key_s = scope.alloc_string(name).expect("alloc prop name");
    scope
      .push_root(Value::String(key_s))
      .expect("push root prop name");
    let key = PropertyKey::from_string(key_s);
    scope
      .heap()
      .object_get_own_data_property_value(global, &key)
      .expect("get prop")
      .unwrap_or(Value::Undefined)
  }

  fn get_global_prop_utf8(host: &mut WindowHost, name: &str) -> Option<String> {
    let value = get_global_prop(host, name);
    let window = host.host_mut().window_mut();
    match value {
      Value::String(s) => Some(
        window
          .heap()
          .get_string(s)
          .expect("get string")
          .to_utf8_lossy(),
      ),
      _ => None,
    }
  }

  fn value_to_string(host: &WindowHost, value: Value) -> String {
    let Value::String(s) = value else {
      panic!("expected a string, got {value:?}");
    };
    host
      .host()
      .window()
      .heap()
      .get_string(s)
      .expect("heap should contain string")
      .to_utf8_lossy()
  }

  #[derive(Default)]
  struct CookieRecordingFetcher {
    cookies: Mutex<Vec<(String, String)>>,
  }

  impl CookieRecordingFetcher {
    fn cookie_header(&self) -> Option<String> {
      let lock = self.cookies.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
      if lock.is_empty() {
        return None;
      }
      Some(
        lock
          .iter()
          .map(|(name, value)| format!("{name}={value}"))
          .collect::<Vec<_>>()
          .join("; "),
      )
    }
  }

  impl ResourceFetcher for CookieRecordingFetcher {
    fn fetch(&self, url: &str) -> Result<FetchedResource> {
      Err(Error::Other(format!(
        "CookieRecordingFetcher does not support fetch: {url}"
      )))
    }

    fn cookie_header_value(&self, _url: &str) -> Option<String> {
      self.cookie_header()
    }

    fn store_cookie_from_document(&self, _url: &str, cookie_string: &str) {
      let first = cookie_string
        .split_once(';')
        .map(|(a, _)| a)
        .unwrap_or(cookie_string);
      let first = first.trim_matches(|c: char| c.is_ascii_whitespace());
      let Some((name, value)) = first.split_once('=') else {
        return;
      };
      let name = name.trim_matches(|c: char| c.is_ascii_whitespace());
      if name.is_empty() {
        return;
      }

      let mut lock = self.cookies.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
      if let Some(existing) = lock.iter_mut().find(|(n, _)| n == name) {
        existing.1 = value.to_string();
      } else {
        lock.push((name.to_string(), value.to_string()));
      }
    }
  }

  #[test]
  fn exec_script_installs_event_loop_for_queue_microtask() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/")?;

    host.exec_script(
      "var g = this; g.__x = 0; g.queueMicrotask(function () { g.__x = 1; });",
    )?;

    assert!(matches!(get_global_prop(&mut host, "__x"), Value::Number(n) if n == 0.0));

    host.perform_microtask_checkpoint()?;

    assert!(matches!(get_global_prop(&mut host, "__x"), Value::Number(n) if n == 1.0));
    Ok(())
  }

  #[test]
  fn exec_script_drains_promise_jobs_at_microtask_checkpoint() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/")?;

    // Nested Promise job: the inner `then` must run in the same microtask checkpoint.
    host.exec_script(
      "var g = this; g.__x = 0; Promise.resolve().then(function () { g.__x = 1; Promise.resolve().then(function () { g.__x = 2; }); });",
    )?;

    assert!(matches!(get_global_prop(&mut host, "__x"), Value::Number(n) if n == 0.0));

    host.perform_microtask_checkpoint()?;

    assert!(matches!(get_global_prop(&mut host, "__x"), Value::Number(n) if n == 2.0));
    Ok(())
  }

  #[test]
  fn exec_script_preserves_microtask_order_between_promise_and_queue_microtask() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/")?;

    // Both Promise jobs and `queueMicrotask` are microtasks in HTML. They must share the same FIFO
    // microtask queue so ordering matches enqueue order.
    host.exec_script(
      "var g = this; g.__x = 0; Promise.resolve().then(function () { g.__x = g.__x * 10 + 1; }); queueMicrotask(function () { g.__x = g.__x * 10 + 2; });",
    )?;

    assert!(matches!(get_global_prop(&mut host, "__x"), Value::Number(n) if n == 0.0));

    host.perform_microtask_checkpoint()?;

    // If Promise jobs are incorrectly drained after `queueMicrotask` callbacks, the result would be
    // `21` instead of `12`.
    assert!(matches!(get_global_prop(&mut host, "__x"), Value::Number(n) if n == 12.0));
    Ok(())
  }

  #[test]
  fn document_cookie_round_trip_is_deterministic() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/")?;

    host.exec_script("document.cookie = 'b=c; Path=/'; document.cookie = 'a=b';")?;

    let cookie = host.exec_script("document.cookie")?;
    assert_eq!(value_to_string(&host, cookie), "a=b; b=c");
    Ok(())
  }

  #[test]
  fn document_cookie_syncs_with_fetcher_cookie_store() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let fetcher = Arc::new(CookieRecordingFetcher::default());
    fetcher.store_cookie_from_document("https://example.invalid/", "z=1");
    let mut host = WindowHost::new_with_fetcher(dom, "https://example.invalid/", fetcher.clone())?;

    let cookie = host.exec_script("document.cookie")?;
    assert_eq!(value_to_string(&host, cookie), "z=1");

    host.exec_script("document.cookie = 'b=c; Path=/'; document.cookie = 'a=b';")?;

    assert_eq!(
      fetcher
        .cookie_header_value("https://example.invalid/")
        .unwrap_or_default(),
      "z=1; b=c; a=b"
    );

    let cookie = host.exec_script("document.cookie")?;
    assert_eq!(value_to_string(&host, cookie), "a=b; b=c; z=1");
    Ok(())
  }

  #[test]
  fn document_cookie_fetcher_sync_handles_empty_cookie_header() -> Result<()> {
    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(HttpFetcher::new());
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new_with_fetcher(dom, "https://example.invalid/", fetcher.clone())?;

    // Cookie is scoped to `/sub`, so it should not be visible on the document at `/`.
    host.exec_script("document.cookie = 'a=b; Path=/sub';")?;
    let cookie = host.exec_script("document.cookie")?;
    assert_eq!(value_to_string(&host, cookie), "");

    // A separate document whose URL path matches the cookie should observe it.
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host_sub = WindowHost::new_with_fetcher(dom, "https://example.invalid/sub", fetcher)?;
    let cookie = host_sub.exec_script("document.cookie")?;
    assert_eq!(value_to_string(&host_sub, cookie), "a=b");
    Ok(())
  }

  #[test]
  fn window_realm_supports_event_constructors_and_create_event() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/")?;

    host.exec_script(
      r#"
      var e1 = document.createEvent("Event");
      e1.initEvent("hello", true, false);
      this.__e1_type = e1.type;
      this.__e1_bubbles = e1.bubbles;
      this.__e1_cancelable = e1.cancelable;

      var e2 = document.createEvent("CustomEvent");
      e2.initCustomEvent("world", false, true, 123);
      this.__e2_type = e2.type;
      this.__e2_detail = e2.detail;

      var e3 = new CustomEvent("ctor", { detail: 456 });
      this.__e3_type = e3.type;
      this.__e3_detail = e3.detail;

      try {
        document.createEvent("NoSuchEvent");
        this.__unsupported = "did_not_throw";
      } catch (e) {
        this.__unsupported = e && e.name;
      }
    "#,
    )?;

    assert_eq!(
      get_global_prop_utf8(&mut host, "__e1_type").as_deref(),
      Some("hello")
    );
    assert!(matches!(
      get_global_prop(&mut host, "__e1_bubbles"),
      Value::Bool(true)
    ));
    assert!(matches!(
      get_global_prop(&mut host, "__e1_cancelable"),
      Value::Bool(false)
    ));

    assert_eq!(
      get_global_prop_utf8(&mut host, "__e2_type").as_deref(),
      Some("world")
    );
    assert!(matches!(
      get_global_prop(&mut host, "__e2_detail"),
      Value::Number(n) if n == 123.0
    ));

    assert_eq!(
      get_global_prop_utf8(&mut host, "__e3_type").as_deref(),
      Some("ctor")
    );
    assert!(matches!(
      get_global_prop(&mut host, "__e3_detail"),
      Value::Number(n) if n == 456.0
    ));

    assert_eq!(
      get_global_prop_utf8(&mut host, "__unsupported").as_deref(),
      Some("NotSupportedError")
    );

    Ok(())
  }

  #[test]
  fn exec_script_error_includes_stack_trace() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new(dom, "https://example.invalid/")?;

    let err = host
      .exec_script("1;\nthrow \"boom\";")
      .expect_err("expected script to throw");
    let Error::Other(msg) = err else {
      panic!("expected Error::Other, got {err:?}");
    };

    assert!(
      msg.contains("boom"),
      "expected message to include thrown string, got {msg:?}"
    );
    assert!(
      msg.contains("at "),
      "expected message to include stack trace, got {msg:?}"
    );
    assert!(
      msg.contains(":2:1"),
      "expected stack trace to include line/col 2:1, got {msg:?}"
    );
    Ok(())
  }
}
