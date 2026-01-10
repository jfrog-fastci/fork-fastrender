use crate::error::{Error, Result};
use crate::js::runtime::with_event_loop;
use crate::js::time::update_time_bindings_clock;
use crate::js::vm_error_format;
use crate::js::window_realm::{WindowRealm, WindowRealmConfig};
use crate::js::window_timers::VmJsEventLoopHooks;
use crate::js::{
  install_window_animation_frame_bindings, install_window_fetch_bindings_with_guard,
  install_window_timers_bindings, CurrentScriptStateHandle, JsExecutionOptions,
  LocationNavigationRequest, ScriptElementSpec, WindowFetchBindings, WindowFetchEnv,
};
use crate::style::media::{MediaContext, MediaType};
use crate::web::events::{Event, EventTargetId};
use std::sync::Arc;
use vm_js::SourceText;

use super::BrowserDocumentDom2;
use super::{BrowserTabHost, BrowserTabJsExecutor, SharedRenderDiagnostics};

/// `vm-js`-backed [`BrowserTabJsExecutor`] that provides a minimal `window`/`document` environment.
///
/// Navigation creates a fresh JS realm for each document (matching browser semantics). The realm
/// receives a `dom_source_id` that resolves to a stable `NonNull<dom2::Document>` pointer for the
/// lifetime of the currently committed document.
pub struct VmJsBrowserTabExecutor {
  realm: Option<WindowRealm>,
  fetch_bindings: Option<WindowFetchBindings>,
  pending_navigation: Option<LocationNavigationRequest>,
  diagnostics: Option<SharedRenderDiagnostics>,
}

impl VmJsBrowserTabExecutor {
  pub fn new() -> Self {
    Self {
      realm: None,
      fetch_bindings: None,
      pending_navigation: None,
      diagnostics: None,
    }
  }

  fn record_js_exception(diag: &SharedRenderDiagnostics, realm: &mut WindowRealm, err: vm_js::VmError) {
    let (message, stack) = vm_error_format::vm_error_to_message_and_stack(realm.heap_mut(), err);
    diag.record_js_exception(message, stack);
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
    self.fetch_bindings = None;
    self.realm = None;
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
    self.diagnostics = document.shared_diagnostics();

    // Tear down the previous realm so we don't leak rooted callbacks or global state across
    // navigations.
    self.fetch_bindings = None;
    self.realm = None;

    let dom_source_id = document.ensure_dom_source_registered();

    let url = document_url.unwrap_or("about:blank");
    let options = document.options();
    let (viewport_w, viewport_h) = options.viewport.unwrap_or((1024, 768));
    let width = viewport_w as f32;
    let height = viewport_h as f32;
    let mut media = match options.media_type {
      MediaType::Print => MediaContext::print(width, height),
      _ => MediaContext::screen(width, height),
    };
    media.media_type = options.media_type;
    if let Some(dpr) = options.device_pixel_ratio {
      media = media.with_device_pixel_ratio(dpr);
    }

    let mut config = WindowRealmConfig::new(url)
      .with_media_context(media)
      .with_dom_source_id(dom_source_id)
      .with_current_script_state(current_script.clone());

    if let Some(diag) = self.diagnostics.clone() {
      let sink: crate::js::ConsoleSink = Arc::new(move |level, heap, args| {
        let message = vm_error_format::format_console_arguments_limited(heap, args);
        diag.record_console_message(level, message);
      });
      config.console_sink = Some(sink);
    }

    let fetcher = document.fetcher();
    let mut realm = WindowRealm::new_with_js_execution_options(config, js_execution_options)
      .map_err(|err| Error::Other(err.to_string()))?;
    realm.set_cookie_fetcher(Arc::clone(&fetcher));

    // Install EventLoop-backed Web APIs (`setTimeout`, `queueMicrotask`, `requestAnimationFrame`, `fetch`).
    let fetch_bindings = {
      let (vm, realm_ref, heap) = realm.vm_realm_and_heap_mut();
      install_window_timers_bindings::<BrowserTabHost>(vm, realm_ref, heap)
        .map_err(|err| Error::Other(err.to_string()))?;
      install_window_animation_frame_bindings::<BrowserTabHost>(vm, realm_ref, heap)
        .map_err(|err| Error::Other(err.to_string()))?;
      install_window_fetch_bindings_with_guard::<BrowserTabHost>(
        vm,
        realm_ref,
        heap,
        WindowFetchEnv::for_document(fetcher, Some(url.to_string())),
      )
      .map_err(|err| Error::Other(err.to_string()))?
    };

    self.fetch_bindings = Some(fetch_bindings);
    self.realm = Some(realm);
    Ok(())
  }

