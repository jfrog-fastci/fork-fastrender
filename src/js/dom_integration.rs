use crate::dom::HTML_NAMESPACE;
use crate::dom2::{Document, NodeId, NodeKind};
use crate::error::Result;
use crate::html::base_url_tracker::resolve_script_src_at_parse_time;

use super::{
  determine_script_type_dom2, ClassicScriptScheduler, DomHost, EventLoop, ScriptElementEvent,
  ScriptElementSpec, ScriptEventDispatcher, ScriptExecutor, ScriptLoader, ScriptType, TaskSource,
};

/// Run a minimal subset of the HTML "prepare the script element" algorithm for dynamically inserted
/// `<script>` elements.
/// 
/// This is intended to be called by DOM mutation bindings after a `<script>` element becomes
/// connected to the document (e.g. after `Node.appendChild`).
///
/// Supported subset:
/// - Classic and module scripts (`type`/`language` mapped via [`determine_script_type_dom2`]).
/// - Dynamic classic external scripts (`src` present and non-empty) are treated as async by default
///   when the element's internal "force async" flag is set (mirroring HTML's default for
///   `document.createElement("script")`).
/// - Dynamic module scripts:
///   - `async` present: execute ASAP once ready.
///   - otherwise: execute in insertion order as soon as possible once ready.
/// - Inline scripts are queued as tasks (rather than executing synchronously inside the DOM
///   mutation call). The event loop's post-task microtask checkpoint naturally applies.
///
/// HTML uses a per-script-element "already started" flag to ensure each `<script>` executes at most
/// once. FastRender stores this on the live `dom2` node (`Node::script_already_started`).
pub fn prepare_dynamic_script_on_insertion<Host>(
  host: &mut Host,
  scheduler: &mut ClassicScriptScheduler<Host>,
  event_loop: &mut EventLoop<Host>,
  inserted_node: NodeId,
) -> Result<()>
where
  Host: DomHost + ScriptLoader + ScriptExecutor + ScriptEventDispatcher,
{
  prepare_dynamic_script(host, scheduler, event_loop, inserted_node)
}

/// Prepare a dynamically-connected `<script>` element after its `src` attribute changes.
///
/// This mirrors the HTML `src` attribute change steps for `<script>` elements.
pub fn prepare_dynamic_script_on_src_attribute_change<Host>(
  host: &mut Host,
  scheduler: &mut ClassicScriptScheduler<Host>,
  event_loop: &mut EventLoop<Host>,
  script: NodeId,
) -> Result<()>
where
  Host: DomHost + ScriptLoader + ScriptExecutor + ScriptEventDispatcher,
{
  prepare_dynamic_script(host, scheduler, event_loop, script)
}

/// Prepare a dynamically-connected `<script>` element after its children change.
///
/// This mirrors the HTML "children changed steps" for `<script>` elements.
pub fn prepare_dynamic_script_on_children_changed<Host>(
  host: &mut Host,
  scheduler: &mut ClassicScriptScheduler<Host>,
  event_loop: &mut EventLoop<Host>,
  script: NodeId,
) -> Result<()>
where
  Host: DomHost + ScriptLoader + ScriptExecutor + ScriptEventDispatcher,
{
  prepare_dynamic_script(host, scheduler, event_loop, script)
}

fn prepare_dynamic_script<Host>(
  host: &mut Host,
  scheduler: &mut ClassicScriptScheduler<Host>,
  event_loop: &mut EventLoop<Host>,
  script: NodeId,
) -> Result<()>
where
  Host: DomHost + ScriptLoader + ScriptExecutor + ScriptEventDispatcher,
{
  let supports_module_scripts = scheduler.options().supports_module_scripts;
  let spec = host.mutate_dom(|dom| {
    if !is_html_script_element(dom, script) {
      return (None, false);
    }

    // HTML element post-connection steps: parser-inserted scripts are prepared by the parser, not by
    // DOM insertion.
    if dom.node(script).script_parser_document {
      return (None, false);
    }

    // HTML: scripts inside inert `<template>` contents are treated as disconnected and must not
    // execute.
    if !dom.is_connected_for_scripting(script) {
      return (None, false);
    }

    // HTML: do nothing when "already started" is true.
    if dom.node(script).script_already_started {
      return (None, false);
    }

    let spec = build_non_parser_inserted_script_spec(dom, script);

    // HTML: if there is no `src` attribute and the inline text is empty, do nothing. Importantly,
    // this must *not* set the "already started" flag so later `src`/text mutations can trigger
    // preparation.
    if !spec.src_attr_present && spec.inline_text.is_empty() {
      return (None, false);
    }

    // Only mark "already started" for script types this helper can execute. Avoid marking
    // unsupported script types so later mutations can still produce a runnable classic script.
    let should_mark_started = match spec.script_type {
      ScriptType::Classic => true,
      ScriptType::Module => supports_module_scripts,
      ScriptType::ImportMap | ScriptType::Unknown => false,
    };
    if should_mark_started && dom.set_script_already_started(script, true).is_err() {
      return (None, false);
    }
    (Some(spec), false)
  });

  let Some(spec) = spec else {
    return Ok(());
  };

  // HTML: in "prepare the script element", if the element has a `nomodule` content attribute and
  // its computed script type is `classic` (and the user agent supports module scripts), the
  // algorithm returns early (the script is not fetched/executed).
  if spec.is_suppressed_by_nomodule(&scheduler.options()) {
    return Ok(());
  }

  match spec.script_type {
    ScriptType::Classic => {
      // External scripts are handled directly by the scheduler so they start loading immediately.
      // Inline scripts are queued as tasks to keep DOM mutation calls non-reentrant.
      if spec.src_attr_present {
        scheduler.handle_script(host, event_loop, spec)?;
        return Ok(());
      }

      scheduler
        .options()
        .check_script_source(&spec.inline_text, "source=inline")?;
      event_loop.queue_task(TaskSource::Script, move |host, event_loop| {
        host.execute_classic_script(&spec.inline_text, &spec, event_loop)
      })?;
      Ok(())
    }
    ScriptType::Module => {
      // When module scripts are unsupported, HTML ignores them. However, the `src` attribute being
      // present but empty/invalid is still an error and must queue an `error` event task (matching
      // the classic script behavior and avoiding inline fallback execution).
      if !supports_module_scripts {
        if spec.src_attr_present && spec.src.is_none() {
          event_loop.queue_task(TaskSource::DOMManipulation, move |host, _event_loop| {
            host.dispatch_script_event(ScriptElementEvent::Error, &spec)
          })?;
        }
        return Ok(());
      }
      // Module scripts must never execute synchronously inside the DOM mutation call stack; route
      // through the scheduler (which always queues module execution as tasks).
      scheduler.handle_script(host, event_loop, spec)?;
      Ok(())
    }
    ScriptType::ImportMap => {
      // Import maps are only meaningful when module scripts are supported. When modules are
      // disabled, treat `type="importmap"` like an unknown script type and ignore it.
      if !supports_module_scripts {
        return Ok(());
      }

      // Import maps are not executed by this dynamic insertion helper yet, but `src` is invalid and
      // must queue an `error` event task (and suppress inline processing).
      if spec.src_attr_present {
        event_loop.queue_task(TaskSource::DOMManipulation, move |host, _event_loop| {
          host.dispatch_script_event(ScriptElementEvent::Error, &spec)
        })?;
      }
      Ok(())
    }
    ScriptType::Unknown => Ok(()),
  }
}

