//! Legacy, RcDom-based HTML parser pausing at parser-inserted `<script>` boundaries.
//!
//! # WARNING: legacy/testing utility
//!
//! This module exists primarily as a small, convenient harness for experiments and unit tests. It
//! drives `html5ever` using `markup5ever_rcdom::RcDom` and exposes parser suspension points
//! (`TokenizerResult::Script`) so callers can observe when a parser-inserted `<script>` element has
//! finished parsing.
//!
//! ## Fundamental limitation
//!
//! The handler is given an **immutable [`crate::dom::DomNode`] snapshot** created by converting the
//! current `RcDom` tree. Any mutation performed by script execution is therefore *not reflected* in
//! the live parser state, so this code cannot implement spec-correct parse-time DOM mutation such
//! as `document.write()` (or any other synchronous DOM changes that affect tokenization/tree
//! building).
//!
//! ## Use instead
//!
//! New work should use the streaming parse+execute pipeline:
//!
//! - HTML streaming parser driver / `dom2` TreeSink: `src/html/streaming_parser.rs`
//! - Parse-time script extraction helpers: [`crate::js::streaming`]
//!
use crate::error::{Error, ParseError, Result};
use crate::js::{determine_script_type, ScriptType};

use html5ever::tendril::StrTendril;
use html5ever::tokenizer::{BufferQueue, Tokenizer};
use html5ever::tree_builder::{TreeBuilder, TreeBuilderOpts};
use html5ever::TokenizerResult;
use markup5ever_rcdom::{Handle, NodeData, RcDom};

/// A parser-inserted `<script>` element boundary reached by the HTML parser.
///
/// The parser pauses immediately after handling the `</script>` end tag in the HTML "text"
/// insertion mode. At this point the DOM contains the `<script>` element and its text children,
/// but no markup that follows the `</script>` tag.
#[derive(Debug, Clone)]
pub struct ScriptToken {
  /// Whether the script element was inserted by the HTML parser.
  ///
  /// For this parser API, this is always `true`.
  pub parser_inserted: bool,
  /// The raw `src` attribute value, if present.
  pub src: Option<String>,
  /// Whether the `async` boolean attribute is present.
  pub async_attr: bool,
  /// Whether the `defer` boolean attribute is present.
  pub defer_attr: bool,
  /// The script type derived from the `type`/`language` attributes.
  pub script_type: ScriptType,
  /// Concatenated inline script text from child text nodes.
  pub inline_text: String,
}

/// Parse HTML with scripting enabled, pausing at parser-inserted script boundaries.
///
/// # WARNING: legacy/testing utility
///
/// This API snapshots the in-progress `RcDom` into an immutable [`crate::dom::DomNode`], so it
/// cannot implement spec-correct parse-time DOM mutation (e.g. `document.write()`).
#[cfg_attr(
  not(test),
  deprecated(
    note = "Legacy RcDom snapshot-based scripting parser; cannot support parse-time DOM mutation (e.g. document.write). Use html::streaming_parser + js::streaming pipeline (dom2 streaming parser) instead (see src/html/streaming_parser.rs)."
  )
)]
pub fn parse_html_with_scripting(
  html: &str,
  mut handler: impl FnMut(&crate::dom::DomNode, ScriptToken) -> Result<()>,
) -> Result<crate::dom::DomNode> {
  let mut parser = ScriptingHtmlParser::new();
  parser.feed(html);
  while let Some(token) = parser.run_until_script_or_eof()? {
    let snapshot = parser.snapshot_dom()?;
    handler(&snapshot, token)?;
    parser.resume();
  }
  parser.finish()
}

/// A pausable HTML parser that can stop at parser-inserted `<script>` end tags.
///
/// # WARNING: legacy/testing utility
///
/// Internally this uses `markup5ever_rcdom::RcDom` and only exposes the DOM via snapshot
/// conversion, which is incompatible with spec-correct parse-time DOM mutation.
#[cfg_attr(
  not(test),
  deprecated(
    note = "Legacy RcDom snapshot-based scripting parser; cannot support parse-time DOM mutation (e.g. document.write). Use html::streaming_parser + js::streaming pipeline (dom2 streaming parser) instead (see src/html/streaming_parser.rs)."
  )
)]
pub struct ScriptingHtmlParser {
  tokenizer: Tokenizer<TreeBuilder<Handle, RcDom>>,
  input: BufferQueue,
  done: bool,
}

