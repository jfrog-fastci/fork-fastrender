use crate::error::{Error, Result};
use crate::js::window_realm::{register_dom_source, unregister_dom_source, WindowRealm, WindowRealmConfig};
use crate::js::{CurrentScriptStateHandle, JsExecutionOptions, LocationNavigationRequest, ScriptElementSpec};
use crate::web::events::{Event, EventTargetId};
use vm_js::HeapLimits;
use std::ptr::NonNull;

use super::BrowserDocumentDom2;
use super::{BrowserTabHost, BrowserTabJsExecutor};

/// `vm-js`-backed [`BrowserTabJsExecutor`] that provides a minimal `window`/`document` environment.
///
/// Navigation creates a fresh JS realm for each document (matching browser semantics) while keeping
/// a stable host-owned DOM pointer registration for the lifetime of the tab.
pub struct VmJsBrowserTabExecutor {
  dom_source_id: Option<u64>,
  realm: Option<WindowRealm>,
  pending_navigation: Option<LocationNavigationRequest>,
}

impl VmJsBrowserTabExecutor {
  pub fn new() -> Self {
    Self {
      dom_source_id: None,
      realm: None,
      pending_navigation: None,
    }
  }

  fn ensure_dom_source_id(&mut self, document: &mut BrowserDocumentDom2) -> u64 {
    if let Some(id) = self.dom_source_id {
      return id;
    }
    // SAFETY: `BrowserTabHost` stores `BrowserDocumentDom2` behind a `Box`, so the `dom2::Document`
    // field address is stable for the lifetime of the tab (even if the tab is moved). The DOM tree
    // itself can be replaced in-place on navigation/parsing, but the pointer remains valid.
    let id = register_dom_source(NonNull::from(document.dom_mut()));
    self.dom_source_id = Some(id);
    id
  }
}

impl Default for VmJsBrowserTabExecutor {
  fn default() -> Self {
    Self::new()
  }
}

impl Drop for VmJsBrowserTabExecutor {
  fn drop(&mut self) {
    // Drop the realm first so any remaining JS globals stop referencing the DOM source id.
    self.realm = None;
    if let Some(id) = self.dom_source_id.take() {
      unregister_dom_source(id);
    }
  }
}

impl BrowserTabJsExecutor for VmJsBrowserTabExecutor {
  fn on_document_base_url_updated(&mut self, base_url: Option<&str>) {
    let Some(realm) = self.realm.as_mut() else {
      return;
    };
    realm.set_base_url(base_url.map(|s| s.to_string()));
  }

  fn reset_for_navigation(
    &mut self,
    document_url: Option<&str>,
    document: &mut BrowserDocumentDom2,
    current_script: &CurrentScriptStateHandle,
    js_execution_options: JsExecutionOptions,
  ) -> Result<()> {
    self.pending_navigation = None;
    // Tear down the previous realm so we don't leak rooted callbacks or global state across
    // navigations.
    self.realm = None;

    let dom_source_id = self.ensure_dom_source_id(document);

    let url = document_url.unwrap_or("about:blank");
    let mut config = WindowRealmConfig::new(url)
      .with_dom_source_id(dom_source_id)
      .with_current_script_state(current_script.clone());

    if let Some(max_bytes) = js_execution_options.max_vm_heap_bytes {
      // Keep the historical "GC at half max heap" behavior.
      let gc_threshold = (max_bytes / 2).min(max_bytes);
      config = config.with_heap_limits(HeapLimits::new(max_bytes, gc_threshold));
    }

    let mut realm = WindowRealm::new(config).map_err(|err| Error::Other(err.to_string()))?;
    realm.set_cookie_fetcher(document.fetcher());
    self.realm = Some(realm);
    Ok(())
  }

  fn execute_classic_script(
    &mut self,
    script_text: &str,
    spec: &ScriptElementSpec,
    _current_script: Option<crate::dom2::NodeId>,
    _document: &mut BrowserDocumentDom2,
    _event_loop: &mut crate::js::EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    let Some(realm) = self.realm.as_mut() else {
      return Err(Error::Other(
        "VmJsBrowserTabExecutor has no active WindowRealm; did reset_for_navigation run?".to_string(),
      ));
    };
    realm.set_base_url(spec.base_url.clone());
    realm.reset_interrupt();
    let source_name = spec
      .src
      .as_deref()
      .unwrap_or("source=inline");
    match realm.exec_script_with_name(source_name, script_text) {
      Ok(_) => Ok(()),
      Err(err) => {
        if let Some(req) = realm.take_pending_navigation_request() {
          // Clear the interrupt flag so the realm can be reused if the embedding chooses to keep
          // executing (e.g. navigation fails and scripts continue running).
          realm.reset_interrupt();
          self.pending_navigation = Some(req);
          return Ok(());
        }
        Err(Error::Other(err.to_string()))
      }
    }
  }

  fn take_navigation_request(&mut self) -> Option<LocationNavigationRequest> {
    self.pending_navigation.take()
  }

  fn dispatch_lifecycle_event(&mut self, target: EventTargetId, event: &Event) -> Result<()> {
    let Some(realm) = self.realm.as_mut() else {
      return Ok(());
    };

    let dispatch_expr = match target {
      EventTargetId::Document => "document.dispatchEvent(e);",
      EventTargetId::Window => "dispatchEvent(e);",
      EventTargetId::Node(_) => return Ok(()),
    };

    let type_lit = serde_json::to_string(&event.type_).unwrap_or_else(|_| "\"\"".to_string());
    let init_lit = serde_json::json!({
      "bubbles": event.bubbles,
      "cancelable": event.cancelable,
      "composed": event.composed,
    })
    .to_string();
    let source = format!(
      "(function(){{const e=new Event({type_lit},{init_lit});{dispatch_expr}}})();",
    );

    realm.reset_interrupt();
    match realm.exec_script(&source) {
      Ok(_) => Ok(()),
      Err(err) => {
        if let Some(req) = realm.take_pending_navigation_request() {
          realm.reset_interrupt();
          self.pending_navigation = Some(req);
          return Ok(());
        }
        Err(Error::Other(err.to_string()))
      }
    }
  }
}
