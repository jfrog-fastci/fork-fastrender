//! JavaScript host integration utilities.
//!
//! See [`docs/html_script_processing.md`](../../docs/html_script_processing.md) for the spec-mapped
//! design of HTML `<script>` processing + parser integration (classic scripts first).
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

pub mod dom_scripts;
// Legacy DOM integration helpers (dynamic `<script>` preparation for dom2 mutations).
//
// This is not part of the canonical vm-js WindowRealm pipeline, but is still referenced by the JS
// test harness and some integration tests (e.g. streaming pipeline tests) while the newer
// vm-js/WebIDL plumbing is being rolled out.
#[path = "legacy/dom_integration.rs"]
pub mod dom_integration;
pub mod dom_host;
pub mod cookie_jar;
pub mod dom2_bindings;
pub mod clock;
pub mod document_lifecycle;
pub mod event_loop;
pub mod host_document;
pub mod html_classic_scripts;
pub mod html_scripting;
pub mod html_script_processing;
pub mod html_script_pipeline;
pub mod html_script_scheduler;
pub mod import_maps;
pub mod module_scripts;
pub mod script_encoding;
pub mod options;
pub mod orchestrator;
pub mod browser_tab;
pub mod page_load;
pub mod script_blocking_stylesheets;
pub mod script_scheduler;
pub mod promise;
pub mod script_loader_resource;
pub(crate) mod sri;
pub mod time;
pub mod url;
pub mod url_resolve;
pub mod streaming;
pub mod streaming_dom2;
pub mod streaming_pipeline;
pub mod fetch;
pub mod webidl;

// --- vm-js embedding (`src/js/vmjs/*`) ---

#[path = "vmjs/dom_platform.rs"]
pub mod dom_platform;
#[path = "vmjs/runtime.rs"]
pub mod runtime;
#[path = "vmjs/vm_limits.rs"]
pub mod vm_limits;
#[path = "vmjs/vm_error_format.rs"]
pub(crate) mod vm_error_format;
#[path = "vmjs/window.rs"]
pub mod window;
#[path = "vmjs/window_abort.rs"]
pub mod window_abort;
#[path = "vmjs/window_animation_frame.rs"]
pub mod window_animation_frame;
#[path = "vmjs/window_env.rs"]
pub mod window_env;
#[path = "vmjs/window_fetch.rs"]
pub mod window_fetch;
#[path = "vmjs/window_realm.rs"]
pub mod window_realm;
#[path = "vmjs/window_timers.rs"]
pub mod window_timers;
#[path = "vmjs/window_url.rs"]
pub mod window_url;

// --- WebIDL runtime + bindings integration (`src/js/webidl/*`) ---

#[path = "webidl/runtime_vmjs.rs"]
pub mod webidl_runtime_vmjs;
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

// --- Legacy runtimes (`src/js/legacy/*`) ---
//
// NOTE: `dom_integration` is declared above. It provides HTML "prepare the script element" helpers
// for dynamically inserted `<script>` elements, and is still referenced by some integration tests.
// Do not re-declare it here.

#[cfg(feature = "quickjs")]
#[path = "legacy/vm_host.rs"]
pub mod vm_host;
#[cfg(feature = "quickjs")]
#[path = "legacy/quickjs_dom.rs"]
pub mod quickjs_dom;

// Legacy vm-js DOM bindings (pre-WebIDL scaffolding). Kept for tests/experiments.
#[path = "legacy/vm_dom.rs"]
pub mod vm_dom;