/// Prepare any dynamic `<script>` elements within a newly-inserted subtree.
///
/// When DOM operations insert a subtree (e.g. appending a `<div>` that already contains a
/// `<script>` child), the HTML spec requires that the insertion steps for each `<script>` element
/// in the subtree run once it becomes connected.
///
/// This helper scans `inserted_root` and all of its descendants (in tree order) and runs
/// [`prepare_dynamic_script_on_insertion`] for each HTML `<script>` element found.
///
/// Note: DOM insertion of a `DocumentFragment` inserts its children rather than the fragment node
/// itself. Callers should pass each inserted child root (captured before insertion) instead of the
/// fragment node.
pub fn prepare_dynamic_scripts_on_subtree_insertion<Host>(
  host: &mut Host,
  scheduler: &mut ClassicScriptScheduler<Host>,
  event_loop: &mut EventLoop<Host>,
  inserted_root: NodeId,
) -> Result<()>
where
  Host: DomHost + ScriptLoader + ScriptExecutor + ScriptEventDispatcher,
{
  let script_nodes = host.with_dom(|dom| {
    let mut out = Vec::new();
    collect_html_script_elements(dom, inserted_root, &mut out);
    out
  });

  for node in script_nodes {
    prepare_dynamic_script_on_insertion(host, scheduler, event_loop, node)?;
  }
  Ok(())
}

fn collect_html_script_elements(dom: &Document, node: NodeId, out: &mut Vec<NodeId>) {
  if is_html_script_element(dom, node) {
    out.push(node);
  }
  for &child in &dom.node(node).children {
    collect_html_script_elements(dom, child, out);
  }
}

fn build_non_parser_inserted_script_spec(dom: &Document, script: NodeId) -> ScriptElementSpec {
  let async_attr = dom.has_attribute(script, "async").unwrap_or(false);
  let force_async = dom.node(script).script_force_async;
  let defer_attr = dom.has_attribute(script, "defer").unwrap_or(false);
  let nomodule_attr = dom.has_attribute(script, "nomodule").unwrap_or(false);
  let referrer_policy = dom
    .get_attribute(script, "referrerpolicy")
    .ok()
    .flatten()
    .and_then(crate::resource::ReferrerPolicy::from_attribute);

  let raw_src = dom.get_attribute(script, "src").ok().flatten();
  let src_attr_present = raw_src.is_some();
  let src = raw_src.and_then(|raw| resolve_script_src_at_parse_time(None, raw));

  let (integrity_attr_present, integrity) =
    super::clamp_integrity_attribute(dom.get_attribute(script, "integrity").ok().flatten());
  let crossorigin = super::parse_crossorigin_attr(dom.get_attribute(script, "crossorigin").ok().flatten());

  let mut inline_text = String::new();
  for &child in &dom.node(script).children {
    if let NodeKind::Text { content } = &dom.node(child).kind {
      inline_text.push_str(content);
    }
  }

  ScriptElementSpec {
    base_url: None,
    src,
    src_attr_present,
    inline_text,
    async_attr,
    force_async,
    defer_attr,
    nomodule_attr,
    crossorigin,
    integrity_attr_present,
    integrity,
    referrer_policy,
    parser_inserted: false,
    node_id: Some(script),
    script_type: determine_script_type_dom2(dom, script),
  }
}

