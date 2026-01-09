use fastrender::dom::enumerate_dom_ids;
use fastrender::dom::DomNode;
use fastrender::dom::DomNodeType;
use fastrender::dom::HTML_NAMESPACE;
use fastrender::geometry::Point;
use fastrender::geometry::Rect;
use fastrender::interaction::InteractionAction;
use fastrender::interaction::InteractionEngine;
use fastrender::interaction::KeyAction;
use fastrender::style::display::FormattingContextType;
use fastrender::style::ComputedStyle;
use fastrender::style::types::PointerEvents;
use fastrender::tree::box_tree::BoxNode;
use fastrender::tree::box_tree::BoxTree;
use fastrender::tree::fragment_tree::FragmentNode;
use fastrender::tree::fragment_tree::FragmentTree;
use selectors::context::QuirksMode;
use std::sync::Arc;
use url::Url;

fn doc(children: Vec<DomNode>) -> DomNode {
  DomNode {
    node_type: DomNodeType::Document {
      quirks_mode: QuirksMode::NoQuirks,
    },
    children,
  }
}

fn el(tag: &str, attrs: Vec<(&str, &str)>, children: Vec<DomNode>) -> DomNode {
  DomNode {
    node_type: DomNodeType::Element {
      tag_name: tag.to_string(),
      namespace: HTML_NAMESPACE.to_string(),
      attributes: attrs
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect(),
    },
    children,
  }
}

fn find_by_id<'a>(root: &'a DomNode, html_id: &str) -> Option<&'a DomNode> {
  let mut stack = vec![root];
  while let Some(node) = stack.pop() {
    if node.get_attribute_ref("id") == Some(html_id) {
      return Some(node);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  None
}

fn attr_value(root: &DomNode, html_id: &str, attr: &str) -> Option<String> {
  find_by_id(root, html_id)
    .and_then(|node| node.get_attribute_ref(attr))
    .map(|v| v.to_string())
}

fn has_attr(root: &DomNode, html_id: &str, attr: &str) -> bool {
  find_by_id(root, html_id)
    .and_then(|node| node.get_attribute_ref(attr))
    .is_some()
}

fn node_id(root: &DomNode, html_id: &str) -> usize {
  let ids = enumerate_dom_ids(root);
  let node = find_by_id(root, html_id).expect("node");
  ids
    .get(&(node as *const DomNode))
    .copied()
    .expect("id present")
}

fn default_style() -> Arc<ComputedStyle> {
  Arc::new(ComputedStyle::default())
}

fn style_with_pointer_events(pointer_events: PointerEvents) -> Arc<ComputedStyle> {
  let mut style = ComputedStyle::default();
  style.pointer_events = pointer_events;
  Arc::new(style)
}

fn find_box_id_for_styled_node(box_tree: &BoxTree, styled_node_id: usize) -> usize {
  let mut stack = vec![&box_tree.root];
  while let Some(node) = stack.pop() {
    if node.styled_node_id == Some(styled_node_id) {
      return node.id;
    }
    if let Some(body) = node.footnote_body.as_deref() {
      stack.push(body);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  panic!("no box found for styled_node_id={styled_node_id}");
}

#[test]
fn radio_click_is_scoped_to_nearest_form() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![
        el(
          "form",
          vec![],
          vec![
            el(
              "input",
              vec![("id", "a1"), ("type", "radio"), ("name", "g"), ("checked", "")],
              vec![],
            ),
            el(
              "input",
              vec![("id", "a2"), ("type", "radio"), ("name", "g")],
              vec![],
            ),
          ],
        ),
        el(
          "form",
          vec![],
          vec![el(
            "input",
            vec![("id", "b1"), ("type", "radio"), ("name", "g"), ("checked", "")],
            vec![],
          )],
        ),
      ],
    )],
  )]);

  let a2_dom_id = node_id(&dom, "a2");

  let mut a2_box = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
  a2_box.styled_node_id = Some(a2_dom_id);
  let box_tree = BoxTree::new(BoxNode::new_block(
    default_style(),
    FormattingContextType::Block,
    vec![a2_box],
  ));
  let a2_box_id = find_box_id_for_styled_node(&box_tree, a2_dom_id);

  let fragment_tree = FragmentTree::new(FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
    vec![FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
      a2_box_id,
      vec![],
    )],
  ));

  let mut engine = InteractionEngine::new();
  assert!(
    engine.pointer_down(&mut dom, &box_tree, &fragment_tree, Point::new(10.0, 10.0)),
    "expected pointer_down to set active state"
  );
  let (changed, action) = engine.pointer_up(
    &mut dom,
    &box_tree,
    &fragment_tree,
    Point::new(10.0, 10.0),
    "https://x/",
  );
  assert!(changed);
  assert_eq!(action, InteractionAction::None);

  assert!(
    !has_attr(&dom, "a1", "checked"),
    "radio in same form should be unchecked"
  );
  assert!(
    has_attr(&dom, "a2", "checked"),
    "clicked radio should be checked"
  );
  assert!(
    has_attr(&dom, "b1", "checked"),
    "radio in a different form should remain checked"
  );
}

