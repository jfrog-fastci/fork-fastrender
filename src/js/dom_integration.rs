use crate::dom::HTML_NAMESPACE;
use crate::dom2::{Document, NodeId, NodeKind};
use crate::error::Result;
use crate::html::base_url_tracker::resolve_script_src_at_parse_time;

use super::html_script_pipeline::{HtmlScriptPipelineHost, ModuleGraphFetchOptions};
use super::html_script_scheduler::ScriptEventKind;

use super::{
  determine_script_type_dom2, DomHost, EventLoop, HtmlScriptScheduler, HtmlScriptSchedulerAction,
  HtmlScriptWork, ScriptElementSpec, ScriptType, TaskSource,
};

/// Helpers that bridge DOM mutation ("insertion steps") to the HTML `<script>` processing model.
///
/// This module intentionally keeps the "extract/collect script elements" and "build a
/// `ScriptElementSpec` snapshot" logic host-agnostic, so embeddings can drive execution via their
/// chosen scheduler/orchestrator.
///
///
/// The main entrypoints used by the `vm-js` DOM bindings are:
/// - [`collect_inserted_script_elements`]
/// - [`build_dynamic_script_element_spec`]
/// - [`prepare_dynamic_script_on_insertion_html`] (plus subtree wrapper)

/// Collect HTML `<script>` elements within newly-inserted roots, in tree order.
///
/// The returned list is derived from a snapshot of the post-insertion DOM tree:
/// callers should collect the list **before** executing any of the scripts, so subsequent DOM
/// mutations (e.g. a script removing a later `<script>` element) do not affect which elements are
/// processed. Individual script execution should still re-check connectivity at execution time.
///
/// Notes:
/// - Only scripts in the HTML namespace are included.
/// - Inert `<template>` subtrees are skipped (matching `dom2::Document` scripting connectedness).
/// - Disconnected roots produce no output.
pub fn collect_inserted_script_elements(dom: &Document, inserted_roots: &[NodeId]) -> Vec<NodeId> {
  let mut out = Vec::new();
  for &root in inserted_roots {
    for node in dom.dom_connected_subtree_preorder(root) {
      if is_html_script_element(dom, node) {
        out.push(node);
      }
    }
  }
  out
}

/// Build a [`ScriptElementSpec`] for a dynamically inserted `<script>` element (`parser_inserted=false`).
///
/// This is intended to be called from DOM insertion steps, after the element becomes connected.
pub fn build_dynamic_script_element_spec(
  dom: &Document,
  script: NodeId,
  document_base_url: Option<&str>,
) -> ScriptElementSpec {
  let async_attr = dom.has_attribute(script, "async").unwrap_or(false);
  let defer_attr = dom.has_attribute(script, "defer").unwrap_or(false);
  let nomodule_attr = dom.has_attribute(script, "nomodule").unwrap_or(false);
  let referrer_policy = dom
    .get_attribute(script, "referrerpolicy")
    .ok()
    .flatten()
    .and_then(super::parse_referrer_policy_attribute);
  let fetch_priority = dom
    .get_attribute(script, "fetchpriority")
    .ok()
    .flatten()
    .and_then(super::take_bounded_script_attribute_value);

  let raw_src = dom.get_attribute(script, "src").ok().flatten();
  let src_attr_present = raw_src.is_some();
  let src = raw_src.and_then(|raw| resolve_script_src_at_parse_time(document_base_url, raw));

  let (integrity_attr_present, integrity) =
    super::clamp_integrity_attribute(dom.get_attribute(script, "integrity").ok().flatten());
  let crossorigin =
    super::parse_crossorigin_attr(dom.get_attribute(script, "crossorigin").ok().flatten());

  let mut inline_text = String::new();
  for &child in &dom.node(script).children {
    if let NodeKind::Text { content } = &dom.node(child).kind {
      inline_text.push_str(content);
    }
  }

  ScriptElementSpec {
    base_url: document_base_url.map(|s| s.to_string()),
    src,
    src_attr_present,
    inline_text,
    async_attr,
    force_async: dom.node(script).script_force_async,
    defer_attr,
    nomodule_attr,
    crossorigin,
    integrity_attr_present,
    integrity,
    referrer_policy,
    fetch_priority,
    parser_inserted: false,
    node_id: Some(script),
    script_type: determine_script_type_dom2(dom, script),
  }
}

