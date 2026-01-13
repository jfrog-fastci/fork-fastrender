//! JavaScript host integration utilities.
//!
//! See [`docs/html_script_processing.md`](../../docs/html_script_processing.md) for the spec-mapped
//! design of HTML `<script>` processing + parser integration (classic + module + import map).
//!
//! # Module layout
//!
//! `src/js/` intentionally contains multiple layers of JavaScript-related code:
//!
//! - `src/js/vmjs/*`: `vm-js` embedding code (WindowRealm/WindowHost + Window Web APIs). This is
//!   the canonical path for executing scripts against a document.
//!   - Canonical entrypoints: [`WindowHost::exec_script`],
//!     [`WindowHostState::exec_script_in_event_loop`], and
//!     [`WindowHostState::exec_script_with_name_in_event_loop`].
//!   - Canonical Promise-job adapter: [`window_timers::VmJsEventLoopHooks`] (Promise jobs must be
//!     routed onto the host [`EventLoop`] microtask queue).
//! - `src/js/webidl/*`: WebIDL runtime scaffolding + generated bindings integration (codegen output,
//!   binding glue, and MVP WebIDL-shaped DOM/event helpers).
//! - `src/js/legacy/*`: deprecated/legacy runtimes and experiments. QuickJS-backed code is gated
//!   behind the `quickjs` Cargo feature.
//!
//! # WARNING: `dom_scripts` is tooling-only
//!
//! The HTML script processing model is *parse-time* semantics: the parser pauses for
//! parser-inserted scripts, the base URL can change as `<base href>` elements are encountered, and
//! `async`/`defer` ordering depends on when scripts are discovered.
//!
//! Scanning an already-built DOM tree in "document order" cannot be spec-correct for executing
//! JavaScript. The [`dom_scripts`] module exists only for best-effort tooling (diagnostics,
//! crawling/bundling) and is deprecated for execution.
//!
//! For spec-correct execution plumbing, construct [`ScriptElementSpec`] at parse time (see
//! [`streaming`]) and feed it into the scheduler/event loop pipeline described in the doc above.

pub mod cookie_jar;
pub mod chrome_navigation_url;
pub use chrome_navigation_url as chrome_api;
pub mod web_storage;
pub mod indexed_db;
pub(crate) mod dom_internal_keys;
pub mod dom2_bindings;
pub mod dom_host;
pub mod dom_scripts;
pub mod console_sink;
pub mod clock;
pub mod document_lifecycle;
pub mod document_write;
pub mod dom_integration;
pub mod event_loop;
pub mod fetch;
pub mod host_document;
pub mod html_scripting;
pub mod html_script_pipeline;
pub mod html_script_scheduler;
pub mod import_maps;
pub mod options;
pub mod orchestrator;
pub mod page_load;
pub mod script_blocking_stylesheets;
pub mod promise;
pub mod realm_module_loader;
pub mod script_encoding;
pub(crate) mod sri;
pub mod streaming;
pub mod streaming_dom2;
pub mod time;
pub mod url;
pub mod url_resolve;
pub mod webidl;

// --- vm-js embedding (`src/js/vmjs/*`) ---

#[path = "vmjs/dom_platform.rs"]
pub mod dom_platform;
#[path = "vmjs/module_loader.rs"]
pub mod module_loader;
#[path = "vmjs/runtime.rs"]
pub mod runtime;
#[path = "vmjs/chrome_events.rs"]
pub mod chrome_events;
#[path = "vmjs/chrome_api.rs"]
pub mod vmjs_chrome_api;
#[path = "vmjs/chrome_command_queue.rs"]
pub mod chrome_command_queue;
#[path = "vmjs/vm_error_format.rs"]
pub(crate) mod vm_error_format;
#[path = "vmjs/vm_limits.rs"]
pub mod vm_limits;
#[path = "vmjs/window.rs"]
pub mod window;
#[path = "vmjs/window_abort.rs"]
pub mod window_abort;
#[path = "vmjs/window_animation_frame.rs"]
pub mod window_animation_frame;
#[path = "vmjs/window_blob.rs"]
pub mod window_blob;
#[path = "vmjs/window_object_url.rs"]
pub mod window_object_url;
#[path = "vmjs/window_crypto.rs"]
pub mod window_crypto;
#[path = "vmjs/window_css.rs"]
pub mod window_css;
#[path = "vmjs/window_file.rs"]
pub mod window_file;
#[path = "vmjs/window_file_reader.rs"]
pub mod window_file_reader;
#[path = "vmjs/window_media.rs"]
pub mod window_media;
#[path = "vmjs/window_dom_rect.rs"]
pub mod window_dom_rect;
#[path = "vmjs/window_indexed_db.rs"]
pub mod window_indexed_db;
#[path = "vmjs/window_text_encoding.rs"]
pub mod window_text_encoding;
#[path = "vmjs/window_streams.rs"]
pub mod window_streams;
#[path = "vmjs/window_env.rs"]
pub mod window_env;
#[path = "vmjs/window_fetch.rs"]
pub mod window_fetch;
#[path = "vmjs/window_form_data.rs"]
pub mod window_form_data;
#[path = "vmjs/window_intersection_observer.rs"]
pub mod window_intersection_observer;
#[path = "vmjs/window_resize_observer.rs"]
pub mod window_resize_observer;
#[path = "vmjs/window_realm.rs"]
pub mod window_realm;
#[path = "vmjs/window_timers.rs"]
pub mod window_timers;
#[path = "vmjs/window_url.rs"]
pub mod window_url;
#[path = "vmjs/window_xml_serializer.rs"]
pub mod window_xml_serializer;
#[path = "vmjs/window_broadcast_channel.rs"]
pub mod window_broadcast_channel;
#[cfg(feature = "direct_websocket")]
#[path = "vmjs/window_websocket.rs"]
pub mod window_websocket;
#[cfg(not(feature = "direct_websocket"))]
#[path = "vmjs/window_websocket_stub.rs"]
pub mod window_websocket;
#[path = "vmjs/window_worker.rs"]
pub mod window_worker;
#[path = "vmjs/window_xhr.rs"]
pub mod window_xhr;