#[allow(deprecated)]
pub use dom_scripts::extract_script_elements;
pub use dom_host::DomHost;
pub use clock::{Clock, RealClock, VirtualClock};
pub use events::{JsDomEvents, JsFunctionHandle};
pub use document_lifecycle::{DocumentLifecycle, DocumentLifecycleHost};
pub use crate::web::dom::DocumentReadyState;
pub use event_loop::{
  AnimationFrameId, EventLoop, MicrotaskCheckpointLimitedOutcome, QueueLimits, RunAnimationFrameOutcome,
  RunLimits, RunNextTaskLimitedOutcome, RunState, RunUntilIdleOutcome, RunUntilIdleStopReason, SpinOutcome,
  Task, TaskSource, TimerId,
};
pub use options::JsExecutionOptions;
pub use host_document::{DocumentHostState, HostDocumentState};
pub use orchestrator::{
  CurrentScriptHost, CurrentScriptState, CurrentScriptStateHandle, ScriptBlockExecutor,
  ScriptExecutionLog, ScriptExecutionLogEntry, ScriptOrchestrator, ScriptSourceSnapshot,
};
pub use import_maps::{
  ImportMap, ImportMapError, ImportMapParseResult, ImportMapState, ImportMapWarning, ImportMapWarningKind,
  ModuleIntegrityMap, ModuleResolutionError, ModuleSpecifierMap, ResolvedModuleSet, ScopeMap, ScopesMap,
  SpecifierAsUrlKind, SpecifierResolutionRecord,
};
pub use browser_tab::{BrowserTab, BrowserTabHost};
pub use dom_bindings::DomJsRealm;
pub use html_classic_scripts::{
  parse_and_run_classic_scripts, ClassicScriptExecutor, ClassicScriptFetcher,
  ResourceFetcherClassicScriptFetcher,
};
pub use module_scripts::ModuleGraphLoader;
pub use runtime::{JsObject, JsRuntime, NativeFunction};
pub use script_scheduler::{
  ClassicScriptScheduler, DiscoveredScript, ScriptElementEvent, ScriptEventDispatcher,
  ScriptExecutor, ScriptId, ScriptLoader, ScriptScheduler, ScriptSchedulerAction,
};
pub use html_script_scheduler::{
  HtmlDiscoveredScript, HtmlScriptId, HtmlScriptScheduler, HtmlScriptSchedulerAction, HtmlScriptWork,
};
pub use script_loader_resource::ResourceScriptLoader;
pub use page_load::{
  HtmlLoadOrchestrator, ScriptExecutor as PageLoadScriptExecutor, ScriptFetcher as PageLoadScriptFetcher,
};
pub use time::{install_time_bindings, TimeBindings, WebTime};
pub use url::{Url, UrlError, UrlLimits, UrlSearchParams};
pub use url_resolve::{resolve_url, UrlResolveError};
pub use url_bindings::{install_url_bindings, install_url_bindings_with_limits};
pub use window_animation_frame::install_window_animation_frame_bindings;
pub use window_fetch::{
  install_window_fetch_bindings, install_window_fetch_bindings_with_guard, unregister_window_fetch_env,
  WindowFetchBindings, WindowFetchEnv,
};
pub use window_timers::install_window_timers_bindings;
pub use window_url::install_window_url_bindings;
pub use window_realm::{
  ConsoleSink, LocationNavigationRequest, WindowRealm, WindowRealmConfig, WindowRealmHost,
};
pub use window::{WindowHost, WindowHostState};
#[cfg(feature = "quickjs")]
pub use vm_host::JsVmHost;
pub use script_blocking_stylesheets::ScriptBlockingStyleSheetSet;
pub use promise::{JsPromise, JsPromiseResolver, JsPromiseValue};
pub use fetch::{fetch, FetchInit, HeadersInit, JsHeaders, JsRequest, JsResponse, RequestInit, WebFetchHost};

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

// HTML defines "ASCII whitespace" as: U+0009 TAB, U+000A LF, U+000C FF, U+000D CR, U+0020 SPACE.
// Notably, ASCII whitespace does *not* include U+000B VT (vertical tab).
// Avoid `str::trim()` because it removes additional Unicode whitespace like NBSP (U+00A0), which
// HTML does not treat as ASCII whitespace and should be preserved (and therefore percent-encoded
// when used in URLs) by URL parsing / attribute processing.
fn trim_ascii_whitespace(value: &str) -> &str {
  value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
}

/// Parse an HTML "CORS settings attribute" value.
///
/// This is used for `<script crossorigin>`, `<img crossorigin>`, etc.
///
/// Returns `None` when the attribute is missing (no CORS).
///
/// Per HTML, the attribute's default/empty/invalid keywords map to the `"anonymous"` state.
pub(crate) fn parse_crossorigin_attr(value: Option<&str>) -> Option<crate::resource::CorsMode> {
  let Some(value) = value else {
    return None;
  };
  let value = trim_ascii_whitespace(value);
  if value.eq_ignore_ascii_case("use-credentials") {
    Some(crate::resource::CorsMode::UseCredentials)
  } else {
    // Empty, `anonymous`, and unknown tokens are treated as `anonymous`.
    Some(crate::resource::CorsMode::Anonymous)
  }
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
  /// [`sri::MAX_INTEGRITY_ATTRIBUTE_BYTES`] and must be treated as invalid metadata (the script must
  /// not execute).
  pub integrity: Option<String>,
  /// Parsed `referrerpolicy` attribute value.
  ///
  /// When `None`, the document's default referrer policy applies.
  pub referrer_policy: Option<crate::resource::ReferrerPolicy>,
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
#[cfg(test)]
mod tests {
  use super::{determine_script_type, determine_script_type_dom2, ScriptType};
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
}
