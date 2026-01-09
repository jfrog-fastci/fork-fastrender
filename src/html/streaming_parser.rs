//! Streaming HTML parser driver for script-aware incremental parsing.
//!
//! This module provides [`StreamingHtmlParser`], a thin driver around
//! [`crate::html::pausable_html5ever::PausableHtml5everParser`] that:
//! - parses into a live [`crate::dom2::Document`],
//! - pauses at parser-inserted `</script>` boundaries (`TokenizerResult::Script`),
//! - supports `document.write`-style input injection (`push_front_str`),
//! - and maintains the parse-time document base URL (`<base href>`).

use crate::dom::HTML_NAMESPACE;
use crate::dom2::{Document, Dom2TreeSink, NodeId, NodeKind};
use crate::html::base_url_tracker::BaseUrlTracker;
use crate::html::pausable_html5ever::{Html5everPump, PausableHtml5everParser};

use html5ever::tree_builder::TreeBuilderOpts;
use html5ever::ParseOpts;
use std::cell::{Ref, RefMut};

/// Output of a [`StreamingHtmlParser::pump`] call.
///
/// The parser yields `Script` immediately after processing a parser-inserted `</script>` end tag.
/// At this point, the DOM contains the `<script>` element and its text children, but **does not**
/// contain any markup that appears after the `</script>` tag.
#[derive(Debug)]
pub enum StreamingParserYield {
  /// Parser hit a `</script>` end tag and must yield to the host.
  Script {
    /// The DOM node id of the `<script>` element.
    script: NodeId,
    /// The parse-time base URL after processing the `</script>` tag but before parsing any
    /// subsequent markup (e.g. a following `<base href>`).
    base_url_at_this_point: Option<String>,
  },
  /// Parser consumed all buffered input but has not been told EOF yet.
  NeedMoreInput,
  /// EOF was signalled and the DOM is finished.
  Finished { document: Document },
}

/// Incremental, script-aware HTML parser that builds a live `dom2` document.
///
/// This is the parser-side foundation for implementing the HTML `<script>` processing model:
/// callers can repeatedly feed input, call [`pump`](Self::pump) until `Script` is yielded, run the
/// script, then resume parsing by calling `pump` again.
pub struct StreamingHtmlParser {
  parser: PausableHtml5everParser<Dom2TreeSink>,
  document_url: Option<String>,
  current_base_url: Option<String>,
}

impl StreamingHtmlParser {
  /// Create a new streaming HTML parser.
  ///
  /// `document_url` is an optional URL hint used as the initial parse-time base URL (and as the
  /// resolution base for a later `<base href>`).
  ///
  /// This constructor enables scripting semantics (affects parsing of elements like `<noscript>`)
  /// so it can be used as the foundation for `<script>` execution.
  pub fn new(document_url: Option<&str>) -> Self {
    Self::new_with_scripting_enabled(document_url, /* scripting_enabled */ true)
  }

  /// Create a new streaming HTML parser with an explicit scripting mode.
  ///
  /// `scripting_enabled` maps directly to `html5ever::tree_builder::TreeBuilderOpts::scripting_enabled`.
  /// When `false`, parsing treats scripting as disabled (notably affecting `<noscript>` handling).
  pub fn new_with_scripting_enabled(document_url: Option<&str>, scripting_enabled: bool) -> Self {
    let sink = Dom2TreeSink::new(document_url);
    let opts = ParseOpts {
      tree_builder: TreeBuilderOpts {
        scripting_enabled,
        ..Default::default()
      },
      ..Default::default()
    };

    Self {
      parser: PausableHtml5everParser::new_document(sink, opts),
      document_url: document_url.map(|s| s.to_string()),
      current_base_url: document_url.map(|s| s.to_string()),
    }
  }

  /// Append decoded Unicode input to the end of the parser's buffered input.
  pub fn push_str(&self, chunk: &str) {
    self.parser.push_str(chunk);
  }

  /// Like `document.write`: inject text before any buffered “remaining input”.
  pub fn push_front_str(&self, chunk: &str) {
    self.parser.push_front_str(chunk);
  }

  /// Signal no more input will arrive.
  pub fn set_eof(&mut self) {
    self.parser.set_eof();
  }

  /// Run the tokenizer/tree-builder until it either needs a script, needs more input, or finishes.
  pub fn pump(&mut self) -> StreamingParserYield {
    match self.parser.pump() {
      Html5everPump::Script(script) => {
        let base_url_at_this_point = self.parser.sink().current_base_url();
        self.current_base_url = base_url_at_this_point.clone();
        StreamingParserYield::Script {
          script,
          base_url_at_this_point,
        }
      }
      Html5everPump::NeedMoreInput => {
        self.current_base_url = self.parser.sink().current_base_url();
        StreamingParserYield::NeedMoreInput
      }
      Html5everPump::Finished(document) => {
        self.current_base_url = compute_final_base_url(&document, self.document_url.as_deref());
        StreamingParserYield::Finished { document }
      }
    }
  }

