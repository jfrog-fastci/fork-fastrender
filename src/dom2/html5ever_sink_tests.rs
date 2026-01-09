use super::Dom2TreeSink;
use super::{Document, NodeId, NodeKind};
use crate::debug::snapshot::snapshot_dom;
use crate::dom::{parse_html_with_options, DomParseOptions};
use crate::html::pausable_html5ever::{Html5everPump, PausableHtml5everParser};
use html5ever::tendril::TendrilSink;
use html5ever::tree_builder::TreeBuilderOpts;
use html5ever::{parse_document, ParseOpts};

fn parse_with_dom2_sink(html: &str) -> Document {
  let opts = ParseOpts {
    tree_builder: html5ever::tree_builder::TreeBuilderOpts {
      scripting_enabled: true,
      ..Default::default()
    },
    ..Default::default()
  };
  parse_document(Dom2TreeSink::new(None), opts).one(html.to_string())
}

#[test]
fn dom2_html5ever_sink_snapshot_matches_legacy_parser() {
  let html = concat!(
    "<!doctype html>",
    "<!-- comment should not render -->",
    "<html><head><title>x</title></head>",
    "<body><div id=a class=b>Hello<span>world</span></div></body></html>"
  );

  let legacy = parse_html_with_options(html, DomParseOptions::with_scripting_enabled(true)).unwrap();
  let doc2 = parse_with_dom2_sink(html);
  let snapshot = doc2.to_renderer_dom();

  assert_eq!(snapshot_dom(&legacy), snapshot_dom(&snapshot));
}

#[test]
fn template_contents_are_present_but_inert_for_scripting() {
  let html = concat!(
    "<!doctype html>",
    "<html><body>",
    "<template><script>inert</script><span>in</span></template>",
    "<script>live</script>",
    "</body></html>"
  );

  let doc = parse_with_dom2_sink(html);

  let template_id = doc
    .nodes()
    .iter()
    .enumerate()
    .find_map(|(idx, node)| match &node.kind {
      NodeKind::Element { tag_name, .. } if tag_name.eq_ignore_ascii_case("template") => Some(NodeId(idx)),
      _ => None,
    })
    .expect("template element not found");

  let template_node = doc.node(template_id);
  assert!(template_node.inert_subtree, "<template> should mark inert_subtree");
  assert!(
    !template_node.children.is_empty(),
    "<template> contents should be present in the tree"
  );

  let mut inert_script: Option<NodeId> = None;
  let mut live_script: Option<NodeId> = None;
  for (idx, node) in doc.nodes().iter().enumerate() {
    let NodeKind::Element { tag_name, .. } = &node.kind else {
      continue;
    };
    if !tag_name.eq_ignore_ascii_case("script") {
      continue;
    }

    let id = NodeId(idx);
    if doc.is_connected_for_scripting(id) {
      live_script = Some(id);
    } else {
      inert_script = Some(id);
    }
  }

  let inert_script = inert_script.expect("expected a script in template contents");
  let live_script = live_script.expect("expected a live script");

  assert!(
    !doc.is_connected_for_scripting(inert_script),
    "template script must not be connected for scripting"
  );
  assert!(
    doc.is_connected_for_scripting(live_script),
    "light DOM script should be connected for scripting"
  );
}

#[test]
fn tokenizer_script_handles_remain_valid_in_final_document() {
  use html5ever::tendril::StrTendril;
  use html5ever::tokenizer::{BufferQueue, Tokenizer};
  use html5ever::tree_builder::{TreeBuilder, TreeBuilderOpts};
  use html5ever::TokenizerResult;

  let html = "<!doctype html><script>1</script><script>2</script><div id=after></div>";
  let opts = ParseOpts {
    tree_builder: TreeBuilderOpts {
      scripting_enabled: true,
      ..Default::default()
    },
    ..Default::default()
  };

  let sink = Dom2TreeSink::new(None);
  let tb = TreeBuilder::new(sink, opts.tree_builder);
  let mut tokenizer = Tokenizer::new(tb, opts.tokenizer);
  let mut input = BufferQueue::default();
  input.push_back(StrTendril::from(html));

  let mut scripts: Vec<NodeId> = Vec::new();
  loop {
    match tokenizer.feed(&mut input) {
      TokenizerResult::Done => break,
      TokenizerResult::Script(handle) => scripts.push(handle),
    }
  }

  tokenizer.end();
  let doc = tokenizer.sink.sink.document();

  assert_eq!(scripts.len(), 2, "expected two <script> pause points");

  for handle in scripts {
    assert!(handle.index() < doc.nodes_len(), "script handle must exist in final Document");
    match &doc.node(handle).kind {
      NodeKind::Element { tag_name, .. } => assert!(tag_name.eq_ignore_ascii_case("script")),
      other => panic!("script handle should refer to an element node, got {other:?}"),
    }
  }
}

