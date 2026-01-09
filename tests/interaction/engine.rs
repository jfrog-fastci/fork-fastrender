use fastrender::dom::enumerate_dom_ids;
use fastrender::dom::DomNode;
use fastrender::dom::DomNodeType;
use fastrender::dom::HTML_NAMESPACE;
use fastrender::geometry::Point;
use fastrender::geometry::Rect;
use fastrender::interaction::InteractionAction;
use fastrender::interaction::InteractionEngine;
use fastrender::interaction::KeyAction;
use fastrender::scroll::ScrollState;
use fastrender::style::display::FormattingContextType;
use fastrender::style::ComputedStyle;
use fastrender::style::types::Appearance;
use fastrender::style::types::PointerEvents;
use fastrender::tree::box_tree::BoxNode;
use fastrender::tree::box_tree::BoxTree;
use fastrender::tree::box_tree::FormControl;
use fastrender::tree::box_tree::FormControlKind;
use fastrender::tree::box_tree::ReplacedType;
use fastrender::tree::box_tree::SelectControl;
use fastrender::tree::box_tree::SelectItem;
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
    engine.pointer_down(
      &mut dom,
      &box_tree,
      &fragment_tree,
      &ScrollState::default(),
      Point::new(10.0, 10.0),
    ),
    "expected pointer_down to set active state"
  );
  let (changed, action) = engine.pointer_up(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(10.0, 10.0),
    "https://x/",
    "https://x/",
  );
  assert!(changed);
  assert!(
    matches!(
      action,
      InteractionAction::FocusChanged { node_id: Some(_) }
        | InteractionAction::Navigate { .. }
        | InteractionAction::None
    ),
    "pointer_up may emit FocusChanged or Navigate"
  );

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
    engine.pointer_move(
      &mut dom,
      &box_tree,
      &fragment_tree,
      &ScrollState::default(),
      Point::new(15.0, 15.0),
    ),
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
      &ScrollState::default(),
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
    engine.pointer_down(
      &mut dom,
      &box_tree,
      &fragment_tree,
      &ScrollState::default(),
      Point::new(15.0, 15.0),
    ),
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
    &ScrollState::default(),
    Point::new(15.0, 15.0),
    "https://x/",
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
  engine.pointer_down(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(10.0, 10.0),
  );
  let (changed, action) = engine.pointer_up(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(10.0, 10.0),
    "https://example.com/base/",
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
  engine.pointer_down(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(10.0, 10.0),
  );
  let (_, action) = engine.pointer_up(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(10.0, 10.0),
    "https://example.com/base/",
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
  engine.pointer_down(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(10.0, 10.0),
  );
  let (_, action) = engine.pointer_up(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(10.0, 10.0),
    "https://example.com/base/",
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
  engine.pointer_down(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
  );
  let (changed, action) = engine.pointer_up(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
    "https://x/",
    "https://x/",
  );
  assert!(changed);
  assert!(
    matches!(
      action,
      InteractionAction::FocusChanged { node_id: Some(_) }
        | InteractionAction::Navigate { .. }
        | InteractionAction::None
    ),
    "pointer_up may emit FocusChanged or Navigate"
  );
  assert!(has_attr(&dom, "cb", "checked"));
  assert_eq!(
    attr_value(&dom, "cb", "data-fastr-focus").as_deref(),
    Some("true"),
    "clicking a checkbox should focus it"
  );
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
  engine.pointer_down(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
  );
  let (changed, action) = engine.pointer_up(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
    "https://x/",
    "https://x/",
  );
  assert!(changed);
  assert!(
    matches!(
      action,
      InteractionAction::FocusChanged { node_id: Some(_) }
        | InteractionAction::Navigate { .. }
        | InteractionAction::None
    ),
    "pointer_up may emit FocusChanged or Navigate"
  );
  assert!(has_attr(&dom, "cb", "checked"));
  assert_eq!(
    attr_value(&dom, "cb", "data-fastr-focus").as_deref(),
    Some("true"),
    "clicking a label should focus the associated checkbox"
  );
}

#[test]
fn radio_click_checks_and_focuses() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![el("input", vec![("id", "r"), ("type", "radio")], vec![])],
    )],
  )]);

  let radio_dom_id = node_id(&dom, "r");
  let mut radio_box = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
  radio_box.styled_node_id = Some(radio_dom_id);
  let box_tree = BoxTree::new(BoxNode::new_block(
    default_style(),
    FormattingContextType::Block,
    vec![radio_box],
  ));

  let radio_box_id = find_box_id_for_styled_node(&box_tree, radio_dom_id);
  let fragment_tree = FragmentTree::new(FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
    vec![FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 20.0, 20.0),
      radio_box_id,
      vec![],
    )],
  ));

  let mut engine = InteractionEngine::new();
  engine.pointer_down(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
  );
  let (changed, action) = engine.pointer_up(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
    "https://x/",
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
  assert!(has_attr(&dom, "r", "checked"), "radio should be checked");
  assert_eq!(
    attr_value(&dom, "r", "data-fastr-focus").as_deref(),
    Some("true"),
    "clicking a radio should focus it"
  );
}

#[test]
fn clicking_outside_focusable_blurs_current_focus() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![
        el("input", vec![("id", "txt"), ("value", "")], vec![]),
        el("div", vec![("id", "outside")], vec![]),
      ],
    )],
  )]);

  let input_dom_id = node_id(&dom, "txt");
  let outside_dom_id = node_id(&dom, "outside");

  let mut input_box = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
  input_box.styled_node_id = Some(input_dom_id);
  let mut outside_box = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
  outside_box.styled_node_id = Some(outside_dom_id);

  let box_tree = BoxTree::new(BoxNode::new_block(
    default_style(),
    FormattingContextType::Block,
    vec![input_box, outside_box],
  ));

  let input_box_id = find_box_id_for_styled_node(&box_tree, input_dom_id);
  let outside_box_id = find_box_id_for_styled_node(&box_tree, outside_dom_id);
  let fragment_tree = FragmentTree::new(FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
    vec![
      FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 0.0, 80.0, 20.0), input_box_id, vec![]),
      FragmentNode::new_block_with_id(
        Rect::from_xywh(0.0, 40.0, 200.0, 160.0),
        outside_box_id,
        vec![],
      ),
    ],
  ));

  let mut engine = InteractionEngine::new();

  // Focus the input.
  engine.pointer_down(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
  );
  let (changed, _) = engine.pointer_up(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
    "https://x/",
    "https://x/",
  );
  assert!(changed);
  assert_eq!(
    attr_value(&dom, "txt", "data-fastr-focus").as_deref(),
    Some("true"),
    "input should be focused after click"
  );

  // Click outside any focusable element to blur.
  engine.pointer_down(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 60.0),
  );
  let (changed, action) = engine.pointer_up(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 60.0),
    "https://x/",
    "https://x/",
  );
  assert!(changed);
  assert_eq!(
    action,
    InteractionAction::FocusChanged { node_id: None },
    "blurring should emit FocusChanged(None)"
  );
  assert!(
    !has_attr(&dom, "txt", "data-fastr-focus"),
    "input focus should be cleared after clicking outside"
  );
}

#[test]
fn typing_updates_focused_input_value_and_sets_focus_visible() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![el(
        "input",
        vec![("id", "txt"), ("value", ""), ("required", "")],
        vec![],
      )],
    )],
  )]);

  assert!(
    !has_attr(&dom, "txt", "data-fastr-user-validity"),
    "user validity hint should not be present initially"
  );

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
  engine.pointer_down(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
  );
  let (changed, action) = engine.pointer_up(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
    "https://x/",
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
    !has_attr(&dom, "txt", "data-fastr-user-validity"),
    "focus should not flip user validity"
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
  assert_eq!(
    attr_value(&dom, "txt", "data-fastr-user-validity").as_deref(),
    Some("true")
  );
}

