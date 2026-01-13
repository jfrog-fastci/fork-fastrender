use crate::error::{Error, ParseError, Result};
use crate::html::base_url_tracker::BaseUrlTracker;
use crate::html::streaming_parser::{StreamingHtmlParser, StreamingParserYield};
use crate::js::streaming_dom2::build_parser_inserted_script_element_spec_dom2;
use crate::js::ScriptElementSpec;

use super::{Document, NodeId};

/// Parse HTML with scripting enabled, pausing at parser-inserted `<script>` boundaries.
///
/// This is a convenience wrapper around [`StreamingHtmlParser`] that:
/// - parses the provided HTML string to completion,
/// - pauses at parser-inserted `</script>` boundaries,
/// - and invokes `on_script` with a mutable reference to the *live* [`Document`] so the caller can
///   execute scripts and perform synchronous DOM mutation.
///
/// The callback is invoked immediately after the `</script>` end tag has been processed, at which
/// point the DOM contains the `<script>` element and its child text nodes but **does not** yet
/// contain any markup that appears after the `</script>` tag.
pub fn parse_html_with_scripting_dom2(
  html: &str,
  document_url: Option<&str>,
  mut on_script: impl FnMut(&mut Document, NodeId, ScriptElementSpec) -> Result<()>,
) -> Result<Document> {
  let mut parser = StreamingHtmlParser::new(document_url);
  parser.push_str(html);
  parser.set_eof();

  loop {
    match parser.pump()? {
      StreamingParserYield::Script {
        script,
        base_url_at_this_point,
      } => {
        // Build `ScriptElementSpec` using the parse-time base URL *at the script boundary*.
        //
        // This is important because `<base href>` applies only after it is parsed/inserted, so a
        // `<script>` before a later `<base>` must resolve its `src` against the older base URL.
        let spec = {
          let doc = parser.document().ok_or_else(|| {
            Error::Parse(ParseError::InvalidHtml {
              message: "HTML streaming parser has no active document sink".to_string(),
              line: 0,
            })
          })?;
          let base = BaseUrlTracker::new(base_url_at_this_point.as_deref());
          build_parser_inserted_script_element_spec_dom2(&doc, script, &base)
        };

        let mut doc = parser.document_mut().ok_or_else(|| {
          Error::Parse(ParseError::InvalidHtml {
            message: "HTML streaming parser has no active document sink".to_string(),
            line: 0,
          })
        })?;
        on_script(&mut doc, script, spec)?;
      }
      StreamingParserYield::NeedMoreInput => {
        return Err(Error::Parse(ParseError::InvalidHtml {
          message: "HTML parser requested more input after EOF".to_string(),
          line: 0,
        }));
      }
      StreamingParserYield::Finished { document } => return Ok(document),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::parse_html_with_scripting_dom2;
  use crate::dom::DomParseOptions;
  use crate::dom2::live_mutation::{LiveMutationEvent, LiveMutationTestRecorder};
  use crate::dom2::{Document, NodeId, NodeKind, RangeId};

  fn find_first_tag(doc: &Document, root: NodeId, tag: &str) -> Option<NodeId> {
    let mut stack: Vec<NodeId> = vec![root];
    while let Some(id) = stack.pop() {
      match &doc.node(id).kind {
        NodeKind::Element { tag_name, .. } if tag_name.eq_ignore_ascii_case(tag) => {
          return Some(id)
        }
        _ => {}
      }
      for &child in doc.node(id).children.iter().rev() {
        stack.push(child);
      }
    }
    None
  }

  #[test]
  fn pauses_at_script_boundary_and_resumes() {
    let html = "<!doctype html><script>1</script><div id=after></div>";
    let mut saw_script = false;
    let doc = parse_html_with_scripting_dom2(html, None, |partial_doc, script_id, spec| {
      saw_script = true;
      assert_eq!(spec.inline_text, "1");
      assert_eq!(spec.node_id, Some(script_id));
      assert!(
        partial_doc.get_element_by_id("after").is_none(),
        "parser should pause before parsing markup after </script>"
      );
      Ok(())
    })
    .unwrap();

    assert!(saw_script, "expected a <script> boundary pause");
    assert!(
      doc.get_element_by_id("after").is_some(),
      "expected parser to resume and parse markup after </script>"
    );
  }

  #[test]
  fn mutations_after_script_boundary_emit_live_mutation_hooks() {
    // This replicates the key "live ranges created during parsing must remain live" failure mode:
    // script runs while parsing is paused, registers itself for live mutation updates, and the HTML
    // parser continues mutating the *same* Document afterwards.
    //
    // `x` is foster-parented out of the table and inserted into <body> before the <table> element.
    // That insertion must go through the structured mutation APIs so the live mutation hooks fire.
    let html = "<!doctype html><table><script>1</script>x</table>";
    let recorder = LiveMutationTestRecorder::default();
    let doc = parse_html_with_scripting_dom2(html, None, |partial_doc, _script_id, _spec| {
      partial_doc.set_live_mutation_hook(Some(Box::new(recorder.clone())));
      Ok(())
    })
    .unwrap();

    let body = find_first_tag(&doc, doc.root(), "body").expect("<body> missing");
    assert_eq!(
      recorder.take(),
      vec![LiveMutationEvent::PreInsert {
        parent: body,
        index: 0,
        count: 1
      }]
    );

    // Sanity check the foster-parenting result to ensure the insertion we observed is the expected
    // one (text node before the table).
    let body_children = doc.node(body).children.clone();
    assert!(
      body_children.len() >= 2,
      "expected <body> to contain foster-parented text + table"
    );
    match &doc.node(body_children[0]).kind {
      NodeKind::Text { content } => assert_eq!(content, "x"),
      other => panic!("expected foster-parented text node, got {other:?}"),
    }
  }

  #[test]
  fn mutations_after_script_boundary_update_live_ranges() {
    // Like the live-mutation test above, but validates that live Range endpoints created while the
    // HTML parser is paused stay in sync when parsing resumes and mutates the same Document.
    //
    // This relies on the foster-parenting insertion of `x` (a text node) into <body> before the
    // <table> element after the </script> boundary.
    let html = "<!doctype html><table><script>1</script>x</table>";
    let mut range_id: Option<RangeId> = None;
    let mut body_id: Option<NodeId> = None;

    let doc = parse_html_with_scripting_dom2(html, None, |partial_doc, _script_id, _spec| {
      let body = find_first_tag(partial_doc, partial_doc.root(), "body").expect("<body> missing");
      body_id = Some(body);

      // At the </script> boundary, <body> contains only the <table> element. Offset 1 is therefore
      // the boundary point after the table.
      let range = partial_doc.create_range();
      partial_doc.range_set_start(range, body, 1).unwrap();
      range_id = Some(range);
      Ok(())
    })
    .unwrap();

    let range = range_id.expect("expected Range to be created during parsing pause");
    let body = body_id.expect("expected <body> to exist at script boundary");

    // After parsing resumes, `x` is inserted before the table, so a boundary point that was
    // previously after the table should now be offset 2 (after both children).
    assert_eq!(doc.range_start_container(range).unwrap(), body);
    assert_eq!(doc.range_end_container(range).unwrap(), body);
    assert_eq!(doc.range_start_offset(range).unwrap(), 2);
    assert_eq!(doc.range_end_offset(range).unwrap(), 2);
  }

  #[test]
  fn range_updates_account_for_shadowroot_template_promotion_after_script_pause() {
    // `dom2` promotes legacy declarative shadow DOM templates (`<template shadowroot=open>`) as a
    // post-processing step after parsing completes. If a script creates a live Range while parsing
    // is paused, that later template removal must still update its boundary-point offsets.
    //
    // This checks a subtle case: the template is removed (shifting light-DOM offsets), then a
    // ShadowRoot node is attached under the host (which must *not* count as a tree child for Range
    // offsets).
    let html = concat!(
      "<!doctype html>",
      "<div id=host>",
      "<template shadowroot=open><span>shadow</span></template>",
      "<script>1</script>",
      "<p id=after></p>",
      "</div>",
    );
    let mut range_id: Option<RangeId> = None;
    let mut host_id: Option<NodeId> = None;

    let doc = parse_html_with_scripting_dom2(html, None, |partial_doc, _script_id, _spec| {
      let host = partial_doc
        .get_element_by_id("host")
        .expect("expected host element to exist at script boundary");
      host_id = Some(host);

      // At the </script> boundary, the host has two light-DOM children: the <template> and the
      // <script>. Offset 2 is the boundary point after the <script>.
      let range = partial_doc.create_range();
      partial_doc.range_set_start(range, host, 2).unwrap();
      partial_doc.range_set_end(range, host, 2).unwrap();
      range_id = Some(range);
      Ok(())
    })
    .unwrap();

    let range = range_id.expect("expected Range to be created during parsing pause");
    let host = host_id.expect("expected host to exist at script boundary");

    // After parsing completes:
    // - The <p> is inserted after the <script>, so the boundary point remains between <script> and <p>.
    // - The `<template shadowroot=open>` is promoted: the template is removed from the light DOM,
    //   shifting offsets left by 1, but the inserted ShadowRoot must not shift offsets.
    assert_eq!(doc.range_start_container(range).unwrap(), host);
    assert_eq!(doc.range_end_container(range).unwrap(), host);
    assert_eq!(doc.range_start_offset(range).unwrap(), 1);
    assert_eq!(doc.range_end_offset(range).unwrap(), 1);
  }

  #[test]
  fn noscript_parsing_depends_on_scripting_enabled() {
    // Place `<noscript>` in the document body so we exercise the InBody rules.
    let html = "<!doctype html><html><body><noscript><p>hi</p></noscript></body></html>";

    // With scripting disabled: `<noscript>` contents are parsed as markup.
    let doc_disabled =
      crate::dom2::parse_html_with_options(html, DomParseOptions::with_scripting_enabled(false))
        .unwrap();
    let noscript_disabled =
      find_first_tag(&doc_disabled, doc_disabled.root(), "noscript").expect("noscript missing");
    assert!(
      find_first_tag(&doc_disabled, noscript_disabled, "p").is_some(),
      "expected <noscript> to contain a <p> element when scripting is disabled"
    );

    // With scripting enabled: `<noscript>` follows the raw-text rules.
    let mut saw_script = false;
    let doc_enabled = parse_html_with_scripting_dom2(html, None, |_doc, _script_id, _spec| {
      saw_script = true;
      Ok(())
    })
    .unwrap();
    assert!(
      !saw_script,
      "unexpected <script> pause for <noscript> input"
    );

    let noscript_enabled =
      find_first_tag(&doc_enabled, doc_enabled.root(), "noscript").expect("noscript missing");
    assert!(
      find_first_tag(&doc_enabled, noscript_enabled, "p").is_none(),
      "expected <noscript> contents not to be parsed as markup when scripting is enabled"
    );

    let noscript_node = doc_enabled.node(noscript_enabled);
    assert_eq!(
      noscript_node.children.len(),
      1,
      "expected <noscript> to contain a single text node child when scripting is enabled"
    );
    match &doc_enabled.node(noscript_node.children[0]).kind {
      NodeKind::Text { content } => assert_eq!(content, "<p>hi</p>"),
      other => panic!("expected text node child, got {other:?}"),
    }
  }

  #[test]
  fn script_spec_uses_base_url_at_script_boundary() {
    let html = r#"<!doctype html>
      <html><head>
        <script src="a.js"></script>
        <base href="https://ex/base/">
      </head></html>"#;
    let document_url = "https://example.com/dir/page.html";

    let mut seen = None;
    let _doc =
      parse_html_with_scripting_dom2(html, Some(document_url), |_doc, _script_id, spec| {
        seen = Some(spec);
        Ok(())
      })
      .unwrap();

    let spec = seen.expect("expected script pause");
    assert_eq!(spec.base_url.as_deref(), Some(document_url));
    assert_eq!(spec.src.as_deref(), Some("https://example.com/dir/a.js"));
  }
}