fn script_text(doc: &Document, script: NodeId) -> String {
  let mut text = String::new();
  for &child in &doc.node(script).children {
    if let NodeKind::Text { content } = &doc.node(child).kind {
      text.push_str(content);
    }
  }
  text
}

#[test]
fn pausable_parser_pauses_at_script_and_dom_is_partial() {
  let opts = ParseOpts {
    tree_builder: TreeBuilderOpts {
      scripting_enabled: true,
      ..Default::default()
    },
    ..Default::default()
  };

  let mut parser = PausableHtml5everParser::new_document(Dom2TreeSink::new(None), opts);
  parser.push_str("<!doctype html><script>1</script><div id=after></div>");
  parser.set_eof();

  let script_id = match parser.pump() {
    Html5everPump::Script(id) => id,
    _ => panic!("expected Script boundary"),
  };

  {
    let doc = parser.sink().document();
    assert_eq!(script_text(&doc, script_id), "1");
    assert!(
      doc.get_element_by_id("after").is_none(),
      "parser should pause before parsing markup after </script>"
    );
  }

  let doc = match parser.pump() {
    Html5everPump::Finished(doc) => doc,
    _ => panic!("expected Finished"),
  };

  assert!(
    doc.get_element_by_id("after").is_some(),
    "expected parser to resume and parse markup after </script>"
  );
}

#[test]
fn pausable_parser_yields_multiple_scripts_in_order() {
  let opts = ParseOpts {
    tree_builder: TreeBuilderOpts {
      scripting_enabled: true,
      ..Default::default()
    },
    ..Default::default()
  };

  let mut parser = PausableHtml5everParser::new_document(Dom2TreeSink::new(None), opts);
  parser.push_str("<!doctype html><script>a</script><p>x</p><script>b</script>");
  parser.set_eof();

  let s1 = match parser.pump() {
    Html5everPump::Script(id) => id,
    _ => panic!("expected first Script boundary"),
  };
  {
    let doc = parser.sink().document();
    assert_eq!(script_text(&doc, s1), "a");
  }

  let s2 = match parser.pump() {
    Html5everPump::Script(id) => id,
    _ => panic!("expected second Script boundary"),
  };
  {
    let doc = parser.sink().document();
    assert_eq!(script_text(&doc, s2), "b");
  }

  match parser.pump() {
    Html5everPump::Finished(_) => {}
    _ => panic!("expected Finished after scripts"),
  }

  assert_ne!(s1, s2);
}

#[test]
fn pausable_parser_template_script_boundaries_are_inert() {
  let opts = ParseOpts {
    tree_builder: TreeBuilderOpts {
      scripting_enabled: true,
      ..Default::default()
    },
    ..Default::default()
  };

  let mut parser = PausableHtml5everParser::new_document(Dom2TreeSink::new(None), opts);
  parser.push_str("<!doctype html><template><script>1</script></template><script>2</script>");
  parser.set_eof();

  let mut saw_connected_script = false;
  let mut saw_inert_script = false;

  loop {
    match parser.pump() {
      Html5everPump::Script(id) => {
        let doc = parser.sink().document();
        let text = script_text(&doc, id);
        if doc.is_connected_for_scripting(id) {
          saw_connected_script = true;
          assert_eq!(text, "2");
        } else {
          saw_inert_script = true;
          assert_eq!(text, "1");
        }
      }
      Html5everPump::NeedMoreInput => panic!("unexpected NeedMoreInput"),
      Html5everPump::Finished(_) => break,
    }
  }

  assert!(
    saw_connected_script,
    "expected the light DOM script to yield a connected-for-scripting pause"
  );
  // Sinks may choose to suppress script pauses for template contents; if they don't, the yielded
  // script must be inert.
  let _ = saw_inert_script;
}