  fn execute_classic_script(
    &mut self,
    script_text: &str,
    spec: &ScriptElementSpec,
    current_script: Option<crate::dom2::NodeId>,
    document: &mut BrowserDocumentDom2,
    event_loop: &mut crate::js::EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    let Some(realm) = self.realm.as_mut() else {
      return Err(Error::Other(
        "VmJsBrowserTabExecutor has no active WindowRealm; did reset_for_navigation run?".to_string(),
      ));
    };
    let diagnostics = self.diagnostics.clone();
    let clock = event_loop.clock();
    let name: Arc<str> = if let Some(url) = spec.src.as_deref() {
      Arc::from(url)
    } else if let Some(node_id) = current_script {
      Arc::from(format!("<inline script node_id={}>", node_id.index()))
    } else {
      Arc::from("<inline>")
    };
    let source = Arc::new(SourceText::new(name, Arc::from(script_text)));

    let exec_result: Result<()> = with_event_loop(event_loop, || {
      update_time_bindings_clock(realm.heap(), clock.clone()).map_err(|err| Error::Other(err.to_string()))?;
      realm.set_base_url(spec.base_url.clone());
      realm.reset_interrupt();

      let mut hooks = VmJsEventLoopHooks::<BrowserTabHost>::new(document);
      let result = realm.exec_script_source_with_host_and_hooks(document, &mut hooks, source);

      if let Some(err) = hooks.finish(realm.heap_mut()) {
        return Err(err);
      }

      match result {
        Ok(_) => Ok(()),
        Err(err) => {
          if vm_error_format::vm_error_is_js_exception(&err) {
            if let Some(diag) = diagnostics.as_ref() {
              Self::record_js_exception(diag, realm, err);
            }
            Ok(())
          } else {
            Err(vm_error_format::vm_error_to_error(realm.heap_mut(), err))
          }
        }
      }
    });

    if let Some(req) = realm.take_pending_navigation_request() {
      // Clear the interrupt flag so the realm can be reused if the embedding chooses to keep
      // executing (e.g. navigation fails and scripts continue running).
      realm.reset_interrupt();
      self.pending_navigation = Some(req);
      return Ok(());
    }

    exec_result
  }

  fn take_navigation_request(&mut self) -> Option<LocationNavigationRequest> {
    self.pending_navigation.take()
  }

  fn dispatch_lifecycle_event(
    &mut self,
    target: EventTargetId,
    event: &Event,
    document: &mut BrowserDocumentDom2,
  ) -> Result<()> {
    let Some(realm) = self.realm.as_mut() else {
      return Ok(());
    };

    let diagnostics = self.diagnostics.clone();

    let dispatch_expr = match target {
      EventTargetId::Document => "document.dispatchEvent(e);",
      EventTargetId::Window => "dispatchEvent(e);",
      EventTargetId::Node(_) | EventTargetId::Opaque(_) => return Ok(()),
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

    let clock = {
      let Some(event_loop) = crate::js::runtime::current_event_loop_mut::<BrowserTabHost>() else {
        return Err(Error::Other(
          "dispatch_lifecycle_event called without an active EventLoop".to_string(),
        ));
      };
      event_loop.clock()
    };

    update_time_bindings_clock(realm.heap(), clock).map_err(|err| Error::Other(err.to_string()))?;
    realm.reset_interrupt();

    let mut hooks = VmJsEventLoopHooks::<BrowserTabHost>::new(document);
    let source_text = Arc::new(SourceText::new("<lifecycle>", Arc::from(source)));
    let result = realm.exec_script_source_with_host_and_hooks(document, &mut hooks, source_text);
    if let Some(err) = hooks.finish(realm.heap_mut()) {
      return Err(err);
    }

    let exec_result = match result {
      Ok(_) => Ok(()),
      Err(err) => {
        if vm_error_format::vm_error_is_js_exception(&err) {
          if let Some(diag) = diagnostics.as_ref() {
            Self::record_js_exception(diag, realm, err);
          }
          Ok(())
        } else {
          Err(vm_error_format::vm_error_to_error(realm.heap_mut(), err))
        }
      }
    };

    if let Some(req) = realm.take_pending_navigation_request() {
      realm.reset_interrupt();
      self.pending_navigation = Some(req);
      return Ok(());
    }

    exec_result
  }

  fn window_realm_mut(&mut self) -> Option<&mut WindowRealm> {
    self.realm.as_mut()
  }
}