#[test]
fn submit_click_navigates_and_marks_user_validity() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![el(
        "form",
        vec![("id", "f")],
        vec![
          el(
            "input",
            vec![("id", "txt"), ("value", ""), ("required", "")],
            vec![],
          ),
          el("input", vec![("id", "submit"), ("type", "submit")], vec![]),
        ],
      )],
    )],
  )]);

  assert!(
    !has_attr(&dom, "f", "data-fastr-user-validity"),
    "form should not be marked initially"
  );
  assert!(
    !has_attr(&dom, "submit", "data-fastr-user-validity"),
    "submit control should not be marked initially"
  );

  let submit_dom_id = node_id(&dom, "submit");
  let mut submit_box = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
  submit_box.styled_node_id = Some(submit_dom_id);
  let box_tree = BoxTree::new(BoxNode::new_block(
    default_style(),
    FormattingContextType::Block,
    vec![submit_box],
  ));

  let submit_box_id = find_box_id_for_styled_node(&box_tree, submit_dom_id);
  let fragment_tree = FragmentTree::new(FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
    vec![FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 80.0, 20.0),
      submit_box_id,
      vec![],
    )],
  ));

  let mut engine = InteractionEngine::new();
  engine.pointer_down(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
  );
  let (changed, action) = engine.pointer_up(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
    "https://x/",
    "https://x/",
  );
  assert!(changed);
  assert!(
    matches!(
      action,
      InteractionAction::FocusChanged { node_id: Some(_) }
        | InteractionAction::Navigate { .. }
        | InteractionAction::None
    ),
    "pointer_up may emit FocusChanged or Navigate"
  );

  assert_eq!(
    attr_value(&dom, "f", "data-fastr-user-validity").as_deref(),
    Some("true")
  );
  assert_eq!(
    attr_value(&dom, "submit", "data-fastr-user-validity").as_deref(),
    Some("true")
  );
}

#[test]
fn submit_button_click_submits_get_form_with_query_and_submitter() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![el(
        "form",
        vec![("id", "form"), ("action", "/search")],
        vec![
          el(
            "input",
            vec![("id", "q"), ("name", "q"), ("value", "hello world")],
            vec![],
          ),
          el(
            "input",
            vec![
              ("id", "c"),
              ("type", "checkbox"),
              ("name", "c"),
              ("value", "yes"),
              ("checked", ""),
            ],
            vec![],
          ),
          el(
            "select",
            vec![("id", "sel"), ("name", "sel")],
            vec![
              el("option", vec![("id", "o1"), ("value", "a")], vec![]),
              el(
                "option",
                vec![("id", "o2"), ("value", "b"), ("selected", "")],
                vec![],
              ),
            ],
          ),
          el(
            "button",
            vec![
              ("id", "submit"),
              ("type", "submit"),
              ("name", "s"),
              ("value", "go"),
            ],
            vec![],
          ),
        ],
      )],
    )],
  )]);
  let submit_dom_id = node_id(&dom, "submit");
  let mut submit_box = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
  submit_box.styled_node_id = Some(submit_dom_id);
  let box_tree = BoxTree::new(BoxNode::new_block(
    default_style(),
    FormattingContextType::Block,
    vec![submit_box],
  ));

  let submit_box_id = find_box_id_for_styled_node(&box_tree, submit_dom_id);
  let fragment_tree = FragmentTree::new(FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
    vec![FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 80.0, 20.0),
      submit_box_id,
      vec![],
    )],
  ));

  let mut engine = InteractionEngine::new();
  engine.pointer_down(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
  );
  let (changed, action) = engine.pointer_up(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
    "https://example.com/doc",
    "https://example.com/",
  );
  assert!(changed);
  assert_eq!(
    action,
    InteractionAction::Navigate {
      href: "https://example.com/search?q=hello+world&c=yes&sel=b&s=go".to_string()
    }
  );
  assert_eq!(
    attr_value(&dom, "form", "data-fastr-user-validity").as_deref(),
    Some("true")
  );
  assert_eq!(
    attr_value(&dom, "submit", "data-fastr-user-validity").as_deref(),
    Some("true")
  );
}

#[test]
fn select_listbox_click_marks_user_validity() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![el(
        "select",
        vec![("id", "sel"), ("size", "2")],
        vec![
          el("option", vec![("id", "o1"), ("selected", "")], vec![]),
          el("option", vec![("id", "o2")], vec![]),
        ],
      )],
    )],
  )]);

  assert!(
    !has_attr(&dom, "sel", "data-fastr-user-validity"),
    "select should not be marked initially"
  );

  let select_dom_id = node_id(&dom, "sel");
  let option_1_dom_id = node_id(&dom, "o1");
  let option_2_dom_id = node_id(&dom, "o2");

  let control = FormControlKind::Select(SelectControl {
    multiple: false,
    size: 2,
    items: Arc::new(vec![
      SelectItem::Option {
        node_id: option_1_dom_id,
        label: "Option 1".to_string(),
        value: "o1".to_string(),
        selected: true,
        disabled: false,
        in_optgroup: false,
        option_node_id: option_1_dom_id,
      },
      SelectItem::Option {
        node_id: option_2_dom_id,
        label: "Option 2".to_string(),
        value: "o2".to_string(),
        selected: false,
        disabled: false,
        in_optgroup: false,
        option_node_id: option_2_dom_id,
      },
    ]),
    selected: vec![0],
  });
  let form_control = FormControl {
    control,
    appearance: Appearance::Auto,
    disabled: false,
    focused: false,
    focus_visible: false,
    required: false,
    invalid: false,
    placeholder_style: None,
    slider_thumb_style: None,
    slider_track_style: None,
    file_selector_button_style: None,
  };

  let mut select_box = BoxNode::new_replaced(
    default_style(),
    ReplacedType::FormControl(form_control),
    None,
    None,
  );
  select_box.styled_node_id = Some(select_dom_id);
  let box_tree = BoxTree::new(BoxNode::new_block(
    default_style(),
    FormattingContextType::Block,
    vec![select_box],
  ));

  let select_box_id = find_box_id_for_styled_node(&box_tree, select_dom_id);
  let fragment_tree = FragmentTree::new(FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
    vec![FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 80.0, 40.0),
      select_box_id,
      vec![],
    )],
  ));

  let mut engine = InteractionEngine::new();
  engine.pointer_down(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 25.0),
  );
  let (changed, _action) = engine.pointer_up(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 25.0),
    "https://x/",
    "https://x/",
  );
  assert!(changed);

  assert!(
    has_attr(&dom, "o2", "selected"),
    "option in second row should be selected"
  );
  assert!(
    !has_attr(&dom, "o1", "selected"),
    "previously selected option should be cleared"
  );
  assert_eq!(
    attr_value(&dom, "sel", "data-fastr-user-validity").as_deref(),
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

  engine.pointer_move(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(10.0, 10.0),
  );
  assert_eq!(
    attr_value(&dom, "link", "data-fastr-hover").as_deref(),
    Some("true"),
    "link should be hovered through overlay"
  );
  assert!(
    !has_attr(&dom, "overlay", "data-fastr-hover"),
    "overlay should not be hovered"
  );

  engine.pointer_down(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(10.0, 10.0),
  );
  let (_, action) = engine.pointer_up(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(10.0, 10.0),
    "https://example.com/",
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
fn form_submit_get_builds_expected_url() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![el(
        "form",
        vec![("id", "f"), ("method", "get"), ("action", "/search")],
        vec![
          el(
            "input",
            vec![("id", "q"), ("name", "q"), ("value", "hello world")],
            vec![],
          ),
          el(
            "input",
            vec![
              ("id", "submit"),
              ("type", "submit"),
              ("name", "go"),
              ("value", "1"),
            ],
            vec![],
          ),
        ],
      )],
    )],
  )]);

  let submit_dom_id = node_id(&dom, "submit");
  let mut submit_box = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
  submit_box.styled_node_id = Some(submit_dom_id);
  let box_tree = BoxTree::new(BoxNode::new_block(
    default_style(),
    FormattingContextType::Block,
    vec![submit_box],
  ));

  let submit_box_id = find_box_id_for_styled_node(&box_tree, submit_dom_id);
  let fragment_tree = FragmentTree::new(FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
    vec![FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 80.0, 20.0),
      submit_box_id,
      vec![],
    )],
  ));

  let mut engine = InteractionEngine::new();
  let scroll = ScrollState::default();
  engine.pointer_down(&mut dom, &box_tree, &fragment_tree, &scroll, Point::new(5.0, 5.0));
  let (_, action) = engine.pointer_up(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &scroll,
    Point::new(5.0, 5.0),
    "https://example.com/page1",
    "https://example.com/base/",
  );

  assert_eq!(
    action,
    InteractionAction::Navigate {
      href: "https://example.com/search?q=hello+world&go=1".to_string()
    }
  );
}

