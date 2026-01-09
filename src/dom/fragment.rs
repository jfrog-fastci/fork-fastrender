use crate::error::Error;
use crate::error::ParseError;
use crate::error::RenderStage;
use crate::error::Result;
use html5ever::parse_fragment;
use html5ever::tendril::TendrilSink;
use html5ever::tree_builder::QuirksMode as HtmlQuirksMode;
use html5ever::tree_builder::TreeBuilderOpts;
use html5ever::ParseOpts;
use markup5ever::LocalName;
use markup5ever::Namespace;
use markup5ever::QualName;
use markup5ever_rcdom::Handle;
use markup5ever_rcdom::NodeData;
use markup5ever_rcdom::RcDom;
use std::io;

use super::DomCompatibilityMode;
use super::DomNode;
use super::DomNodeType;
use super::DomParseOptions;
use super::QuirksMode;
use super::HTML_NAMESPACE;

fn map_quirks_mode_to_html(mode: QuirksMode) -> HtmlQuirksMode {
  match mode {
    QuirksMode::Quirks => HtmlQuirksMode::Quirks,
    QuirksMode::LimitedQuirks => HtmlQuirksMode::LimitedQuirks,
    QuirksMode::NoQuirks => HtmlQuirksMode::NoQuirks,
  }
}

/// Parse an HTML fragment (per HTML fragment parsing) in a given element context.
///
/// This is the canonical fragment parser for `Element.innerHTML` / `outerHTML` semantics. Unlike
/// `parse_html`, this uses `html5ever::parse_fragment` so context-sensitive fixups (e.g. table
/// insertion modes) match browser behavior.
pub fn parse_html_fragment(
  html: &str,
  context_tag: &str,
  context_namespace: &str,
  options: DomParseOptions,
  document_quirks: QuirksMode,
) -> Result<Vec<DomNode>> {
  let opts = ParseOpts {
    tree_builder: TreeBuilderOpts {
      scripting_enabled: options.scripting_enabled,
      quirks_mode: map_quirks_mode_to_html(document_quirks),
      ..Default::default()
    },
    ..Default::default()
  };

  let context_name = fragment_context_qual_name(context_tag, context_namespace);

  let html5ever_timer = super::dom_parse_diagnostics_timer();
  let reader = io::Cursor::new(html.as_bytes());
  let mut reader = super::DeadlineCheckedRead::new(reader);

  // Note: we parse from UTF-8, matching `parse_html`.
  // `html5ever::parse_fragment` requires `context_element_allows_scripting` as a separate flag from
  // `TreeBuilderOpts::scripting_enabled`. It is only used to determine the tokenizer initial state
  // for `<noscript>` contexts; wire it through from `options.scripting_enabled` so fragment parsing
  // matches `parse_html` / browser `innerHTML` semantics.
  let context_element_allows_scripting = options.scripting_enabled;
  let dom: RcDom = parse_fragment(
    RcDom::default(),
    opts,
    context_name,
    Vec::new(),
    context_element_allows_scripting,
  )
    .from_utf8()
    .read_from(&mut reader)
    .map_err(|e| {
      if e.kind() == io::ErrorKind::TimedOut {
        if let Some(timeout) = e
          .get_ref()
          .and_then(|inner| inner.downcast_ref::<crate::error::RenderError>())
        {
          return Error::Render(timeout.clone());
        }
        return Error::Render(crate::error::RenderError::Timeout {
          stage: RenderStage::DomParse,
          elapsed: crate::render_control::active_deadline()
            .as_ref()
            .map(|deadline| deadline.elapsed())
            .unwrap_or_default(),
        });
      }

      Error::Parse(ParseError::InvalidHtml {
        message: format!("Failed to parse HTML fragment: {}", e),
        line: 0,
      })
    })?;
  if let Some(start) = html5ever_timer {
    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
    super::with_dom_parse_diagnostics(|diag| {
      diag.html5ever_ms += elapsed_ms;
    });
  }

  let convert_timer = super::dom_parse_diagnostics_timer();
  let mut deadline_counter = 0usize;
  let mut converted: Vec<DomNode> = Vec::new();
  // `html5ever::parse_fragment` inserts parsed nodes as children of a synthetic context element.
  // Extract that element's children rather than returning the context wrapper itself.
  //
  // Clone handles out so we don't hold a RefCell borrow over conversion.
  let handles: Vec<Handle> = fragment_children_from_rcdom(&dom);
  converted.reserve(handles.len());
  for handle in handles {
    if let Some(node) = super::convert_handle_to_node(
      &handle,
      document_quirks,
      options.scripting_enabled,
      &mut deadline_counter,
    )? {
      converted.push(node);
    }
  }
  if let Some(start) = convert_timer {
    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
    super::with_dom_parse_diagnostics(|diag| {
      diag.convert_ms += elapsed_ms;
    });
  }

  // Post-processing (shadow roots + optional compatibility mutations) expects a root node. Use a
  // synthetic container matching the context element so declarative shadow DOM templates can be
  // promoted when `innerHTML` is set on a host element.
  let mut container = DomNode {
    node_type: DomNodeType::Element {
      tag_name: context_tag.to_string(),
      namespace: normalize_dom_namespace(context_namespace).to_string(),
      attributes: Vec::new(),
    },
    children: converted,
  };

  let shadow_attach_timer = super::dom_parse_diagnostics_timer();
  super::attach_shadow_roots(&mut container, &mut deadline_counter)?;
  if let Some(start) = shadow_attach_timer {
    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
    super::with_dom_parse_diagnostics(|diag| {
      diag.shadow_attach_ms += elapsed_ms;
    });
  }

  if matches!(
    options.compatibility_mode,
    DomCompatibilityMode::Compatibility
  ) {
    let compat_timer = super::dom_parse_diagnostics_timer();
    super::apply_dom_compatibility_mutations(&mut container, &mut deadline_counter)?;
    if let Some(start) = compat_timer {
      let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
      super::with_dom_parse_diagnostics(|diag| {
        diag.compat_ms += elapsed_ms;
      });
    }
  }

  Ok(std::mem::take(&mut container.children))
}