#[test]
fn hover_chain_applies_to_ancestors() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![el(
        "div",
        vec![("id", "outer")],
        vec![el("span", vec![("id", "inner")], vec![])],
      )],
    )],
  )]);

  let inner_dom_id = node_id(&dom, "inner");
  let outer_dom_id = node_id(&dom, "outer");

  let mut inner_box = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
  inner_box.styled_node_id = Some(inner_dom_id);
  let mut outer_box = BoxNode::new_block(
    default_style(),
    FormattingContextType::Block,
    vec![inner_box],
  );
  outer_box.styled_node_id = Some(outer_dom_id);
  let box_tree = BoxTree::new(BoxNode::new_block(
    default_style(),
    FormattingContextType::Block,
    vec![outer_box],
  ));

  let outer_box_id = find_box_id_for_styled_node(&box_tree, outer_dom_id);
  let inner_box_id = find_box_id_for_styled_node(&box_tree, inner_dom_id);

  let fragment_tree = FragmentTree::new(FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
    vec![FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
      outer_box_id,
      vec![FragmentNode::new_block_with_id(
        Rect::from_xywh(10.0, 10.0, 20.0, 20.0),
        inner_box_id,
        vec![],
      )],
    )],
  ));

  let mut engine = InteractionEngine::new();
  assert!(
    engine.pointer_move(&mut dom, &box_tree, &fragment_tree, Point::new(15.0, 15.0)),
    "pointer_move should set hover attrs"
  );
  for id in ["inner", "outer", "body", "html"] {
    assert_eq!(
      attr_value(&dom, id, "data-fastr-hover").as_deref(),
      Some("true"),
      "{id} should be hovered"
    );
  }

  assert!(
    engine.pointer_move(
      &mut dom,
      &box_tree,
      &fragment_tree,
      Point::new(150.0, 150.0)
    ),
    "moving off target should clear hover attrs"
  );
  for id in ["inner", "outer", "body", "html"] {
    assert!(
      !has_attr(&dom, id, "data-fastr-hover"),
      "{id} hover should be cleared"
    );
  }
}