#[test]
fn form_submit_get_skips_unchecked_checkbox() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![el(
        "form",
        vec![("id", "f"), ("method", "get"), ("action", "/search")],
        vec![
          el("input", vec![("id", "q"), ("name", "q"), ("value", "hi")], vec![]),
          el(
            "input",
            vec![("id", "cb"), ("type", "checkbox"), ("name", "c"), ("value", "yes")],
            vec![],
          ),
          el(
            "input",
            vec![
              ("id", "submit"),
              ("type", "submit"),
              ("name", "go"),
              ("value", "1"),
            ],
            vec![],
          ),
        ],
      )],
    )],
  )]);

  let submit_dom_id = node_id(&dom, "submit");
  let mut submit_box = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
  submit_box.styled_node_id = Some(submit_dom_id);
  let box_tree = BoxTree::new(BoxNode::new_block(
    default_style(),
    FormattingContextType::Block,
    vec![submit_box],
  ));

  let submit_box_id = find_box_id_for_styled_node(&box_tree, submit_dom_id);
  let fragment_tree = FragmentTree::new(FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
    vec![FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 80.0, 20.0),
      submit_box_id,
      vec![],
    )],
  ));

  let mut engine = InteractionEngine::new();
  let scroll = ScrollState::default();
  engine.pointer_down(&mut dom, &box_tree, &fragment_tree, &scroll, Point::new(5.0, 5.0));
  let (_, action) = engine.pointer_up(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &scroll,
    Point::new(5.0, 5.0),
    "https://example.com/page1",
    "https://example.com/base/",
  );

  assert_eq!(
    action,
    InteractionAction::Navigate {
      href: "https://example.com/search?q=hi&go=1".to_string()
    }
  );
}

#[test]
fn form_submit_get_includes_checked_checkbox() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![el(
        "form",
        vec![("id", "f"), ("method", "get"), ("action", "/search")],
        vec![
          el("input", vec![("id", "q"), ("name", "q"), ("value", "hi")], vec![]),
          el(
            "input",
            vec![
              ("id", "cb"),
              ("type", "checkbox"),
              ("name", "c"),
              ("value", "yes"),
              ("checked", ""),
            ],
            vec![],
          ),
          el(
            "input",
            vec![
              ("id", "submit"),
              ("type", "submit"),
              ("name", "go"),
              ("value", "1"),
            ],
            vec![],
          ),
        ],
      )],
    )],
  )]);

  let submit_dom_id = node_id(&dom, "submit");
  let mut submit_box = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
  submit_box.styled_node_id = Some(submit_dom_id);
  let box_tree = BoxTree::new(BoxNode::new_block(
    default_style(),
    FormattingContextType::Block,
    vec![submit_box],
  ));

  let submit_box_id = find_box_id_for_styled_node(&box_tree, submit_dom_id);
  let fragment_tree = FragmentTree::new(FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
    vec![FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 80.0, 20.0),
      submit_box_id,
      vec![],
    )],
  ));

  let mut engine = InteractionEngine::new();
  let scroll = ScrollState::default();
  engine.pointer_down(&mut dom, &box_tree, &fragment_tree, &scroll, Point::new(5.0, 5.0));
  let (_, action) = engine.pointer_up(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &scroll,
    Point::new(5.0, 5.0),
    "https://example.com/page1",
    "https://example.com/base/",
  );

  assert_eq!(
    action,
    InteractionAction::Navigate {
      href: "https://example.com/search?q=hi&c=yes&go=1".to_string()
    }
  );
}

#[test]
fn dropdown_select_click_emits_open_dropdown_action_with_select_model() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![el(
        "select",
        vec![("id", "sel")],
        vec![
          el(
            "option",
            vec![("id", "o-a"), ("value", "a")],
            vec![DomNode {
              node_type: DomNodeType::Text {
                content: "Alpha".to_string(),
              },
              children: vec![],
            }],
          ),
          el(
            "option",
            vec![("id", "o-b"), ("value", "b"), ("disabled", "")],
            vec![DomNode {
              node_type: DomNodeType::Text {
                content: "Beta".to_string(),
              },
              children: vec![],
            }],
          ),
          el(
            "option",
            vec![("id", "o-c"), ("value", "c")],
            vec![DomNode {
              node_type: DomNodeType::Text {
                content: "Gamma".to_string(),
              },
              children: vec![],
            }],
          ),
        ],
      )],
    )],
  )]);

  let select_dom_id = node_id(&dom, "sel");
  let option_a_dom_id = node_id(&dom, "o-a");
  let option_b_dom_id = node_id(&dom, "o-b");
  let option_c_dom_id = node_id(&dom, "o-c");
  let expected_control = SelectControl {
    multiple: false,
    size: 1,
    items: Arc::new(vec![
      SelectItem::Option {
        node_id: option_a_dom_id,
        label: "Alpha".to_string(),
        value: "a".to_string(),
        selected: true,
        disabled: false,
        in_optgroup: false,
        option_node_id: option_a_dom_id,
      },
      SelectItem::Option {
        node_id: option_b_dom_id,
        label: "Beta".to_string(),
        value: "b".to_string(),
        selected: false,
        disabled: true,
        in_optgroup: false,
        option_node_id: option_b_dom_id,
      },
      SelectItem::Option {
        node_id: option_c_dom_id,
        label: "Gamma".to_string(),
        value: "c".to_string(),
        selected: false,
        disabled: false,
        in_optgroup: false,
        option_node_id: option_c_dom_id,
      },
    ]),
    selected: vec![0],
  };

  let form_control = FormControl {
    control: FormControlKind::Select(expected_control.clone()),
    appearance: Appearance::Auto,
    placeholder_style: None,
    slider_thumb_style: None,
    slider_track_style: None,
    file_selector_button_style: None,
    disabled: false,
    focused: false,
    focus_visible: false,
    required: false,
    invalid: false,
  };

  let mut select_box = BoxNode::new_replaced(
    default_style(),
    ReplacedType::FormControl(form_control),
    None,
    None,
  );
  select_box.styled_node_id = Some(select_dom_id);

  let box_tree = BoxTree::new(BoxNode::new_block(
    default_style(),
    FormattingContextType::Block,
    vec![select_box],
  ));

  let select_box_id = find_box_id_for_styled_node(&box_tree, select_dom_id);
  let fragment_tree = FragmentTree::new(FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
    vec![FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 100.0, 30.0),
      select_box_id,
      vec![],
    )],
  ));

  let mut engine = InteractionEngine::new();
  engine.pointer_down(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
  );
  let (changed, action) = engine.pointer_up(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
    "https://x/",
    "https://x/",
  );
  assert!(changed);
  assert_eq!(
    action,
    InteractionAction::OpenSelectDropdown {
      select_node_id: select_dom_id,
      control: expected_control.clone(),
    }
  );
  assert_eq!(
    attr_value(&dom, "sel", "data-fastr-focus").as_deref(),
    Some("true"),
    "clicking a select should focus it"
  );
  assert!(
    !has_attr(&dom, "sel", "data-fastr-focus-visible"),
    "pointer focus should not set focus-visible"
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
  engine.pointer_down(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(10.0, 10.0),
  );
  let (_, action) = engine.pointer_up(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(10.0, 10.0),
    "https://example.com/",
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
  engine.pointer_down(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
  );
  let (_, action) = engine.pointer_up(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
    "https://x/",
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
  engine.pointer_down(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
  );
  let (_, action) = engine.pointer_up(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
    "https://x/",
    "https://x/",
  );
  assert!(
    matches!(
      action,
      InteractionAction::FocusChanged { node_id: Some(_) } | InteractionAction::None
    ),
    "pointer_up may emit FocusChanged"
  );
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
  engine.pointer_down(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
  );
  engine.pointer_up(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
    "https://x/",
    "https://x/",
  );
  engine.text_input(&mut dom, "X");
  engine.key_action(&mut dom, KeyAction::Backspace);
  assert_eq!(attr_value(&dom, "disabled", "value").as_deref(), Some("hi"));

  // Readonly input.
  engine.pointer_down(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 45.0),
  );
  engine.pointer_up(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 45.0),
    "https://x/",
    "https://x/",
  );
  engine.text_input(&mut dom, "X");
  engine.key_action(&mut dom, KeyAction::Backspace);
  assert_eq!(attr_value(&dom, "readonly", "value").as_deref(), Some("hi"));
}