fn apply_html_script_scheduler_actions_dynamic_insertion<Host>(
  host: &mut Host,
  event_loop: &mut EventLoop<Host>,
  actions: Vec<HtmlScriptSchedulerAction<NodeId>>,
) -> Result<()>
where
  Host: HtmlScriptPipelineHost,
{
  for action in actions {
    match action {
      HtmlScriptSchedulerAction::StartClassicFetch {
        script_id, url, ..
      } => {
        host.start_classic_fetch(script_id, &url)?;
      }
      HtmlScriptSchedulerAction::StartModuleGraphFetch {
        script_id, url, ..
      } => {
        host.start_module_graph_fetch(script_id, &url, ModuleGraphFetchOptions::default())?;
      }
      HtmlScriptSchedulerAction::StartInlineModuleGraphFetch {
        script_id,
        source_text,
        base_url,
        ..
      } => {
        host.start_inline_module_graph_fetch(
          script_id,
          &source_text,
          base_url.as_deref(),
          ModuleGraphFetchOptions::default(),
        )?;
      }
      HtmlScriptSchedulerAction::BlockParserUntilExecuted { .. } => {
        // Dynamically inserted scripts never block parsing, but keep this action in the API so the
        // same scheduler can be used for parser integration.
      }
      HtmlScriptSchedulerAction::ExecuteNow { node_id, work, .. } => match work {
        // Import maps must be processed promptly so they affect subsequent module graph fetches.
        HtmlScriptWork::ImportMap {
          source_text,
          base_url,
        } => {
          host.register_import_map(&source_text, base_url.as_deref(), node_id, event_loop)?;
        }
        HtmlScriptWork::Classic { source_text } => {
          let source_text = source_text;
          event_loop.queue_task(TaskSource::Script, move |host, event_loop| {
            host.execute_classic_script(source_text.as_deref(), node_id, event_loop)
          })?;
        }
        HtmlScriptWork::Module { source_text } => {
          let source_text = source_text;
          event_loop.queue_task(TaskSource::Script, move |host, event_loop| {
            host.execute_module_script(source_text.as_deref(), node_id, event_loop)
          })?;
        }
      },
      HtmlScriptSchedulerAction::QueueTask { node_id, work, .. } => match work {
        HtmlScriptWork::ImportMap {
          source_text,
          base_url,
        } => {
          event_loop.queue_task(TaskSource::Script, move |host, event_loop| {
            host.register_import_map(&source_text, base_url.as_deref(), node_id, event_loop)
          })?;
        }
        HtmlScriptWork::Classic { source_text } => {
          event_loop.queue_task(TaskSource::Script, move |host, event_loop| {
            host.execute_classic_script(source_text.as_deref(), node_id, event_loop)
          })?;
        }
        HtmlScriptWork::Module { source_text } => {
          event_loop.queue_task(TaskSource::Script, move |host, event_loop| {
            host.execute_module_script(source_text.as_deref(), node_id, event_loop)
          })?;
        }
      },
      HtmlScriptSchedulerAction::QueueScriptEventTask { node_id, event, .. } => {
        let event_name: &'static str = match event {
          ScriptEventKind::Load => "load",
          ScriptEventKind::Error => "error",
        };
        event_loop.queue_task(TaskSource::DOMManipulation, move |host, event_loop| {
          debug_assert_eq!(
            event_loop.currently_running_task().map(|t| t.source),
            Some(TaskSource::DOMManipulation),
            "script element event tasks must run on the DOM manipulation task source"
          );
          host.dispatch_script_element_event(event_loop, node_id, event_name)?;
          Ok(())
        })?;
      }
    }
  }
  Ok(())
}