fn is_html_script_element(dom: &Document, node: NodeId) -> bool {
  let kind = &dom.node(node).kind;
  let NodeKind::Element {
    tag_name,
    namespace,
    ..
  } = kind
  else {
    return false;
  };

  if !tag_name.eq_ignore_ascii_case("script") {
    return false;
  }

  // `dom2` normalizes the HTML namespace to the empty string, but accept the full namespace URI
  // too.
  namespace.is_empty() || namespace == HTML_NAMESPACE
}

#[cfg(test)]
mod tests {
  use super::{build_non_parser_inserted_script_spec, prepare_dynamic_script_on_insertion};
  use crate::dom2::Document;
  use crate::error::Result;
  use crate::js::{
    ClassicScriptScheduler, DomHost, EventLoop, JsExecutionOptions, RunLimits, ScriptElementEvent,
    ScriptElementSpec, ScriptEventDispatcher, ScriptExecutor, ScriptLoader,
  };
  use crate::resource::{FetchCredentialsMode, FetchDestination};
  use selectors::context::QuirksMode;

  struct TestHost {
    dom: Document,
    started_loads: Vec<String>,
    executed: Vec<String>,
    events: Vec<ScriptElementEvent>,
    next_handle: u32,
  }

  impl TestHost {
    fn new(dom: Document) -> Self {
      Self {
        dom,
        started_loads: Vec::new(),
        executed: Vec::new(),
        events: Vec::new(),
        next_handle: 1,
      }
    }
  }

  impl DomHost for TestHost {
    fn with_dom<R, F>(&self, f: F) -> R
    where
      F: FnOnce(&Document) -> R,
    {
      f(&self.dom)
    }

    fn mutate_dom<R, F>(&mut self, f: F) -> R
    where
      F: FnOnce(&mut Document) -> (R, bool),
    {
      let (result, _changed) = f(&mut self.dom);
      result
    }
  }

  impl ScriptLoader for TestHost {
    type Handle = u32;

    fn load_blocking(
      &mut self,
      url: &str,
      _destination: FetchDestination,
      _credentials_mode: FetchCredentialsMode,
    ) -> Result<String> {
      self.started_loads.push(url.to_string());
      Ok(String::new())
    }

    fn start_load(
      &mut self,
      url: &str,
      _destination: FetchDestination,
      _credentials_mode: FetchCredentialsMode,
    ) -> Result<Self::Handle> {
      self.started_loads.push(url.to_string());
      let handle = self.next_handle;
      self.next_handle = self.next_handle.wrapping_add(1);
      Ok(handle)
    }

    fn poll_complete(&mut self) -> Result<Option<(Self::Handle, String)>> {
      Ok(None)
    }
  }

  impl ScriptExecutor for TestHost {
    fn execute_classic_script(
      &mut self,
      script_text: &str,
      _spec: &ScriptElementSpec,
      _event_loop: &mut EventLoop<Self>,
    ) -> Result<()> {
      self.executed.push(script_text.to_string());
      Ok(())
    }

    fn execute_module_script(
      &mut self,
      script_text: &str,
      _spec: &ScriptElementSpec,
      _event_loop: &mut EventLoop<Self>,
    ) -> Result<()> {
      self.executed.push(script_text.to_string());
      Ok(())
    }
  }

  impl ScriptEventDispatcher for TestHost {
    fn dispatch_script_event(
      &mut self,
      event: ScriptElementEvent,
      _spec: &ScriptElementSpec,
    ) -> Result<()> {
      self.events.push(event);
      Ok(())
    }
  }

  #[test]
  fn dynamic_script_spec_force_async_defaults_true_for_dom_created_scripts() {
    let mut dom = Document::new(QuirksMode::NoQuirks);
    let script = dom.create_element("script", "");
    dom
      .append_child(dom.root(), script)
      .expect("append_child should succeed");
    assert!(
      dom.node(script).script_force_async,
      "expected dom2 create_element('script') to default script_force_async=true"
    );

    let spec = build_non_parser_inserted_script_spec(&dom, script);
    assert!(
      spec.force_async,
      "expected ScriptElementSpec.force_async=true for DOM-created dynamic scripts"
    );
  }

  #[test]
  fn dynamic_script_spec_force_async_defaults_false_for_inner_html_parsed_scripts() {
    let mut dom = Document::new(QuirksMode::NoQuirks);
    let container = dom.create_element("div", "");
    dom
      .append_child(dom.root(), container)
      .expect("append_child should succeed");

    dom
      .set_inner_html(container, "<script id=s>console.log(1)</script>")
      .expect("set_inner_html should succeed");
    let script = dom
      .get_element_by_id("s")
      .expect("expected script to be present after set_inner_html");

    assert!(
      dom.node(script).script_already_started,
      "innerHTML/outerHTML parsing must mark scripts as already started"
    );
    assert!(
      !dom.node(script).script_force_async,
      "innerHTML/outerHTML parsing must set script_force_async=false"
    );

    let spec = build_non_parser_inserted_script_spec(&dom, script);
    assert!(
      !spec.force_async,
      "expected ScriptElementSpec.force_async=false for fragment-parser-created scripts"
    );
  }