#[test]
fn tab_key_traverses_focusable_elements_in_dom_order_and_wraps() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![
        // Not focusable (disabled).
        el("input", vec![("id", "disabled_first"), ("disabled", "")], vec![]),
        // 1: <a href>
        el("a", vec![("id", "a1"), ("href", "/a1")], vec![]),
        // Not focusable (no href).
        el("a", vec![("id", "a_no_href")], vec![]),
        el(
          "div",
          vec![("id", "container")],
          vec![
            // Not focusable.
            el("span", vec![("id", "spacer")], vec![]),
            // Not focusable (<input type=hidden>).
            el("input", vec![("id", "hidden"), ("type", "hidden")], vec![]),
            // 2: <input>
            el("input", vec![("id", "i1")], vec![]),
          ],
        ),
        // Not focusable (disabled).
        el("button", vec![("id", "b_disabled"), ("disabled", "")], vec![]),
        // 3: <button>
        el("button", vec![("id", "b1")], vec![]),
        // Not focusable (data-fastr-inert subtree).
        el(
          "div",
          vec![("id", "data_inert"), ("data-fastr-inert", "true")],
          vec![el("input", vec![("id", "i_inert2")], vec![])],
        ),
        // Not focusable (tabindex=-1).
        el(
          "a",
          vec![("id", "a_skip"), ("href", "/skip"), ("tabindex", "-1")],
          vec![],
        ),
        // Not focusable (inert subtree).
        el(
          "div",
          vec![("id", "inert_container"), ("inert", "")],
          vec![el("textarea", vec![("id", "ta_inert")], vec![])],
        ),
        // 4: <textarea>
        el("textarea", vec![("id", "ta1")], vec![]),
        // 5: <select>
        el("select", vec![("id", "s1")], vec![]),
        // 6: <a href>
        el("a", vec![("id", "a2"), ("href", "/a2")], vec![]),
      ],
    )],
  )]);

  let mut engine = InteractionEngine::new();
  let focusables = ["a1", "i1", "b1", "ta1", "s1", "a2"];
  let mut prev: Option<&str> = None;

  for expected in focusables
    .iter()
    .copied()
    .chain(std::iter::once(focusables[0]))
  {
    assert!(
      engine.key_action(&mut dom, KeyAction::Tab),
      "tab should move focus"
    );
    assert_eq!(
      attr_value(&dom, expected, "data-fastr-focus").as_deref(),
      Some("true"),
      "{expected} should be focused"
    );
    assert_eq!(
      attr_value(&dom, expected, "data-fastr-focus-visible").as_deref(),
      Some("true"),
      "{expected} should be focus-visible (keyboard modality)"
    );

    if let Some(prev_id) = prev {
      assert!(
        !has_attr(&dom, prev_id, "data-fastr-focus"),
        "{prev_id} focus should be cleared"
      );
      assert!(
        !has_attr(&dom, prev_id, "data-fastr-focus-visible"),
        "{prev_id} focus-visible should be cleared"
      );
    }

    for skipped in [
      "disabled_first",
      "a_no_href",
      "hidden",
      "b_disabled",
      "i_inert2",
      "a_skip",
      "ta_inert",
    ] {
      assert!(
        !has_attr(&dom, skipped, "data-fastr-focus"),
        "{skipped} must be skipped by tab traversal"
      );
    }

    prev = Some(expected);
  }
}

#[test]
fn listbox_select_click_sets_selected_option_and_focuses_select() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![el(
        "select",
        vec![("id", "s"), ("size", "3")],
        vec![
          el("option", vec![("id", "o1"), ("selected", "")], vec![]),
          el("option", vec![("id", "o2")], vec![]),
          el("option", vec![("id", "o3"), ("disabled", "")], vec![]),
        ],
      )],
    )],
  )]);

  let select_dom_id = node_id(&dom, "s");
  let o1_dom_id = node_id(&dom, "o1");
  let o2_dom_id = node_id(&dom, "o2");
  let o3_dom_id = node_id(&dom, "o3");

  let control = FormControlKind::Select(SelectControl {
    multiple: false,
    size: 3,
    items: Arc::new(vec![
      SelectItem::Option {
        node_id: o1_dom_id,
        label: "One".to_string(),
        value: "1".to_string(),
        selected: true,
        disabled: false,
        in_optgroup: false,
        option_node_id: o1_dom_id,
      },
      SelectItem::Option {
        node_id: o2_dom_id,
        label: "Two".to_string(),
        value: "2".to_string(),
        selected: false,
        disabled: false,
        in_optgroup: false,
        option_node_id: o2_dom_id,
      },
      SelectItem::Option {
        node_id: o3_dom_id,
        label: "Three".to_string(),
        value: "3".to_string(),
        selected: false,
        disabled: true,
        in_optgroup: false,
        option_node_id: o3_dom_id,
      },
    ]),
    selected: vec![0],
  });

  let mut select_box = BoxNode::new_replaced(
    default_style(),
    ReplacedType::FormControl(FormControl {
      control,
      appearance: Appearance::Auto,
      placeholder_style: None,
      slider_thumb_style: None,
      slider_track_style: None,
      file_selector_button_style: None,
      disabled: false,
      focused: false,
      focus_visible: false,
      required: false,
      invalid: false,
    }),
    None,
    None,
  );
  select_box.styled_node_id = Some(select_dom_id);

  let box_tree = BoxTree::new(BoxNode::new_block(
    default_style(),
    FormattingContextType::Block,
    vec![select_box],
  ));
  let select_box_id = find_box_id_for_styled_node(&box_tree, select_dom_id);

  // Height=30px, size=3 => 10px per row. Y=15 selects row index 1 (<option id=o2>).
  let fragment_tree = FragmentTree::new(FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
    vec![FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 100.0, 30.0),
      select_box_id,
      vec![],
    )],
  ));

  let mut engine = InteractionEngine::new();
  engine.pointer_down(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 15.0),
  );
  let (_, action) = engine.pointer_up(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 15.0),
    "https://x/",
    "https://x/",
  );

  assert_eq!(
    action,
    InteractionAction::FocusChanged {
      node_id: Some(select_dom_id)
    }
  );
  assert_eq!(
    attr_value(&dom, "s", "data-fastr-focus").as_deref(),
    Some("true"),
    "clicking a select should focus it"
  );

  assert!(
    !has_attr(&dom, "o1", "selected"),
    "single-select listbox should clear previously selected option"
  );
  assert!(has_attr(&dom, "o2", "selected"), "clicked row should be selected");
  assert_eq!(
    attr_value(&dom, "s", "data-fastr-user-validity").as_deref(),
    Some("true"),
    "user mutation should mark the select for :user-invalid matching"
  );

  // Clicking a disabled option row must not change selection.
  engine.pointer_down(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 25.0),
  );
  let (_, action) = engine.pointer_up(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 25.0),
    "https://x/",
    "https://x/",
  );
  assert_eq!(action, InteractionAction::None);
  assert!(has_attr(&dom, "o2", "selected"));
  assert!(!has_attr(&dom, "o3", "selected"));
}