impl ScriptingHtmlParser {
  pub fn new() -> Self {
    let dom = RcDom::default();
    let parse_opts = html5ever::ParseOpts {
      tree_builder: TreeBuilderOpts {
        scripting_enabled: true,
        ..Default::default()
      },
      ..Default::default()
    };

    let tb = TreeBuilder::new(dom, parse_opts.tree_builder);
    let tokenizer = Tokenizer::new(tb, parse_opts.tokenizer);
    Self {
      tokenizer,
      input: BufferQueue::default(),
      done: false,
    }
  }

  pub fn feed(&mut self, html: &str) {
    self.input.push_back(StrTendril::from(html));
  }

  /// Run the parser until a script boundary is reached or EOF.
  pub fn run_until_script_or_eof(&mut self) -> Result<Option<ScriptToken>> {
    if self.done {
      return Ok(None);
    }

    loop {
      match self.tokenizer.feed(&mut self.input) {
        TokenizerResult::Done => {
          self.done = true;
          return Ok(None);
        }
        TokenizerResult::Script(script) => {
          return Ok(Some(script_token_from_handle(&script)));
        }
      }
    }
  }

  pub fn resume(&mut self) {}

  pub fn snapshot_dom(&self) -> Result<crate::dom::DomNode> {
    // We don't yet expose the real quirks mode mid-parse; it is not needed for script pausing
    // invariants and is recovered precisely in `finish()`.
    let quirks_mode = selectors::context::QuirksMode::NoQuirks;
    let mut deadline_counter = 0usize;
    let root =
      super::convert_handle_to_node(&self.tokenizer.sink.sink.document, quirks_mode, &mut deadline_counter)?
        .ok_or_else(|| {
          Error::Parse(ParseError::InvalidHtml {
            message: "DOM conversion produced no document root node".to_string(),
            line: 0,
          })
        })?;
    Ok(root)
  }

  pub fn finish(mut self) -> Result<crate::dom::DomNode> {
    if !self.done {
      // Drain any remaining buffered input, erroring if we encounter an unhandled script pause.
      loop {
        match self.tokenizer.feed(&mut self.input) {
          TokenizerResult::Done => break,
          TokenizerResult::Script(_) => {
            return Err(Error::Parse(ParseError::InvalidHtml {
              message: "HTML parser finished while still paused at a <script> boundary".to_string(),
              line: 0,
            }))
          }
        }
      }
      self.done = true;
    }

    // Emit EOF and finalize the tree builder.
    self.tokenizer.end();

    let quirks_mode = super::map_quirks_mode(self.tokenizer.sink.sink.quirks_mode.get());

    let mut deadline_counter = 0usize;
    let mut root = super::convert_handle_to_node(
      &self.tokenizer.sink.sink.document,
      quirks_mode,
      &mut deadline_counter,
    )?
      .ok_or_else(|| {
        Error::Parse(ParseError::InvalidHtml {
          message: "DOM conversion produced no document root node".to_string(),
          line: 0,
        })
      })?;
    super::attach_shadow_roots(&mut root, &mut deadline_counter)?;
    Ok(root)
  }
}

fn node_attrs(handle: &Handle) -> Vec<(String, String)> {
  match &handle.data {
    NodeData::Element { attrs, .. } => attrs
      .borrow()
      .iter()
      .map(|attr| (attr.name.local.to_string(), attr.value.to_string()))
      .collect(),
    _ => Vec::new(),
  }
}