  #[test]
  fn dynamic_script_javascript_src_does_not_start_fetch() -> Result<()> {
    let mut dom = Document::new(QuirksMode::NoQuirks);
    let script = dom.create_element("script", "");
    dom
      .append_child(dom.root(), script)
      .expect("append_child should succeed");
    dom
      .set_attribute(script, "src", "javascript:alert(1)")
      .expect("set_attribute should succeed");
    let text = dom.create_text("INLINE");
    dom.append_child(script, text).expect("append_child");

    let mut host = TestHost::new(dom);
    let mut scheduler = ClassicScriptScheduler::<TestHost>::new();
    let mut event_loop = EventLoop::<TestHost>::new();

    prepare_dynamic_script_on_insertion(&mut host, &mut scheduler, &mut event_loop, script)?;
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    assert!(
      host.started_loads.is_empty(),
      "expected no loader fetch for javascript: src"
    );
    assert!(
      host.executed.is_empty(),
      "expected no inline execution when src attribute is present"
    );
    assert_eq!(
      host.events,
      vec![ScriptElementEvent::Error],
      "expected an error event task for javascript: src"
    );
    Ok(())
  }

  #[test]
  fn dynamic_script_javascript_src_trims_ascii_whitespace_before_scheme_check() -> Result<()> {
    let mut dom = Document::new(QuirksMode::NoQuirks);
    let script = dom.create_element("script", "");
    dom
      .append_child(dom.root(), script)
      .expect("append_child should succeed");
    dom
      .set_attribute(script, "src", " \tjavascript:alert(1)\n")
      .expect("set_attribute should succeed");
    let text = dom.create_text("INLINE");
    dom.append_child(script, text).expect("append_child");

    let mut host = TestHost::new(dom);
    let mut scheduler = ClassicScriptScheduler::<TestHost>::new();
    let mut event_loop = EventLoop::<TestHost>::new();

    prepare_dynamic_script_on_insertion(&mut host, &mut scheduler, &mut event_loop, script)?;
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    assert!(
      host.started_loads.is_empty(),
      "expected no loader fetch for javascript: src (even with ASCII whitespace)"
    );
    assert!(
      host.executed.is_empty(),
      "expected no inline execution when src attribute is present"
    );
    Ok(())
  }

  #[test]
  fn dynamic_script_src_trims_ascii_whitespace() -> Result<()> {
    // `prepare_dynamic_script_on_insertion` is for scripts created/inserted via DOM APIs. Build a
    // DOM-created `<script>` element so `Node::script_parser_document` is false.
    let mut dom = Document::new(QuirksMode::NoQuirks);
    let script = dom.create_element("script", "");
    dom
      .set_attribute(script, "src", "\thttps://example.com/a.js\n")
      .expect("set_attribute should succeed");
    dom
      .append_child(dom.root(), script)
      .expect("append_child should succeed");

    let mut host = TestHost::new(dom);
    let mut scheduler = ClassicScriptScheduler::<TestHost>::new();
    let mut event_loop = EventLoop::<TestHost>::new();

    prepare_dynamic_script_on_insertion(&mut host, &mut scheduler, &mut event_loop, script)?;

    assert_eq!(host.started_loads, vec!["https://example.com/a.js".to_string()]);
    Ok(())
  }

  #[test]
  fn dynamic_script_relative_src_is_preserved_without_base_url() -> Result<()> {
    // Like the test above, use a DOM-created `<script>` element to exercise the dynamic insertion
    // helper.
    let mut dom = Document::new(QuirksMode::NoQuirks);
    let script = dom.create_element("script", "");
    dom
      .set_attribute(script, "src", "a.js")
      .expect("set_attribute should succeed");
    dom
      .append_child(dom.root(), script)
      .expect("append_child should succeed");

    let mut host = TestHost::new(dom);
    let mut scheduler = ClassicScriptScheduler::<TestHost>::new();
    let mut event_loop = EventLoop::<TestHost>::new();

    prepare_dynamic_script_on_insertion(&mut host, &mut scheduler, &mut event_loop, script)?;

    assert_eq!(host.started_loads, vec!["a.js".to_string()]);
    Ok(())
  }

  #[test]
  fn dynamic_script_empty_src_suppresses_inline_fallback() -> Result<()> {
    let mut dom = Document::new(QuirksMode::NoQuirks);
    let script = dom.create_element("script", "");
    dom
      .append_child(dom.root(), script)
      .expect("append_child should succeed");
    dom
      .set_attribute(script, "src", "")
      .expect("set_attribute should succeed");
    let text = dom.create_text("INLINE");
    dom.append_child(script, text).expect("append_child");

    let mut host = TestHost::new(dom);
    let mut scheduler = ClassicScriptScheduler::<TestHost>::new();
    let mut event_loop = EventLoop::<TestHost>::new();

    prepare_dynamic_script_on_insertion(&mut host, &mut scheduler, &mut event_loop, script)?;
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    assert!(
      host.started_loads.is_empty(),
      "expected no fetch for empty src"
    );
    assert!(
      host.executed.is_empty(),
      "expected no inline execution when src attribute is present"
    );
    assert_eq!(
      host.events,
      vec![ScriptElementEvent::Error],
      "expected an error event task for empty src"
    );
    Ok(())
  }