#[test]
fn active_chain_sets_on_down_and_clears_on_up() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![el(
        "div",
        vec![("id", "outer")],
        vec![el("span", vec![("id", "inner")], vec![])],
      )],
    )],
  )]);

  let inner_dom_id = node_id(&dom, "inner");
  let outer_dom_id = node_id(&dom, "outer");

  let mut inner_box = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
  inner_box.styled_node_id = Some(inner_dom_id);
  let mut outer_box = BoxNode::new_block(
    default_style(),
    FormattingContextType::Block,
    vec![inner_box],
  );
  outer_box.styled_node_id = Some(outer_dom_id);
  let box_tree = BoxTree::new(BoxNode::new_block(
    default_style(),
    FormattingContextType::Block,
    vec![outer_box],
  ));

  let outer_box_id = find_box_id_for_styled_node(&box_tree, outer_dom_id);
  let inner_box_id = find_box_id_for_styled_node(&box_tree, inner_dom_id);

  let fragment_tree = FragmentTree::new(FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
    vec![FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
      outer_box_id,
      vec![FragmentNode::new_block_with_id(
        Rect::from_xywh(10.0, 10.0, 20.0, 20.0),
        inner_box_id,
        vec![],
      )],
    )],
  ));

  let mut engine = InteractionEngine::new();
  assert!(
    engine.pointer_down(&mut dom, &box_tree, &fragment_tree, Point::new(15.0, 15.0)),
    "pointer_down should set active attrs"
  );
  for id in ["inner", "outer", "body", "html"] {
    assert_eq!(
      attr_value(&dom, id, "data-fastr-active").as_deref(),
      Some("true"),
      "{id} should be active"
    );
  }

  let (changed, action) = engine.pointer_up(
    &mut dom,
    &box_tree,
    &fragment_tree,
    Point::new(15.0, 15.0),
    "https://x/",
  );
  assert!(changed);
  assert_eq!(action, InteractionAction::None);

  for id in ["inner", "outer", "body", "html"] {
    assert!(
      !has_attr(&dom, id, "data-fastr-active"),
      "{id} active should be cleared"
    );
  }
}

#[test]
fn link_click_emits_navigation_with_resolved_url() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![el("a", vec![("id", "link"), ("href", "foo")], vec![])],
    )],
  )]);

  let link_dom_id = node_id(&dom, "link");
  let mut link_box = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
  link_box.styled_node_id = Some(link_dom_id);
  let box_tree = BoxTree::new(BoxNode::new_block(
    default_style(),
    FormattingContextType::Block,
    vec![link_box],
  ));

  let link_box_id = find_box_id_for_styled_node(&box_tree, link_dom_id);
  let fragment_tree = FragmentTree::new(FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
    vec![FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 50.0, 50.0),
      link_box_id,
      vec![],
    )],
  ));

  let mut engine = InteractionEngine::new();
  engine.pointer_down(&mut dom, &box_tree, &fragment_tree, Point::new(10.0, 10.0));
  let (changed, action) = engine.pointer_up(
    &mut dom,
    &box_tree,
    &fragment_tree,
    Point::new(10.0, 10.0),
    "https://example.com/base/",
  );
  assert!(changed);
  assert_eq!(
    action,
    InteractionAction::Navigate {
      href: "https://example.com/base/foo".to_string()
    }
  );
  assert_eq!(
    attr_value(&dom, "link", "data-fastr-visited").as_deref(),
    Some("true")
  );
}

#[test]
fn link_click_trims_ascii_whitespace_but_preserves_nbsp() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![el("a", vec![("id", "link"), ("href", " \u{00A0} ")], vec![])],
    )],
  )]);

  let link_dom_id = node_id(&dom, "link");
  let mut link_box = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
  link_box.styled_node_id = Some(link_dom_id);
  let box_tree = BoxTree::new(BoxNode::new_block(
    default_style(),
    FormattingContextType::Block,
    vec![link_box],
  ));

  let link_box_id = find_box_id_for_styled_node(&box_tree, link_dom_id);
  let fragment_tree = FragmentTree::new(FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
    vec![FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 50.0, 50.0),
      link_box_id,
      vec![],
    )],
  ));

  let mut engine = InteractionEngine::new();
  engine.pointer_down(&mut dom, &box_tree, &fragment_tree, Point::new(10.0, 10.0));
  let (_, action) = engine.pointer_up(
    &mut dom,
    &box_tree,
    &fragment_tree,
    Point::new(10.0, 10.0),
    "https://example.com/base/",
  );

  let expected = Url::parse("https://example.com/base/")
    .unwrap()
    .join("\u{00A0}")
    .unwrap()
    .to_string();
  assert_eq!(action, InteractionAction::Navigate { href: expected });
  assert_eq!(
    attr_value(&dom, "link", "data-fastr-visited").as_deref(),
    Some("true")
  );
}