#[test]
fn multiple_listbox_select_click_toggles_selected_option_without_clearing_others() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![el(
        "select",
        vec![("id", "s"), ("size", "3"), ("multiple", "")],
        vec![
          el("option", vec![("id", "o1"), ("selected", "")], vec![]),
          el("option", vec![("id", "o2")], vec![]),
          el("option", vec![("id", "o3")], vec![]),
        ],
      )],
    )],
  )]);

  let select_dom_id = node_id(&dom, "s");
  let o1_dom_id = node_id(&dom, "o1");
  let o2_dom_id = node_id(&dom, "o2");
  let o3_dom_id = node_id(&dom, "o3");

  let control = FormControlKind::Select(SelectControl {
    multiple: true,
    size: 3,
    items: Arc::new(vec![
      SelectItem::Option {
        node_id: o1_dom_id,
        label: "One".to_string(),
        value: "1".to_string(),
        selected: true,
        disabled: false,
        in_optgroup: false,
        option_node_id: o1_dom_id,
      },
      SelectItem::Option {
        node_id: o2_dom_id,
        label: "Two".to_string(),
        value: "2".to_string(),
        selected: false,
        disabled: false,
        in_optgroup: false,
        option_node_id: o2_dom_id,
      },
      SelectItem::Option {
        node_id: o3_dom_id,
        label: "Three".to_string(),
        value: "3".to_string(),
        selected: false,
        disabled: false,
        in_optgroup: false,
        option_node_id: o3_dom_id,
      },
    ]),
    selected: vec![0],
  });

  let mut select_box = BoxNode::new_replaced(
    default_style(),
    ReplacedType::FormControl(FormControl {
      control,
      appearance: Appearance::Auto,
      placeholder_style: None,
      slider_thumb_style: None,
      slider_track_style: None,
      file_selector_button_style: None,
      disabled: false,
      focused: false,
      focus_visible: false,
      required: false,
      invalid: false,
    }),
    None,
    None,
  );
  select_box.styled_node_id = Some(select_dom_id);

  let box_tree = BoxTree::new(BoxNode::new_block(
    default_style(),
    FormattingContextType::Block,
    vec![select_box],
  ));
  let select_box_id = find_box_id_for_styled_node(&box_tree, select_dom_id);

  // Height=30px, size=3 => 10px per row. Y=15 selects row index 1 (<option id=o2>).
  let fragment_tree = FragmentTree::new(FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
    vec![FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 100.0, 30.0),
      select_box_id,
      vec![],
    )],
  ));

  let mut engine = InteractionEngine::new();

  // Toggle <option id=o2> on.
  engine.pointer_down(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 15.0),
  );
  let (_, action) = engine.pointer_up(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 15.0),
    "https://x/",
    "https://x/",
  );
  assert_eq!(
    action,
    InteractionAction::FocusChanged {
      node_id: Some(select_dom_id)
    }
  );
  assert!(has_attr(&dom, "o1", "selected"));
  assert!(has_attr(&dom, "o2", "selected"));
  assert!(!has_attr(&dom, "o3", "selected"));

  // Toggle <option id=o2> off (multiple-select should not clear other selections).
  engine.pointer_down(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 15.0),
  );
  let (_, action) = engine.pointer_up(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 15.0),
    "https://x/",
    "https://x/",
  );
  assert_eq!(action, InteractionAction::None);
  assert!(has_attr(&dom, "o1", "selected"));
  assert!(!has_attr(&dom, "o2", "selected"));
  assert!(!has_attr(&dom, "o3", "selected"));
}

#[test]
fn listbox_select_click_accounts_for_element_scroll_offset() {
  let option_ids = [
    "o0", "o1", "o2", "o3", "o4", "o5", "o6", "o7", "o8", "o9",
  ];
  let options = option_ids
    .iter()
    .map(|&id| el("option", vec![("id", id)], vec![]))
    .collect();

  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![el("select", vec![("id", "s"), ("size", "3")], options)],
    )],
  )]);

  let select_dom_id = node_id(&dom, "s");
  let items = Arc::new(
    option_ids
      .iter()
      .enumerate()
      .map(|(idx, &id)| {
        let option_node_id = node_id(&dom, id);
        SelectItem::Option {
          node_id: option_node_id,
          label: format!("Option {idx}"),
          value: idx.to_string(),
          selected: idx == 0,
          disabled: false,
          in_optgroup: false,
          option_node_id,
        }
      })
      .collect::<Vec<_>>(),
  );

  let control = FormControlKind::Select(SelectControl {
    multiple: false,
    size: 3,
    items,
    selected: vec![0],
  });

  let mut select_box = BoxNode::new_replaced(
    default_style(),
    ReplacedType::FormControl(FormControl {
      control,
      appearance: Appearance::Auto,
      placeholder_style: None,
      slider_thumb_style: None,
      slider_track_style: None,
      file_selector_button_style: None,
      disabled: false,
      focused: false,
      focus_visible: false,
      required: false,
      invalid: false,
    }),
    None,
    None,
  );
  select_box.styled_node_id = Some(select_dom_id);

  let box_tree = BoxTree::new(BoxNode::new_block(
    default_style(),
    FormattingContextType::Block,
    vec![select_box],
  ));
  let select_box_id = find_box_id_for_styled_node(&box_tree, select_dom_id);

  // Height=30px, size=3 => 10px per row.
  let fragment_tree = FragmentTree::new(FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
    vec![FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 100.0, 30.0),
      select_box_id,
      vec![],
    )],
  ));

  let mut elements = std::collections::HashMap::new();
  // Scroll down by 2 rows; clicking at y=5 should select row index 2 (<option id=o2>).
  elements.insert(select_box_id, Point::new(0.0, 20.0));
  let scroll = ScrollState::from_parts(Point::ZERO, elements);

  let mut engine = InteractionEngine::new();
  engine.pointer_down(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &scroll,
    Point::new(5.0, 5.0),
  );
  engine.pointer_up(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &scroll,
    Point::new(5.0, 5.0),
    "https://x/",
    "https://x/",
  );

  assert!(has_attr(&dom, "o2", "selected"));
  assert!(!has_attr(&dom, "o0", "selected"));
}

