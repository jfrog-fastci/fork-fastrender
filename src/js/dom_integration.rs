use crate::dom::HTML_NAMESPACE;
use crate::dom2::{Document, NodeId, NodeKind};
use crate::error::Result;

use super::{
  determine_script_type_dom2, ClassicScriptScheduler, DomHost, EventLoop, ScriptElementSpec,
  ScriptExecutor, ScriptLoader, ScriptType, TaskSource,
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
  Host: DomHost + ScriptLoader + ScriptExecutor,
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

  // Only classic scripts are supported by the MVP scheduler.
  if spec.script_type != ScriptType::Classic {
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

fn build_non_parser_inserted_script_spec(dom: &Document, script: NodeId) -> ScriptElementSpec {
  let async_attr = dom.has_attribute(script, "async").unwrap_or(false);
  let defer_attr = dom.has_attribute(script, "defer").unwrap_or(false);

  let src_attr_present = dom.has_attribute(script, "src").unwrap_or(false);
  let src = dom
    .get_attribute(script, "src")
    .ok()
    .flatten()
    .map(trim_ascii_whitespace)
    .filter(|v| !v.is_empty())
    .map(|v| v.to_string());

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

// HTML defines "ASCII whitespace" as: U+0009 TAB, U+000A LF, U+000C FF, U+000D CR, U+0020 SPACE.
// Notably, this does *not* include U+000B VT (vertical tab).
fn trim_ascii_whitespace(value: &str) -> &str {
  value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
}