#[test]
fn link_click_with_non_ascii_href_does_not_panic() {
  let href = "\u{00E9}\u{00E9}\u{00E9}\u{00E9}\u{00E9}\u{00E9}";
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![el("a", vec![("id", "link"), ("href", href)], vec![])],
    )],
  )]);

  let link_dom_id = node_id(&dom, "link");
  let mut link_box = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
  link_box.styled_node_id = Some(link_dom_id);
  let box_tree = BoxTree::new(BoxNode::new_block(
    default_style(),
    FormattingContextType::Block,
    vec![link_box],
  ));

  let link_box_id = find_box_id_for_styled_node(&box_tree, link_dom_id);
  let fragment_tree = FragmentTree::new(FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
    vec![FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 50.0, 50.0),
      link_box_id,
      vec![],
    )],
  ));

  let mut engine = InteractionEngine::new();
  engine.pointer_down(&mut dom, &box_tree, &fragment_tree, Point::new(10.0, 10.0));
  let (_, action) = engine.pointer_up(
    &mut dom,
    &box_tree,
    &fragment_tree,
    Point::new(10.0, 10.0),
    "https://example.com/base/",
  );

  let expected = Url::parse("https://example.com/base/")
    .unwrap()
    .join(href)
    .unwrap()
    .to_string();
  assert_eq!(action, InteractionAction::Navigate { href: expected });
  assert_eq!(
    attr_value(&dom, "link", "data-fastr-visited").as_deref(),
    Some("true")
  );
}

#[test]
fn checkbox_click_toggles_checked_attribute() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![el(
        "input",
        vec![("id", "cb"), ("type", "checkbox")],
        vec![],
      )],
    )],
  )]);

  let cb_dom_id = node_id(&dom, "cb");
  let mut cb_box = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
  cb_box.styled_node_id = Some(cb_dom_id);
  let box_tree = BoxTree::new(BoxNode::new_block(
    default_style(),
    FormattingContextType::Block,
    vec![cb_box],
  ));

  let cb_box_id = find_box_id_for_styled_node(&box_tree, cb_dom_id);
  let fragment_tree = FragmentTree::new(FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
    vec![FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 20.0, 20.0),
      cb_box_id,
      vec![],
    )],
  ));

  let mut engine = InteractionEngine::new();
  engine.pointer_down(&mut dom, &box_tree, &fragment_tree, Point::new(5.0, 5.0));
  let (changed, action) = engine.pointer_up(
    &mut dom,
    &box_tree,
    &fragment_tree,
    Point::new(5.0, 5.0),
    "https://x/",
  );
  assert!(changed);
  assert_eq!(action, InteractionAction::None);
  assert!(has_attr(&dom, "cb", "checked"));
}

#[test]
fn label_click_activates_associated_checkbox() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![
        el("label", vec![("id", "lbl"), ("for", "cb")], vec![]),
        el("input", vec![("id", "cb"), ("type", "checkbox")], vec![]),
      ],
    )],
  )]);

  let label_dom_id = node_id(&dom, "lbl");
  let cb_dom_id = node_id(&dom, "cb");

  let mut label_box = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
  label_box.styled_node_id = Some(label_dom_id);
  let mut cb_box = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
  cb_box.styled_node_id = Some(cb_dom_id);

  let box_tree = BoxTree::new(BoxNode::new_block(
    default_style(),
    FormattingContextType::Block,
    vec![label_box, cb_box],
  ));

  let label_box_id = find_box_id_for_styled_node(&box_tree, label_dom_id);
  let fragment_tree = FragmentTree::new(FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
    vec![
      FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 0.0, 40.0, 20.0), label_box_id, vec![]),
      // Checkbox fragment exists but we won't click it in this test.
      FragmentNode::new_block_with_id(
        Rect::from_xywh(0.0, 40.0, 20.0, 20.0),
        find_box_id_for_styled_node(&box_tree, cb_dom_id),
        vec![],
      ),
    ],
  ));

  let mut engine = InteractionEngine::new();
  engine.pointer_down(&mut dom, &box_tree, &fragment_tree, Point::new(5.0, 5.0));
  let (changed, action) = engine.pointer_up(
    &mut dom,
    &box_tree,
    &fragment_tree,
    Point::new(5.0, 5.0),
    "https://x/",
  );
  assert!(changed);
  assert_eq!(action, InteractionAction::None);
  assert!(has_attr(&dom, "cb", "checked"));
}

