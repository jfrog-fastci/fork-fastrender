use crate::dom2;
use crate::error::{Error, Result};
use crate::js::host_document::DocumentHostState;
use crate::js::orchestrator::CurrentScriptHost;
use crate::js::runtime::with_event_loop;
use crate::js::window_realm::{WindowRealm, WindowRealmConfig, WindowRealmHost};
use crate::js::{
  install_window_animation_frame_bindings, install_window_timers_bindings, DomHost, EventLoop,
  RunLimits, RunUntilIdleOutcome, TaskSource,
};

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
    Ok(Self {
      host: WindowHostState::new(dom, document_url)?,
      event_loop: EventLoop::new(),
    })
  }

  pub fn from_renderer_dom(root: &crate::dom::DomNode, document_url: impl Into<String>) -> Result<Self> {
    Self::new(dom2::Document::from_renderer_dom(root), document_url)
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
    let (host, event_loop) = (&mut self.host, &mut self.event_loop);
    with_event_loop(event_loop, || {
      host
        .window_mut()
        .exec_script(source)
        .map_err(|e| Error::Other(e.to_string()))
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
  document: DocumentHostState,
  window: WindowRealm,
}

impl WindowHostState {
  pub fn new(dom: dom2::Document, document_url: impl Into<String>) -> Result<Self> {
    let document_url = document_url.into();
    let document = DocumentHostState::new(dom);
    let mut window = WindowRealm::new(
      WindowRealmConfig::new(document_url.clone())
        .with_current_script_state(document.current_script_state().clone()),
    )
    .map_err(|e| Error::Other(e.to_string()))?;

    // Install timer bindings (`setTimeout`, `setInterval`, `queueMicrotask`) so scripts executed in
    // this host can schedule work onto the accompanying `EventLoop`.
    {
      let (vm, realm, heap) = window.vm_realm_and_heap_mut();
      install_window_timers_bindings::<WindowHostState>(vm, realm, heap)
        .map_err(|e| Error::Other(e.to_string()))?;
      install_window_animation_frame_bindings::<WindowHostState>(vm, realm, heap)
        .map_err(|e| Error::Other(e.to_string()))?;
    }

    Ok(Self {
      base_url: Some(document_url.clone()),
      document_url,
      document,
      window,
    })
  }

  pub fn from_renderer_dom(root: &crate::dom::DomNode, document_url: impl Into<String>) -> Result<Self> {
    Self::new(dom2::Document::from_renderer_dom(root), document_url)
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

  use selectors::context::QuirksMode;
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
}