fn handle_children(handle: &Handle) -> Vec<Handle> {
  handle.children.borrow().iter().cloned().collect()
}

fn fragment_children_from_rcdom(rcdom: &RcDom) -> Vec<Handle> {
  let children = handle_children(&rcdom.document);

  // `html5ever`'s RcDom fragment parsing can return a synthetic `<html>` element as the sole
  // significant child of the document, with the actual fragment nodes as its children. Strip that
  // wrapper so callers can insert the returned nodes directly (innerHTML/outerHTML semantics).
  let significant: Vec<Handle> = children
    .iter()
    .filter(|handle| !matches!(handle.data, NodeData::Doctype { .. } | NodeData::Comment { .. }))
    .cloned()
    .collect();

  if significant.len() == 1 {
    if let NodeData::Element { name, .. } = &significant[0].data {
      if name.ns.as_ref() == HTML_NAMESPACE && name.local.as_ref().eq_ignore_ascii_case("html") {
        return handle_children(&significant[0]);
      }
    }
  }

  significant
}

fn normalize_parse_namespace(namespace: &str) -> &str {
  if namespace.is_empty() {
    return HTML_NAMESPACE;
  }
  namespace
}

fn normalize_dom_namespace(namespace: &str) -> &str {
  let namespace = normalize_parse_namespace(namespace);
  if namespace == HTML_NAMESPACE {
    ""
  } else {
    namespace
  }
}