#[test]
fn typing_updates_focused_input_value_and_sets_focus_visible() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![el("input", vec![("id", "txt"), ("value", "")], vec![])],
    )],
  )]);

  let input_dom_id = node_id(&dom, "txt");
  let mut input_box = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
  input_box.styled_node_id = Some(input_dom_id);
  let box_tree = BoxTree::new(BoxNode::new_block(
    default_style(),
    FormattingContextType::Block,
    vec![input_box],
  ));

  let input_box_id = find_box_id_for_styled_node(&box_tree, input_dom_id);
  let fragment_tree = FragmentTree::new(FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
    vec![FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 80.0, 20.0),
      input_box_id,
      vec![],
    )],
  ));

  let mut engine = InteractionEngine::new();
  engine.pointer_down(&mut dom, &box_tree, &fragment_tree, Point::new(5.0, 5.0));
  let (changed, action) = engine.pointer_up(
    &mut dom,
    &box_tree,
    &fragment_tree,
    Point::new(5.0, 5.0),
    "https://x/",
  );
  assert!(changed);
  assert!(
    matches!(
      action,
      InteractionAction::FocusChanged { node_id: Some(_) } | InteractionAction::None
    ),
    "pointer_up may emit FocusChanged"
  );
  assert_eq!(
    attr_value(&dom, "txt", "data-fastr-focus").as_deref(),
    Some("true")
  );
  assert!(
    !has_attr(&dom, "txt", "data-fastr-focus-visible"),
    "pointer focus should not set focus-visible"
  );

  assert!(
    engine.text_input(&mut dom, "abc"),
    "text_input should mutate the DOM"
  );
  assert_eq!(attr_value(&dom, "txt", "value").as_deref(), Some("abc"));
  assert_eq!(
    attr_value(&dom, "txt", "data-fastr-focus-visible").as_deref(),
    Some("true")
  );
}

#[test]
fn pointer_events_none_overlay_does_not_block_link_hover_or_click() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![
        el("a", vec![("id", "link"), ("href", "foo")], vec![]),
        el("div", vec![("id", "overlay")], vec![]),
      ],
    )],
  )]);

  let link_dom_id = node_id(&dom, "link");
  let overlay_dom_id = node_id(&dom, "overlay");

  let mut link_box = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
  link_box.styled_node_id = Some(link_dom_id);

  let mut overlay_box = BoxNode::new_block(
    style_with_pointer_events(PointerEvents::None),
    FormattingContextType::Block,
    vec![],
  );
  overlay_box.styled_node_id = Some(overlay_dom_id);

  let box_tree = BoxTree::new(BoxNode::new_block(
    default_style(),
    FormattingContextType::Block,
    vec![link_box, overlay_box],
  ));

  let link_box_id = find_box_id_for_styled_node(&box_tree, link_dom_id);
  let overlay_box_id = find_box_id_for_styled_node(&box_tree, overlay_dom_id);

  // Overlay fragment is topmost but should be skipped due to pointer-events:none.
  let fragment_tree = FragmentTree::new(FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
    vec![
      FragmentNode::new_block_with_id(
        Rect::from_xywh(0.0, 0.0, 50.0, 50.0),
        link_box_id,
        vec![],
      ),
      FragmentNode::new_block_with_id(
        Rect::from_xywh(0.0, 0.0, 50.0, 50.0),
        overlay_box_id,
        vec![],
      ),
    ],
  ));

  let mut engine = InteractionEngine::new();

  engine.pointer_move(&mut dom, &box_tree, &fragment_tree, Point::new(10.0, 10.0));
  assert_eq!(
    attr_value(&dom, "link", "data-fastr-hover").as_deref(),
    Some("true"),
    "link should be hovered through overlay"
  );
  assert!(
    !has_attr(&dom, "overlay", "data-fastr-hover"),
    "overlay should not be hovered"
  );

  engine.pointer_down(&mut dom, &box_tree, &fragment_tree, Point::new(10.0, 10.0));
  let (_, action) = engine.pointer_up(
    &mut dom,
    &box_tree,
    &fragment_tree,
    Point::new(10.0, 10.0),
    "https://example.com/",
  );
  assert_eq!(
    action,
    InteractionAction::Navigate {
      href: "https://example.com/foo".to_string()
    }
  );
}