  #[test]
  fn dynamic_module_script_empty_src_queues_error_event() -> Result<()> {
    let mut dom = Document::new(QuirksMode::NoQuirks);
    let script = dom.create_element("script", "");
    dom.append_child(dom.root(), script).expect("append_child");
    dom
      .set_attribute(script, "type", "module")
      .expect("set_attribute");
    dom.set_attribute(script, "src", "").expect("set_attribute");
    let text = dom.create_text("INLINE");
    dom.append_child(script, text).expect("append_child");

    let mut host = TestHost::new(dom);
    let mut scheduler = ClassicScriptScheduler::<TestHost>::new();
    let mut event_loop = EventLoop::<TestHost>::new();

    prepare_dynamic_script_on_insertion(&mut host, &mut scheduler, &mut event_loop, script)?;
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    assert!(
      host.started_loads.is_empty(),
      "expected no fetch to be started for unsupported module script dynamic insertion"
    );
    assert!(
      host.executed.is_empty(),
      "expected no inline execution when src attribute is present"
    );
    assert_eq!(
      host.events,
      vec![ScriptElementEvent::Error],
      "expected module scripts with empty src to queue an error event task"
    );
    Ok(())
  }

  #[test]
  fn dynamic_importmap_script_with_src_queues_error_event() -> Result<()> {
    let mut dom = Document::new(QuirksMode::NoQuirks);
    let script = dom.create_element("script", "");
    dom.append_child(dom.root(), script).expect("append_child");
    dom
      .set_attribute(script, "type", "importmap")
      .expect("set_attribute");
    dom
      .set_attribute(script, "src", "https://example.invalid/map.json")
      .expect("set_attribute");
    let text = dom.create_text("{\"imports\":{}}");
    dom.append_child(script, text).expect("append_child");

    let mut host = TestHost::new(dom);
    let mut scheduler = ClassicScriptScheduler::<TestHost>::new();
    let mut event_loop = EventLoop::<TestHost>::new();

    prepare_dynamic_script_on_insertion(&mut host, &mut scheduler, &mut event_loop, script)?;
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    assert!(
      host.started_loads.is_empty(),
      "expected no fetch to be started for importmap scripts with src"
    );
    assert!(
      host.executed.is_empty(),
      "expected no execution for importmap scripts"
    );
    assert_eq!(
      host.events,
      vec![ScriptElementEvent::Error],
      "expected importmap scripts with src to queue an error event task"
    );
    Ok(())
  }

  #[test]
  fn dynamic_script_crossorigin_use_credentials_trims_ascii_whitespace() -> Result<()> {
    let mut dom = Document::new(QuirksMode::NoQuirks);
    let script = dom.create_element("script", "");
    dom
      .append_child(dom.root(), script)
      .expect("append_child should succeed");
    dom
      .set_attribute(script, "crossorigin", " \tuse-credentials\t ")
      .expect("set_attribute should succeed");

    let spec = build_non_parser_inserted_script_spec(&dom, script);
    assert_eq!(
      spec.crossorigin,
      Some(crate::resource::CorsMode::UseCredentials)
    );
    Ok(())
  }

  #[test]
  fn dynamic_script_crossorigin_vertical_tab_is_not_ascii_whitespace() -> Result<()> {
    let mut dom = Document::new(QuirksMode::NoQuirks);
    let script = dom.create_element("script", "");
    dom
      .append_child(dom.root(), script)
      .expect("append_child should succeed");
    dom
      .set_attribute(script, "crossorigin", "\u{000B}use-credentials\u{000B}")
      .expect("set_attribute should succeed");

    let spec = build_non_parser_inserted_script_spec(&dom, script);
    assert_eq!(spec.crossorigin, Some(crate::resource::CorsMode::Anonymous));
    Ok(())
  }

  #[test]
  fn dynamic_script_crossorigin_nbsp_is_not_ascii_whitespace() -> Result<()> {
    let mut dom = Document::new(QuirksMode::NoQuirks);
    let script = dom.create_element("script", "");
    dom
      .append_child(dom.root(), script)
      .expect("append_child should succeed");
    dom
      .set_attribute(script, "crossorigin", "\u{00A0}use-credentials\u{00A0}")
      .expect("set_attribute should succeed");

    let spec = build_non_parser_inserted_script_spec(&dom, script);
    assert_eq!(spec.crossorigin, Some(crate::resource::CorsMode::Anonymous));
    Ok(())
  }

  #[test]
  fn dynamic_script_spec_exposes_force_async_internal_slot() {
    let mut doc = Document::new(QuirksMode::NoQuirks);
    let script = doc.create_element("script", "");
    doc.append_child(doc.root(), script).expect("append_child");

    // HTML: for dynamically created scripts, `force async` defaults to true.
    let spec = build_non_parser_inserted_script_spec(&doc, script);
    assert!(!spec.parser_inserted);
    assert!(spec.force_async);

    // If host code toggles the internal slot (e.g. `script.async = false`), the built spec should
    // reflect that value.
    doc.node_mut(script).script_force_async = false;
    let spec2 = build_non_parser_inserted_script_spec(&doc, script);
    assert!(!spec2.force_async);
  }

  #[test]
  fn dynamic_inline_nomodule_script_executes_when_module_scripts_not_supported() -> Result<()> {
    // This test exercises dynamic insertion behavior, so construct the element via DOM APIs rather
    // than HTML parsing (which marks scripts as parser-inserted and prepares them elsewhere).
    let mut dom = Document::new(QuirksMode::NoQuirks);
    let script = dom.create_element("script", "");
    dom
      .set_bool_attribute(script, "nomodule", true)
      .expect("set_bool_attribute");
    let text = dom.create_text("RUN");
    dom.append_child(script, text).expect("append_child");
    dom.append_child(dom.root(), script).expect("append_child");

    let mut host = TestHost::new(dom);
    let mut options = JsExecutionOptions::default();
    options.supports_module_scripts = false;
    let mut scheduler = ClassicScriptScheduler::<TestHost>::with_options(options);
    let mut event_loop = EventLoop::<TestHost>::new();

    prepare_dynamic_script_on_insertion(&mut host, &mut scheduler, &mut event_loop, script)?;
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    assert!(host.started_loads.is_empty());
    assert_eq!(
      host.executed,
      vec!["RUN".to_string()],
      "nomodule does not suppress scripts when module scripts are not supported"
    );
    Ok(())
  }