fn script_token_from_handle(handle: &Handle) -> ScriptToken {
  let attrs = node_attrs(handle);
  let src = attrs
    .iter()
    .find(|(k, _)| k.eq_ignore_ascii_case("src"))
    .map(|(_, v)| v.to_string());
  let async_attr = attrs.iter().any(|(k, _)| k.eq_ignore_ascii_case("async"));
  let defer_attr = attrs.iter().any(|(k, _)| k.eq_ignore_ascii_case("defer"));

  let mut inline_text = String::new();
  for child in handle.children.borrow().iter() {
    if let NodeData::Text { contents } = &child.data {
      inline_text.push_str(&contents.borrow());
    }
  }

  let script_dom = crate::dom::DomNode {
    node_type: crate::dom::DomNodeType::Element {
      tag_name: "script".to_string(),
      namespace: String::new(),
      attributes: attrs,
    },
    children: Vec::new(),
  };
  let script_type = determine_script_type(&script_dom);

  ScriptToken {
    parser_inserted: true,
    src,
    async_attr,
    defer_attr,
    script_type,
    inline_text,
  }
}

#[cfg(test)]
mod tests {
  use super::parse_html_with_scripting;
  use crate::dom::{parse_html, DomNode, DomNodeType};

  fn dom_contains_id(root: &DomNode, id: &str) -> bool {
    let mut stack: Vec<&DomNode> = vec![root];
    while let Some(node) = stack.pop() {
      if node
        .get_attribute_ref("id")
        .is_some_and(|value| value == id)
      {
        return true;
      }
      for child in node.children.iter().rev() {
        stack.push(child);
      }
    }
    false
  }

  fn find_first_tag<'a>(root: &'a DomNode, tag: &str) -> Option<&'a DomNode> {
    let mut stack: Vec<&DomNode> = vec![root];
    while let Some(node) = stack.pop() {
      if node.tag_name().is_some_and(|t| t.eq_ignore_ascii_case(tag)) {
        return Some(node);
      }
      for child in node.children.iter().rev() {
        stack.push(child);
      }
    }
    None
  }

  #[test]
  fn pauses_at_script_boundary() {
    let html = "<!doctype html><script>1</script><div id=after></div>";
    let mut saw_script = false;
    let dom = parse_html_with_scripting(html, |partial_dom, token| {
      saw_script = true;
      assert_eq!(token.inline_text, "1");
      assert!(
        !dom_contains_id(partial_dom, "after"),
        "parser should pause before parsing markup after </script>"
      );
      Ok(())
    })
    .unwrap();
    assert!(saw_script, "expected a <script> boundary pause");
    assert!(
      dom_contains_id(&dom, "after"),
      "expected parser to resume and parse markup after </script>"
    );
  }

  #[test]
  fn noscript_parsing_depends_on_scripting_enabled() {
    // Place `<noscript>` in the document body so we exercise the InBody rules.
    let html = "<!doctype html><html><body><noscript><p>hi</p></noscript></body></html>";

    // With scripting disabled (existing `parse_html`): `<noscript>` contents are parsed as markup.
    let dom_disabled = parse_html(html).unwrap();
    let noscript_disabled = find_first_tag(&dom_disabled, "noscript").expect("noscript missing");
    assert!(
      find_first_tag(noscript_disabled, "p").is_some(),
      "expected <noscript> to contain a <p> element when scripting is disabled"
    );

    // With scripting enabled: `<noscript>` follows the raw-text rules.
    let mut saw_script = false;
    let dom_enabled = parse_html_with_scripting(html, |_partial_dom, _token| {
      saw_script = true;
      Ok(())
    })
    .unwrap();
    assert!(!saw_script, "unexpected <script> pause for <noscript> input");

    let noscript_enabled = find_first_tag(&dom_enabled, "noscript").expect("noscript missing");
    assert!(
      find_first_tag(noscript_enabled, "p").is_none(),
      "expected <noscript> contents not to be parsed as markup when scripting is enabled"
    );
    assert_eq!(
      noscript_enabled.children.len(),
      1,
      "expected <noscript> to contain a single text node child when scripting is enabled"
    );
    match &noscript_enabled.children[0].node_type {
      DomNodeType::Text { content } => {
        assert_eq!(content, "<p>hi</p>");
      }
      other => panic!("expected text node child, got {other:?}"),
    }
  }
}