  /// Borrow the current partially-built document.
  ///
  /// The returned borrow must not be held across calls to [`pump`](Self::pump), since pumping will
  /// mutate the underlying DOM via interior mutability.
  ///
  /// # Panics
  /// Panics if called after the parser has finished (after `pump` returns [`Finished`](StreamingParserYield::Finished)).
  pub fn document(&self) -> Ref<'_, Document> {
    self.parser.sink().document()
  }

  /// Mutably borrow the current partially-built document.
  ///
  /// The returned borrow must not be held across calls to [`pump`](Self::pump), since pumping will
  /// mutate the underlying DOM via interior mutability.
  ///
  /// # Panics
  /// Panics if called after the parser has finished (after `pump` returns [`Finished`](StreamingParserYield::Finished)).
  pub fn document_mut(&self) -> RefMut<'_, Document> {
    self.parser.sink().document_mut()
  }

  /// Returns the current parse-time base URL.
  ///
  /// This is updated at the start of every [`pump`](Self::pump) call and remains available after
  /// the parser has finished.
  pub fn current_base_url(&self) -> Option<String> {
    self.current_base_url.clone()
  }

  /// Returns the `document_url` hint used to initialize this parser.
  pub fn document_url(&self) -> Option<&str> {
    self.document_url.as_deref()
  }
}

fn compute_final_base_url(doc: &Document, document_url: Option<&str>) -> Option<String> {
  // The `Dom2TreeSink` (and its internal `BaseUrlTracker`) are consumed once html5ever finishes
  // parsing, but callers still want to query the final base URL (e.g. tests, future DOM APIs).
  //
  // Recompute it from the finished DOM by replaying the same `BaseUrlTracker` insertion logic in
  // final tree order.
  let mut tracker = BaseUrlTracker::new(document_url);

  #[derive(Clone, Copy)]
  struct Ctx {
    in_head: bool,
    in_foreign_namespace: bool,
    in_template: bool,
  }

  fn is_html_namespace(namespace: &str) -> bool {
    namespace.is_empty() || namespace == HTML_NAMESPACE
  }

  fn element_fields<'a>(
    kind: &'a NodeKind,
  ) -> Option<(&'a str, &'a str, &'a [(String, String)])> {
    match kind {
      NodeKind::Element {
        tag_name,
        namespace,
        attributes,
      } => Some((tag_name, namespace, attributes)),
      NodeKind::Slot {
        namespace,
        attributes,
        ..
      } => Some(("slot", namespace, attributes)),
      _ => None,
    }
  }

  let root = doc.root();
  let root_ctx = Ctx {
    in_head: false,
    in_foreign_namespace: false,
    in_template: false,
  };

  let mut stack: Vec<(NodeId, Ctx)> = doc
    .node(root)
    .children
    .iter()
    .rev()
    .copied()
    .map(|child| (child, root_ctx))
    .collect();

  while let Some((id, parent_ctx)) = stack.pop() {
    if let Some((tag_name, namespace, attrs)) = element_fields(&doc.node(id).kind) {
      tracker.on_element_inserted(
        tag_name,
        namespace,
        attrs,
        parent_ctx.in_head,
        parent_ctx.in_foreign_namespace,
        parent_ctx.in_template,
      );
    }

    let mut child_ctx = parent_ctx;
    if doc.node(id).inert_subtree {
      child_ctx.in_template = true;
    }

    match &doc.node(id).kind {
      NodeKind::Element {
        tag_name,
        namespace,
        ..
      } => {
        if tag_name.eq_ignore_ascii_case("head") && is_html_namespace(namespace) {
          child_ctx.in_head = true;
        }
        if !is_html_namespace(namespace) {
          child_ctx.in_foreign_namespace = true;
        }
      }
      NodeKind::Slot { namespace, .. } => {
        if !is_html_namespace(namespace) {
          child_ctx.in_foreign_namespace = true;
        }
      }
      NodeKind::Document { .. }
      | NodeKind::ShadowRoot { .. }
      | NodeKind::Text { .. }
      | NodeKind::Comment { .. }
      | NodeKind::ProcessingInstruction { .. }
      | NodeKind::Doctype { .. } => {}
    }

    for &child in doc.node(id).children.iter().rev() {
      stack.push((child, child_ctx));
    }
  }

  tracker.current_base_url()
}