#[path = "vmjs/window_structured_clone.rs"]
pub mod window_structured_clone;

#[path = "vmjs/window_message_channel.rs"]
pub mod window_message_channel;
#[cfg(test)]
#[path = "vmjs/regression_tests.rs"]
mod vmjs_regression_tests;

#[cfg(all(test, feature = "quickjs"))]
#[path = "legacy/quickjs_fetch.rs"]
mod quickjs_fetch;

#[cfg(all(test, feature = "quickjs"))]
#[path = "legacy/quickjs_url.rs"]
mod quickjs_url;

#[cfg(all(test, feature = "quickjs"))]
#[path = "legacy/quickjs/fetch.rs"]
mod quickjs_fetch_bindings;

// --- WebIDL runtime + bindings integration (`src/js/webidl/*`) ---

#[path = "webidl/bindings/mod.rs"]
pub mod bindings;
#[path = "webidl/dom_bindings/mod.rs"]
pub mod dom_bindings;
#[path = "webidl/events.rs"]
pub mod events;
#[path = "webidl/events_bindings.rs"]
pub mod events_bindings;
#[path = "webidl/url_bindings.rs"]
pub mod url_bindings;
#[path = "webidl/runtime_vmjs.rs"]
pub mod webidl_runtime_vmjs;

// --- Legacy runtimes (`src/js/legacy/*`) ---
//
// NOTE: `dom_integration` is declared above to keep the stable `crate::js::dom_integration` path.
// It provides HTML "prepare the script element" helpers for dynamically inserted `<script>`
// elements. It is still referenced by integration tests and DOM-mutation plumbing. Do not
// re-declare it here.
#[cfg(feature = "quickjs")]
#[path = "legacy/quickjs_dom.rs"]
pub mod quickjs_dom;
#[cfg(feature = "quickjs")]
#[path = "legacy/vm_host.rs"]
pub mod vm_host;

// Legacy vm-js DOM bindings (pre-WebIDL scaffolding). Kept for tests/experiments.
#[path = "legacy/dom_bindings_context.rs"]
pub mod dom_bindings_context;
#[path = "legacy/vm_dom.rs"]
pub mod vm_dom;

// Legacy `vm-js` embeddings kept for tests and experimental script execution paths.
#[path = "legacy/ecma_embed.rs"]
pub mod ecma_embed;
#[path = "legacy/ecma_vm_runtime.rs"]
pub mod ecma_vm_runtime;
pub use crate::web::dom::DocumentReadyState;
pub use clock::{Clock, RealClock, VirtualClock};
pub use document_lifecycle::{DocumentLifecycle, DocumentLifecycleHost, LoadBlockerKind};
pub use document_write::{with_document_write_state, DocumentWriteLimitError, DocumentWriteState};
pub use dom_bindings::DomJsRealm;
pub use dom_host::{DomHost, DomHostVmJs};
#[allow(deprecated)]
pub use dom_scripts::extract_script_elements;
pub use event_loop::{
  AnimationFrameId, EventLoop, ExternalTaskQueueHandle, IdleCallbackId,
  MicrotaskCheckpointLimitedOutcome, QueueLimits, RunAnimationFrameOutcome, RunLimits,
  RunNextTaskLimitedOutcome, RunState, RunUntilIdleOutcome, RunUntilIdleStopReason, SpinOutcome,
  Task, TaskSource, TimerId,
};
pub use events::{JsDomEvents, JsFunctionHandle};
pub use fetch::{
  fetch, FetchInit, HeadersInit, JsHeaders, JsRequest, JsResponse, RequestInit, WebFetchHost,
};
pub use host_document::{DocumentHostState, HostDocumentState};
pub use html_script_scheduler::{
  HtmlDiscoveredScript, HtmlScriptId, HtmlScriptScheduler, HtmlScriptSchedulerAction,
  HtmlScriptWork,
};
pub use import_maps::{
  ImportMap, ImportMapError, ImportMapLimits, ImportMapParseResult, ImportMapState,
  ImportMapWarning, ImportMapWarningKind, ModuleIntegrityMap, ModuleResolutionError,
  ModuleSpecifierMap, ResolvedModuleSet, ResolvedModuleSetIndex, ScopeMap, ScopesMap,
  SpecifierAsUrlKind, SpecifierResolutionRecord,
};
pub use module_loader::VmJsModuleLoader;
pub use options::{JsExecutionOptions, ParseBudget};
pub use orchestrator::{
  CurrentScriptHost, CurrentScriptState, CurrentScriptStateHandle, ScriptBlockExecutor,
  ScriptExecutionLog, ScriptExecutionLogEntry, ScriptOrchestrator, ScriptSourceSnapshot,
};
pub use page_load::{
  HtmlLoadOrchestrator, ScriptExecutor as PageLoadScriptExecutor,
  ScriptFetcher as PageLoadScriptFetcher,
};
pub use promise::{JsPromise, JsPromiseResolver, JsPromiseValue};
pub use realm_module_loader::{ModuleKey, ModuleLoader, ModuleLoaderHandle};
pub use runtime::{JsObject, JsRuntime, NativeFunction};
pub use script_blocking_stylesheets::ScriptBlockingStyleSheetSet;
pub use time::{install_time_bindings, TimeBindings, WebTime};
pub use url::{Url, UrlError, UrlLimits, UrlSearchParams};
pub use url_bindings::{install_url_bindings, install_url_bindings_with_limits};
pub use url_resolve::{resolve_url, UrlResolveError};
pub use vm_dom::{install_dom_bindings, install_dom_bindings_with_limits};
#[cfg(feature = "quickjs")]
pub use vm_host::JsVmHost;
pub use window::{WindowHost, WindowHostState};
pub use window_animation_frame::install_window_animation_frame_bindings;
pub use window_blob::install_window_blob_bindings;
pub use window_file::install_window_file_bindings;
pub use window_file_reader::install_window_file_reader_bindings;
pub use window_fetch::{
  install_window_fetch_bindings, install_window_fetch_bindings_with_guard,
  unregister_window_fetch_env, WindowFetchBindings, WindowFetchEnv,
};
pub use window_form_data::install_window_form_data_bindings;
pub use window_realm::{
  ConsoleSink, LocationNavigationRequest, WindowRealm, WindowRealmConfig, WindowRealmHost,
};
pub use window_timers::install_window_timers_bindings;
pub use window_url::install_window_url_bindings;
pub use window_xhr::{
  install_window_xhr_bindings, install_window_xhr_bindings_with_guard, unregister_window_xhr_env,
  WindowXhrBindings, WindowXhrEnv,
};
pub use window_websocket::{
  install_window_websocket_bindings, install_window_websocket_bindings_with_guard,
  install_window_websocket_ipc_bindings, install_window_websocket_ipc_bindings_with_guard,
  unregister_window_websocket_env, WindowWebSocketBindings, WindowWebSocketEnv,
  WindowWebSocketIpcEnv, WebSocketIpcCommand, WebSocketIpcEvent,
};