#[test]
fn inert_link_does_not_navigate() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![el(
        "a",
        vec![("id", "link"), ("href", "foo"), ("inert", "")],
        vec![],
      )],
    )],
  )]);

  let link_dom_id = node_id(&dom, "link");
  let mut link_box = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
  link_box.styled_node_id = Some(link_dom_id);
  let box_tree = BoxTree::new(BoxNode::new_block(
    default_style(),
    FormattingContextType::Block,
    vec![link_box],
  ));

  let link_box_id = find_box_id_for_styled_node(&box_tree, link_dom_id);
  let fragment_tree = FragmentTree::new(FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
    vec![FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 50.0, 50.0),
      link_box_id,
      vec![],
    )],
  ));

  let mut engine = InteractionEngine::new();
  engine.pointer_down(&mut dom, &box_tree, &fragment_tree, Point::new(10.0, 10.0));
  let (_, action) = engine.pointer_up(
    &mut dom,
    &box_tree,
    &fragment_tree,
    Point::new(10.0, 10.0),
    "https://example.com/",
  );
  assert_eq!(action, InteractionAction::None);
  assert!(
    !has_attr(&dom, "link", "data-fastr-visited"),
    "inert link should not be marked visited"
  );
}

#[test]
fn disabled_checkbox_does_not_toggle_checked() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![el(
        "input",
        vec![("id", "cb"), ("type", "checkbox"), ("disabled", "")],
        vec![],
      )],
    )],
  )]);

  let cb_dom_id = node_id(&dom, "cb");
  let mut cb_box = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
  cb_box.styled_node_id = Some(cb_dom_id);
  let box_tree = BoxTree::new(BoxNode::new_block(
    default_style(),
    FormattingContextType::Block,
    vec![cb_box],
  ));

  let cb_box_id = find_box_id_for_styled_node(&box_tree, cb_dom_id);
  let fragment_tree = FragmentTree::new(FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
    vec![FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 20.0, 20.0),
      cb_box_id,
      vec![],
    )],
  ));

  let mut engine = InteractionEngine::new();
  engine.pointer_down(&mut dom, &box_tree, &fragment_tree, Point::new(5.0, 5.0));
  let (_, action) = engine.pointer_up(
    &mut dom,
    &box_tree,
    &fragment_tree,
    Point::new(5.0, 5.0),
    "https://x/",
  );
  assert_eq!(action, InteractionAction::None);
  assert!(
    !has_attr(&dom, "cb", "checked"),
    "disabled checkbox must not toggle checked"
  );
}