#[test]
fn focused_link_enter_activates_navigation_and_marks_visited() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![el("a", vec![("id", "link"), ("href", "foo")], vec![])],
    )],
  )]);

  let link_id = node_id(&dom, "link");

  let mut engine = InteractionEngine::new();
  engine.focus_node_id(&mut dom, Some(link_id), false);

  let (changed, action) = engine.key_activate(
    &mut dom,
    KeyAction::Enter,
    "https://example.com/doc",
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
  assert_eq!(
    attr_value(&dom, "link", "data-fastr-focus-visible").as_deref(),
    Some("true")
  );
}

#[test]
fn tab_cycles_focus_between_link_and_input_and_sets_focus_visible() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![
        el("a", vec![("id", "link"), ("href", "foo")], vec![]),
        el("input", vec![("id", "txt")], vec![]),
      ],
    )],
  )]);

  let link_id = node_id(&dom, "link");
  let input_id = node_id(&dom, "txt");

  let mut engine = InteractionEngine::new();

  let (changed, action) =
    engine.key_activate(&mut dom, KeyAction::Tab, "https://x/", "https://example.com/base/");
  assert!(changed, "Tab should focus the first focusable element");
  assert_eq!(
    action,
    InteractionAction::FocusChanged {
      node_id: Some(link_id)
    }
  );
  assert_eq!(
    attr_value(&dom, "link", "data-fastr-focus").as_deref(),
    Some("true")
  );
  assert_eq!(
    attr_value(&dom, "link", "data-fastr-focus-visible").as_deref(),
    Some("true")
  );
  assert!(!has_attr(&dom, "txt", "data-fastr-focus"));

  let (_, action) =
    engine.key_activate(&mut dom, KeyAction::Tab, "https://x/", "https://example.com/base/");
  assert_eq!(
    action,
    InteractionAction::FocusChanged {
      node_id: Some(input_id)
    }
  );
  assert!(!has_attr(&dom, "link", "data-fastr-focus"));
  assert_eq!(
    attr_value(&dom, "txt", "data-fastr-focus").as_deref(),
    Some("true")
  );
  assert_eq!(
    attr_value(&dom, "txt", "data-fastr-focus-visible").as_deref(),
    Some("true")
  );

  // Wrap at the end.
  let (_, action) =
    engine.key_activate(&mut dom, KeyAction::Tab, "https://x/", "https://example.com/base/");
  assert_eq!(
    action,
    InteractionAction::FocusChanged {
      node_id: Some(link_id)
    }
  );
}

#[test]
fn focused_checkbox_space_toggles_checked() {
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

  let cb_id = node_id(&dom, "cb");

  let mut engine = InteractionEngine::new();
  engine.focus_node_id(&mut dom, Some(cb_id), false);

  let (changed, action) =
    engine.key_activate(&mut dom, KeyAction::Space, "https://x/", "https://x/");
  assert!(changed);
  assert_eq!(action, InteractionAction::None);
  assert!(has_attr(&dom, "cb", "checked"));
  assert_eq!(
    attr_value(&dom, "cb", "data-fastr-user-validity").as_deref(),
    Some("true")
  );
  assert_eq!(
    attr_value(&dom, "cb", "data-fastr-focus-visible").as_deref(),
    Some("true")
  );
}

#[test]
fn arrow_down_changes_focused_dropdown_select_selection() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![el(
        "select",
        vec![("id", "sel")],
        vec![
          el("option", vec![("id", "o1"), ("selected", "")], vec![]),
          el("option", vec![("id", "o2"), ("disabled", "")], vec![]),
          el("option", vec![("id", "o3")], vec![]),
        ],
      )],
    )],
  )]);

  let select_id = node_id(&dom, "sel");

  let mut engine = InteractionEngine::new();
  engine.focus_node_id(&mut dom, Some(select_id), false);

  let (changed, action) =
    engine.key_activate(&mut dom, KeyAction::ArrowDown, "https://x/", "https://x/");
  assert!(changed, "expected ArrowDown to change selection");
  assert_eq!(action, InteractionAction::None);

  assert!(!has_attr(&dom, "o1", "selected"));
  assert!(!has_attr(&dom, "o2", "selected"));
  assert!(has_attr(&dom, "o3", "selected"));
  assert_eq!(
    attr_value(&dom, "sel", "data-fastr-user-validity").as_deref(),
    Some("true")
  );
}

#[test]
fn enter_on_text_input_submits_get_form() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![el(
        "form",
        vec![("id", "form"), ("action", "/search")],
        vec![el(
          "input",
          vec![("id", "q"), ("name", "q"), ("value", "abc")],
          vec![],
        )],
      )],
    )],
  )]);

  assert!(
    !has_attr(&dom, "form", "data-fastr-user-validity"),
    "form should not be marked initially"
  );

  let input_id = node_id(&dom, "q");

  let mut engine = InteractionEngine::new();
  engine.focus_node_id(&mut dom, Some(input_id), false);

  let (changed, action) = engine.key_activate(
    &mut dom,
    KeyAction::Enter,
    "https://example.com/doc",
    "https://example.com/base/",
  );

  assert!(changed);
  assert_eq!(
    action,
    InteractionAction::Navigate {
      href: "https://example.com/search?q=abc".to_string()
    }
  );
  assert_eq!(
    attr_value(&dom, "form", "data-fastr-user-validity").as_deref(),
    Some("true"),
    "Enter submission should mark form user validity"
  );
}

#[test]
fn range_input_drag_updates_value_and_clamps_to_max() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![el(
        "input",
        vec![("id", "r"), ("type", "range"), ("min", "0"), ("max", "10"), ("value", "0")],
        vec![],
      )],
    )],
  )]);

  let range_dom_id = node_id(&dom, "r");
  let mut range_box = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
  range_box.styled_node_id = Some(range_dom_id);
  let box_tree = BoxTree::new(BoxNode::new_block(
    default_style(),
    FormattingContextType::Block,
    vec![range_box],
  ));
  let range_box_id = find_box_id_for_styled_node(&box_tree, range_dom_id);

  let fragment_tree = FragmentTree::new(FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
    vec![FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 100.0, 20.0),
      range_box_id,
      vec![],
    )],
  ));

  let mut engine = InteractionEngine::new();
  let scroll = ScrollState::default();
  engine.pointer_down(&mut dom, &box_tree, &fragment_tree, &scroll, Point::new(0.0, 10.0));

  engine.pointer_move(&mut dom, &box_tree, &fragment_tree, &scroll, Point::new(25.0, 10.0));
  assert_eq!(attr_value(&dom, "r", "value").as_deref(), Some("3"));
  assert!(
    has_attr(&dom, "r", "data-fastr-user-validity"),
    "changing a range value should mark user validity"
  );

  engine.pointer_move(&mut dom, &box_tree, &fragment_tree, &scroll, Point::new(75.0, 10.0));
  assert_eq!(attr_value(&dom, "r", "value").as_deref(), Some("8"));

  // Drag beyond the right edge: clamp at max.
  engine.pointer_move(&mut dom, &box_tree, &fragment_tree, &scroll, Point::new(150.0, 10.0));
  assert_eq!(attr_value(&dom, "r", "value").as_deref(), Some("10"));
}

