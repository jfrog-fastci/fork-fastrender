use crate::dom::HTML_NAMESPACE;
use crate::dom2::{Document, NodeId, NodeKind};
use crate::error::Result;
use crate::html::base_url_tracker::resolve_script_src_at_parse_time;

use super::{
  determine_script_type_dom2, ClassicScriptScheduler, DomHost, EventLoop, ScriptElementSpec,
  ScriptElementEvent, ScriptEventDispatcher, ScriptExecutor, ScriptLoader, ScriptType, TaskSource,
};

/// Run a minimal subset of the HTML "prepare the script element" algorithm for dynamically inserted
/// `<script>` elements.
///
/// This is intended to be called by DOM mutation bindings after a `<script>` element becomes
/// connected to the document (e.g. after `Node.appendChild`).
///
/// Supported subset:
/// - Classic scripts only (`type`/`language` mapped via [`determine_script_type_dom2`]).
/// - External scripts (`src` present and non-empty) are treated as async by default because
///   `parser_inserted=false` is passed into [`ClassicScriptScheduler`].
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
  let crossorigin = dom
    .get_attribute(script, "crossorigin")
    .ok()
    .flatten()
    .map(|value| {
      let value = super::trim_ascii_whitespace(value);
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

  let raw_src = dom
    .get_attribute(script, "src")
    .ok()
    .flatten()
    .map(|v| v.to_string());
  let src_attr_present = raw_src.is_some();
  let src = raw_src.as_deref().and_then(|raw| resolve_script_src_at_parse_time(None, raw));

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
    crossorigin,
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