#[test]
fn checkbox_toggle_clears_indeterminate_and_aria_checked_mixed() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![el(
        "input",
        vec![
          ("id", "cb"),
          ("type", "checkbox"),
          ("indeterminate", ""),
          ("aria-checked", "mixed"),
        ],
        vec![],
      )],
    )],
  )]);

  let cb_dom_id = node_id(&dom, "cb");
  let mut cb_box = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
  cb_box.styled_node_id = Some(cb_dom_id);
  let box_tree = BoxTree::new(BoxNode::new_block(
    default_style(),
    FormattingContextType::Block,
    vec![cb_box],
  ));

  let cb_box_id = find_box_id_for_styled_node(&box_tree, cb_dom_id);
  let fragment_tree = FragmentTree::new(FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
    vec![FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 20.0, 20.0),
      cb_box_id,
      vec![],
    )],
  ));

  let mut engine = InteractionEngine::new();
  engine.pointer_down(&mut dom, &box_tree, &fragment_tree, Point::new(5.0, 5.0));
  let (_, action) = engine.pointer_up(
    &mut dom,
    &box_tree,
    &fragment_tree,
    Point::new(5.0, 5.0),
    "https://x/",
  );
  assert_eq!(action, InteractionAction::None);
  assert!(has_attr(&dom, "cb", "checked"));
  assert!(
    !has_attr(&dom, "cb", "indeterminate"),
    "toggle should clear indeterminate"
  );
  assert!(
    !has_attr(&dom, "cb", "aria-checked"),
    "toggle should clear aria-checked=mixed"
  );
}

#[test]
fn disabled_and_readonly_inputs_ignore_typing_and_backspace() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![
        el(
          "input",
          vec![("id", "disabled"), ("value", "hi"), ("disabled", "")],
          vec![],
        ),
        el(
          "input",
          vec![("id", "readonly"), ("value", "hi"), ("readonly", "")],
          vec![],
        ),
      ],
    )],
  )]);

  let disabled_dom_id = node_id(&dom, "disabled");
  let readonly_dom_id = node_id(&dom, "readonly");

  let mut disabled_box = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
  disabled_box.styled_node_id = Some(disabled_dom_id);
  let mut readonly_box = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
  readonly_box.styled_node_id = Some(readonly_dom_id);
  let box_tree = BoxTree::new(BoxNode::new_block(
    default_style(),
    FormattingContextType::Block,
    vec![disabled_box, readonly_box],
  ));

  let disabled_box_id = find_box_id_for_styled_node(&box_tree, disabled_dom_id);
  let readonly_box_id = find_box_id_for_styled_node(&box_tree, readonly_dom_id);
  let fragment_tree = FragmentTree::new(FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
    vec![
      FragmentNode::new_block_with_id(
        Rect::from_xywh(0.0, 0.0, 80.0, 20.0),
        disabled_box_id,
        vec![],
      ),
      FragmentNode::new_block_with_id(
        Rect::from_xywh(0.0, 40.0, 80.0, 20.0),
        readonly_box_id,
        vec![],
      ),
    ],
  ));

  let mut engine = InteractionEngine::new();

  // Disabled input.
  engine.pointer_down(&mut dom, &box_tree, &fragment_tree, Point::new(5.0, 5.0));
  engine.pointer_up(
    &mut dom,
    &box_tree,
    &fragment_tree,
    Point::new(5.0, 5.0),
    "https://x/",
  );
  engine.text_input(&mut dom, "X");
  engine.key_action(&mut dom, KeyAction::Backspace);
  assert_eq!(attr_value(&dom, "disabled", "value").as_deref(), Some("hi"));

  // Readonly input.
  engine.pointer_down(&mut dom, &box_tree, &fragment_tree, Point::new(5.0, 45.0));
  engine.pointer_up(
    &mut dom,
    &box_tree,
    &fragment_tree,
    Point::new(5.0, 45.0),
    "https://x/",
  );
  engine.text_input(&mut dom, "X");
  engine.key_action(&mut dom, KeyAction::Backspace);
  assert_eq!(attr_value(&dom, "readonly", "value").as_deref(), Some("hi"));
}