#[test]
fn range_click_sets_min_max_and_snaps_to_step() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![el(
        "input",
        vec![
          ("id", "r"),
          ("type", "range"),
          ("min", "0"),
          ("max", "100"),
          ("step", "10"),
          ("value", "50"),
        ],
        vec![],
      )],
    )],
  )]);

  let range_dom_id = node_id(&dom, "r");
  let mut range_box = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
  range_box.styled_node_id = Some(range_dom_id);
  let box_tree = BoxTree::new(BoxNode::new_block(
    default_style(),
    FormattingContextType::Block,
    vec![range_box],
  ));
  let range_box_id = find_box_id_for_styled_node(&box_tree, range_dom_id);

  let fragment_tree = FragmentTree::new(FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
    vec![FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 100.0, 20.0),
      range_box_id,
      vec![],
    )],
  ));

  let mut engine = InteractionEngine::new();
  let scroll = ScrollState::default();

  // Left edge should set min.
  engine.pointer_down(&mut dom, &box_tree, &fragment_tree, &scroll, Point::new(0.0, 10.0));
  engine.pointer_up(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &scroll,
    Point::new(0.0, 10.0),
    "https://x/",
    "https://x/",
  );
  assert_eq!(attr_value(&dom, "r", "value").as_deref(), Some("0"));

  // Near 56% should snap to the nearest step.
  engine.pointer_down(&mut dom, &box_tree, &fragment_tree, &scroll, Point::new(56.0, 10.0));
  engine.pointer_up(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &scroll,
    Point::new(56.0, 10.0),
    "https://x/",
    "https://x/",
  );
  assert_eq!(attr_value(&dom, "r", "value").as_deref(), Some("60"));

  // Right edge should set max.
  engine.pointer_down(&mut dom, &box_tree, &fragment_tree, &scroll, Point::new(100.0, 10.0));
  engine.pointer_up(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &scroll,
    Point::new(100.0, 10.0),
    "https://x/",
    "https://x/",
  );
  assert_eq!(attr_value(&dom, "r", "value").as_deref(), Some("100"));
}

#[test]
fn range_arrow_keys_step_value() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![el(
        "input",
        vec![
          ("id", "r"),
          ("type", "range"),
          ("min", "0"),
          ("max", "10"),
          ("step", "2"),
          ("value", "4"),
        ],
        vec![],
      )],
    )],
  )]);

  let range_dom_id = node_id(&dom, "r");

  let mut engine = InteractionEngine::new();
  engine.focus_node_id(&mut dom, Some(range_dom_id), true);

  engine.key_action(&mut dom, KeyAction::ArrowUp);
  assert_eq!(attr_value(&dom, "r", "value").as_deref(), Some("6"));
  assert!(
    has_attr(&dom, "r", "data-fastr-user-validity"),
    "arrow key range changes should mark user validity"
  );

  engine.key_action(&mut dom, KeyAction::ArrowDown);
  assert_eq!(attr_value(&dom, "r", "value").as_deref(), Some("4"));

  // Clamp at min.
  engine.key_action(&mut dom, KeyAction::ArrowDown);
  engine.key_action(&mut dom, KeyAction::ArrowDown);
  engine.key_action(&mut dom, KeyAction::ArrowDown);
  assert_eq!(attr_value(&dom, "r", "value").as_deref(), Some("0"));
}

#[test]
fn disabled_and_readonly_range_inputs_do_not_update_value() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![
        el(
          "input",
          vec![
            ("id", "disabled"),
            ("type", "range"),
            ("min", "0"),
            ("max", "10"),
            ("value", "0"),
            ("disabled", ""),
          ],
          vec![],
        ),
        el(
          "input",
          vec![
            ("id", "readonly"),
            ("type", "range"),
            ("min", "0"),
            ("max", "10"),
            ("value", "0"),
            ("readonly", ""),
          ],
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
        Rect::from_xywh(0.0, 0.0, 100.0, 20.0),
        disabled_box_id,
        vec![],
      ),
      FragmentNode::new_block_with_id(
        Rect::from_xywh(0.0, 40.0, 100.0, 20.0),
        readonly_box_id,
        vec![],
      ),
    ],
  ));

  let mut engine = InteractionEngine::new();
  let scroll = ScrollState::default();

  // Disabled range.
  engine.pointer_down(&mut dom, &box_tree, &fragment_tree, &scroll, Point::new(0.0, 10.0));
  engine.pointer_move(&mut dom, &box_tree, &fragment_tree, &scroll, Point::new(100.0, 10.0));
  engine.pointer_up(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &scroll,
    Point::new(100.0, 10.0),
    "https://x/",
    "https://x/",
  );
  assert_eq!(attr_value(&dom, "disabled", "value").as_deref(), Some("0"));
  assert!(
    !has_attr(&dom, "disabled", "data-fastr-user-validity"),
    "disabled range must not flip user validity"
  );

  // Readonly range.
  engine.pointer_down(&mut dom, &box_tree, &fragment_tree, &scroll, Point::new(0.0, 50.0));
  engine.pointer_move(&mut dom, &box_tree, &fragment_tree, &scroll, Point::new(100.0, 50.0));
  engine.pointer_up(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &scroll,
    Point::new(100.0, 50.0),
    "https://x/",
    "https://x/",
  );
  assert_eq!(attr_value(&dom, "readonly", "value").as_deref(), Some("0"));
  assert!(
    !has_attr(&dom, "readonly", "data-fastr-user-validity"),
    "readonly range must not flip user validity"
  );
}

#[test]
fn range_click_focuses_input() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![el(
        "input",
        vec![("id", "r"), ("type", "range"), ("min", "0"), ("max", "100"), ("value", "10")],
        vec![],
      )],
    )],
  )]);

  let range_dom_id = node_id(&dom, "r");
  let mut range_box = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
  range_box.styled_node_id = Some(range_dom_id);
  let box_tree = BoxTree::new(BoxNode::new_block(
    default_style(),
    FormattingContextType::Block,
    vec![range_box],
  ));
  let range_box_id = find_box_id_for_styled_node(&box_tree, range_dom_id);
  let fragment_tree = FragmentTree::new(FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
    vec![FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 100.0, 20.0),
      range_box_id,
      vec![],
    )],
  ));

  let mut engine = InteractionEngine::new();
  let scroll = ScrollState::default();
  engine.pointer_down(&mut dom, &box_tree, &fragment_tree, &scroll, Point::new(10.0, 10.0));
  let (_, action) = engine.pointer_up(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &scroll,
    Point::new(10.0, 10.0),
    "https://x/",
    "https://x/",
  );
  assert!(
    matches!(
      action,
      InteractionAction::FocusChanged { node_id: Some(_) } | InteractionAction::None
    ),
    "pointer_up may emit FocusChanged"
  );
  assert_eq!(
    attr_value(&dom, "r", "data-fastr-focus").as_deref(),
    Some("true"),
    "clicking a range input should focus it"
  );
  assert!(
    !has_attr(&dom, "r", "data-fastr-focus-visible"),
    "pointer focus should not set focus-visible"
  );
}

#[test]
fn tabindex_zero_element_click_focuses_without_focus_visible() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![el("div", vec![("id", "t"), ("tabindex", "0")], vec![])],
    )],
  )]);

  let dom_id = node_id(&dom, "t");
  let mut node = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
  node.styled_node_id = Some(dom_id);
  let box_tree = BoxTree::new(BoxNode::new_block(
    default_style(),
    FormattingContextType::Block,
    vec![node],
  ));
  let box_id = find_box_id_for_styled_node(&box_tree, dom_id);
  let fragment_tree = FragmentTree::new(FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
    vec![FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 100.0, 20.0),
      box_id,
      vec![],
    )],
  ));

  let mut engine = InteractionEngine::new();
  let scroll = ScrollState::default();
  engine.pointer_down(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &scroll,
    Point::new(10.0, 10.0),
  );
  let (changed, action) = engine.pointer_up(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &scroll,
    Point::new(10.0, 10.0),
    "https://x/",
    "https://x/",
  );
  assert!(changed);
  match action {
    InteractionAction::FocusChanged { node_id } => assert_eq!(node_id, Some(dom_id)),
    InteractionAction::None => {}
    other => panic!("unexpected pointer_up action: {other:?}"),
  }
  assert_eq!(
    attr_value(&dom, "t", "data-fastr-focus").as_deref(),
    Some("true")
  );
  assert!(
    !has_attr(&dom, "t", "data-fastr-focus-visible"),
    "pointer focus should not set focus-visible"
  );
}