/// The script processing mode for a `<script>` element.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptType {
  /// A classic script (default when `type` is missing/empty or a JS MIME type).
  Classic,
  /// An ECMAScript module script (`type="module"`).
  Module,
  /// An import map (`type="importmap"`).
  ImportMap,
  /// An unrecognized script type (not executable by the HTML script processing model).
  Unknown,
}

// ---------------------------------------------------------------------------
// Test shims
// ---------------------------------------------------------------------------
//
// `src/js/vmjs/*` is the canonical location for the vm-js embedding implementation, but the public
// module layout historically flattened those modules under `crate::js::*`.
//
// Some integration guidance (and agent tasks) refer to vm-js tests under a `js::vmjs::*` module
// path. Provide a small `cfg(test)` shim so `cargo test --lib js::vmjs::window` and
// `cargo test --lib js::vmjs::module_loader` match at least the core vm-js integration tests.
#[cfg(test)]
mod vmjs {
  pub mod window {
    use crate::dom2;
    use crate::error::Result;
    use crate::js::window::WindowHost;
    use crate::resource::{FetchedResource, ResourceFetcher};
    use selectors::context::QuirksMode;
    use std::sync::Arc;
    use vm_js::{PropertyKey, Value};

    #[derive(Debug, Default)]
    struct NoFetchResourceFetcher;

