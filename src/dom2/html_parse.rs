use crate::dom::{DomCompatibilityMode, DomParseOptions};
use crate::error::{Error, ParseError, Result};
use crate::html::streaming_parser::{StreamingHtmlParser, StreamingParserYield};

use super::Document;

/// Parse HTML directly into a [`dom2::Document`](crate::dom2::Document).
///
/// This is the dom2-first equivalent of [`crate::dom::parse_html_with_options`], backed by html5ever
/// driving the `dom2` html5ever `TreeSink` (see [`crate::dom2::Dom2TreeSink`]).
///
/// Notes:
/// - Script execution is not performed. When the underlying parser yields at `<script>` boundaries,
///   those yields are ignored and parsing continues.
/// - `options.scripting_enabled` controls html5ever parsing semantics (not execution); it primarily
///   affects parsing of elements such as `<noscript>`.
pub fn parse_html_with_options(html: &str, options: DomParseOptions) -> Result<Document> {
  let mut parser =
    StreamingHtmlParser::new_with_scripting_enabled(/* document_url */ None, options.scripting_enabled);
  parser.push_str(html);
  parser.set_eof();

  let mut document = loop {
    match parser.pump() {
      StreamingParserYield::Script { .. } => {
        // Pure parsing: ignore script execution and continue parsing.
      }
      StreamingParserYield::NeedMoreInput => {
        return Err(Error::Parse(ParseError::InvalidHtml {
          message: "HTML parser requested more input after EOF".to_string(),
          line: 0,
        }));
      }
      StreamingParserYield::Finished { document } => break document,
    }
  };

  document.attach_shadow_roots();

  if matches!(options.compatibility_mode, DomCompatibilityMode::Compatibility) {
    // Reuse the existing compatibility mutation logic (implemented for the renderer's immutable
    // `DomNode` tree) by snapshotting to a renderer DOM, applying mutations, and importing back.
    let mut snapshot = document.to_renderer_dom();
    let mut deadline_counter = 0usize;
    crate::dom::apply_dom_compatibility_mutations(&mut snapshot, &mut deadline_counter)?;
    document = Document::from_renderer_dom(&snapshot);
  }

  Ok(document)
}

/// Parse HTML into a [`dom2::Document`](crate::dom2::Document) with default [`DomParseOptions`].
pub fn parse_html(html: &str) -> Result<Document> {
  parse_html_with_options(html, DomParseOptions::default())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::debug::snapshot::snapshot_dom;

  #[test]
  fn parse_html_dom2_roundtrips_to_renderer_snapshot() {
    let html = concat!(
      "<!doctype html>",
      "<html><head><title>x</title></head><body>",
      "<div id=a>Hello<wbr>world</div>",
      "<template id=t>",
      "<span>tmpl</span>",
      "<svg><circle cx=1 cy=2 r=3></circle></svg>",
      "</template>",
      "<svg viewBox='0 0 10 10'><text>hi</text></svg>",
      "</body></html>",
    );

    let dom = crate::dom::parse_html(html).unwrap();
    let doc2 = parse_html(html).unwrap();
    let dom2_snapshot = doc2.to_renderer_dom();
    assert_eq!(snapshot_dom(&dom), snapshot_dom(&dom2_snapshot));
  }
}

