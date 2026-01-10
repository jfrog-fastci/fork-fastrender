use crate::dom::HTML_NAMESPACE;
use crate::dom2::{Document, NodeId, NodeKind};
use crate::error::Result;
use crate::html::base_url_tracker::resolve_script_src_at_parse_time;

use super::{
  determine_script_type_dom2, ClassicScriptScheduler, DomHost, EventLoop, ScriptElementEvent,
  ScriptElementSpec, ScriptEventDispatcher, ScriptExecutor, ScriptLoader, ScriptType, TaskSource,
  trim_ascii_whitespace,
};

fn trim_ascii_whitespace(value: &str) -> &str {
  value.trim_matches(|c: char| c.is_ascii_whitespace())
}

/// Run a minimal subset of the HTML "prepare the script element" algorithm for dynamically inserted
/// `<script>` elements.
///
/// This is intended to be called by DOM mutation bindings after a `<script>` element becomes
/// connected to the document (e.g. after `Node.appendChild`).
///
/// Supported subset:
/// - Classic scripts only (`type`/`language` mapped via [`determine_script_type_dom2`]).
/// - External scripts (`src` present and non-empty) are treated as async-by-default because
///   `force_async=true` is passed into [`ClassicScriptScheduler`] for dynamically inserted scripts.
/// - Inline scripts are queued as `TaskSource::Script` tasks (rather than executing synchronously
///   inside the DOM mutation call). The event loop's post-task microtask checkpoint naturally
///   applies.
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
  let spec = host.mutate_dom(|dom| {
    if !is_html_script_element(dom, inserted_node) {
      return (None, false);
    }

    // HTML element post-connection steps: parser-inserted scripts are prepared by the parser, not by
    // DOM insertion.
    if dom.node(inserted_node).script_parser_document {
      return (None, false);
    }

    // HTML: scripts inside inert `<template>` contents are treated as disconnected and must not
    // execute.
    if !dom.is_connected_for_scripting(inserted_node) {
      return (None, false);
    }

    // HTML: do nothing when "already started" is true.
    if dom.node(inserted_node).script_already_started {
      return (None, false);
    }
    dom.node_mut(inserted_node).script_already_started = true;

    let spec = build_non_parser_inserted_script_spec(dom, inserted_node);
    (Some(spec), false)
  });

  let Some(spec) = spec else {
    return Ok(());
  };

  match spec.script_type {
    ScriptType::Classic => {
      if spec.is_suppressed_by_nomodule(&scheduler.options()) {
        return Ok(());
      }

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
    ScriptType::Module | ScriptType::ImportMap => {
      // Modules/import maps are not executed by this MVP dynamic insertion helper, but HTML still
      // requires that `src=""` / invalid `src` queues an error event task and suppresses inline
      // execution.
      if spec.src_attr_present && spec.src.as_deref().filter(|s| !s.is_empty()).is_none() {
        event_loop.queue_task(TaskSource::DOMManipulation, move |host, _event_loop| {
          host.dispatch_script_event(ScriptElementEvent::Error, &spec)
        })?;
      } else if matches!(spec.script_type, ScriptType::ImportMap) && spec.src_attr_present {
        // Import maps forbid `src` entirely; treat any `src` presence as an error.
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
  let defer_attr = dom.has_attribute(script, "defer").unwrap_or(false);
  let nomodule_attr = dom.has_attribute(script, "nomodule").unwrap_or(false);
  let crossorigin = dom
    .get_attribute(script, "crossorigin")
    .ok()
    .flatten()
    .map(|value| {
      let value = trim_ascii_whitespace(value);
      if value.eq_ignore_ascii_case("use-credentials") {
        crate::resource::CorsMode::UseCredentials
      } else {
        crate::resource::CorsMode::Anonymous
      }
    });
  let integrity = dom
    .get_attribute(script, "integrity")
    .ok()
    .flatten()
    .map(|value| value.to_string());
  let referrer_policy = dom
    .get_attribute(script, "referrerpolicy")
    .ok()
    .flatten()
    .and_then(crate::resource::ReferrerPolicy::from_attribute);

  let raw_src = dom.get_attribute(script, "src").ok().flatten();
  let src_attr_present = raw_src.is_some();
  let src = raw_src.and_then(|raw| resolve_script_src_at_parse_time(None, raw));

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
    defer_attr,
    nomodule_attr,
    crossorigin,
    integrity,
    referrer_policy,
    parser_inserted: false,
    force_async: dom.node(script).script_force_async,
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
  use super::prepare_dynamic_script_on_insertion;
  use crate::dom2::Document;
  use crate::error::Result;
  use crate::js::{
    ClassicScriptScheduler, DomHost, EventLoop, RunLimits, ScriptElementEvent, ScriptElementSpec,
    ScriptEventDispatcher, ScriptExecutor, ScriptLoader,
  };
  use crate::resource::FetchDestination;
  use selectors::context::QuirksMode;

  struct TestHost {
    dom: Document,
    started_loads: Vec<String>,
    executed: Vec<String>,
    next_handle: u32,
  }

  impl TestHost {
    fn new(dom: Document) -> Self {
      Self {
        dom,
        started_loads: Vec::new(),
        executed: Vec::new(),
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

    fn load_blocking(&mut self, url: &str, _destination: FetchDestination) -> Result<String> {
      self.started_loads.push(url.to_string());
      Ok(String::new())
    }

    fn start_load(&mut self, url: &str, _destination: FetchDestination) -> Result<Self::Handle> {
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
  }

  impl ScriptEventDispatcher for TestHost {
    fn dispatch_script_event(
      &mut self,
      _event: ScriptElementEvent,
      _spec: &ScriptElementSpec,
    ) -> Result<()> {
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

    let spec = super::build_non_parser_inserted_script_spec(&dom, script);
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

    let spec = super::build_non_parser_inserted_script_spec(&dom, script);
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
    Ok(())
  }

  #[test]
  fn dynamic_script_src_trims_ascii_whitespace() -> Result<()> {
    let mut dom = Document::new(QuirksMode::NoQuirks);
    let script = dom.create_element("script", "");
    dom
      .append_child(dom.root(), script)
      .expect("append_child should succeed");
    dom
      .set_attribute(script, "src", "\thttps://example.com/a.js\n")
      .expect("set_attribute should succeed");

    let mut host = TestHost::new(dom);
    let mut scheduler = ClassicScriptScheduler::<TestHost>::new();
    let mut event_loop = EventLoop::<TestHost>::new();

    prepare_dynamic_script_on_insertion(&mut host, &mut scheduler, &mut event_loop, script)?;

    assert_eq!(host.started_loads, vec!["https://example.com/a.js".to_string()]);
    Ok(())
  }

  #[test]
  fn dynamic_script_relative_src_is_preserved_without_base_url() -> Result<()> {
    let mut dom = Document::new(QuirksMode::NoQuirks);
    let script = dom.create_element("script", "");
    dom
      .append_child(dom.root(), script)
      .expect("append_child should succeed");
    dom
      .set_attribute(script, "src", "a.js")
      .expect("set_attribute should succeed");

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

    let spec = super::build_non_parser_inserted_script_spec(&dom, script);
    assert_eq!(
      spec.crossorigin,
      Some(crate::resource::CorsMode::UseCredentials)
    );
    Ok(())
  }
}

#[cfg(test)]
mod nomodule_tests {
  use super::*;
  use crate::js::{JsExecutionOptions, RunLimits};
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
    ) -> Result<String> {
      self.started_loads.push(url.to_string());
      Ok(String::new())
    }

    fn start_load(
      &mut self,
      url: &str,
      _destination: crate::resource::FetchDestination,
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
  }

  impl ScriptEventDispatcher for Host {
    fn dispatch_script_event(&mut self, _event: ScriptElementEvent, _spec: &ScriptElementSpec) -> Result<()> {
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