    impl ResourceFetcher for NoFetchResourceFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        Err(crate::Error::Other(format!(
          "NoFetchResourceFetcher does not support fetch: {url}"
        )))
      }
    }

    fn make_host(dom: dom2::Document, document_url: impl Into<String>) -> Result<WindowHost> {
      WindowHost::new_with_fetcher(dom, document_url, Arc::new(NoFetchResourceFetcher))
    }

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

    #[test]
    fn window_onload_runs_via_event_handler_attribute() -> Result<()> {
      let dom = dom2::Document::new(QuirksMode::NoQuirks);
      let mut host = make_host(dom, "https://example.invalid/")?;

      host.exec_script(
        "globalThis.__called = false;\n\
         globalThis.onload = function (e) {\n\
           globalThis.__called = (\n\
             this === globalThis &&\n\
             e && e.type === 'load' &&\n\
             e.target === globalThis &&\n\
             e.currentTarget === globalThis &&\n\
             e.eventPhase === 2\n\
           );\n\
         };\n\
         globalThis.dispatchEvent(new Event('load'));\n",
      )?;

      assert!(matches!(get_global_prop(&mut host, "__called"), Value::Bool(true)));
      Ok(())
    }

    #[test]
    fn document_onvisibilitychange_runs_via_event_handler_attribute() -> Result<()> {
      let dom = dom2::Document::new(QuirksMode::NoQuirks);
      let mut host = make_host(dom, "https://example.invalid/")?;

      host.exec_script(
        "globalThis.__called = false;\n\
         document.onvisibilitychange = function (e) {\n\
           globalThis.__called = (\n\
             this === document &&\n\
             e && e.type === 'visibilitychange' &&\n\
             e.target === document &&\n\
             e.currentTarget === document &&\n\
             e.eventPhase === 2\n\
           );\n\
         };\n\
         document.dispatchEvent(new Event('visibilitychange'));\n",
      )?;

      assert!(matches!(get_global_prop(&mut host, "__called"), Value::Bool(true)));
      Ok(())
    }

    #[test]
    fn window_onerror_uses_special_signature_and_return_true_cancels() -> Result<()> {
      let dom = dom2::Document::new(QuirksMode::NoQuirks);
      let mut host = make_host(dom, "https://example.invalid/")?;

      host.exec_script(
        "globalThis.__argc = 0;\n\
         globalThis.__a0 = '';\n\
         globalThis.__a1 = '';\n\
         globalThis.__a2 = 0;\n\
         globalThis.__a3 = 0;\n\
         globalThis.__a4_code = 0;\n\
         globalThis.__dispatch_result = null;\n\
         globalThis.__default_prevented = false;\n\
         globalThis.onerror = function (message, filename, lineno, colno, error) {\n\
           globalThis.__argc = arguments.length;\n\
           globalThis.__a0 = String(message);\n\
           globalThis.__a1 = String(filename);\n\
           globalThis.__a2 = lineno;\n\
           globalThis.__a3 = colno;\n\
           globalThis.__a4_code = error && error.code;\n\
           return true;\n\
         };\n\
         var e = new Event('error', { cancelable: true });\n\
         e.message = 'boom';\n\
         e.filename = 'file.js';\n\
         e.lineno = 10;\n\
         e.colno = 20;\n\
         e.error = { code: 42 };\n\
         globalThis.__dispatch_result = globalThis.dispatchEvent(e);\n\
         globalThis.__default_prevented = e.defaultPrevented;\n",
      )?;

      assert!(matches!(
        get_global_prop(&mut host, "__argc"),
        Value::Number(n) if n == 5.0
      ));
      assert_eq!(get_global_prop_utf8(&mut host, "__a0").as_deref(), Some("boom"));
      assert_eq!(
        get_global_prop_utf8(&mut host, "__a1").as_deref(),
        Some("file.js")
      );
      assert!(matches!(
        get_global_prop(&mut host, "__a2"),
        Value::Number(n) if n == 10.0
      ));
      assert!(matches!(
        get_global_prop(&mut host, "__a3"),
        Value::Number(n) if n == 20.0
      ));
      assert!(matches!(
        get_global_prop(&mut host, "__a4_code"),
        Value::Number(n) if n == 42.0
      ));
      assert!(matches!(
        get_global_prop(&mut host, "__dispatch_result"),
        Value::Bool(false)
      ));
      assert!(matches!(
        get_global_prop(&mut host, "__default_prevented"),
        Value::Bool(true)
      ));
      Ok(())
    }
  }

  pub mod module_loader {
    use crate::dom2;
    use crate::error::{Error, Result};
    use crate::js::import_maps::{create_import_map_parse_result, register_import_map, ImportMapState};
    use crate::js::{EventLoop, VmJsModuleLoader, WindowHostState};
    use crate::resource::{FetchDestination, FetchRequest, FetchedResource, ResourceFetcher};
    use selectors::context::QuirksMode;
    use std::collections::HashMap;
    use std::sync::Arc;
    use url::Url;
    use vm_js::{Budget, PropertyKey, Value};

    struct MapFetcher {
      map: HashMap<String, FetchedResource>,
    }

    impl MapFetcher {
      fn new(map: HashMap<String, FetchedResource>) -> Self {
        Self { map }
      }
    }

    impl ResourceFetcher for MapFetcher {
      fn fetch(&self, url: &str) -> crate::Result<FetchedResource> {
        self.fetch_with_request(FetchRequest::new(url, FetchDestination::Other))
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> crate::Result<FetchedResource> {
        self
          .map
          .get(req.url)
          .cloned()
          .ok_or_else(|| Error::Other(format!("no fixture for url {url}", url = req.url)))
      }
    }

    fn get_global_prop(host: &mut WindowHostState, name: &str) -> Value {
      let window = host.window_mut();
      let (_vm, realm, heap) = window.vm_realm_and_heap_mut();
      let mut scope = heap.scope();
      let global = realm.global_object();
      scope.push_root(Value::Object(global)).expect("root global");
      let key_s = scope.alloc_string(name).expect("alloc name");
      scope.push_root(Value::String(key_s)).expect("root name");
      let key = PropertyKey::from_string(key_s);
      scope
        .heap()
        .object_get_own_data_property_value(global, &key)
        .expect("get prop")
        .unwrap_or(Value::Undefined)
    }

    fn get_global_prop_utf8(host: &mut WindowHostState, name: &str) -> Option<String> {
      let value = get_global_prop(host, name);
      let window = host.window_mut();
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

    #[test]
    fn module_import_meta_resolve_resolves_relative_specifier() -> Result<()> {
      let entry_url = "https://example.com/dir/entry.js";
      let dep_url = "https://example.com/dir/dep.js";
      let document_url = "https://example.com/index.html";
      let mapped_url = "https://example.com/mapped.js";

      let mut map = HashMap::<String, FetchedResource>::new();
      map.insert(
        entry_url.to_string(),
        FetchedResource::new(
          r#"
            globalThis.resolved = import.meta.resolve("./dep.js");
            globalThis.mapped = import.meta.resolve("foo");
          "#
          .as_bytes()
          .to_vec(),
          Some("application/javascript".to_string()),
        ),
      );

      let fetcher = Arc::new(MapFetcher::new(map));
      let dom = dom2::Document::new(QuirksMode::NoQuirks);
      let mut host = WindowHostState::new_with_fetcher(dom, document_url, fetcher.clone())?;
      let mut event_loop = EventLoop::<WindowHostState>::new();

      host
        .window_mut()
        .vm_mut()
        .set_budget(Budget::unlimited(100));

      let mut import_map_state = ImportMapState::default();
      let base_url = Url::parse(document_url)
        .map_err(|err| Error::Other(format!("invalid test base URL: {err}")))?;
      let parse_result = create_import_map_parse_result(r#"{"imports":{"foo":"./mapped.js"}}"#, &base_url);
      register_import_map(&mut import_map_state, parse_result)
        .map_err(|err| Error::Other(err.to_string()))?;

      let mut loader = VmJsModuleLoader::new(fetcher, document_url);
      loader.evaluate_module_url_with_import_maps(
        &mut host,
        &mut event_loop,
        &mut import_map_state,
        entry_url,
      )?;

      assert_eq!(
        get_global_prop_utf8(&mut host, "resolved").as_deref(),
        Some(dep_url)
      );
      assert_eq!(
        get_global_prop_utf8(&mut host, "mapped").as_deref(),
        Some(mapped_url)
      );
      loader.teardown(&mut host)?;
      Ok(())
    }
  }

  pub mod window_blob {
    use crate::js::window_realm::{WindowRealm, WindowRealmConfig};
    use vm_js::{Value, VmError};

    #[test]
    fn blob_ctor_treats_detached_buffers_as_empty() -> Result<(), VmError> {
      let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/"))?;

      let result = realm.exec_script(
        r#"
(() => {
  // Detach the buffer via the structured clone transfer list.
  if (typeof structuredClone !== "function") {
    return "skip";
  }

  const buf = new ArrayBuffer(4);
  const view = new Uint8Array(buf);
  structuredClone(buf, { transfer: [buf] }); // Detaches `buf`.
  if (buf.byteLength !== 0) {
    return "transfer did not detach";
  }

  try {
    const b = new Blob([buf]);
    if (b.size !== 0) {
      return "ArrayBuffer size was " + b.size;
    }
  } catch (e) {
    return "ArrayBuffer threw " + (e && e.name ? e.name : String(e));
  }

  try {
    const b = new Blob([view]);
    if (b.size !== 0) {
      return "Uint8Array size was " + b.size;
    }
  } catch (e) {
    return "Uint8Array threw " + (e && e.name ? e.name : String(e));
  }

  return "ok";
})()
"#,
      )?;

      let Value::String(s) = result else {
        return Err(VmError::InvariantViolation(
          "expected string result from Blob detached-buffer shim test",
        ));
      };

      let value = realm.heap().get_string(s)?.to_utf8_lossy();
      if value != "skip" {
        assert_ne!(value, "transfer did not detach");
        assert_eq!(value, "ok");
      }

      realm.teardown();
      Ok(())
    }
  }
}

// HTML defines "ASCII whitespace" as: U+0009 TAB, U+000A LF, U+000C FF, U+000D CR, U+0020 SPACE.
// Notably, ASCII whitespace does *not* include U+000B VT (vertical tab).
// Avoid `str::trim()` because it removes additional Unicode whitespace like NBSP (U+00A0), which
// HTML does not treat as ASCII whitespace and should be preserved (and therefore percent-encoded
// when used in URLs) by URL parsing / attribute processing.
fn trim_ascii_whitespace(value: &str) -> &str {
  value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
}

pub(crate) const MAX_SCRIPT_ATTRIBUTE_VALUE_BYTES: usize = 8 * 1024;

/// Parse an HTML "CORS settings attribute" value.
///
/// This is used for `<script crossorigin>`, `<img crossorigin>`, etc.
///
/// Recognizes `anonymous` and `use-credentials` case-insensitively after trimming ASCII whitespace.
/// Returns `None` when the attribute is missing or has an invalid value.
pub(crate) fn parse_crossorigin_attr(value: Option<&str>) -> Option<crate::resource::CorsMode> {
  value.and_then(parse_cors_settings_attribute)
}

pub(crate) fn parse_cors_settings_attribute(value: &str) -> Option<crate::resource::CorsMode> {
  let value = trim_ascii_whitespace(value);
  if value.is_empty() || value.eq_ignore_ascii_case("anonymous") {
    return Some(crate::resource::CorsMode::Anonymous);
  }
  if value.eq_ignore_ascii_case("use-credentials") {
    return Some(crate::resource::CorsMode::UseCredentials);
  }
  None
}

pub(crate) fn parse_referrer_policy_attribute(
  value: &str,
) -> Option<crate::resource::ReferrerPolicy> {
  crate::resource::ReferrerPolicy::parse_value_list(value)
}

pub(crate) fn take_bounded_script_attribute_value(value: &str) -> Option<String> {
  if value.len() > MAX_SCRIPT_ATTRIBUTE_VALUE_BYTES {
    return None;
  }
  Some(value.to_string())
}

/// A parsed `<script>` element, normalized into a scheduler-friendly record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptElementSpec {
  /// The document base URL used to resolve relative script URLs, if known.
  pub base_url: Option<String>,
  /// The resolved `src` URL, if present and resolvable.
  pub src: Option<String>,
  /// Whether the `src` attribute is present on the element (even if empty or not resolvable).
  ///
  /// HTML treats the presence of `src` as suppressing inline script execution: when present, the
  /// element's child text content must be ignored even if the `src` value is empty/invalid.
  pub src_attr_present: bool,
  /// The concatenated inline script text from child text nodes.
  pub inline_text: String,
  /// Whether the `async` boolean attribute is present.
  pub async_attr: bool,
  /// Whether the script element's internal "force async" flag is set.
  ///
  /// HTML uses this per-element flag to make dynamically created scripts async-by-default (even
  /// without an `async` attribute). Assigning `script.async = false` clears this flag so the script
  /// can run in insertion order.
  ///
  /// Expected defaults:
  /// - DOM-created `<script>` elements (e.g. `document.createElement("script")`): `true`
  /// - Parser-created scripts (full document parsing): `false`
  /// - Fragment-parser-created scripts (e.g. `innerHTML` parsing): `false`
  pub force_async: bool,
  /// Whether the `defer` boolean attribute is present.
  pub defer_attr: bool,
  /// Whether the `nomodule` boolean attribute is present.
  ///
  /// When module scripts are supported, classic scripts with `nomodule` must be skipped.
  pub nomodule_attr: bool,
  /// Parsed `crossorigin` attribute state (CORS settings attribute).
  ///
  /// When `None`, classic scripts are fetched in `no-cors` mode (no CORS enforcement).
  /// When `Some`, scripts are fetched in `cors` mode and CORS response headers are enforced.
  pub crossorigin: Option<crate::resource::CorsMode>,
  /// Whether the `integrity` attribute is present on the element.
  pub integrity_attr_present: bool,
  /// Raw `integrity` attribute value (Subresource Integrity) when within a bounded size limit.
  ///
  /// When `integrity_attr_present` is true but this field is `None`, the attribute exceeded
  /// `sri::MAX_INTEGRITY_ATTRIBUTE_BYTES` and must be treated as invalid metadata (the script must
  /// not execute).
  pub integrity: Option<String>,
  /// Parsed `referrerpolicy` attribute value.
  ///
  /// When `None`, the document's default referrer policy applies.
  pub referrer_policy: Option<crate::resource::ReferrerPolicy>,
  /// Raw `fetchpriority` attribute value (bounded).
  pub fetch_priority: Option<String>,
  /// Whether the script was inserted by the HTML parser.
  ///
  /// This affects scheduling (`defer` only applies to parser-inserted scripts; parser-inserted
  /// scripts can block parsing).
  ///
  /// When building specs during HTML parsing, this should be `true`. Best-effort DOM scans may set
  /// this to `true` as a default, but dynamically inserted scripts should use `false`.
  pub parser_inserted: bool,
  /// `dom2` node ID for the `<script>` element, if known.
  ///
  /// This is used for `Document.currentScript` bookkeeping during execution. For post-parse DOM
  /// scans (`dom_scripts`) and legacy DOM-based parser paths that don't have a `dom2::NodeId`
  /// available, this will be `None`.
  pub node_id: Option<crate::dom2::NodeId>,
  /// The script type (classic/module/importmap/unknown) derived from element attributes.
  pub script_type: ScriptType,
}

impl ScriptElementSpec {
  /// Whether this script should be suppressed due to the `nomodule` attribute.
  #[inline]
  pub fn is_suppressed_by_nomodule(&self, options: &JsExecutionOptions) -> bool {
    options.supports_module_scripts && self.script_type == ScriptType::Classic && self.nomodule_attr
  }

  /// Whether the script should be treated as "async" for scheduling purposes.
  ///
  /// This mirrors the platform behavior where the async IDL attribute reflects both:
  /// - the presence of the `async` content attribute, and
  /// - the internal "force async" flag used for dynamic scripts.
  pub fn is_effectively_async(&self) -> bool {
    self.async_attr || self.force_async
  }
}

pub(crate) fn clamp_integrity_attribute(raw: Option<&str>) -> (bool, Option<String>) {
  let Some(raw) = raw else {
    return (false, None);
  };
  if raw.len() > sri::MAX_INTEGRITY_ATTRIBUTE_BYTES {
    return (true, None);
  }
  (true, Some(raw.to_string()))
}

/// Determine the script type for a `<script>` element based on `type`/`language` attributes.
///
/// This follows the HTML Standard script preparation rules for computing the script block type
/// string and then mapping it to `classic`/`module`/`importmap`/unknown.
fn determine_script_type_from_attrs(
  type_attr: Option<&str>,
  language_attr: Option<&str>,
) -> ScriptType {
  // Compute the "script block's type string" per the HTML Standard:
  // - `type=""` => defaults to `text/javascript`
  // - no `type` + `language=""` => defaults to `text/javascript`
  // - no `type` + no `language` => defaults to `text/javascript`
  // - otherwise:
  //   - `type=<value>` => ASCII whitespace stripped
  //   - `language=<value>` => `text/<value>` (no trimming)
  //
  // Notably, whitespace-only values do *not* count as empty-string defaults.
  let type_string = if let Some(value) = type_attr {
    if value.is_empty() {
      "text/javascript".to_string()
    } else {
      trim_ascii_whitespace(value).to_string()
    }
  } else if let Some(value) = language_attr {
    if value.is_empty() {
      "text/javascript".to_string()
    } else {
      format!("text/{}", value)
    }
  } else {
    "text/javascript".to_string()
  };

  // `module` / `importmap` must match exactly (after trimming performed above).
  if type_string.eq_ignore_ascii_case("module") {
    return ScriptType::Module;
  }
  if type_string.eq_ignore_ascii_case("importmap") {
    return ScriptType::ImportMap;
  }

  // JavaScript MIME type essence match (WHATWG MIME Sniffing + HTML).
  let mime_essence = type_string
    .split_once(';')
    .map(|(essence, _)| trim_ascii_whitespace(essence))
    .unwrap_or(type_string.as_str())
    .trim_matches(|c: char| c.is_ascii_whitespace());

  const JS_MIME_TYPE_ESSENCES: [&str; 16] = [
    "application/ecmascript",
    "application/javascript",
    "application/x-ecmascript",
    "application/x-javascript",
    "text/ecmascript",
    "text/javascript",
    "text/javascript1.0",
    "text/javascript1.1",
    "text/javascript1.2",
    "text/javascript1.3",
    "text/javascript1.4",
    "text/javascript1.5",
    "text/jscript",
    "text/livescript",
    "text/x-ecmascript",
    "text/x-javascript",
  ];
  if JS_MIME_TYPE_ESSENCES
    .iter()
    .any(|ty| mime_essence.eq_ignore_ascii_case(ty))
  {
    return ScriptType::Classic;
  }

  ScriptType::Unknown
}

pub fn determine_script_type(script: &crate::dom::DomNode) -> ScriptType {
  let Some(tag_name) = script.tag_name() else {
    return ScriptType::Unknown;
  };
  if !tag_name.eq_ignore_ascii_case("script") {
    return ScriptType::Unknown;
  }

  determine_script_type_from_attrs(
    script.get_attribute_ref("type"),
    script.get_attribute_ref("language"),
  )
}

pub fn determine_script_type_dom2(
  doc: &crate::dom2::Document,
  node: crate::dom2::NodeId,
) -> ScriptType {
  use crate::dom2::NodeKind;

  let NodeKind::Element { tag_name, .. } = &doc.node(node).kind else {
    return ScriptType::Unknown;
  };
  if !tag_name.eq_ignore_ascii_case("script") {
    return ScriptType::Unknown;
  }

  determine_script_type_from_attrs(
    doc.get_attribute(node, "type").ok().flatten(),
    doc.get_attribute(node, "language").ok().flatten(),
  )
}

pub(crate) fn prepare_script_element_dom2(
  doc: &mut crate::dom2::Document,
  script: crate::dom2::NodeId,
  spec: &ScriptElementSpec,
) -> bool {
  if doc.node(script).script_already_started {
    return false;
  }

  doc.reset_parser_inserted_script_internal_slots(script);

  // Whether the caller should hand this script off to the scheduler / execution pipeline.
  //
  // NOTE: This is intentionally broader than "will execute":
  // - External scripts with `src` present but empty/invalid must still be processed so the scheduler
  //   can queue an `error` event task (HTML: `src` presence suppresses inline fallback).
  // - Module / import map scripts are not executed everywhere yet, but still participate in error
  //   event dispatch for invalid `src`.
  if matches!(spec.script_type, ScriptType::Unknown) {
    return false;
  }

  // `integrity` attribute clamping: if present but too large, the metadata is invalid and the
  // script must not execute.
  if spec.integrity_attr_present && spec.integrity.is_none() {
    // For external scripts, still allow the scheduler to see the element so it can dispatch `error`
    // events and suppress inline fallback (HTML: `src` presence blocks inline execution).
    return spec.src_attr_present;
  }

  // HTML "script-processing-empty": if there is no `src` attribute and the source text is empty,
  // "prepare a script" returns early *after* clearing parser-document/force-async above.
  // https://html.spec.whatwg.org/multipage/scripting.html#script-processing-empty
  spec.src_attr_present || !spec.inline_text.is_empty()
}
#[cfg(test)]
mod tests {
  use super::{
    determine_script_type, determine_script_type_dom2, prepare_script_element_dom2, ScriptElementSpec,
    ScriptType,
  };
  use crate::dom::{DomNode, DomNodeType};
  use crate::dom2::Document as Dom2Document;
  use selectors::context::QuirksMode;

  fn script(attrs: &[(&str, &str)]) -> DomNode {
    DomNode {
      node_type: DomNodeType::Element {
        tag_name: "script".to_string(),
        namespace: String::new(),
        attributes: attrs
          .iter()
          .map(|(k, v)| (k.to_string(), v.to_string()))
          .collect(),
      },
      children: Vec::new(),
    }
  }

  fn dom2_script(attrs: &[(&str, &str)]) -> (Dom2Document, crate::dom2::NodeId) {
    let mut doc = Dom2Document::new(QuirksMode::NoQuirks);
    let script = doc.create_element("script", "");
    for (name, value) in attrs {
      doc
        .set_attribute(script, name, value)
        .expect("set_attribute should succeed");
    }
    doc
      .append_child(doc.root(), script)
      .expect("append_child should succeed");
    (doc, script)
  }

  fn classic_script_spec(node_id: crate::dom2::NodeId, async_attr: bool, inline_text: &str) -> ScriptElementSpec {
    ScriptElementSpec {
      base_url: None,
      src: None,
      src_attr_present: false,
      inline_text: inline_text.to_string(),
      async_attr,
      force_async: false,
      defer_attr: false,
      nomodule_attr: false,
      crossorigin: None,
      integrity_attr_present: false,
      integrity: None,
      referrer_policy: None,
      fetch_priority: None,
      parser_inserted: true,
      node_id: Some(node_id),
      script_type: ScriptType::Classic,
    }
  }

  #[test]
  fn defaults_to_classic_without_type_or_language() {
    let node = script(&[]);
    assert_eq!(determine_script_type(&node), ScriptType::Classic);
  }

  #[test]
  fn type_empty_string_defaults_to_classic() {
    let node = script(&[("type", "")]);
    assert_eq!(determine_script_type(&node), ScriptType::Classic);
  }

  #[test]
  fn type_whitespace_does_not_default_and_is_unknown() {
    let node = script(&[("type", "  ")]);
    assert_eq!(determine_script_type(&node), ScriptType::Unknown);
  }

  #[test]
  fn language_empty_string_defaults_to_classic_when_no_type() {
    let node = script(&[("language", "")]);
    assert_eq!(determine_script_type(&node), ScriptType::Classic);
  }

  #[test]
  fn language_ecmascript_maps_to_classic() {
    let node = script(&[("language", "ecmascript")]);
    assert_eq!(determine_script_type(&node), ScriptType::Classic);
  }

  #[test]
  fn legacy_javascript_mime_types_map_to_classic() {
    for ty in [
      "text/javascript1.5",
      "text/jscript",
      "text/livescript",
      "text/x-javascript",
      "application/x-javascript",
    ] {
      let node = script(&[("type", ty)]);
      assert_eq!(
        determine_script_type(&node),
        ScriptType::Classic,
        "type={ty}"
      );
    }
  }

  #[test]
  fn module_and_importmap_require_exact_match() {
    let node = script(&[("type", "module")]);
    assert_eq!(determine_script_type(&node), ScriptType::Module);
    let node = script(&[("type", "module; charset=utf-8")]);
    assert_eq!(determine_script_type(&node), ScriptType::Unknown);

    let node = script(&[("type", "importmap")]);
    assert_eq!(determine_script_type(&node), ScriptType::ImportMap);
    let node = script(&[("type", "importmap; foo=bar")]);
    assert_eq!(determine_script_type(&node), ScriptType::Unknown);
  }

  #[test]
  fn type_trimming_is_ascii_only() {
    // HTML trims ASCII whitespace, not all Unicode whitespace.
    let nbsp = "\u{00A0}";
    let module_trailing = format!("module{nbsp}");
    let module_wrapped = format!("{nbsp}module{nbsp}");
    let js_trailing = format!("text/javascript{nbsp}");

    let node = script(&[("type", module_trailing.as_str())]);
    assert_eq!(determine_script_type(&node), ScriptType::Unknown);
    let node = script(&[("type", module_wrapped.as_str())]);
    assert_eq!(determine_script_type(&node), ScriptType::Unknown);

    let node = script(&[("type", js_trailing.as_str())]);
    assert_eq!(determine_script_type(&node), ScriptType::Unknown);
  }

  #[test]
  fn type_trimming_excludes_vertical_tab() {
    // HTML's ASCII whitespace definition does not include U+000B VT.
    let vt = "\u{000B}";

    let module_wrapped = format!("{vt}module{vt}");
    let node = script(&[("type", module_wrapped.as_str())]);
    assert_eq!(determine_script_type(&node), ScriptType::Unknown);

    let js_wrapped = format!("{vt}text/javascript{vt}");
    let node = script(&[("type", js_wrapped.as_str())]);
    assert_eq!(determine_script_type(&node), ScriptType::Unknown);
  }

  #[test]
  fn dom2_script_type_matches_legacy_for_common_cases() {
    for attrs in [
      vec![],
      vec![("type", "")],
      vec![("type", "  ")],
      vec![("type", "module")],
      vec![("type", "importmap")],
      vec![("language", "")],
      vec![("language", "ecmascript")],
      vec![("TyPe", "text/javascript")],
    ] {
      let legacy = script(&attrs);
      let (doc, script_id) = dom2_script(&attrs);
      assert_eq!(
        determine_script_type_dom2(&doc, script_id),
        determine_script_type(&legacy),
        "attrs={attrs:?}"
      );
    }
  }

  #[test]
  fn prepare_script_clears_parser_document_and_sets_force_async_when_async_attr_absent() {
    let (mut doc, script) = dom2_script(&[]);
    // Simulate a parser-inserted script element (as produced by html5ever parsing).
    doc.node_mut(script).script_parser_document = true;
    doc.node_mut(script).script_force_async = false;

    let spec = classic_script_spec(script, /* async_attr */ false, "console.log(1)");
    assert!(prepare_script_element_dom2(&mut doc, script, &spec));

    let node = doc.node(script);
    assert!(!node.script_parser_document);
    assert!(node.script_force_async);
  }

  #[test]
  fn prepare_script_clears_parser_document_but_does_not_force_async_when_async_attr_present() {
    let (mut doc, script) = dom2_script(&[("async", "")]);
    // Simulate a parser-inserted script element (as produced by html5ever parsing).
    doc.node_mut(script).script_parser_document = true;
    doc.node_mut(script).script_force_async = false;

    let spec = classic_script_spec(script, /* async_attr */ true, "console.log(1)");
    assert!(prepare_script_element_dom2(&mut doc, script, &spec));

    let node = doc.node(script);
    assert!(!node.script_parser_document);
    assert!(
      !node.script_force_async,
      "force-async must remain cleared when the async attribute is present"
    );
  }
}