fn fragment_context_qual_name(context_tag: &str, context_namespace: &str) -> QualName {
  let ns = normalize_parse_namespace(context_namespace);
  let ns: Namespace = ns.into();
  // HTML local names are ASCII-lowercased by the HTML parser. Use the same normalization for the
  // fragment context to match browser behavior.
  let local: LocalName = if ns.as_ref() == HTML_NAMESPACE {
    context_tag.to_ascii_lowercase().into()
  } else {
    context_tag.into()
  };
  QualName::new(None, ns, local)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn collect_preorder_tags(node: &DomNode, out: &mut Vec<String>) {
    if let DomNodeType::Element { tag_name, .. } = &node.node_type {
      out.push(tag_name.to_ascii_lowercase());
    }
    for child in &node.children {
      collect_preorder_tags(child, out);
    }
  }

  fn collect_preorder_tags_from_roots(nodes: &[DomNode]) -> Vec<String> {
    let mut out = Vec::new();
    for node in nodes {
      collect_preorder_tags(node, &mut out);
    }
    out
  }

  fn find_first_element_by_tag<'a>(node: &'a DomNode, tag: &str) -> Option<&'a DomNode> {
    if let DomNodeType::Element { tag_name, .. } = &node.node_type {
      if tag_name.eq_ignore_ascii_case(tag) {
        return Some(node);
      }
    }
    for child in &node.children {
      if let Some(found) = find_first_element_by_tag(child, tag) {
        return Some(found);
      }
    }
    None
  }

  fn find_first_text(node: &DomNode) -> Option<&str> {
    if let DomNodeType::Text { content } = &node.node_type {
      return Some(content.as_str());
    }
    for child in &node.children {
      if let Some(found) = find_first_text(child) {
        return Some(found);
      }
    }
    None
  }

  #[test]
  fn parse_fragment_in_div_context() {
    let nodes = parse_html_fragment(
      "<span id=a>hi</span>tail",
      "div",
      HTML_NAMESPACE,
      DomParseOptions::default(),
      QuirksMode::NoQuirks,
    )
    .expect("parse fragment");

    assert_eq!(nodes.len(), 2, "expected 2 top-level nodes");

    let span = &nodes[0];
    match &span.node_type {
      DomNodeType::Element {
        tag_name,
        attributes,
        ..
      } => {
        assert_eq!(tag_name, "span");
        assert!(
          attributes
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("id") && v == "a"),
          "expected id=a attribute, got {attributes:?}"
        );
      }
      other => panic!("expected span element node, got {other:?}"),
    }
    assert_eq!(
      span.children.len(),
      1,
      "expected span to have a single text child"
    );
    match &span.children[0].node_type {
      DomNodeType::Text { content } => assert_eq!(content, "hi"),
      other => panic!("expected span text child, got {other:?}"),
    }

    match &nodes[1].node_type {
      DomNodeType::Text { content } => assert_eq!(content, "tail"),
      other => panic!("expected tail text node, got {other:?}"),
    }
  }

  #[test]
  fn parse_fragment_in_table_context_smoke() {
    let nodes = parse_html_fragment(
      "<tr><td>x</td></tr>",
      "table",
      HTML_NAMESPACE,
      DomParseOptions::default(),
      QuirksMode::NoQuirks,
    )
    .expect("parse fragment");

    let tags = collect_preorder_tags_from_roots(&nodes);
    let tbody = tags.iter().position(|t| t == "tbody");
    let tr = tags.iter().position(|t| t == "tr");
    let td = tags.iter().position(|t| t == "td");
    assert!(
      tbody.is_some(),
      "expected tbody fixup in table context; got tags {tags:?}"
    );
    assert!(tr.is_some(), "expected a tr descendant; got tags {tags:?}");
    assert!(td.is_some(), "expected a td descendant; got tags {tags:?}");
    assert!(
      tbody.unwrap() < tr.unwrap() && tr.unwrap() < td.unwrap(),
      "expected tbody/tr/td in order; got tags {tags:?}"
    );

    let td_node = nodes
      .iter()
      .find_map(|node| find_first_element_by_tag(node, "td"))
      .expect("expected td element");
    let text = find_first_text(td_node).expect("expected td text");
    assert_eq!(text, "x");
  }

  #[test]
  fn parse_fragment_preserves_template_contents() {
    let nodes = parse_html_fragment(
      "<template><span>in</span></template>",
      "div",
      HTML_NAMESPACE,
      DomParseOptions::default(),
      QuirksMode::NoQuirks,
    )
    .expect("parse fragment");

    assert_eq!(nodes.len(), 1);
    let template = &nodes[0];
    match &template.node_type {
      DomNodeType::Element { tag_name, .. } => assert_eq!(tag_name, "template"),
      other => panic!("expected template element, got {other:?}"),
    }
    assert_eq!(
      template.children.len(),
      1,
      "expected template contents to include span"
    );
    let span = &template.children[0];
    match &span.node_type {
      DomNodeType::Element { tag_name, .. } => assert_eq!(tag_name, "span"),
      other => panic!("expected span element, got {other:?}"),
    }
    assert_eq!(span.children.len(), 1, "expected span text child");
    match &span.children[0].node_type {
      DomNodeType::Text { content } => assert_eq!(content, "in"),
      other => panic!("expected span text child, got {other:?}"),
    }
  }

  #[test]
  fn parse_fragment_with_scripting_disabled_parses_noscript_children_as_dom() {
    let nodes = parse_html_fragment(
      "<noscript><p>fallback</p></noscript>",
      "div",
      HTML_NAMESPACE,
      DomParseOptions::with_scripting_enabled(false),
      QuirksMode::NoQuirks,
    )
    .expect("parse fragment");
    assert_eq!(nodes.len(), 1);

    let noscript = &nodes[0];
    match &noscript.node_type {
      DomNodeType::Element { tag_name, .. } => assert_eq!(tag_name, "noscript"),
      other => panic!("expected noscript element, got {other:?}"),
    }

    assert!(
      noscript.children.iter().any(|child| {
        matches!(&child.node_type, DomNodeType::Element { tag_name, .. } if tag_name.eq_ignore_ascii_case("p"))
      }),
      "<noscript> should parse its contents as normal DOM when scripting is disabled"
    );
  }

  #[test]
  fn parse_fragment_with_scripting_enabled_parses_noscript_children_as_text() {
    let nodes = parse_html_fragment(
      "<noscript><p>fallback</p></noscript>",
      "div",
      HTML_NAMESPACE,
      DomParseOptions::with_scripting_enabled(true),
      QuirksMode::NoQuirks,
    )
    .expect("parse fragment");
    assert_eq!(nodes.len(), 1);

    let noscript = &nodes[0];
    assert_eq!(
      noscript.children.len(),
      1,
      "<noscript> should have a single text child when scripting is enabled"
    );
    match &noscript.children[0].node_type {
      DomNodeType::Text { content } => {
        assert!(
          content.contains("<p>fallback</p>"),
          "noscript text should contain raw HTML: {content:?}"
        );
      }
      other => panic!("expected noscript child to be text, got {other:?}"),
    }
  }

  #[test]
  fn dom_parse_fragment_timeout_is_cooperative() {
    use crate::error::Error;
    use crate::error::RenderStage;
    use crate::render_control::{with_deadline, RenderDeadline};
    use std::time::Duration;

    let deadline = RenderDeadline::new(Some(Duration::from_millis(0)), None);
    let result = with_deadline(Some(&deadline), || {
      parse_html_fragment(
        "<span>hi</span>",
        "div",
        HTML_NAMESPACE,
        DomParseOptions::default(),
        QuirksMode::NoQuirks,
      )
    });

    match result {
      Err(Error::Render(crate::error::RenderError::Timeout { stage, .. })) => {
        assert_eq!(stage, RenderStage::DomParse);
      }
      other => panic!("expected dom_parse timeout, got {other:?}"),
    }
  }

  #[test]
  fn parse_fragment_noscript_context_respects_scripting_enabled() {
    // When scripting is enabled, `<noscript>` is tokenized as rawtext, so markup becomes text.
    let nodes = parse_html_fragment(
      "<span>hi</span>",
      "noscript",
      HTML_NAMESPACE,
      DomParseOptions::with_scripting_enabled(true),
      QuirksMode::NoQuirks,
    )
    .expect("parse fragment");
    assert_eq!(nodes.len(), 1);
    match &nodes[0].node_type {
      DomNodeType::Text { content } => assert_eq!(content, "<span>hi</span>"),
      other => panic!("expected rawtext in noscript context, got {other:?}"),
    }

    // When scripting is disabled, `<noscript>` is tokenized normally and markup is parsed into DOM.
    let nodes = parse_html_fragment(
      "<span>hi</span>",
      "noscript",
      HTML_NAMESPACE,
      DomParseOptions::with_scripting_enabled(false),
      QuirksMode::NoQuirks,
    )
    .expect("parse fragment");
    assert_eq!(nodes.len(), 1);
    match &nodes[0].node_type {
      DomNodeType::Element { tag_name, .. } => assert_eq!(tag_name, "span"),
      other => panic!("expected element parsing in noscript context, got {other:?}"),
    }
  }

  #[test]
  fn parse_fragment_respects_document_quirks_mode() {
    // html5ever's tree builder behavior for some tags depends on quirks mode. In particular,
    // when a `<table>` start tag is seen in the "in body" insertion mode, a `<p>` element is only
    // implicitly closed when the document is *not* in quirks mode.
    //
    // This affects `innerHTML` on quirks-mode documents.
    let html = "<p>one<table><tr><td>x</td></tr></table>two";

    let nodes_no_quirks = parse_html_fragment(
      html,
      "div",
      HTML_NAMESPACE,
      DomParseOptions::default(),
      QuirksMode::NoQuirks,
    )
    .expect("parse fragment no quirks");
    assert_eq!(
      nodes_no_quirks.len(),
      3,
      "expected <p>, <table>, and trailing text nodes in no-quirks mode"
    );
    assert!(matches!(
      &nodes_no_quirks[0].node_type,
      DomNodeType::Element { tag_name, .. } if tag_name.eq_ignore_ascii_case("p")
    ));
    assert!(matches!(
      &nodes_no_quirks[1].node_type,
      DomNodeType::Element { tag_name, .. } if tag_name.eq_ignore_ascii_case("table")
    ));
    assert!(matches!(
      &nodes_no_quirks[2].node_type,
      DomNodeType::Text { content } if content == "two"
    ));

    let nodes_quirks = parse_html_fragment(
      html,
      "div",
      HTML_NAMESPACE,
      DomParseOptions::default(),
      QuirksMode::Quirks,
    )
    .expect("parse fragment quirks");
    assert_eq!(
      nodes_quirks.len(),
      1,
      "expected a single <p> root node in quirks mode (table stays inside <p>)"
    );
    assert!(matches!(
      &nodes_quirks[0].node_type,
      DomNodeType::Element { tag_name, .. } if tag_name.eq_ignore_ascii_case("p")
    ));
    let tags = collect_preorder_tags_from_roots(&nodes_quirks);
    assert!(
      tags.iter().any(|t| t == "table"),
      "expected quirks-mode <p> to contain a <table> descendant; got tags {tags:?}"
    );
  }

  #[test]
  fn parse_fragment_in_svg_namespace_context() {
    let nodes = parse_html_fragment(
      r#"<circle cx="1" cy="2" r="3"></circle>"#,
      "svg",
      crate::dom::SVG_NAMESPACE,
      DomParseOptions::default(),
      QuirksMode::NoQuirks,
    )
    .expect("parse fragment");

    assert_eq!(nodes.len(), 1);
    let circle = &nodes[0];
    match &circle.node_type {
      DomNodeType::Element {
        tag_name,
        namespace,
        attributes,
      } => {
        assert_eq!(tag_name, "circle");
        assert_eq!(namespace, crate::dom::SVG_NAMESPACE);
        assert!(
          attributes
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("cx") && v == "1"),
          "expected cx=1 attribute, got {attributes:?}"
        );
      }
      other => panic!("expected <circle> element, got {other:?}"),
    }
  }
}