  #[test]
  fn dynamic_external_nomodule_script_starts_fetch_when_module_scripts_not_supported() -> Result<()> {
    // This test exercises dynamic insertion behavior, so construct the element via DOM APIs rather
    // than HTML parsing (which marks scripts as parser-inserted and prepares them elsewhere).
    let mut dom = Document::new(QuirksMode::NoQuirks);
    let script = dom.create_element("script", "");
    dom
      .set_bool_attribute(script, "nomodule", true)
      .expect("set_bool_attribute");
    dom.set_attribute(script, "src", "a.js").expect("set_attribute");
    dom.append_child(dom.root(), script).expect("append_child");

    let mut host = TestHost::new(dom);
    let mut options = JsExecutionOptions::default();
    options.supports_module_scripts = false;
    let mut scheduler = ClassicScriptScheduler::<TestHost>::with_options(options);
    let mut event_loop = EventLoop::<TestHost>::new();

    prepare_dynamic_script_on_insertion(&mut host, &mut scheduler, &mut event_loop, script)?;
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    assert_eq!(
      host.started_loads,
      vec!["a.js".to_string()],
      "nomodule does not suppress external scripts when module scripts are not supported"
    );
    assert!(host.executed.is_empty());
    Ok(())
  }
}

#[cfg(test)]
mod nomodule_tests {
  use super::*;
  use crate::js::{JsExecutionOptions, RunLimits};
  use crate::resource::FetchCredentialsMode;
  use selectors::context::QuirksMode;

  struct Host {
    dom: Document,
    started_loads: Vec<String>,
    executed: Vec<String>,
  }

  impl Default for Host {
    fn default() -> Self {
      Self {
        dom: Document::new(QuirksMode::NoQuirks),
        started_loads: Vec::new(),
        executed: Vec::new(),
      }
    }
  }

  impl DomHost for Host {
    fn with_dom<R, F>(&self, f: F) -> R
    where
      F: FnOnce(&Document) -> R,
    {
      f(&self.dom)
    }

    fn mutate_dom<R, F>(&mut self, f: F) -> R
    where
      F: FnOnce(&mut Document) -> (R, bool),
    {
      let (result, _changed) = f(&mut self.dom);
      result
    }
  }

  impl ScriptLoader for Host {
    type Handle = usize;

    fn load_blocking(
      &mut self,
      url: &str,
      _destination: crate::resource::FetchDestination,
      _credentials_mode: FetchCredentialsMode,
    ) -> Result<String> {
      self.started_loads.push(url.to_string());
      Ok(String::new())
    }

    fn start_load(
      &mut self,
      url: &str,
      _destination: crate::resource::FetchDestination,
      _credentials_mode: FetchCredentialsMode,
    ) -> Result<Self::Handle> {
      self.started_loads.push(url.to_string());
      Ok(self.started_loads.len())
    }

    fn poll_complete(&mut self) -> Result<Option<(Self::Handle, String)>> {
      Ok(None)
    }
  }

  impl ScriptExecutor for Host {
    fn execute_classic_script(
      &mut self,
      script_text: &str,
      _spec: &ScriptElementSpec,
      _event_loop: &mut EventLoop<Self>,
    ) -> Result<()> {
      self.executed.push(script_text.to_string());
      Ok(())
    }

    fn execute_module_script(
      &mut self,
      script_text: &str,
      _spec: &ScriptElementSpec,
      _event_loop: &mut EventLoop<Self>,
    ) -> Result<()> {
      self.executed.push(script_text.to_string());
      Ok(())
    }
  }

  impl ScriptEventDispatcher for Host {
    fn dispatch_script_event(
      &mut self,
      _event: ScriptElementEvent,
      _spec: &ScriptElementSpec,
    ) -> Result<()> {
      Ok(())
    }
  }

  fn build_doc_with_script(
    attrs: &[(&str, &str)],
    bool_attrs: &[&str],
    text: Option<&str>,
  ) -> (Document, NodeId) {
    let mut doc = Document::new(QuirksMode::NoQuirks);
    let html = doc.create_element("html", "");
    doc.append_child(doc.root(), html).expect("append_child");
    let script = doc.create_element("script", "");
    for (k, v) in attrs {
      doc.set_attribute(script, k, v).expect("set_attribute");
    }
    for name in bool_attrs {
      doc
        .set_bool_attribute(script, name, true)
        .expect("set_bool_attribute");
    }
    if let Some(text) = text {
      let t = doc.create_text(text);
      doc.append_child(script, t).expect("append_child");
    }
    doc.append_child(html, script).expect("append_child");
    (doc, script)
  }