/// Run the HTML "prepare the script element" insertion steps for a dynamically inserted `<script>`
/// element, using [`HtmlScriptScheduler`].
///
/// This supports classic scripts, module scripts, and import maps, and applies the HTML scheduler's
/// actions through host hooks.
pub fn prepare_dynamic_script_on_insertion_html<Host>(
  host: &mut Host,
  scheduler: &mut HtmlScriptScheduler<NodeId>,
  event_loop: &mut EventLoop<Host>,
  inserted_node: NodeId,
  document_base_url: Option<&str>,
) -> Result<()>
where
  Host: DomHost + HtmlScriptPipelineHost,
{
  let modules_supported = scheduler.modules_supported();
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

    let spec = build_dynamic_script_element_spec(dom, inserted_node, document_base_url);

    // HTML: if there is no `src` attribute and the inline text is empty, do nothing. Importantly,
    // this must *not* set the "already started" flag so later `src`/text mutations can trigger
    // preparation.
    if !spec.src_attr_present && spec.inline_text.is_empty() {
      return (None, false);
    }

    // `integrity` attribute clamping: if present but too large, the metadata is invalid and the
    // script must not execute. Inline scripts should behave like other "early-out" cases and remain
    // eligible for later mutations.
    if spec.integrity_attr_present && spec.integrity.is_none() && !spec.src_attr_present {
      return (None, false);
    }

    // Only mark the element as started for script types that participate in HTML script processing.
    // Unknown types are ignored and should remain eligible if later mutated into a runnable script.
    let should_mark_started = match spec.script_type {
      ScriptType::Classic => true,
      ScriptType::Module | ScriptType::ImportMap => modules_supported,
      ScriptType::Unknown => false,
    };
    if should_mark_started {
      let _ = dom.set_script_already_started(inserted_node, true);
    }

    (Some(spec), false)
  });

  let Some(spec) = spec else {
    return Ok(());
  };

  // Invalid integrity metadata: external scripts should queue an `error` event and must not start a
  // fetch. (Inline scripts with invalid integrity are handled above and leave the element eligible
  // for later mutation.)
  if spec.integrity_attr_present && spec.integrity.is_none() {
    let should_dispatch_error = match spec.script_type {
      ScriptType::Classic => true,
      ScriptType::Module | ScriptType::ImportMap => modules_supported,
      ScriptType::Unknown => false,
    };
    if should_dispatch_error {
      event_loop.queue_task(TaskSource::DOMManipulation, move |host, event_loop| {
        host.dispatch_script_element_event(event_loop, inserted_node, "error")
      })?;
    }
    return Ok(());
  }

  // HTML passes a per-script "base URL at discovery" into classic and module script preparation.
  // Use the document base URL snapshot from spec building so inline module scripts can resolve
  // relative imports and import maps can be registered with the correct base.
  let base_url_at_discovery = spec.base_url.clone();
  let discovered = scheduler.discovered_script(spec, inserted_node, base_url_at_discovery)?;
  apply_html_script_scheduler_actions_dynamic_insertion(host, event_loop, discovered.actions)?;
  Ok(())
}