#[cfg(test)]
  mod tests {
    use super::{StreamingHtmlParser, StreamingParserYield};
    use crate::dom2::{Document, NodeId, NodeKind};
    use crate::html::base_url_tracker::BaseUrlTracker;
    use crate::js::streaming_dom2::build_parser_inserted_script_element_spec_dom2;
    use crate::js::ScriptElementSpec;

  fn find_first_element(doc: &Document, tag: &str) -> Option<NodeId> {
    let mut stack = vec![doc.root()];
    while let Some(id) = stack.pop() {
      if let NodeKind::Element { tag_name, .. } = &doc.node(id).kind {
        if tag_name.eq_ignore_ascii_case(tag) {
          return Some(id);
        }
      }
      for &child in doc.node(id).children.iter().rev() {
        stack.push(child);
      }
    }
    None
  }

  fn element_children(doc: &Document, parent: NodeId) -> Vec<NodeId> {
    doc
      .node(parent)
      .children
      .iter()
      .copied()
      .filter(|&id| matches!(doc.node(id).kind, NodeKind::Element { .. } | NodeKind::Slot { .. }))
      .collect()
  }

  fn text_children_concat(doc: &Document, parent: NodeId) -> String {
    doc
      .node(parent)
      .children
      .iter()
      .filter_map(|&id| match &doc.node(id).kind {
        NodeKind::Text { content } => Some(content.as_str()),
        _ => None,
      })
      .collect::<String>()
  }

  fn run_incremental(html_chunks: &[&str]) -> (Vec<String>, Document) {
    let mut parser = StreamingHtmlParser::new(None);
    let mut scripts: Vec<String> = Vec::new();

    for &chunk in html_chunks {
      parser.push_str(chunk);
      loop {
        match parser.pump() {
          StreamingParserYield::Script { script, .. } => {
            let doc = parser.document();
            scripts.push(text_children_concat(&doc, script));
          }
          StreamingParserYield::NeedMoreInput => break,
          StreamingParserYield::Finished { document } => return (scripts, document),
        }
      }
    }

    parser.set_eof();
    loop {
      match parser.pump() {
        StreamingParserYield::Script { script, .. } => {
          let doc = parser.document();
          scripts.push(text_children_concat(&doc, script));
        }
        StreamingParserYield::NeedMoreInput => panic!("unexpected NeedMoreInput after EOF"),
        StreamingParserYield::Finished { document } => return (scripts, document),
      }
    }
  }

  #[test]
  fn yields_two_scripts_in_document_order_then_finishes() {
    let (scripts, _doc) = run_incremental(&[
      "<!doctype html><script>a</script><p>x</p><script>b</script>",
    ]);
    assert_eq!(scripts, vec!["a".to_string(), "b".to_string()]);
  }

  #[test]
  fn chunked_input_yields_identical_scripts() {
    let (scripts_full, _doc_full) = run_incremental(&[
      "<!doctype html><script>a</script><p>x</p><script>b</script>",
    ]);

    let (scripts_chunked, _doc_chunked) = run_incremental(&[
      "<!doctype html><scr",
      "ipt>a</scr",
      "ipt><p>x</p><script>b</scr",
      "ipt>",
    ]);

    assert_eq!(scripts_chunked, scripts_full);
  }

  #[test]
  fn push_front_str_injects_before_buffered_remainder() {
    let mut parser = StreamingHtmlParser::new(None);

    // Feed markup such that after the first script yields, there is already buffered input
    // remaining in the same chunk.
    parser.push_str("<body><script>1</script><p>after</p></body>");
    match parser.pump() {
      StreamingParserYield::Script { .. } => {}
      other => panic!("expected Script yield, got {other:?}"),
    }

    // Inject markup that should be parsed before the remaining `<p>after</p>` in the input stream
    // (document.write semantics while paused for a script).
    parser.push_front_str("<p>injected</p>");
    parser.set_eof();

    let doc = loop {
      match parser.pump() {
        StreamingParserYield::NeedMoreInput => panic!("unexpected NeedMoreInput after EOF"),
        StreamingParserYield::Script { .. } => panic!("unexpected second script yield"),
        StreamingParserYield::Finished { document } => break document,
      }
    };

    let body = find_first_element(&doc, "body").expect("missing <body>");
    let body_elements = element_children(&doc, body);
    assert_eq!(body_elements.len(), 3);

    match &doc.node(body_elements[0]).kind {
      NodeKind::Element { tag_name, .. } => assert!(tag_name.eq_ignore_ascii_case("script")),
      _ => panic!("expected script element"),
    }

    let injected_p = body_elements[1];
    match &doc.node(injected_p).kind {
      NodeKind::Element { tag_name, .. } => assert!(tag_name.eq_ignore_ascii_case("p")),
      _ => panic!("expected injected <p> element"),
    }
    assert_eq!(text_children_concat(&doc, injected_p), "injected");

    let after_p = body_elements[2];
    match &doc.node(after_p).kind {
      NodeKind::Element { tag_name, .. } => assert!(tag_name.eq_ignore_ascii_case("p")),
      _ => panic!("expected after <p> element"),
    }
    assert_eq!(text_children_concat(&doc, after_p), "after");
  }

  #[test]
  fn base_url_updates_only_after_base_is_inserted() {
    let mut parser = StreamingHtmlParser::new(Some("https://example.com/doc.html"));
    parser.push_str(
      "<head><script src=a.js></script><base href=https://ex/base/><script src=b.js></script></head>",
    );
    parser.set_eof();

    match parser.pump() {
      StreamingParserYield::Script {
        base_url_at_this_point,
        ..
      } => {
        assert_eq!(
          base_url_at_this_point.as_deref(),
          Some("https://example.com/doc.html")
        );
        assert_eq!(
          parser.current_base_url().as_deref(),
          Some("https://example.com/doc.html")
        );
      }
      other => panic!("expected Script yield, got {other:?}"),
    }

    match parser.pump() {
      StreamingParserYield::Script {
        base_url_at_this_point,
        ..
      } => {
        assert_eq!(base_url_at_this_point.as_deref(), Some("https://ex/base/"));
        assert_eq!(parser.current_base_url().as_deref(), Some("https://ex/base/"));
      }
      other => panic!("expected Script yield, got {other:?}"),
    }

    match parser.pump() {
      StreamingParserYield::Finished { .. } => {}
      other => panic!("expected Finished, got {other:?}"),
    }
  }

  fn parse_and_collect_script_specs(
    html: &str,
    document_url: Option<&str>,
  ) -> Vec<ScriptElementSpec> {
    let mut parser = StreamingHtmlParser::new(document_url);
    parser.push_str(html);
    parser.set_eof();

    let mut specs = Vec::new();
    loop {
      match parser.pump() {
        StreamingParserYield::Script {
          script,
          base_url_at_this_point,
        } => {
          let doc = parser.document();
          let base = BaseUrlTracker::new(base_url_at_this_point.as_deref());
          let spec = build_parser_inserted_script_element_spec_dom2(&doc, script, &base);
          specs.push(spec);
        }
        StreamingParserYield::NeedMoreInput => panic!("unexpected NeedMoreInput after EOF"),
        StreamingParserYield::Finished { .. } => break,
      }
    }
    specs
  }

  #[test]
  fn script_before_base_href_uses_document_url() {
    let html = r#"<!doctype html><head><script src="a.js"></script><base href="https://ex/base/"></head>"#;
    let specs = parse_and_collect_script_specs(html, Some("https://ex/doc.html"));
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].base_url.as_deref(), Some("https://ex/doc.html"));
    assert_eq!(specs[0].src.as_deref(), Some("https://ex/a.js"));
  }

  #[test]
  fn script_before_base_href_without_document_url_keeps_relative_src() {
    let html = r#"<!doctype html><head><script src="a.js"></script><base href="https://ex/base/"></head>"#;
    let specs = parse_and_collect_script_specs(html, None);
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].base_url, None);
    assert_eq!(specs[0].src.as_deref(), Some("a.js"));
  }

  #[test]
  fn script_after_base_href_uses_base_url() {
    let html = r#"<!doctype html><head><base href="https://ex/base/"><script src="a.js"></script></head>"#;
    let specs = parse_and_collect_script_specs(html, Some("https://ex/doc.html"));
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].base_url.as_deref(), Some("https://ex/base/"));
    assert_eq!(specs[0].src.as_deref(), Some("https://ex/base/a.js"));
  }

  #[test]
  fn base_in_template_does_not_freeze_base_url() {
    let html = r#"<!doctype html><head>
      <template><base href="https://worse.example/"></template>
      <base href="https://good.example/">
      <script src="a.js"></script>
    </head>"#;
    let specs = parse_and_collect_script_specs(html, Some("https://ex/doc.html"));
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].base_url.as_deref(), Some("https://good.example/"));
    assert_eq!(specs[0].src.as_deref(), Some("https://good.example/a.js"));
  }

  #[test]
  fn base_in_foreign_namespace_does_not_update_base_url() {
    let html = r#"<!doctype html><body>
      <svg><foreignObject><base href="https://worse.example/"></base></foreignObject></svg>
      <script src="a.js"></script>
    </body>"#;
    let specs = parse_and_collect_script_specs(html, Some("https://ex/doc.html"));
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].base_url.as_deref(), Some("https://ex/doc.html"));
    assert_eq!(specs[0].src.as_deref(), Some("https://ex/a.js"));
  }
}