  #[test]
  fn dynamic_inline_nomodule_script_is_suppressed_when_module_scripts_supported() -> Result<()> {
    let (dom, script) = build_doc_with_script(&[], &["nomodule"], Some("RUN"));
    let mut host = Host {
      dom,
      ..Host::default()
    };
    let mut options = JsExecutionOptions::default();
    options.supports_module_scripts = true;
    let mut scheduler = ClassicScriptScheduler::<Host>::with_options(options);
    let mut event_loop = EventLoop::<Host>::new();

    prepare_dynamic_script_on_insertion(&mut host, &mut scheduler, &mut event_loop, script)?;
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    assert!(host.executed.is_empty(), "expected nomodule script not to execute");
    Ok(())
  }

  #[test]
  fn dynamic_inline_script_still_executes_when_module_scripts_supported() -> Result<()> {
    let (dom, script) = build_doc_with_script(&[], &[], Some("RUN"));
    let mut host = Host {
      dom,
      ..Host::default()
    };
    let mut options = JsExecutionOptions::default();
    options.supports_module_scripts = true;
    let mut scheduler = ClassicScriptScheduler::<Host>::with_options(options);
    let mut event_loop = EventLoop::<Host>::new();

    prepare_dynamic_script_on_insertion(&mut host, &mut scheduler, &mut event_loop, script)?;
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;

    assert_eq!(host.executed, vec!["RUN".to_string()]);
    Ok(())
  }

  #[test]
  fn dynamic_external_nomodule_script_does_not_start_fetch_when_module_scripts_supported() -> Result<()> {
    let (dom, script) = build_doc_with_script(&[("src", "a.js")], &["nomodule"], None);
    let mut host = Host {
      dom,
      ..Host::default()
    };
    let mut options = JsExecutionOptions::default();
    options.supports_module_scripts = true;
    let mut scheduler = ClassicScriptScheduler::<Host>::with_options(options);
    let mut event_loop = EventLoop::<Host>::new();

    prepare_dynamic_script_on_insertion(&mut host, &mut scheduler, &mut event_loop, script)?;
    assert!(
      host.started_loads.is_empty(),
      "expected no fetch to be started for nomodule external scripts"
    );
    Ok(())
  }
}

#[cfg(test)]
mod dynamic_mutation_tests {
  use super::{
    prepare_dynamic_script_on_children_changed, prepare_dynamic_script_on_insertion,
    prepare_dynamic_script_on_src_attribute_change, ClassicScriptScheduler, DomHost, EventLoop,
    ScriptElementEvent, ScriptElementSpec, ScriptEventDispatcher, ScriptExecutor, ScriptLoader,
  };
  use crate::dom2::Document;
  use crate::error::Result;
  use crate::js::RunLimits;
  use crate::resource::{FetchCredentialsMode, FetchDestination};
  use std::collections::{HashMap, VecDeque};

  struct Host {
    dom: Document,
    next_handle: usize,
    handle_by_url: HashMap<String, usize>,
    completion_queue: VecDeque<(usize, String)>,
    started_urls: Vec<String>,
    executed: Vec<(String, bool)>,
  }

  impl Host {
    fn new(dom: Document) -> Self {
      Self {
        dom,
        next_handle: 0,
        handle_by_url: HashMap::new(),
        completion_queue: VecDeque::new(),
        started_urls: Vec::new(),
        executed: Vec::new(),
      }
    }

    fn complete_url(&mut self, url: &str, source: &str) {
      let Some(handle) = self.handle_by_url.get(url).copied() else {
        panic!("attempted to complete unknown url={url}");
      };
      self
        .completion_queue
        .push_back((handle, source.to_string()));
    }
  }

  impl DomHost for Host {
    fn with_dom<R, F>(&self, f: F) -> R
    where
      F: FnOnce(&Document) -> R,
    {
      f(&self.dom)
    }

    fn mutate_dom<R, F>(&mut self, f: F) -> R
    where
      F: FnOnce(&mut Document) -> (R, bool),
    {
      let (result, _changed) = f(&mut self.dom);
      result
    }
  }

  impl ScriptLoader for Host {
    type Handle = usize;

    fn load_blocking(
      &mut self,
      url: &str,
      _destination: FetchDestination,
      _credentials_mode: FetchCredentialsMode,
    ) -> Result<String> {
      Err(crate::error::Error::Other(format!(
        "unexpected blocking script load for url={url}"
      )))
    }

    fn start_load(
      &mut self,
      url: &str,
      _destination: FetchDestination,
      _credentials_mode: FetchCredentialsMode,
    ) -> Result<Self::Handle> {
      let handle = self.next_handle;
      self.next_handle += 1;
      self.started_urls.push(url.to_string());
      self.handle_by_url.insert(url.to_string(), handle);
      Ok(handle)
    }

    fn poll_complete(&mut self) -> Result<Option<(Self::Handle, String)>> {
      Ok(self.completion_queue.pop_front())
    }
  }

  impl ScriptExecutor for Host {
    fn execute_classic_script(
      &mut self,
      script_text: &str,
      spec: &ScriptElementSpec,
      _event_loop: &mut EventLoop<Self>,
    ) -> Result<()> {
      self.executed.push((script_text.to_string(), spec.force_async));
      Ok(())
    }

    fn execute_module_script(
      &mut self,
      script_text: &str,
      spec: &ScriptElementSpec,
      _event_loop: &mut EventLoop<Self>,
    ) -> Result<()> {
      self.executed.push((script_text.to_string(), spec.force_async));
      Ok(())
    }
  }

  impl ScriptEventDispatcher for Host {
    fn dispatch_script_event(
      &mut self,
      _event: ScriptElementEvent,
      _spec: &ScriptElementSpec,
    ) -> Result<()> {
      Ok(())
    }
  }