/// Prepare any dynamic `<script>` elements within a newly-inserted subtree using
/// [`prepare_dynamic_script_on_insertion_html`].
pub fn prepare_dynamic_scripts_on_subtree_insertion_html<Host>(
  host: &mut Host,
  scheduler: &mut HtmlScriptScheduler<NodeId>,
  event_loop: &mut EventLoop<Host>,
  inserted_root: NodeId,
  document_base_url: Option<&str>,
) -> Result<()>
where
  Host: DomHost + HtmlScriptPipelineHost,
{
  let script_nodes = host.with_dom(|dom| collect_inserted_script_elements(dom, &[inserted_root]));

  for node in script_nodes {
    prepare_dynamic_script_on_insertion_html(
      host,
      scheduler,
      event_loop,
      node,
      document_base_url,
    )?;
  }
  Ok(())
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
mod dynamic_insertion_html_scheduler_tests {
  use super::{
    apply_html_script_scheduler_actions_dynamic_insertion, prepare_dynamic_script_on_insertion_html,
    DomHost, EventLoop, HtmlScriptScheduler,
  };
  use crate::dom2::{Document, NodeId};
  use crate::error::Result;
  use crate::js::event_loop::RunLimits;
  use crate::js::html_script_pipeline::{
    HtmlScriptPipelineHost, ModuleGraphFetchOptions, ScriptElementEventHost,
  };
  use crate::js::{CurrentScriptHost, CurrentScriptStateHandle, HtmlScriptId};
  use selectors::context::QuirksMode;
  use std::collections::HashMap;
  
  struct Host {
    dom: Document,
    html: NodeId,
    current_script_state: CurrentScriptStateHandle,
    started_classic_fetches: Vec<(HtmlScriptId, String)>,
    started_module_graph_fetches: Vec<(HtmlScriptId, String)>,
    started_inline_module_fetches: Vec<(HtmlScriptId, String)>,
    ids_by_url: HashMap<String, HtmlScriptId>,
    log: Vec<String>,
  }
 
  impl Host {
    fn new() -> Self {
      let mut dom = Document::new(QuirksMode::NoQuirks);
      let html = dom.create_element("html", "");
      dom.append_child(dom.root(), html).expect("append_child");
      Self {
        dom,
        html,
        current_script_state: CurrentScriptStateHandle::default(),
        started_classic_fetches: Vec::new(),
        started_module_graph_fetches: Vec::new(),
        started_inline_module_fetches: Vec::new(),
        ids_by_url: HashMap::new(),
        log: Vec::new(),
      }
    }
 
    fn append_script(&mut self, attrs: &[(&str, &str)], text: Option<&str>, force_async: bool) -> NodeId {
      let html = self.html;
      self.mutate_dom(|dom| {
        let script = dom.create_element("script", "");
        dom.node_mut(script).script_force_async = force_async;
        for (k, v) in attrs {
          dom.set_attribute(script, k, v).expect("set_attribute");
        }
        if let Some(text) = text {
          let text_node = dom.create_text(text);
          dom.append_child(script, text_node).expect("append_child");
        }
        dom.append_child(html, script).expect("append_child");
        (script, true)
      })
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
  
  impl ScriptElementEventHost for Host {
    fn dispatch_script_element_event(
      &mut self,
      _event_loop: &mut EventLoop<Self>,
      _script: NodeId,
      _event_name: &'static str,
    ) -> Result<()> {
      Ok(())
    }
  }
  
  impl CurrentScriptHost for Host {
    fn current_script_state(&self) -> &CurrentScriptStateHandle {
      &self.current_script_state
    }
  }
  
  impl HtmlScriptPipelineHost for Host {
    fn start_classic_fetch(&mut self, script_id: HtmlScriptId, url: &str) -> Result<()> {
      self
        .started_classic_fetches
        .push((script_id, url.to_string()));
      self.ids_by_url.insert(url.to_string(), script_id);
      Ok(())
    }
  
    fn start_module_graph_fetch(
      &mut self,
      script_id: HtmlScriptId,
      url: &str,
      _options: ModuleGraphFetchOptions,
    ) -> Result<()> {
      self
        .started_module_graph_fetches
        .push((script_id, url.to_string()));
      self.ids_by_url.insert(url.to_string(), script_id);
      Ok(())
    }
  
    fn start_inline_module_graph_fetch(
      &mut self,
      script_id: HtmlScriptId,
      source_text: &str,
      _base_url: Option<&str>,
      _options: ModuleGraphFetchOptions,
    ) -> Result<()> {
      self
        .started_inline_module_fetches
        .push((script_id, source_text.to_string()));
      Ok(())
    }
  
    fn execute_classic_script(
      &mut self,
      source_text: Option<&str>,
      _script_node_id: NodeId,
      _event_loop: &mut EventLoop<Self>,
    ) -> Result<()> {
      self
        .log
        .push(format!("exec:classic:{}", source_text.unwrap_or("<null>")));
      Ok(())
    }
  
    fn execute_module_script(
      &mut self,
      module_handle: Option<&str>,
      _script_node_id: NodeId,
      _event_loop: &mut EventLoop<Self>,
    ) -> Result<()> {
      self
        .log
        .push(format!("exec:module:{}", module_handle.unwrap_or("<null>")));
      Ok(())
    }
  
    fn register_import_map(
      &mut self,
      source_text: &str,
      _base_url: Option<&str>,
      _script_node_id: NodeId,
      _event_loop: &mut EventLoop<Self>,
    ) -> Result<()> {
      self.log.push(format!("exec:importmap:{source_text}"));
      Ok(())
    }
  }
 
  #[test]
  fn dynamic_external_classic_scripts_execute_in_insertion_order_even_if_fetch_completes_out_of_order(
  ) -> Result<()> {
    let mut host = Host::new();
    let mut scheduler = HtmlScriptScheduler::<NodeId>::new();
    let mut event_loop = EventLoop::<Host>::new();
  
    let script_a = host.append_script(&[("src", "https://example.com/a.js")], None, false);
    prepare_dynamic_script_on_insertion_html(&mut host, &mut scheduler, &mut event_loop, script_a, None)?;
    let script_b = host.append_script(&[("src", "https://example.com/b.js")], None, false);
    prepare_dynamic_script_on_insertion_html(&mut host, &mut scheduler, &mut event_loop, script_b, None)?;
 
    let id_a = *host.ids_by_url.get("https://example.com/a.js").expect("id_a");
    let id_b = *host.ids_by_url.get("https://example.com/b.js").expect("id_b");
 
    // Complete the second fetch first; the scheduler must not execute it until the first is ready.
    let actions = scheduler.classic_fetch_completed(id_b, "B".to_string())?;
    apply_html_script_scheduler_actions_dynamic_insertion(&mut host, &mut event_loop, actions)?;
    let actions = scheduler.classic_fetch_completed(id_a, "A".to_string())?;
    apply_html_script_scheduler_actions_dynamic_insertion(&mut host, &mut event_loop, actions)?;
 
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;
    assert_eq!(
      host.log,
      vec!["exec:classic:A".to_string(), "exec:classic:B".to_string()]
    );
    Ok(())
  }
 
  #[test]
  fn dynamic_external_module_scripts_execute_in_insertion_order_even_if_graph_completes_out_of_order(
  ) -> Result<()> {
    let mut host = Host::new();
    let mut scheduler = HtmlScriptScheduler::<NodeId>::new();
    let mut event_loop = EventLoop::<Host>::new();
 
    let script_a = host.append_script(
      &[("type", "module"), ("src", "https://example.com/a.mjs")],
      None,
      false,
    );
    prepare_dynamic_script_on_insertion_html(&mut host, &mut scheduler, &mut event_loop, script_a, None)?;
    let script_b = host.append_script(
      &[("type", "module"), ("src", "https://example.com/b.mjs")],
      None,
      false,
    );
    prepare_dynamic_script_on_insertion_html(&mut host, &mut scheduler, &mut event_loop, script_b, None)?;
 
    let id_a = *host
      .ids_by_url
      .get("https://example.com/a.mjs")
      .expect("id_a");
    let id_b = *host
      .ids_by_url
      .get("https://example.com/b.mjs")
      .expect("id_b");
 
    let actions = scheduler.module_graph_completed(id_b, "MB".to_string())?;
    apply_html_script_scheduler_actions_dynamic_insertion(&mut host, &mut event_loop, actions)?;
    let actions = scheduler.module_graph_completed(id_a, "MA".to_string())?;
    apply_html_script_scheduler_actions_dynamic_insertion(&mut host, &mut event_loop, actions)?;
 
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;
    assert_eq!(
      host.log,
      vec!["exec:module:MA".to_string(), "exec:module:MB".to_string()]
    );
    Ok(())
  }
 
  #[test]
  fn dynamic_inline_module_script_is_queued_and_does_not_execute_synchronously() -> Result<()> {
    let mut host = Host::new();
    let mut scheduler = HtmlScriptScheduler::<NodeId>::new();
    let mut event_loop = EventLoop::<Host>::new();
 
    // `force_async=true` is the default for DOM-created scripts; keep it to ensure the inline module
    // is treated as async-like by the scheduler.
    let script = host.append_script(&[("type", "module")], Some("INLINE"), true);
    prepare_dynamic_script_on_insertion_html(&mut host, &mut scheduler, &mut event_loop, script, None)?;
 
    assert!(
      host.log.is_empty(),
      "expected no synchronous script execution during DOM insertion"
    );
    assert_eq!(host.started_inline_module_fetches.len(), 1);
    let script_id = host.started_inline_module_fetches[0].0;
 
    let actions = scheduler.module_graph_completed(script_id, "INLINE-MOD".to_string())?;
    apply_html_script_scheduler_actions_dynamic_insertion(&mut host, &mut event_loop, actions)?;
    assert!(
      host.log.is_empty(),
      "expected inline module execution to be queued as a task"
    );
 
    event_loop.run_until_idle(&mut host, RunLimits::unbounded())?;
    assert_eq!(host.log, vec!["exec:module:INLINE-MOD".to_string()]);
    Ok(())
  }
}
