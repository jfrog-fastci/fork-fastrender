#![cfg(test)]

use super::Dom2TreeSink;
use super::live_mutation::{LiveMutationEvent, LiveMutationTestRecorder};
use super::{Document, NodeId, NodeKind, SlotAssignmentMode};
use crate::debug::snapshot::snapshot_dom;
use crate::dom::{parse_html_with_options, DomParseOptions, MATHML_NAMESPACE};
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

  let legacy =
    parse_html_with_options(html, DomParseOptions::with_scripting_enabled(true)).unwrap();
  let doc2 = parse_with_dom2_sink(html);
  let snapshot = doc2.to_renderer_dom();

  assert_eq!(snapshot_dom(&legacy), snapshot_dom(&snapshot));
}

#[test]
fn dom2_html5ever_sink_snapshot_matches_legacy_parser_with_declarative_shadow_dom() {
  let html = concat!(
    "<!doctype html>",
    "<html><body>",
    "<div id=host>",
    "<template shadowroot=open>",
    "<slot name=s></slot><span id=shadow>shadow</span>",
    "</template>",
    "<span id=light>light</span>",
    "</div>",
    "</body></html>",
  );

  let legacy =
    parse_html_with_options(html, DomParseOptions::with_scripting_enabled(true)).unwrap();
  let doc2 = parse_with_dom2_sink(html);
  let snapshot = doc2.to_renderer_dom();

  assert_eq!(snapshot_dom(&legacy), snapshot_dom(&snapshot));

  assert!(
    doc2
      .nodes()
      .iter()
      .any(|node| matches!(node.kind, NodeKind::ShadowRoot { .. })),
    "expected dom2 html5ever sink to attach declarative shadow roots"
  );
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
      NodeKind::Element { tag_name, .. } if tag_name.eq_ignore_ascii_case("template") => {
        Some(NodeId(idx))
      }
      _ => None,
    })
    .expect("template element not found");

  let template_node = doc.node(template_id);
  assert!(
    template_node.inert_subtree,
    "<template> should mark inert_subtree"
  );
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
    assert!(
      handle.index() < doc.nodes_len(),
      "script handle must exist in final Document"
    );
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

  let script_id = match parser.pump().unwrap() {
    Html5everPump::Script(id) => id,
    _ => panic!("expected Script boundary"),
  };

  {
    let doc = parser
      .sink()
      .expect("expected parser sink to be available while parsing")
      .document();
    assert_eq!(script_text(&doc, script_id), "1");
    assert!(
      doc.get_element_by_id("after").is_none(),
      "parser should pause before parsing markup after </script>"
    );
  }

  let doc = match parser.pump().unwrap() {
    Html5everPump::Finished(doc) => doc,
    _ => panic!("expected Finished"),
  };

  assert!(
    doc.get_element_by_id("after").is_some(),
    "expected parser to resume and parse markup after </script>"
  );
}