  #[test]
  fn insertion_of_empty_script_does_not_mark_started() -> Result<()> {
    let mut host = Host::new(Document::new(selectors::context::QuirksMode::NoQuirks));
    let script = host.dom.create_element("script", "");
    host.dom.append_child(host.dom.root(), script).unwrap();

    let mut scheduler = ClassicScriptScheduler::<Host>::new();
    let mut event_loop = EventLoop::<Host>::new();

    prepare_dynamic_script_on_insertion(&mut host, &mut scheduler, &mut event_loop, script)?;

    assert!(
      !host.dom.node(script).script_already_started,
      "empty dynamic scripts must not be marked already started"
    );
    assert!(host.executed.is_empty(), "empty scripts should not execute");
    Ok(())
  }

  #[test]
  fn parser_inserted_scripts_are_ignored_by_insertion_helper() -> Result<()> {
    let mut host = Host::new(Document::new(selectors::context::QuirksMode::NoQuirks));
    let script = host.dom.create_element("script", "");
    host
      .dom
      .set_attribute(script, "src", "https://example.com/a.js")
      .unwrap();
    host.dom.append_child(host.dom.root(), script).unwrap();
    host.dom.node_mut(script).script_parser_document = true;

    let mut scheduler = ClassicScriptScheduler::<Host>::new();
    let mut event_loop = EventLoop::<Host>::new();

    prepare_dynamic_script_on_insertion(&mut host, &mut scheduler, &mut event_loop, script)?;

    assert!(
      !host.dom.node(script).script_already_started,
      "parser-inserted scripts should be prepared by the parser, not DOM insertion"
    );
    assert!(host.started_urls.is_empty());
    assert!(host.executed.is_empty());
    Ok(())
  }

  #[test]
  fn empty_script_executes_once_when_children_added() -> Result<()> {
    let mut host = Host::new(Document::new(selectors::context::QuirksMode::NoQuirks));
    let script = host.dom.create_element("script", "");
    host.dom.append_child(host.dom.root(), script).unwrap();

    let mut scheduler = ClassicScriptScheduler::<Host>::new();
    let mut event_loop = EventLoop::<Host>::new();

    prepare_dynamic_script_on_insertion(&mut host, &mut scheduler, &mut event_loop, script)?;
    assert!(
      !host.dom.node(script).script_already_started,
      "empty dynamic scripts must not be marked started on insertion"
    );

    let text = host.dom.create_text("console.log(1);");
    host.dom.append_child(script, text).unwrap();

    prepare_dynamic_script_on_children_changed(&mut host, &mut scheduler, &mut event_loop, script)?;
    assert!(
      host.executed.is_empty(),
      "dynamic inline scripts must be queued as tasks, not executed synchronously"
    );

    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;
    assert_eq!(host.executed.len(), 1);
    assert_eq!(host.executed[0].0, "console.log(1);");
    assert!(host.dom.node(script).script_already_started);

    // Subsequent mutations must not re-execute the script.
    let text2 = host.dom.create_text("console.log(2);");
    host.dom.append_child(script, text2).unwrap();
    prepare_dynamic_script_on_children_changed(&mut host, &mut scheduler, &mut event_loop, script)?;
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;
    assert_eq!(
      host.executed.len(),
      1,
      "already-started scripts must not execute twice"
    );
    Ok(())
  }

  #[test]
  fn empty_script_executes_once_when_src_set() -> Result<()> {
    let mut host = Host::new(Document::new(selectors::context::QuirksMode::NoQuirks));
    let script = host.dom.create_element("script", "");
    host.dom.append_child(host.dom.root(), script).unwrap();

    let mut scheduler = ClassicScriptScheduler::<Host>::new();
    let mut event_loop = EventLoop::<Host>::new();

    prepare_dynamic_script_on_insertion(&mut host, &mut scheduler, &mut event_loop, script)?;
    assert!(
      !host.dom.node(script).script_already_started,
      "empty dynamic scripts must not be marked started on insertion"
    );

    // Ensure `force_async` is captured from the dom2 node.
    host.dom.node_mut(script).script_force_async = false;

    host
      .dom
      .set_attribute(script, "src", "https://example.com/a.js")
      .unwrap();
    prepare_dynamic_script_on_src_attribute_change(&mut host, &mut scheduler, &mut event_loop, script)?;
    assert!(
      host.dom.node(script).script_already_started,
      "setting src should prepare and mark the script started"
    );
    assert_eq!(
      host.started_urls,
      vec!["https://example.com/a.js".to_string()],
      "expected external script load to be started"
    );

    host.complete_url("https://example.com/a.js", "EXT-A");
    scheduler.poll(&mut host, &mut event_loop)?;
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;
    assert_eq!(
      host.executed,
      vec![("EXT-A".to_string(), false)],
      "expected external script to execute once with captured force_async state"
    );

    // Re-setting src after execution must not re-run.
    host
      .dom
      .set_attribute(script, "src", "https://example.com/b.js")
      .unwrap();
    prepare_dynamic_script_on_src_attribute_change(&mut host, &mut scheduler, &mut event_loop, script)?;
    assert_eq!(
      host.started_urls,
      vec!["https://example.com/a.js".to_string()],
      "already-started scripts must not start a new load on src changes"
    );
    scheduler.poll(&mut host, &mut event_loop)?;
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;
    assert_eq!(host.executed.len(), 1);
    Ok(())
  }
}