#[test]
fn tab_traverses_focusable_elements_in_tree_order_and_skips_inert_disabled_and_tabindex_negative() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![
        el("input", vec![("id", "first")], vec![]),
        el("textarea", vec![("id", "ta")], vec![]),
        el("a", vec![("id", "link"), ("href", "/")], vec![]),
        el("button", vec![("id", "skip"), ("tabindex", "-1")], vec![]),
        el("input", vec![("id", "disabled"), ("disabled", "")], vec![]),
        el(
          "div",
          vec![("id", "inert"), ("data-fastr-inert", "true")],
          vec![el("input", vec![("id", "inert-input")], vec![])],
        ),
        el("div", vec![("id", "tabbed"), ("tabindex", "0")], vec![]),
        el("button", vec![("id", "last")], vec![]),
      ],
    )],
  )]);

  // Click to focus the first input (pointer focus doesn't set focus-visible).
  let first_dom_id = node_id(&dom, "first");
  let mut first_box = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
  first_box.styled_node_id = Some(first_dom_id);
  let box_tree = BoxTree::new(BoxNode::new_block(
    default_style(),
    FormattingContextType::Block,
    vec![first_box],
  ));

  let first_box_id = find_box_id_for_styled_node(&box_tree, first_dom_id);
  let fragment_tree = FragmentTree::new(FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
    vec![FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 80.0, 20.0),
      first_box_id,
      vec![],
    )],
  ));

  let mut engine = InteractionEngine::new();
  let scroll = ScrollState::default();
  engine.pointer_down(&mut dom, &box_tree, &fragment_tree, &scroll, Point::new(5.0, 5.0));
  let (changed, _) = engine.pointer_up(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &scroll,
    Point::new(5.0, 5.0),
    "https://x/",
    "https://x/",
  );
  assert!(changed);
  assert_eq!(
    attr_value(&dom, "first", "data-fastr-focus").as_deref(),
    Some("true")
  );
  assert!(
    !has_attr(&dom, "first", "data-fastr-focus-visible"),
    "pointer focus should not set focus-visible"
  );

  // Tab sequence: first -> textarea -> link -> tabindex=0 div -> last button -> wrap to first.
  assert!(engine.key_action(&mut dom, KeyAction::Tab));
  assert_eq!(
    attr_value(&dom, "ta", "data-fastr-focus").as_deref(),
    Some("true")
  );
  assert_eq!(
    attr_value(&dom, "ta", "data-fastr-focus-visible").as_deref(),
    Some("true")
  );

  assert!(engine.key_action(&mut dom, KeyAction::Tab));
  assert_eq!(
    attr_value(&dom, "link", "data-fastr-focus").as_deref(),
    Some("true")
  );
  assert_eq!(
    attr_value(&dom, "link", "data-fastr-focus-visible").as_deref(),
    Some("true")
  );

  assert!(
    engine.key_action(&mut dom, KeyAction::Tab),
    "should skip tabindex=-1, disabled and inert descendants"
  );
  assert_eq!(
    attr_value(&dom, "tabbed", "data-fastr-focus").as_deref(),
    Some("true")
  );
  assert!(!has_attr(&dom, "skip", "data-fastr-focus"));
  assert!(!has_attr(&dom, "disabled", "data-fastr-focus"));
  assert!(!has_attr(&dom, "inert-input", "data-fastr-focus"));

  assert!(engine.key_action(&mut dom, KeyAction::Tab));
  assert_eq!(
    attr_value(&dom, "last", "data-fastr-focus").as_deref(),
    Some("true")
  );
  assert_eq!(
    attr_value(&dom, "last", "data-fastr-focus-visible").as_deref(),
    Some("true")
  );

  assert!(engine.key_action(&mut dom, KeyAction::Tab));
  assert_eq!(
    attr_value(&dom, "first", "data-fastr-focus").as_deref(),
    Some("true")
  );
  assert_eq!(
    attr_value(&dom, "first", "data-fastr-focus-visible").as_deref(),
    Some("true")
  );
}

#[test]
fn tab_focuses_first_focusable_element_when_nothing_focused() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![
        el("div", vec![("id", "plain")], vec![]),
        el("button", vec![("id", "btn")], vec![]),
        el("input", vec![("id", "inp")], vec![]),
      ],
    )],
  )]);

  let mut engine = InteractionEngine::new();
  assert!(engine.key_action(&mut dom, KeyAction::Tab));
  assert_eq!(
    attr_value(&dom, "btn", "data-fastr-focus").as_deref(),
    Some("true")
  );
  assert_eq!(
    attr_value(&dom, "btn", "data-fastr-focus-visible").as_deref(),
    Some("true")
  );

  assert!(engine.key_action(&mut dom, KeyAction::Tab));
  assert_eq!(
    attr_value(&dom, "inp", "data-fastr-focus").as_deref(),
    Some("true")
  );
  assert_eq!(
    attr_value(&dom, "inp", "data-fastr-focus-visible").as_deref(),
    Some("true")
  );
}

#[test]
fn shift_tab_focuses_last_focusable_element_when_nothing_focused() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![
        el("div", vec![("id", "plain")], vec![]),
        el("button", vec![("id", "btn")], vec![]),
        el("input", vec![("id", "inp")], vec![]),
      ],
    )],
  )]);

  let mut engine = InteractionEngine::new();
  assert!(engine.key_action(&mut dom, KeyAction::ShiftTab));
  assert_eq!(
    attr_value(&dom, "inp", "data-fastr-focus").as_deref(),
    Some("true")
  );
  assert_eq!(
    attr_value(&dom, "inp", "data-fastr-focus-visible").as_deref(),
    Some("true")
  );

  // Traverse backward.
  assert!(engine.key_action(&mut dom, KeyAction::ShiftTab));
  assert_eq!(
    attr_value(&dom, "btn", "data-fastr-focus").as_deref(),
    Some("true")
  );
  assert_eq!(
    attr_value(&dom, "btn", "data-fastr-focus-visible").as_deref(),
    Some("true")
  );

  // Wrap backward.
  assert!(engine.key_action(&mut dom, KeyAction::ShiftTab));
  assert_eq!(
    attr_value(&dom, "inp", "data-fastr-focus").as_deref(),
    Some("true")
  );
}

#[test]
fn tab_focuses_area_href_elements() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![
        el("div", vec![("id", "plain")], vec![]),
        el("area", vec![("id", "area"), ("href", "/a")], vec![]),
        el("input", vec![("id", "inp")], vec![]),
      ],
    )],
  )]);

  let mut engine = InteractionEngine::new();
  assert!(engine.key_action(&mut dom, KeyAction::Tab));
  assert_eq!(
    attr_value(&dom, "area", "data-fastr-focus").as_deref(),
    Some("true")
  );
  assert_eq!(
    attr_value(&dom, "area", "data-fastr-focus-visible").as_deref(),
    Some("true")
  );

  assert!(engine.key_action(&mut dom, KeyAction::Tab));
  assert_eq!(
    attr_value(&dom, "inp", "data-fastr-focus").as_deref(),
    Some("true")
  );
}