#[test]
fn pausable_parser_updates_live_ranges_on_parser_insertion_after_script_pause() {
  // Scripts can create live ranges while parsing is paused at `</script>`. When parsing resumes,
  // parser-driven insertions must update those ranges per DOM's "insert" algorithm.
  let opts = ParseOpts {
    tree_builder: TreeBuilderOpts {
      scripting_enabled: true,
      ..Default::default()
    },
    ..Default::default()
  };

  // Text inside <table> is foster-parented into <body> before the <table>.
  let mut parser = PausableHtml5everParser::new_document(Dom2TreeSink::new(None), opts);
  parser.push_str("<!doctype html><table><script>1</script>foo</table>");
  parser.set_eof();

  let _script_id = match parser.pump().unwrap() {
    Html5everPump::Script(id) => id,
    _ => panic!("expected Script boundary"),
  };

  // While paused, create a range collapsed at the end of <body> (after the <table>).
  let (range, body, original_offset) = {
    let sink = parser
      .sink()
      .expect("expected parser sink to be available while parsing");
    let mut doc = sink.document_mut();
    let body = doc.body().expect("expected <body> to exist");
    let original_offset = doc.node(body).children.len();

    let range = doc.create_range();
    doc.range_set_start(range, body, original_offset)
      .expect("setStart should succeed");
    doc.range_set_end(range, body, original_offset)
      .expect("setEnd should succeed");
    (range, body, original_offset)
  };

  let doc = match parser.pump().unwrap() {
    Html5everPump::Finished(doc) => doc,
    Html5everPump::NeedMoreInput => panic!("unexpected NeedMoreInput with EOF signalled"),
    Html5everPump::Script(_) => panic!("unexpected additional script pause"),
  };

  assert_eq!(doc.range_start_container(range).unwrap(), body);
  assert_eq!(doc.range_end_container(range).unwrap(), body);
  assert_eq!(doc.range_start_offset(range).unwrap(), original_offset + 1);
  assert_eq!(doc.range_end_offset(range).unwrap(), original_offset + 1);
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

  let s1 = match parser.pump().unwrap() {
    Html5everPump::Script(id) => id,
    _ => panic!("expected first Script boundary"),
  };
  {
    let doc = parser
      .sink()
      .expect("expected parser sink to be available while parsing")
      .document();
    assert_eq!(script_text(&doc, s1), "a");
  }

  let s2 = match parser.pump().unwrap() {
    Html5everPump::Script(id) => id,
    _ => panic!("expected second Script boundary"),
  };
  {
    let doc = parser
      .sink()
      .expect("expected parser sink to be available while parsing")
      .document();
    assert_eq!(script_text(&doc, s2), "b");
  }

  match parser.pump().unwrap() {
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
    match parser.pump().unwrap() {
      Html5everPump::Script(id) => {
        let doc = parser
          .sink()
          .expect("expected parser sink to be available while parsing")
          .document();
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

#[test]
fn mathml_annotation_xml_integration_point_allows_html_parsing_inside_annotation_xml() {
  let html = r#"<!doctype html>
    <html><body>
      <math>
        <annotation-xml encoding="text/html"><div>ok</div></annotation-xml>
      </math>
    </body></html>"#;

  let doc = parse_with_dom2_sink(html);

  let mut saw_annotation_xml = None::<NodeId>;
  let mut saw_html_div = false;

  let mut stack: Vec<NodeId> = vec![doc.root()];
  while let Some(id) = stack.pop() {
    let node = doc.node(id);
    if let NodeKind::Element {
      tag_name,
      namespace,
      ..
    } = &node.kind
    {
      if tag_name == "annotation-xml" && namespace == MATHML_NAMESPACE {
        saw_annotation_xml = Some(id);
      }
      if tag_name == "div" && namespace.is_empty() {
        saw_html_div = true;
      }
    }
    for &child in node.children.iter().rev() {
      stack.push(child);
    }
  }

  let annotation_xml_id = saw_annotation_xml.expect("expected MathML annotation-xml element");
  assert!(
    doc
      .node(annotation_xml_id)
      .mathml_annotation_xml_integration_point,
    "annotation-xml node should be marked as a MathML annotation-xml integration point"
  );
  assert!(
    saw_html_div,
    "expected <div> inside annotation-xml to be parsed as an HTML element"
  );
}

fn find_shadow_root_child(doc: &Document, host: NodeId) -> Option<NodeId> {
  doc
    .node(host)
    .children
    .iter()
    .copied()
    .find(|&child| matches!(doc.node(child).kind, NodeKind::ShadowRoot { .. }))
}

fn find_node_by_id_attribute(doc: &Document, needle: &str) -> Option<NodeId> {
  if needle.is_empty() {
    return None;
  }
  doc.nodes().iter().enumerate().find_map(|(idx, node)| {
    let attrs = match &node.kind {
      NodeKind::Element { attributes, .. } | NodeKind::Slot { attributes, .. } => attributes,
      _ => return None,
    };
    attrs
      .iter()
      .any(|(name, value)| name.eq_ignore_ascii_case("id") && value == needle)
      .then_some(NodeId(idx))
  })
}

#[test]
fn declarative_shadow_rootmode_attaches_and_routes_template_contents() {
  use crate::dom::ShadowRootMode;

  let doc = parse_with_dom2_sink(
    "<!doctype html><div id=host><template shadowrootmode=open shadowrootdelegatesfocus><slot></slot></template><p id=light>light</p></div>",
  );

  let host = doc.get_element_by_id("host").expect("host element missing");
  let shadow_root = find_shadow_root_child(&doc, host).expect("expected ShadowRoot child");

  assert_eq!(
    doc.node(host).children.first().copied(),
    Some(shadow_root),
    "ShadowRoot should be the first child of the host"
  );
  assert!(
    doc
      .node(host)
      .children
      .iter()
      .all(|&child| child == shadow_root
        || !matches!(doc.node(child).kind, NodeKind::ShadowRoot { .. })),
    "host should only have one shadow root child"
  );

  match &doc.node(shadow_root).kind {
    NodeKind::ShadowRoot {
      mode,
      delegates_focus,
      slot_assignment,
      clonable,
      serializable,
      ..
    } => {
      assert_eq!(*mode, ShadowRootMode::Open);
      assert!(
        *delegates_focus,
        "shadowrootdelegatesfocus should set delegates_focus"
      );
      assert_eq!(
        *slot_assignment,
        SlotAssignmentMode::Named,
        "declarative shadow roots should default to named slot assignment mode"
      );
      assert!(!*clonable, "declarative shadow roots should default clonable=false");
      assert!(
        !*serializable,
        "declarative shadow roots should default serializable=false"
      );
    }
    other => panic!("expected ShadowRoot kind, got {other:?}"),
  }

  assert!(
    doc
      .node(shadow_root)
      .children
      .iter()
      .any(|&child| matches!(doc.node(child).kind, NodeKind::Slot { .. })),
    "expected <slot> inside shadow root"
  );

  let light_p = doc.get_element_by_id("light").expect("light <p> missing");
  assert_eq!(doc.node(light_p).parent, Some(host));
  assert_ne!(doc.node(light_p).parent, Some(shadow_root));
}

#[test]
fn second_shadowrootmode_template_falls_back_to_inert_template_element() {
  let doc = parse_with_dom2_sink(
    "<!doctype html><div id=host>\
     <template shadowrootmode=open><p>shadow</p></template>\
     <template shadowrootmode=open><script id=second></script></template>\
     </div>",
  );

  let host = doc.get_element_by_id("host").expect("host element missing");
  let shadow_root = find_shadow_root_child(&doc, host).expect("expected ShadowRoot child");

  let templates: Vec<NodeId> = doc
    .node(host)
    .children
    .iter()
    .copied()
    .filter(|&child| match &doc.node(child).kind {
      NodeKind::Element { tag_name, .. } => tag_name.eq_ignore_ascii_case("template"),
      _ => false,
    })
    .collect();

  assert_eq!(
    templates.len(),
    1,
    "expected the second <template> to remain in the light DOM"
  );
  let template = templates[0];
  assert!(
    doc.node(template).inert_subtree,
    "fallback <template> contents must be inert"
  );

  let second_script =
    find_node_by_id_attribute(&doc, "second").expect("script inside fallback template missing");
  assert!(
    !doc.is_connected_for_scripting(second_script),
    "script inside inert fallback template should not be connected for scripting"
  );
  assert_ne!(
    doc.node(second_script).parent,
    Some(shadow_root),
    "fallback template contents must not be inserted into the shadow root"
  );
}

#[test]
fn pausable_parser_attaches_shadowrootmode_during_parse_before_script_pause() {
  use crate::js::orchestrator::{
    CurrentScriptHost, CurrentScriptStateHandle, ScriptBlockExecutor, ScriptOrchestrator,
  };
  use crate::js::{DomHost, ScriptType};

  #[derive(Clone)]
  struct Host {
    dom: Document,
    script_state: CurrentScriptStateHandle,
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

  impl CurrentScriptHost for Host {
    fn current_script_state(&self) -> &CurrentScriptStateHandle {
      &self.script_state
    }
  }

  #[derive(Default)]
  struct RecordingExecutor {
    observed: Vec<Option<NodeId>>,
  }

  impl ScriptBlockExecutor<Host> for RecordingExecutor {
    fn execute_script(
      &mut self,
      host: &mut Host,
      _orchestrator: &mut ScriptOrchestrator,
      _script: NodeId,
      _script_type: ScriptType,
    ) -> crate::Result<()> {
      self.observed.push(host.current_script());
      Ok(())
    }
  }

  let opts = ParseOpts {
    tree_builder: TreeBuilderOpts {
      scripting_enabled: true,
      ..Default::default()
    },
    ..Default::default()
  };

  let mut parser = PausableHtml5everParser::new_document(Dom2TreeSink::new(None), opts);
  parser.push_str(
    "<!doctype html><div id=host><template shadowrootmode=open><script id=s>1</script></template></div>",
  );
  parser.set_eof();

  let script_id = match parser.pump().unwrap() {
    Html5everPump::Script(id) => id,
    _ => panic!("expected Script boundary"),
  };

  let dom = parser
    .sink()
    .expect("expected parser sink to be available while parsing")
    .document()
    .to_owned();
  assert_eq!(script_text(&dom, script_id), "1");
  assert!(
    dom.is_connected_for_scripting(script_id),
    "script inside declarative shadow root should be connected for scripting"
  );

  let host_id = dom.get_element_by_id("host").expect("host element missing");
  let shadow_root = find_shadow_root_child(&dom, host_id).expect("expected ShadowRoot child");
  assert!(
    dom
      .ancestors(script_id)
      .any(|ancestor| ancestor == shadow_root),
    "expected paused <script> to be a descendant of the attached ShadowRoot"
  );

  // Also assert `Document.currentScript` semantics for classic scripts inside shadow trees.
  let mut host = Host {
    dom,
    script_state: CurrentScriptStateHandle::default(),
  };
  let mut orchestrator = ScriptOrchestrator::new();
  let mut executor = RecordingExecutor::default();
  orchestrator
    .execute_script_element(&mut host, script_id, ScriptType::Classic, &mut executor)
    .unwrap();
  assert_eq!(
    executor.observed,
    vec![None],
    "Document.currentScript must be null for classic scripts in shadow trees"
  );
}

#[test]
fn declarative_shadow_dom_insertion_emits_live_pre_insert_hook() {
  use html5ever::tendril::StrTendril;
  use html5ever::tree_builder::TreeSink;
  use markup5ever::interface::Attribute;
  use markup5ever::{LocalName, Namespace, QualName};

  let sink = Dom2TreeSink::new(None);
  let recorder = LiveMutationTestRecorder::default();

  let (host, template) = {
    let mut doc = sink.document_mut();
    doc.set_live_mutation_hook(Some(Box::new(recorder.clone())));

    let host = doc.push_node(
      NodeKind::Element {
        tag_name: "div".to_string(),
        namespace: String::new(),
        prefix: None,
        attributes: Vec::new(),
      },
      None,
      /* inert_subtree */ false,
    );
    let template = doc.push_node(
      NodeKind::Element {
        tag_name: "template".to_string(),
        namespace: String::new(),
        prefix: None,
        attributes: Vec::new(),
      },
      None,
      /* inert_subtree */ false,
    );
    (host, template)
  };

  let attrs = vec![Attribute {
    name: QualName::new(None, Namespace::from(""), LocalName::from("shadowrootmode")),
    value: StrTendril::from("open"),
  }];

  assert!(
    sink.attach_declarative_shadow(&host, &template, &attrs),
    "expected declarative shadow root attachment to succeed"
  );

  assert_eq!(
    recorder.take(),
    vec![LiveMutationEvent::PreInsert {
      parent: host,
      index: 0,
      count: 1,
    }]
  );
}

#[test]
fn declarative_shadow_dom_insertion_does_not_shift_live_range_offsets() {
  use html5ever::tendril::StrTendril;
  use html5ever::tree_builder::TreeSink;
  use markup5ever::interface::Attribute;
  use markup5ever::{LocalName, Namespace, QualName};

  let sink = Dom2TreeSink::new(None);

  // Pre-populate the host with a single light-DOM child so a boundary point at offset 1 would shift
  // if the ShadowRoot insertion were incorrectly counted as a tree child.
  let (host, template, range) = {
    let mut doc = sink.document_mut();

    let host = doc.push_node(
      NodeKind::Element {
        tag_name: "div".to_string(),
        namespace: String::new(),
        prefix: None,
        attributes: Vec::new(),
      },
      None,
      /* inert_subtree */ false,
    );
    let _light_child = doc.push_node(
      NodeKind::Element {
        tag_name: "span".to_string(),
        namespace: String::new(),
        prefix: None,
        attributes: Vec::new(),
      },
      Some(host),
      /* inert_subtree */ false,
    );

    let template = doc.push_node(
      NodeKind::Element {
        tag_name: "template".to_string(),
        namespace: String::new(),
        prefix: None,
        attributes: Vec::new(),
      },
      None,
      /* inert_subtree */ false,
    );

    let range = doc.create_range();
    doc
      .range_set_start(range, host, 1)
      .expect("expected setStart to succeed");
    doc
      .range_set_end(range, host, 1)
      .expect("expected setEnd to succeed");

    (host, template, range)
  };

  let attrs = vec![Attribute {
    name: QualName::new(None, Namespace::from(""), LocalName::from("shadowrootmode")),
    value: StrTendril::from("open"),
  }];

  assert!(
    sink.attach_declarative_shadow(&host, &template, &attrs),
    "expected declarative shadow root attachment to succeed"
  );

  let doc = sink.document();
  assert_eq!(doc.range_start_container(range).unwrap(), host);
  assert_eq!(doc.range_end_container(range).unwrap(), host);
  assert_eq!(doc.range_start_offset(range).unwrap(), 1);
  assert_eq!(doc.range_end_offset(range).unwrap(), 1);
}

#[test]
fn append_text_emits_live_replace_data_hook() {
  use html5ever::tendril::StrTendril;
  use html5ever::tree_builder::NodeOrText;
  use html5ever::tree_builder::TreeSink;

  let sink = Dom2TreeSink::new(None);
  let recorder = LiveMutationTestRecorder::default();

  let parent = {
    let mut doc = sink.document_mut();
    doc.set_live_mutation_hook(Some(Box::new(recorder.clone())));

    let root = doc.root();
    doc.push_node(
      NodeKind::Element {
        tag_name: "div".to_string(),
        namespace: String::new(),
        prefix: None,
        attributes: Vec::new(),
      },
      Some(root),
      /* inert_subtree */ false,
    )
  };
  let _ = recorder.take();

  sink.append(&parent, NodeOrText::AppendText(StrTendril::from("hi")));
  let text_id = {
    let doc = sink.document();
    doc.node(parent).children[0]
  };
  sink.append(&parent, NodeOrText::AppendText(StrTendril::from("bye")));

  assert_eq!(
    recorder.take(),
    vec![
      LiveMutationEvent::PreInsert {
        parent,
        index: 0,
        count: 1,
      },
      LiveMutationEvent::ReplaceData {
        node: text_id,
        offset: 2,
        removed_len: 0,
        inserted_len: 3,
      },
    ]
  );
}
