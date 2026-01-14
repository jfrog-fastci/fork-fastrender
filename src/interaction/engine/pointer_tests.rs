use super::{InteractionAction, InteractionEngine, KeyAction};
use crate::dom::{enumerate_dom_ids, DomNode, DomNodeType, ShadowRootMode, HTML_NAMESPACE};
use crate::geometry::{Point, Rect};
use crate::scroll::ScrollState;
use crate::style::display::FormattingContextType;
use crate::style::types::{Appearance, LineHeight, PointerEvents};
use crate::style::ComputedStyle;
use crate::text::caret::CaretAffinity;
use crate::tree::box_tree::{
  BoxNode, BoxTree, FormControl, FormControlKind, ReplacedType, SelectControl, SelectItem, TextControlKind,
};
use crate::tree::fragment_tree::{FragmentNode, FragmentTree, TextSourceRange};
use crate::ui::messages::{PointerButton, PointerModifiers};
use crate::ui::render_worker::viewport_point_for_pos_css;
use crate::Length;
use selectors::context::QuirksMode;
use std::sync::Arc;
use url::Url;

fn doc(children: Vec<DomNode>) -> DomNode {
  DomNode {
    node_type: DomNodeType::Document {
      quirks_mode: QuirksMode::NoQuirks,
      scripting_enabled: true,
      is_html_document: true,
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

fn text(content: &str) -> DomNode {
  DomNode {
    node_type: DomNodeType::Text {
      content: content.to_string(),
    },
    children: vec![],
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

fn find_text_by_content<'a>(root: &'a DomNode, content: &str) -> Option<&'a DomNode> {
  let mut stack = vec![root];
  while let Some(node) = stack.pop() {
    if matches!(&node.node_type, DomNodeType::Text { content: c } if c == content) {
      return Some(node);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  None
}

fn find_by_id_mut<'a>(root: &'a mut DomNode, html_id: &str) -> Option<&'a mut DomNode> {
  if root.get_attribute_ref("id") == Some(html_id) {
    return Some(root);
  }
  for child in root.children.iter_mut() {
    if let Some(found) = find_by_id_mut(child, html_id) {
      return Some(found);
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

fn text_node_id(root: &DomNode, content: &str) -> usize {
  let ids = enumerate_dom_ids(root);
  let node = find_text_by_content(root, content).expect("text node");
  ids
    .get(&(node as *const DomNode))
    .copied()
    .expect("id present")
}

fn default_style() -> Arc<ComputedStyle> {
  Arc::new(ComputedStyle::default())
}

fn text_input_form_control(value: &str) -> FormControl {
  FormControl {
    control: FormControlKind::Text {
      value: value.to_string(),
      placeholder: None,
      placeholder_style: None,
      size_attr: None,
      kind: TextControlKind::Plain,
      caret: value.chars().count(),
      caret_affinity: CaretAffinity::Downstream,
      selection: None,
    },
    appearance: Appearance::Auto,
    placeholder_style: None,
    slider_thumb_style: None,
    slider_track_style: None,
    progress_bar_style: None,
    progress_value_style: None,
    meter_bar_style: None,
    meter_optimum_value_style: None,
    meter_suboptimum_value_style: None,
    meter_even_less_good_value_style: None,
    file_selector_button_style: None,
    disabled: false,
    focused: false,
    focus_visible: false,
    required: false,
    invalid: false,
    ime_preedit: None,
  }
}

fn textarea_form_control(value: &str) -> FormControl {
  FormControl {
    control: FormControlKind::TextArea {
      value: value.to_string(),
      placeholder: None,
      placeholder_style: None,
      rows: None,
      cols: None,
      caret: value.chars().count(),
      caret_affinity: CaretAffinity::Downstream,
      selection: None,
    },
    appearance: Appearance::Auto,
    placeholder_style: None,
    slider_thumb_style: None,
    slider_track_style: None,
    progress_bar_style: None,
    progress_value_style: None,
    meter_bar_style: None,
    meter_optimum_value_style: None,
    meter_suboptimum_value_style: None,
    meter_even_less_good_value_style: None,
    file_selector_button_style: None,
    disabled: false,
    focused: false,
    focus_visible: false,
    required: false,
    invalid: false,
    ime_preedit: None,
  }
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
fn details_summary_click_on_descendant_sets_click_target_and_toggles_open() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![el(
        "details",
        vec![("id", "d")],
        vec![
          el(
            "summary",
            vec![("id", "s")],
            vec![el("span", vec![("id", "inner")], vec![text("Title")])],
          ),
          el("div", vec![("id", "content")], vec![text("Hidden")]),
        ],
      )],
    )],
  )]);

  let summary_dom_id = node_id(&dom, "s");
  let inner_dom_id = node_id(&dom, "inner");

  let mut inner_box = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
  inner_box.styled_node_id = Some(inner_dom_id);
  let mut summary_box =
    BoxNode::new_block(default_style(), FormattingContextType::Block, vec![inner_box]);
  summary_box.styled_node_id = Some(summary_dom_id);
  let box_tree = BoxTree::new(BoxNode::new_block(
    default_style(),
    FormattingContextType::Block,
    vec![summary_box],
  ));

  let summary_box_id = find_box_id_for_styled_node(&box_tree, summary_dom_id);
  let inner_box_id = find_box_id_for_styled_node(&box_tree, inner_dom_id);
  let fragment_tree = FragmentTree::new(FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
    vec![FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 200.0, 40.0),
      summary_box_id,
      vec![FragmentNode::new_block_with_id(
        Rect::from_xywh(0.0, 0.0, 40.0, 40.0),
        inner_box_id,
        vec![],
      )],
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

  let (_dom_changed, action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    // Release within the `<summary>` but outside the `<span id=inner>`.
    Point::new(190.0, 10.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
    "https://example.com/base/",
    "https://example.com/base/",
  );
  assert_eq!(
    action,
    InteractionAction::FocusChanged {
      node_id: Some(summary_dom_id),
    }
  );

  assert!(
    has_attr(&dom, "d", "open"),
    "expected click within <summary> subtree to toggle <details open>"
  );
  assert_eq!(
    engine.interaction_state().focused,
    Some(summary_dom_id),
    "summary activation should focus the <summary>"
  );
  assert_eq!(
    engine.take_last_click_target(),
    Some(summary_dom_id),
    "summary activation should report the <summary> as the click target"
  );
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
              vec![
                ("id", "a1"),
                ("type", "radio"),
                ("name", "g"),
                ("checked", ""),
              ],
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
            vec![
              ("id", "b1"),
              ("type", "radio"),
              ("name", "g"),
              ("checked", ""),
            ],
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
  let (changed, action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(10.0, 10.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
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
    "pointer_move should set hover state"
  );
  let state = engine.interaction_state();
  for id in ["inner", "outer", "body", "html"] {
    let node_id = node_id(&dom, id);
    assert_eq!(state.is_hovered(node_id), true, "{id} should be hovered");
    assert!(
      !has_attr(&dom, id, "data-fastr-hover"),
      "{id} must not have a data-fastr-hover attribute"
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
    "moving off target should clear hover state"
  );
  let state = engine.interaction_state();
  for id in ["inner", "outer", "body", "html"] {
    let node_id = node_id(&dom, id);
    assert!(!state.is_hovered(node_id), "{id} hover should be cleared");
    assert!(
      !has_attr(&dom, id, "data-fastr-hover"),
      "{id} must not have a data-fastr-hover attribute"
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
    "pointer_down should set active state"
  );
  let state = engine.interaction_state();
  for id in ["inner", "outer", "body", "html"] {
    let node_id = node_id(&dom, id);
    assert_eq!(state.is_active(node_id), true, "{id} should be active");
    assert!(
      !has_attr(&dom, id, "data-fastr-active"),
      "{id} must not have a data-fastr-active attribute"
    );
  }

  let (changed, action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(15.0, 15.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
    "https://x/",
    "https://x/",
  );
  assert!(changed);
  assert_eq!(action, InteractionAction::None);

  let state = engine.interaction_state();
  for id in ["inner", "outer", "body", "html"] {
    let node_id = node_id(&dom, id);
    assert!(!state.is_active(node_id), "{id} active should be cleared");
    assert!(
      !has_attr(&dom, id, "data-fastr-active"),
      "{id} must not have a data-fastr-active attribute"
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
  let (changed, action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(10.0, 10.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
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
  assert_eq!(engine.take_last_visited_candidate(), Some(link_dom_id));
  assert!(engine.mark_link_visited(link_dom_id));
  assert!(engine.interaction_state().is_visited_link(link_dom_id));
  assert!(!has_attr(&dom, "link", "data-fastr-visited"));
}

#[test]
fn link_middle_click_opens_in_new_tab() {
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
  let (_, action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(10.0, 10.0),
    PointerButton::Middle,
    PointerModifiers::default(),
    true,
    "https://example.com/base/",
    "https://example.com/base/",
  );
  assert_eq!(
    action,
    InteractionAction::OpenInNewTab {
      href: "https://example.com/base/foo".to_string()
    }
  );
  assert_eq!(engine.take_last_visited_candidate(), Some(link_dom_id));
  assert!(engine.mark_link_visited(link_dom_id));
  assert!(engine.interaction_state().is_visited_link(link_dom_id));
  assert_eq!(
    engine.interaction_state().focused,
    None,
    "middle-click should not move focus"
  );
  assert_eq!(
    engine.take_last_click_target(),
    Some(link_dom_id),
    "middle-click should populate last_click_target (used for dispatching auxclick events)"
  );
}

#[test]
fn link_command_click_opens_in_new_tab() {
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

  let modifiers = if cfg!(target_os = "macos") {
    PointerModifiers::META
  } else {
    PointerModifiers::CTRL
  };

  let mut engine = InteractionEngine::new();
  engine.pointer_down(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(10.0, 10.0),
  );
  let (_, action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(10.0, 10.0),
    PointerButton::Primary,
    modifiers,
    true,
    "https://example.com/base/",
    "https://example.com/base/",
  );
  assert_eq!(
    action,
    InteractionAction::OpenInNewTab {
      href: "https://example.com/base/foo".to_string()
    }
  );
  assert_eq!(engine.take_last_visited_candidate(), Some(link_dom_id));
  assert!(engine.mark_link_visited(link_dom_id));
  assert!(engine.interaction_state().is_visited_link(link_dom_id));
}

#[test]
fn link_target_blank_opens_in_new_tab() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![el(
        "a",
        vec![("id", "link"), ("href", "foo"), ("target", "  _BLANK  ")],
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
  let (_, action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(10.0, 10.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
    "https://example.com/base/",
    "https://example.com/base/",
  );
  assert_eq!(
    action,
    InteractionAction::OpenInNewTab {
      href: "https://example.com/base/foo".to_string()
    }
  );
  assert_eq!(engine.take_last_visited_candidate(), Some(link_dom_id));
  assert!(engine.mark_link_visited(link_dom_id));
  assert!(engine.interaction_state().is_visited_link(link_dom_id));
}

#[test]
fn checkbox_secondary_click_does_not_toggle_or_focus() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![el(
        "input",
        vec![("id", "c"), ("type", "checkbox")],
        vec![],
      )],
    )],
  )]);

  let checkbox_dom_id = node_id(&dom, "c");
  let mut checkbox_box = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
  checkbox_box.styled_node_id = Some(checkbox_dom_id);
  let box_tree = BoxTree::new(BoxNode::new_block(
    default_style(),
    FormattingContextType::Block,
    vec![checkbox_box],
  ));

  let checkbox_box_id = find_box_id_for_styled_node(&box_tree, checkbox_dom_id);
  let fragment_tree = FragmentTree::new(FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
    vec![FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 50.0, 50.0),
      checkbox_box_id,
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
  let (_changed, action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(10.0, 10.0),
    PointerButton::Secondary,
    PointerModifiers::default(),
    true,
    "https://example.com/base/",
    "https://example.com/base/",
  );
  assert_eq!(action, InteractionAction::None);
  assert!(
    !has_attr(&dom, "c", "checked"),
    "secondary-click should not toggle checkboxes"
  );
  assert_eq!(
    engine.interaction_state().focused,
    None,
    "secondary-click should not move focus"
  );
  assert_eq!(
    engine.take_last_click_target(),
    None,
    "secondary-click should not populate last_click_target (used for dispatching click events)"
  );
}

#[test]
fn img_usemap_area_click_emits_navigation_and_sets_area_visited() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![
        el("img", vec![("id", "img"), ("usemap", "#m")], vec![]),
        el(
          "map",
          vec![("id", "m")],
          vec![el(
            "area",
            vec![
              ("id", "area"),
              ("href", "foo"),
              ("shape", "rect"),
              ("coords", "0,0,10,10"),
            ],
            vec![],
          )],
        ),
      ],
    )],
  )]);

  let body_dom_id = node_id(&dom, "body");
  let img_dom_id = node_id(&dom, "img");
  let area_dom_id = node_id(&dom, "area");

  let mut img_box = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
  img_box.styled_node_id = Some(img_dom_id);
  let mut body_box =
    BoxNode::new_block(default_style(), FormattingContextType::Block, vec![img_box]);
  body_box.styled_node_id = Some(body_dom_id);
  let box_tree = BoxTree::new(body_box);

  let img_box_id = find_box_id_for_styled_node(&box_tree, img_dom_id);
  let fragment_tree = FragmentTree::new(FragmentNode::new_block_with_id(
    Rect::from_xywh(50.0, 50.0, 200.0, 200.0),
    box_tree.root.id,
    vec![FragmentNode::new_block_with_id(
      Rect::from_xywh(10.0, 10.0, 100.0, 100.0),
      img_box_id,
      vec![],
    )],
  ));

  let mut engine = InteractionEngine::new();
  engine.pointer_down(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(65.0, 65.0),
  );
  let (_, action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(65.0, 65.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
    "https://example.com/base/",
    "https://example.com/base/",
  );
  assert_eq!(
    action,
    InteractionAction::Navigate {
      href: "https://example.com/base/foo".to_string()
    }
  );
  assert_eq!(engine.take_last_visited_candidate(), Some(area_dom_id));
  assert!(engine.mark_link_visited(area_dom_id));
  assert!(engine.interaction_state().is_visited_link(area_dom_id));
  assert!(!has_attr(&dom, "area", "data-fastr-visited"));
}

#[test]
fn anchor_activation_appends_ismap_coordinates() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![el(
        "a",
        vec![("id", "link"), ("href", "foo")],
        vec![el("img", vec![("id", "img"), ("ismap", "")], vec![])],
      )],
    )],
  )]);

  let link_dom_id = node_id(&dom, "link");
  let img_dom_id = node_id(&dom, "img");

  let mut img_box = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
  img_box.styled_node_id = Some(img_dom_id);
  let mut link_box =
    BoxNode::new_block(default_style(), FormattingContextType::Block, vec![img_box]);
  link_box.styled_node_id = Some(link_dom_id);
  let box_tree = BoxTree::new(link_box);

  let img_box_id = find_box_id_for_styled_node(&box_tree, img_dom_id);
  let fragment_tree = FragmentTree::new(FragmentNode::new_block_with_id(
    Rect::from_xywh(50.0, 50.0, 200.0, 200.0),
    box_tree.root.id,
    vec![FragmentNode::new_block_with_id(
      Rect::from_xywh(10.0, 10.0, 100.0, 100.0),
      img_box_id,
      vec![],
    )],
  ));

  let mut engine = InteractionEngine::new();
  // Image absolute origin: (50+10, 50+10) = (60, 60). Click at (75, 95) => local (15, 35).
  engine.pointer_down(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(75.0, 95.0),
  );
  let (_, action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(75.0, 95.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
    "https://example.com/base/",
    "https://example.com/base/",
  );
  assert_eq!(
    action,
    InteractionAction::Navigate {
      href: "https://example.com/base/foo?15,35".to_string()
    }
  );
  assert_eq!(engine.take_last_visited_candidate(), Some(link_dom_id));
  assert!(engine.mark_link_visited(link_dom_id));
  assert!(engine.interaction_state().is_visited_link(link_dom_id));
  assert!(!has_attr(&dom, "link", "data-fastr-visited"));
}

#[test]
fn link_click_trims_ascii_whitespace_but_preserves_nbsp() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![el(
        "a",
        vec![("id", "link"), ("href", " \u{00A0} ")],
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
  let (_, action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(10.0, 10.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
    "https://example.com/base/",
    "https://example.com/base/",
  );

  let expected = Url::parse("https://example.com/base/")
    .unwrap()
    .join("\u{00A0}")
    .unwrap()
    .to_string();
  assert_eq!(action, InteractionAction::Navigate { href: expected });
  assert_eq!(engine.take_last_visited_candidate(), Some(link_dom_id));
  assert!(engine.mark_link_visited(link_dom_id));
  assert!(engine.interaction_state().is_visited_link(link_dom_id));
  assert!(!has_attr(&dom, "link", "data-fastr-visited"));
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
  let (_, action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(10.0, 10.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
    "https://example.com/base/",
    "https://example.com/base/",
  );

  let expected = Url::parse("https://example.com/base/")
    .unwrap()
    .join(href)
    .unwrap()
    .to_string();
  assert_eq!(action, InteractionAction::Navigate { href: expected });
  assert_eq!(engine.take_last_visited_candidate(), Some(link_dom_id));
  assert!(engine.mark_link_visited(link_dom_id));
  assert!(engine.interaction_state().is_visited_link(link_dom_id));
  assert!(!has_attr(&dom, "link", "data-fastr-visited"));
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
  let (changed, action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
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
  assert_eq!(engine.interaction_state().focused, Some(cb_dom_id));
  assert!(!engine.interaction_state().focus_visible);
  assert!(!has_attr(&dom, "cb", "data-fastr-focus"));
}

#[test]
fn range_drag_ignores_sentinel_pointer_positions() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![el(
        "input",
        vec![
          ("id", "range"),
          ("type", "range"),
          ("min", "0"),
          ("max", "100"),
          ("value", "50"),
        ],
        vec![],
      )],
    )],
  )]);

  let range_dom_id = node_id(&dom, "range");
  let mut range_box = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
  range_box.styled_node_id = Some(range_dom_id);
  let box_tree = BoxTree::new(BoxNode::new_block(
    default_style(),
    FormattingContextType::Block,
    vec![range_box],
  ));

  let range_box_id = find_box_id_for_styled_node(&box_tree, range_dom_id);
  // Simulate a scrolled viewport so that the browser UI's pointer-leave sentinel needs to be
  // translated by `viewport_point_for_pos_css` to remain negative after applying scroll.
  let scroll = ScrollState::with_viewport(Point::new(50.0, 25.0));

  let fragment_tree = FragmentTree::new(FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 400.0, 400.0),
    vec![FragmentNode::new_block_with_id(
      // The range control lives in page coordinates; place it beyond the scroll offset so viewport
      // coordinates remain non-negative for the initial pointer-down.
      Rect::from_xywh(60.0, 40.0, 100.0, 20.0),
      range_box_id,
      vec![],
    )],
  ));

  let mut engine = InteractionEngine::new();
  assert!(
    engine.pointer_down(
      &mut dom,
      &box_tree,
      &fragment_tree,
      &scroll,
      // Viewport point that maps to a click around 75% along the slider in page space.
      Point::new(85.0, 30.0),
    ),
    "expected pointer_down to set active state and update range value"
  );
  let value_after_down = attr_value(&dom, "range", "value")
    .and_then(|v| v.parse::<f64>().ok())
    .expect("range value after pointer_down");
  assert!(
    (value_after_down - 75.0).abs() < 1e-6,
    "expected pointer_down to set range value to ~75, got {value_after_down}"
  );

  // The browser UI uses a sentinel `(-1, -1)` pointer position when the cursor leaves the page.
  // Range drags should ignore this rather than snapping to the minimum value.
  let sentinel_viewport_point = viewport_point_for_pos_css(&scroll, (-1.0, -1.0));
  engine.pointer_move(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &scroll,
    sentinel_viewport_point,
  );
  let value_after_move = attr_value(&dom, "range", "value")
    .and_then(|v| v.parse::<f64>().ok())
    .expect("range value after pointer_move");
  assert!(
    (value_after_move - value_after_down).abs() < 1e-6,
    "expected sentinel pointer_move to keep range value at {value_after_down}, got {value_after_move}"
  );

  let (_changed, _action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &scroll,
    sentinel_viewport_point,
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
    "https://example.com/",
    "https://example.com/",
  );

  let value_after_up = attr_value(&dom, "range", "value")
    .and_then(|v| v.parse::<f64>().ok())
    .expect("range value after pointer_up");
  assert!(
    (value_after_up - value_after_down).abs() < 1e-6,
    "expected sentinel pointer_up to keep range value at {value_after_down}, got {value_after_up}"
  );
}

#[test]
fn text_selection_drag_ignores_sentinel_pointer_positions() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![el("input", vec![("id", "txt"), ("value", "hello")], vec![])],
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

  let scroll = ScrollState::with_viewport(Point::new(50.0, 25.0));
  let fragment_tree = FragmentTree::new(FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 400.0, 400.0),
    vec![FragmentNode::new_block_with_id(
      // Place the input beyond the scroll offset so our initial drag points stay in viewport space.
      Rect::from_xywh(80.0, 60.0, 200.0, 30.0),
      input_box_id,
      vec![],
    )],
  ));

  let mut engine = InteractionEngine::new();

  // Start a selection drag near the left edge (caret at/near start).
  engine.pointer_down(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &scroll,
    Point::new(35.0, 50.0),
  );

  // Drag to the right edge to create a selection.
  engine.pointer_move(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &scroll,
    Point::new(225.0, 50.0),
  );

  let before = engine
    .interaction_state()
    .text_edit_for(input_dom_id)
    .copied()
    .expect("expected text edit state after drag");
  assert!(
    before.selection.is_some(),
    "expected drag to create a selection highlight"
  );

  // The browser UI uses a sentinel `(-1, -1)` pointer position when the cursor leaves the page.
  // Text selection drags should ignore this rather than collapsing the selection to the start.
  let sentinel_viewport_point = viewport_point_for_pos_css(&scroll, (-1.0, -1.0));
  engine.pointer_move(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &scroll,
    sentinel_viewport_point,
  );

  let after = engine
    .interaction_state()
    .text_edit_for(input_dom_id)
    .copied()
    .expect("expected text edit state after sentinel drag move");
  assert_eq!(
    after, before,
    "sentinel pointer_move must not update caret/selection state during a drag"
  );
}

#[test]
fn cancelled_click_does_not_blur_focused_control() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![el("input", vec![("id", "txt"), ("type", "text")], vec![])],
    )],
  )]);

  let txt_dom_id = node_id(&dom, "txt");
  let mut txt_box = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
  txt_box.styled_node_id = Some(txt_dom_id);
  let box_tree = BoxTree::new(BoxNode::new_block(
    default_style(),
    FormattingContextType::Block,
    vec![txt_box],
  ));

  let txt_box_id = find_box_id_for_styled_node(&box_tree, txt_dom_id);
  let fragment_tree = FragmentTree::new(FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
    vec![FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 100.0, 20.0),
      txt_box_id,
      vec![],
    )],
  ));

  let mut engine = InteractionEngine::new();
  let (changed, _action) = engine.focus_node_id(&mut dom, Some(txt_dom_id), false);
  assert!(changed, "expected focus_node_id to set focus flags");
  assert_eq!(engine.interaction_state().focused, Some(txt_dom_id));
  assert!(!engine.interaction_state().focus_visible);
  assert!(!has_attr(&dom, "txt", "data-fastr-focus"));

  engine.pointer_down(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(10.0, 10.0),
  );

  // Release outside the element (no click qualifies), which should not blur the previously focused
  // control.
  let (_changed, action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(150.0, 10.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
    "https://example.com/",
    "https://example.com/",
  );
  assert_eq!(action, InteractionAction::None);
  assert_eq!(engine.interaction_state().focused, Some(txt_dom_id));
  assert!(!engine.interaction_state().focus_visible);
  assert!(!has_attr(&dom, "txt", "data-fastr-focus"));
}

#[test]
fn space_key_toggles_focused_checkbox() {
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
  let _ = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
    "https://x/",
    "https://x/",
  );
  assert!(
    has_attr(&dom, "cb", "checked"),
    "click should check the box"
  );

  let (changed, action) =
    engine.key_activate(&mut dom, KeyAction::Space, "https://x/", "https://x/");
  assert!(changed, "space should toggle the checkbox");
  assert_eq!(action, InteractionAction::None);
  assert!(
    !has_attr(&dom, "cb", "checked"),
    "space should uncheck the focused checkbox"
  );
  assert_eq!(engine.interaction_state().focused, Some(cb_dom_id));
  assert!(engine.interaction_state().focus_visible);
  assert!(!has_attr(&dom, "cb", "data-fastr-focus-visible"));
}

#[test]
fn space_key_activates_focused_radio() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![el(
        "form",
        vec![],
        vec![
          el(
            "input",
            vec![
              ("id", "r1"),
              ("type", "radio"),
              ("name", "g"),
              ("checked", ""),
            ],
            vec![],
          ),
          el(
            "input",
            vec![("id", "r2"), ("type", "radio"), ("name", "g")],
            vec![],
          ),
        ],
      )],
    )],
  )]);

  let r1_dom_id = node_id(&dom, "r1");
  let mut r1_box = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
  r1_box.styled_node_id = Some(r1_dom_id);
  let box_tree = BoxTree::new(BoxNode::new_block(
    default_style(),
    FormattingContextType::Block,
    vec![r1_box],
  ));
  let r1_box_id = find_box_id_for_styled_node(&box_tree, r1_dom_id);
  let fragment_tree = FragmentTree::new(FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
    vec![FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 20.0, 20.0),
      r1_box_id,
      vec![],
    )],
  ));

  let mut engine = InteractionEngine::new();
  // Focus r1 via pointer click. It starts checked, so activation should be a no-op.
  engine.pointer_down(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
  );
  let _ = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
    "https://x/",
    "https://x/",
  );

  // Simulate script clearing checked state while the control remains focused.
  let r1 = find_by_id_mut(&mut dom, "r1").expect("r1 node");
  crate::interaction::dom_mutation::remove_attr(r1, "checked");
  assert!(!has_attr(&dom, "r1", "checked"));

  let (changed, action) =
    engine.key_activate(&mut dom, KeyAction::Space, "https://x/", "https://x/");
  assert!(changed, "space should activate the radio");
  assert_eq!(action, InteractionAction::None);
  assert!(
    has_attr(&dom, "r1", "checked"),
    "space should check the focused radio"
  );
  assert!(
    !has_attr(&dom, "r2", "checked"),
    "other radios in the group should remain unchecked"
  );
  assert_eq!(engine.interaction_state().focused, Some(r1_dom_id));
  assert!(engine.interaction_state().focus_visible);
  assert!(!has_attr(&dom, "r1", "data-fastr-focus-visible"));
}

#[test]
fn enter_key_on_focused_link_emits_navigation() {
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
  let _ = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(10.0, 10.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
    "https://example.com/base/",
    "https://example.com/base/",
  );

  let (changed, action) = engine.key_activate(
    &mut dom,
    KeyAction::Enter,
    "https://example.com/base/",
    "https://example.com/base/",
  );
  assert!(changed, "enter should set focus-visible");
  assert_eq!(
    action,
    InteractionAction::Navigate {
      href: "https://example.com/base/foo".to_string()
    }
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
  let (changed, action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
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
  assert_eq!(engine.interaction_state().focused, Some(cb_dom_id));
  assert!(!engine.interaction_state().focus_visible);
  assert!(!has_attr(&dom, "cb", "data-fastr-focus"));
}

#[test]
fn label_for_ignores_non_form_control_target() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![
        el("label", vec![("id", "lbl"), ("for", "x")], vec![]),
        el("a", vec![("id", "x"), ("href", "/foo")], vec![]),
      ],
    )],
  )]);

  let label_dom_id = node_id(&dom, "lbl");
  let anchor_dom_id = node_id(&dom, "x");

  let mut label_box = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
  label_box.styled_node_id = Some(label_dom_id);
  let box_tree = BoxTree::new(BoxNode::new_block(
    default_style(),
    FormattingContextType::Block,
    vec![label_box],
  ));

  let label_box_id = find_box_id_for_styled_node(&box_tree, label_dom_id);
  let fragment_tree = FragmentTree::new(FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
    vec![FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 40.0, 20.0),
      label_box_id,
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
  let (_changed, action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
    "https://x/",
    "https://x/",
  );

  assert!(
    !matches!(action, InteractionAction::Navigate { .. }),
    "label[for] should only resolve to labelable form controls"
  );
  assert!(!engine.interaction_state().is_visited_link(anchor_dom_id));
  assert_ne!(engine.interaction_state().focused, Some(anchor_dom_id));
  assert!(!has_attr(&dom, "x", "data-fastr-visited"));
  assert!(!has_attr(&dom, "x", "data-fastr-focus"));
}

#[test]
fn label_for_does_not_cross_shadow_root_boundary() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![
        el("input", vec![("id", "cb"), ("type", "checkbox")], vec![]),
        el(
          "div",
          vec![("id", "host")],
          vec![DomNode {
            node_type: DomNodeType::ShadowRoot {
              mode: ShadowRootMode::Open,
              delegates_focus: false,
            },
            children: vec![el("label", vec![("id", "lbl"), ("for", "cb")], vec![])],
          }],
        ),
      ],
    )],
  )]);

  let label_dom_id = node_id(&dom, "lbl");

  let mut label_box = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
  label_box.styled_node_id = Some(label_dom_id);
  let box_tree = BoxTree::new(BoxNode::new_block(
    default_style(),
    FormattingContextType::Block,
    vec![label_box],
  ));

  let label_box_id = find_box_id_for_styled_node(&box_tree, label_dom_id);
  let fragment_tree = FragmentTree::new(FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
    vec![FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 40.0, 20.0),
      label_box_id,
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
  let (_changed, _action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
    "https://x/",
    "https://x/",
  );

  assert!(
    !has_attr(&dom, "cb", "checked"),
    "label `for` must not match an element outside the label's tree root (shadow root boundary)"
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
  let (changed, action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
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
  assert_eq!(engine.interaction_state().focused, Some(radio_dom_id));
  assert!(!engine.interaction_state().focus_visible);
  assert!(!has_attr(&dom, "r", "data-fastr-focus"));
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
  let (changed, _) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
    "https://x/",
    "https://x/",
  );
  assert!(changed);
  assert_eq!(engine.interaction_state().focused, Some(input_dom_id));
  assert!(!engine.interaction_state().focus_visible);
  assert!(!has_attr(&dom, "txt", "data-fastr-focus"));

  // Click outside any focusable element to blur.
  engine.pointer_down(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 60.0),
  );
  let (changed, action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 60.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
    "https://x/",
    "https://x/",
  );
  assert!(changed);
  assert_eq!(
    action,
    InteractionAction::FocusChanged { node_id: None },
    "blurring should emit FocusChanged(None)"
  );
  assert_eq!(engine.interaction_state().focused, None);
  assert!(!engine.interaction_state().focus_visible);
  assert!(!has_attr(&dom, "txt", "data-fastr-focus"));
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
  let (changed, action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
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
  assert_eq!(engine.interaction_state().focused, Some(input_dom_id));
  assert!(!engine.interaction_state().focus_visible);
  assert!(!has_attr(&dom, "txt", "data-fastr-focus-visible"));
  assert!(
    !has_attr(&dom, "txt", "data-fastr-user-validity"),
    "focus should not flip user validity"
  );
  assert!(
    !engine.interaction_state().has_user_validity(input_dom_id),
    "focus should not flip user validity"
  );

  assert!(
    engine.text_input(&mut dom, "abc"),
    "text_input should mutate the DOM"
  );
  assert_eq!(attr_value(&dom, "txt", "value").as_deref(), Some("abc"));
  assert_eq!(engine.interaction_state().focused, Some(input_dom_id));
  assert!(engine.interaction_state().focus_visible);
  assert!(!has_attr(&dom, "txt", "data-fastr-focus-visible"));
  assert!(
    !has_attr(&dom, "txt", "data-fastr-user-validity"),
    "renderer must not inject data-fastr-user-validity onto the DOM"
  );
  assert!(engine.interaction_state().has_user_validity(input_dom_id));
}

#[test]
fn arrow_left_uses_box_tree_direction_for_text_controls() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![el("input", vec![("id", "txt"), ("value", "••")], vec![])],
    )],
  )]);
  let input_dom_id = node_id(&dom, "txt");

  let mut rtl_style = ComputedStyle::default();
  rtl_style.direction = crate::style::types::Direction::Rtl;

  let mut input_box = BoxNode::new_block(Arc::new(rtl_style), FormattingContextType::Block, vec![]);
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
  let (_changed, _action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
    "https://x/",
    "https://x/",
  );
  assert_eq!(engine.interaction_state().focused, Some(input_dom_id));

  // Place the caret at the logical start. In RTL contexts that maps to the visually-rightmost stop,
  // so a visual left-arrow move should advance the caret to the next character.
  engine.key_action_with_box_tree(&mut dom, Some(&box_tree), KeyAction::Home);
  let caret = engine
    .interaction_state()
    .text_edit_for(input_dom_id)
    .expect("expected caret state")
    .caret;
  assert_eq!(caret, 0);

  // Without a `BoxTree` snapshot, arrow-left falls back to inferred LTR direction.
  engine.key_action(&mut dom, KeyAction::ArrowLeft);
  let caret = engine
    .interaction_state()
    .text_edit_for(input_dom_id)
    .expect("expected caret state")
    .caret;
  assert_eq!(caret, 0, "LTR fallback should keep caret at the start");

  engine.key_action_with_box_tree(&mut dom, Some(&box_tree), KeyAction::ArrowLeft);
  let caret = engine
    .interaction_state()
    .text_edit_for(input_dom_id)
    .expect("expected caret state")
    .caret;
  assert_eq!(caret, 1, "RTL caret should advance when moving left");
}

#[test]
fn dir_auto_input_value_infers_rtl_direction_for_caret_movement() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![el(
        "input",
        vec![("id", "txt"), ("dir", "auto"), ("value", "אב")],
        vec![],
      )],
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
  engine.pointer_down(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
  );
  let (_changed, _action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
    "https://x/",
    "https://x/",
  );
  assert_eq!(engine.interaction_state().focused, Some(input_dom_id));

  engine.key_action(&mut dom, KeyAction::Home);
  let caret = engine
    .interaction_state()
    .text_edit_for(input_dom_id)
    .expect("expected caret state")
    .caret;
  assert_eq!(caret, 0);

  // With `dir=auto`, input directionality should be derived from its value. "אב" begins with a
  // strong RTL character, so moving left should advance the caret.
  engine.key_action(&mut dom, KeyAction::ArrowLeft);
  let caret = engine
    .interaction_state()
    .text_edit_for(input_dom_id)
    .expect("expected caret state")
    .caret;
  assert_eq!(caret, 1);
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
  let (changed, action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
    "https://x/",
    "https://x/",
  );
  assert!(changed);
  assert_eq!(
    action,
    InteractionAction::Navigate {
      href: "https://x/".to_string()
    },
    "clicking a submit control should attempt a GET navigation"
  );

  assert!(
    !has_attr(&dom, "f", "data-fastr-user-validity"),
    "renderer must not inject data-fastr-user-validity onto the DOM"
  );
  assert!(
    !has_attr(&dom, "submit", "data-fastr-user-validity"),
    "renderer must not inject data-fastr-user-validity onto the DOM"
  );
  let form_dom_id = node_id(&dom, "f");
  assert!(engine.interaction_state().has_user_validity(form_dom_id));
  assert!(engine.interaction_state().has_user_validity(submit_dom_id));
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
  let (changed, action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
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
  assert!(
    !has_attr(&dom, "form", "data-fastr-user-validity"),
    "renderer must not inject data-fastr-user-validity onto the DOM"
  );
  assert!(
    !has_attr(&dom, "submit", "data-fastr-user-validity"),
    "renderer must not inject data-fastr-user-validity onto the DOM"
  );
  let form_dom_id = node_id(&dom, "form");
  assert!(engine.interaction_state().has_user_validity(form_dom_id));
  assert!(engine.interaction_state().has_user_validity(submit_dom_id));
}

#[test]
fn submit_click_navigates_with_get_query_and_encodes_space_as_plus() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![el(
        "form",
        vec![("id", "f"), ("action", "/search")],
        vec![
          el(
            "input",
            vec![("id", "q"), ("name", "q"), ("value", "a b")],
            vec![],
          ),
          el("input", vec![("id", "submit"), ("type", "submit")], vec![]),
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

  let (_changed, action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
    "https://example.com/doc",
    "https://example.com/base/",
  );

  assert_eq!(
    action,
    InteractionAction::Navigate {
      href: "https://example.com/search?q=a+b".to_string()
    }
  );
}

#[test]
fn submit_click_sanitizes_input_values() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![el(
        "form",
        vec![("id", "f"), ("action", "/submit")],
        vec![
          el(
            "input",
            vec![
              ("id", "n"),
              ("type", "number"),
              ("name", "n"),
              ("value", "abc"),
            ],
            vec![],
          ),
          el(
            "input",
            vec![
              ("id", "d"),
              ("type", "date"),
              ("name", "d"),
              ("value", "2020-13-01"),
            ],
            vec![],
          ),
          el(
            "input",
            vec![
              ("id", "c"),
              ("type", "color"),
              ("name", "c"),
              ("value", "not-a-color"),
            ],
            vec![],
          ),
          el(
            "input",
            vec![("id", "t"), ("name", "t"), ("value", "a\nb")],
            vec![],
          ),
          el("input", vec![("id", "submit"), ("type", "submit")], vec![]),
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

  let (_changed, action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
    "https://example.com/doc",
    "https://example.com/",
  );

  assert_eq!(
    action,
    InteractionAction::Navigate {
      href: "https://example.com/submit?n=&d=&c=%23000000&t=ab".to_string()
    },
    "form submission should use each control's sanitized value"
  );
}

#[test]
fn submit_click_strips_action_fragment() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![el(
        "form",
        vec![("id", "f"), ("action", "/search#frag")],
        vec![
          el(
            "input",
            vec![("id", "q"), ("name", "q"), ("value", "abc")],
            vec![],
          ),
          el("input", vec![("id", "submit"), ("type", "submit")], vec![]),
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

  let (_changed, action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
    "https://example.com/doc",
    "https://example.com/",
  );

  assert_eq!(
    action,
    InteractionAction::Navigate {
      href: "https://example.com/search?q=abc".to_string()
    }
  );
}

#[test]
fn submit_click_uses_form_attr_idref_owner() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![
        el(
          "form",
          vec![("id", "f"), ("action", "/search")],
          vec![el(
            "input",
            vec![("id", "q"), ("name", "q"), ("value", "abc")],
            vec![],
          )],
        ),
        el(
          "input",
          vec![("id", "submit"), ("type", "submit"), ("form", "f")],
          vec![],
        ),
      ],
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

  let (_changed, action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
    "https://example.com/doc",
    "https://example.com/base/",
  );

  assert_eq!(
    action,
    InteractionAction::Navigate {
      href: "https://example.com/search?q=abc".to_string()
    }
  );
}

#[test]
fn submit_click_form_attr_does_not_match_form_inside_template_contents() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![
        el(
          "template",
          vec![("id", "tmpl")],
          vec![el(
            "form",
            vec![("id", "f"), ("action", "/search")],
            vec![el(
              "input",
              vec![("id", "q"), ("name", "q"), ("value", "abc")],
              vec![],
            )],
          )],
        ),
        el(
          "input",
          vec![("id", "submit"), ("type", "submit"), ("form", "f")],
          vec![],
        ),
      ],
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

  let (_changed, action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
    "https://example.com/doc",
    "https://example.com/base/",
  );

  // The only matching `id="f"` is inside an inert `<template>` subtree, so no form owner is found
  // and no navigation occurs.
  assert_eq!(
    action,
    InteractionAction::FocusChanged {
      node_id: Some(submit_dom_id)
    }
  );
  assert!(
    !has_attr(&dom, "f", "data-fastr-user-validity"),
    "renderer must not inject data-fastr-user-validity onto the DOM"
  );
  assert!(
    !has_attr(&dom, "submit", "data-fastr-user-validity"),
    "renderer must not inject data-fastr-user-validity onto the DOM"
  );
  let form_dom_id = node_id(&dom, "f");
  assert!(!engine.interaction_state().has_user_validity(form_dom_id));
  assert!(engine.interaction_state().has_user_validity(submit_dom_id));
}

#[test]
fn submit_click_does_not_mark_form_user_validity_across_shadow_root_boundary() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![
        el(
          "form",
          vec![("id", "f"), ("action", "/search")],
          vec![el(
            "input",
            vec![("id", "q"), ("name", "q"), ("value", "light")],
            vec![],
          )],
        ),
        el(
          "div",
          vec![("id", "host")],
          vec![DomNode {
            node_type: DomNodeType::ShadowRoot {
              mode: ShadowRootMode::Open,
              delegates_focus: false,
            },
            children: vec![el(
              "input",
              vec![("id", "submit"), ("type", "submit"), ("form", "f")],
              vec![],
            )],
          }],
        ),
      ],
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

  let (_changed, action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
    "https://example.com/doc",
    "https://example.com/base/",
  );

  // `form="f"` should not cross the shadow root boundary, so the light DOM `<form id="f">` should
  // not be flagged as user-validity.
  assert_eq!(
    action,
    InteractionAction::FocusChanged {
      node_id: Some(submit_dom_id)
    }
  );
  assert!(
    !has_attr(&dom, "submit", "data-fastr-user-validity"),
    "renderer must not inject data-fastr-user-validity onto the DOM"
  );
  assert!(
    !has_attr(&dom, "f", "data-fastr-user-validity"),
    "renderer must not inject data-fastr-user-validity onto the DOM"
  );
  let form_dom_id = node_id(&dom, "f");
  assert!(!engine.interaction_state().has_user_validity(form_dom_id));
  assert!(engine.interaction_state().has_user_validity(submit_dom_id));
}

#[test]
fn submit_click_includes_selected_select_option_in_query() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![el(
        "form",
        vec![("id", "f"), ("action", "/search")],
        vec![
          el(
            "select",
            vec![("id", "sel"), ("name", "s")],
            vec![
              el(
                "option",
                vec![("id", "o1")],
                vec![DomNode {
                  node_type: DomNodeType::Text {
                    content: "One".to_string(),
                  },
                  children: vec![],
                }],
              ),
              el(
                "option",
                vec![("id", "o2"), ("selected", "")],
                vec![DomNode {
                  node_type: DomNodeType::Text {
                    content: "Two".to_string(),
                  },
                  children: vec![],
                }],
              ),
            ],
          ),
          el("input", vec![("id", "submit"), ("type", "submit")], vec![]),
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

  let (_changed, action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
    "https://example.com/doc",
    "https://example.com/base/",
  );

  assert_eq!(
    action,
    InteractionAction::Navigate {
      href: "https://example.com/search?s=Two".to_string()
    }
  );
}

#[test]
fn submit_click_single_select_prefers_last_selected_option_in_tree_order() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![el(
        "form",
        vec![("id", "f"), ("action", "/search")],
        vec![
          el(
            "select",
            vec![("id", "sel"), ("name", "s")],
            vec![
              el(
                "option",
                vec![("id", "o1"), ("value", "a"), ("selected", "")],
                vec![],
              ),
              el(
                "option",
                vec![("id", "o2"), ("value", "b"), ("selected", "")],
                vec![],
              ),
            ],
          ),
          el("input", vec![("id", "submit"), ("type", "submit")], vec![]),
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

  let (_changed, action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
    "https://example.com/doc",
    "https://example.com/base/",
  );

  assert_eq!(
    action,
    InteractionAction::Navigate {
      href: "https://example.com/search?s=b".to_string()
    },
    "for single-selects, the last <option selected> should win when multiple are marked selected"
  );
}

#[test]
fn submit_click_includes_form_associated_control_outside_form_in_query() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![
        el(
          "form",
          vec![("id", "f"), ("action", "/search")],
          vec![el(
            "input",
            vec![("id", "submit"), ("type", "submit")],
            vec![],
          )],
        ),
        el(
          "input",
          vec![("id", "q"), ("name", "q"), ("value", "abc"), ("form", "f")],
          vec![],
        ),
      ],
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

  let (_changed, action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
    "https://example.com/doc",
    "https://example.com/base/",
  );

  assert_eq!(
    action,
    InteractionAction::Navigate {
      href: "https://example.com/search?q=abc".to_string()
    }
  );
}

#[test]
fn submit_click_prefers_select_option_value_attribute_over_text_content() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![el(
        "form",
        vec![("id", "f"), ("action", "/search")],
        vec![
          el(
            "select",
            vec![("id", "sel"), ("name", "s")],
            vec![
              el("option", vec![("id", "o1"), ("value", "1")], vec![]),
              el(
                "option",
                vec![("id", "o2"), ("value", "2"), ("selected", "")],
                vec![],
              ),
            ],
          ),
          el("input", vec![("id", "submit"), ("type", "submit")], vec![]),
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

  let (_changed, action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
    "https://example.com/doc",
    "https://example.com/base/",
  );

  assert_eq!(
    action,
    InteractionAction::Navigate {
      href: "https://example.com/search?s=2".to_string()
    }
  );
}

#[test]
fn submit_click_defaults_action_to_document_url_when_action_is_missing() {
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
            vec![("id", "q"), ("name", "q"), ("value", "abc")],
            vec![],
          ),
          el("input", vec![("id", "submit"), ("type", "submit")], vec![]),
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

  let (_changed, action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
    "https://example.com/doc",
    "https://example.com/base/",
  );

  assert_eq!(
    action,
    InteractionAction::Navigate {
      href: "https://example.com/doc?q=abc".to_string()
    }
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
    "renderer must not inject data-fastr-user-validity onto the DOM"
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
      },
      SelectItem::Option {
        node_id: option_2_dom_id,
        label: "Option 2".to_string(),
        value: "o2".to_string(),
        selected: false,
        disabled: false,
        in_optgroup: false,
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
    progress_bar_style: None,
    progress_value_style: None,
    meter_bar_style: None,
    meter_optimum_value_style: None,
    meter_suboptimum_value_style: None,
    meter_even_less_good_value_style: None,
    file_selector_button_style: None,
    ime_preedit: None,
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
  let (changed, _action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 25.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
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
  assert!(
    !has_attr(&dom, "sel", "data-fastr-user-validity"),
    "renderer must not inject data-fastr-user-validity onto the DOM"
  );
  assert!(engine.interaction_state().has_user_validity(select_dom_id));
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
      FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 0.0, 50.0, 50.0), link_box_id, vec![]),
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
  assert!(
    engine.interaction_state().is_hovered(link_dom_id),
    "link should be hovered through overlay"
  );
  assert!(
    !engine.interaction_state().is_hovered(overlay_dom_id),
    "overlay should not be hovered"
  );
  assert!(!has_attr(&dom, "link", "data-fastr-hover"));
  assert!(!has_attr(&dom, "overlay", "data-fastr-hover"));

  engine.pointer_down(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(10.0, 10.0),
  );
  let (_, action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(10.0, 10.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
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
  engine.pointer_down(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &scroll,
    Point::new(5.0, 5.0),
  );
  let (_, action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
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
          el(
            "input",
            vec![("id", "q"), ("name", "q"), ("value", "hi")],
            vec![],
          ),
          el(
            "input",
            vec![
              ("id", "cb"),
              ("type", "checkbox"),
              ("name", "c"),
              ("value", "yes"),
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
  engine.pointer_down(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &scroll,
    Point::new(5.0, 5.0),
  );
  let (_, action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
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
          el(
            "input",
            vec![("id", "q"), ("name", "q"), ("value", "hi")],
            vec![],
          ),
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
  engine.pointer_down(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &scroll,
    Point::new(5.0, 5.0),
  );
  let (_, action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
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
      },
      SelectItem::Option {
        node_id: option_b_dom_id,
        label: "Beta".to_string(),
        value: "b".to_string(),
        selected: false,
        disabled: true,
        in_optgroup: false,
      },
      SelectItem::Option {
        node_id: option_c_dom_id,
        label: "Gamma".to_string(),
        value: "c".to_string(),
        selected: false,
        disabled: false,
        in_optgroup: false,
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
    progress_bar_style: None,
    progress_value_style: None,
    meter_bar_style: None,
    meter_optimum_value_style: None,
    meter_suboptimum_value_style: None,
    meter_even_less_good_value_style: None,
    file_selector_button_style: None,
    disabled: false,
    focused: false,
    focus_visible: false,
    required: false,
    invalid: false,
    ime_preedit: None,
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
  let (changed, action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
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
  assert_eq!(engine.interaction_state().focused, Some(select_dom_id));
  assert!(!engine.interaction_state().focus_visible);
  assert!(!has_attr(&dom, "sel", "data-fastr-focus"));
  assert!(!has_attr(&dom, "sel", "data-fastr-focus-visible"));
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
  let (_, action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(10.0, 10.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
    "https://example.com/",
    "https://example.com/",
  );
  assert_eq!(action, InteractionAction::None);
  assert!(!engine.interaction_state().is_visited_link(link_dom_id));
  assert!(!has_attr(&dom, "link", "data-fastr-visited"));
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
  let (_, action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
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
  let (_, action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
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
  engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
    "https://x/",
    "https://x/",
  );
  engine.text_input(&mut dom, "X");
  let _ = engine.key_action(&mut dom, KeyAction::Backspace);
  assert_eq!(attr_value(&dom, "disabled", "value").as_deref(), Some("hi"));

  // Readonly input.
  engine.pointer_down(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 45.0),
  );
  engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 45.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
    "https://x/",
    "https://x/",
  );
  engine.text_input(&mut dom, "X");
  let _ = engine.key_action(&mut dom, KeyAction::Backspace);
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
        el(
          "input",
          vec![("id", "disabled_first"), ("disabled", "")],
          vec![],
        ),
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
        el(
          "button",
          vec![("id", "b_disabled"), ("disabled", "")],
          vec![],
        ),
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
    let expected_dom_id = node_id(&dom, expected);
    assert_eq!(
      engine.interaction_state().focused,
      Some(expected_dom_id),
      "{expected} should be focused"
    );
    assert!(
      engine.interaction_state().focus_visible,
      "{expected} should be focus-visible (keyboard modality)"
    );
    assert!(!has_attr(&dom, expected, "data-fastr-focus"));
    assert!(!has_attr(&dom, expected, "data-fastr-focus-visible"));

    if let Some(prev_id) = prev {
      let prev_dom_id = node_id(&dom, prev_id);
      assert_ne!(
        engine.interaction_state().focused,
        Some(prev_dom_id),
        "{prev_id} focus should be cleared"
      );
      assert!(!has_attr(&dom, prev_id, "data-fastr-focus"));
      assert!(!has_attr(&dom, prev_id, "data-fastr-focus-visible"));
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
      let skipped_dom_id = node_id(&dom, skipped);
      assert_ne!(
        engine.interaction_state().focused,
        Some(skipped_dom_id),
        "{skipped} must be skipped by tab traversal"
      );
      assert!(!has_attr(&dom, skipped, "data-fastr-focus"));
    }

    prev = Some(expected);
  }
}

#[test]
fn shift_tab_key_traverses_focusable_elements_in_reverse_dom_order_and_wraps() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![
        // Not focusable (disabled).
        el(
          "input",
          vec![("id", "disabled_first"), ("disabled", "")],
          vec![],
        ),
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
        el(
          "button",
          vec![("id", "b_disabled"), ("disabled", "")],
          vec![],
        ),
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
    .rev()
    .chain(std::iter::once(focusables[focusables.len() - 1]))
  {
    assert!(
      engine.key_action(&mut dom, KeyAction::ShiftTab),
      "shift+tab should move focus"
    );
    let expected_dom_id = node_id(&dom, expected);
    assert_eq!(
      engine.interaction_state().focused,
      Some(expected_dom_id),
      "{expected} should be focused"
    );
    assert!(
      engine.interaction_state().focus_visible,
      "{expected} should be focus-visible (keyboard modality)"
    );
    assert!(!has_attr(&dom, expected, "data-fastr-focus"));
    assert!(!has_attr(&dom, expected, "data-fastr-focus-visible"));

    if let Some(prev_id) = prev {
      let prev_dom_id = node_id(&dom, prev_id);
      assert_ne!(
        engine.interaction_state().focused,
        Some(prev_dom_id),
        "{prev_id} focus should be cleared"
      );
      assert!(!has_attr(&dom, prev_id, "data-fastr-focus"));
      assert!(!has_attr(&dom, prev_id, "data-fastr-focus-visible"));
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
      let skipped_dom_id = node_id(&dom, skipped);
      assert_ne!(
        engine.interaction_state().focused,
        Some(skipped_dom_id),
        "{skipped} must be skipped by tab traversal"
      );
      assert!(!has_attr(&dom, skipped, "data-fastr-focus"));
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
      },
      SelectItem::Option {
        node_id: o2_dom_id,
        label: "Two".to_string(),
        value: "2".to_string(),
        selected: false,
        disabled: false,
        in_optgroup: false,
      },
      SelectItem::Option {
        node_id: o3_dom_id,
        label: "Three".to_string(),
        value: "3".to_string(),
        selected: false,
        disabled: true,
        in_optgroup: false,
      },
    ]),
    selected: vec![0],
  });

  let mut select_style = ComputedStyle::default();
  select_style.font_size = 10.0;
  select_style.root_font_size = 10.0;
  select_style.line_height = LineHeight::Length(Length::px(10.0));

  let mut select_box = BoxNode::new_replaced(
    Arc::new(select_style),
    ReplacedType::FormControl(FormControl {
      control,
      appearance: Appearance::Auto,
      placeholder_style: None,
      slider_thumb_style: None,
      slider_track_style: None,
      progress_bar_style: None,
      progress_value_style: None,
      meter_bar_style: None,
      meter_optimum_value_style: None,
      meter_suboptimum_value_style: None,
      meter_even_less_good_value_style: None,
      file_selector_button_style: None,
      disabled: false,
      focused: false,
      focus_visible: false,
      required: false,
      invalid: false,
      ime_preedit: None,
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

  // line-height=10px => 10px per row. Y=15 selects row index 1 (<option id=o2>).
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
  let (_, action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 15.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
    "https://x/",
    "https://x/",
  );

  assert_eq!(
    action,
    InteractionAction::FocusChanged {
      node_id: Some(select_dom_id)
    }
  );
  assert_eq!(engine.interaction_state().focused, Some(select_dom_id));
  assert!(!engine.interaction_state().focus_visible);
  assert!(!has_attr(&dom, "s", "data-fastr-focus"));

  assert!(
    !has_attr(&dom, "o1", "selected"),
    "single-select listbox should clear previously selected option"
  );
  assert!(
    has_attr(&dom, "o2", "selected"),
    "clicked row should be selected"
  );
  assert!(
    !has_attr(&dom, "s", "data-fastr-user-validity"),
    "renderer must not inject data-fastr-user-validity onto the DOM"
  );
  assert!(
    engine.interaction_state().has_user_validity(select_dom_id),
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
  let (_, action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 25.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
    "https://x/",
    "https://x/",
  );
  assert_eq!(action, InteractionAction::None);
  assert!(has_attr(&dom, "o2", "selected"));
  assert!(!has_attr(&dom, "o3", "selected"));
}

#[test]
fn listbox_select_click_uses_painted_row_list_not_dom_options() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![el(
        "select",
        vec![("id", "s"), ("size", "2")],
        vec![
          el("option", vec![("id", "o1"), ("selected", "")], vec![]),
          el(
            "option",
            vec![("id", "o_hidden"), ("style", "display:none")],
            vec![],
          ),
          el("option", vec![("id", "o2")], vec![]),
        ],
      )],
    )],
  )]);

  let select_dom_id = node_id(&dom, "s");
  let o1_dom_id = node_id(&dom, "o1");
  let o2_dom_id = node_id(&dom, "o2");

  // The painted `SelectControl.items` excludes the hidden `<option>`.
  let control = FormControlKind::Select(SelectControl {
    multiple: false,
    size: 2,
    items: Arc::new(vec![
      SelectItem::Option {
        node_id: o1_dom_id,
        label: "One".to_string(),
        value: "1".to_string(),
        selected: true,
        disabled: false,
        in_optgroup: false,
      },
      SelectItem::Option {
        node_id: o2_dom_id,
        label: "Two".to_string(),
        value: "2".to_string(),
        selected: false,
        disabled: false,
        in_optgroup: false,
      },
    ]),
    selected: vec![0],
  });

  let mut select_style = ComputedStyle::default();
  select_style.font_size = 10.0;
  select_style.root_font_size = 10.0;
  select_style.line_height = LineHeight::Length(Length::px(10.0));

  let mut select_box = BoxNode::new_replaced(
    Arc::new(select_style),
    ReplacedType::FormControl(FormControl {
      control,
      appearance: Appearance::Auto,
      placeholder_style: None,
      slider_thumb_style: None,
      slider_track_style: None,
      progress_bar_style: None,
      progress_value_style: None,
      meter_bar_style: None,
      meter_optimum_value_style: None,
      meter_suboptimum_value_style: None,
      meter_even_less_good_value_style: None,
      file_selector_button_style: None,
      disabled: false,
      focused: false,
      focus_visible: false,
      required: false,
      invalid: false,
      ime_preedit: None,
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

  // line-height=10px => y=15 selects painted row index 1, which maps to `<option id=o2>`.
  let fragment_tree = FragmentTree::new(FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
    vec![FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 100.0, 20.0),
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
  engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 15.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
    "https://x/",
    "https://x/",
  );

  assert!(
    !has_attr(&dom, "o1", "selected"),
    "expected selection to move off the first visible option"
  );
  assert!(
    has_attr(&dom, "o2", "selected"),
    "expected click to select o2"
  );
  assert!(
    !has_attr(&dom, "o_hidden", "selected"),
    "hidden options must not consume painted rows"
  );
  assert!(
    !has_attr(&dom, "s", "data-fastr-user-validity"),
    "renderer must not inject data-fastr-user-validity onto the DOM"
  );
  assert!(engine.interaction_state().has_user_validity(select_dom_id));
}

#[test]
fn listbox_select_click_in_blank_area_is_noop() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![el(
        "select",
        vec![("id", "s"), ("size", "4")],
        vec![
          el("option", vec![("id", "o1"), ("selected", "")], vec![]),
          el("option", vec![("id", "o2")], vec![]),
        ],
      )],
    )],
  )]);

  assert!(
    !has_attr(&dom, "s", "data-fastr-user-validity"),
    "select should not be marked initially"
  );

  let select_dom_id = node_id(&dom, "s");
  let o1_dom_id = node_id(&dom, "o1");
  let o2_dom_id = node_id(&dom, "o2");

  // Only 2 items are painted even though `size=4`, leaving blank space below.
  let control = FormControlKind::Select(SelectControl {
    multiple: false,
    size: 4,
    items: Arc::new(vec![
      SelectItem::Option {
        node_id: o1_dom_id,
        label: "One".to_string(),
        value: "1".to_string(),
        selected: true,
        disabled: false,
        in_optgroup: false,
      },
      SelectItem::Option {
        node_id: o2_dom_id,
        label: "Two".to_string(),
        value: "2".to_string(),
        selected: false,
        disabled: false,
        in_optgroup: false,
      },
    ]),
    selected: vec![0],
  });

  let mut select_style = ComputedStyle::default();
  select_style.font_size = 10.0;
  select_style.root_font_size = 10.0;
  select_style.line_height = LineHeight::Length(Length::px(10.0));

  let mut select_box = BoxNode::new_replaced(
    Arc::new(select_style),
    ReplacedType::FormControl(FormControl {
      control,
      appearance: Appearance::Auto,
      placeholder_style: None,
      slider_thumb_style: None,
      slider_track_style: None,
      progress_bar_style: None,
      progress_value_style: None,
      meter_bar_style: None,
      meter_optimum_value_style: None,
      meter_suboptimum_value_style: None,
      meter_even_less_good_value_style: None,
      file_selector_button_style: None,
      disabled: false,
      focused: false,
      focus_visible: false,
      required: false,
      invalid: false,
      ime_preedit: None,
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

  // line-height=10px; the fragment is tall enough for 4 rows (40px) but we only have 2 options.
  // Clicking at y=35 is in the blank area and must not change selection.
  let fragment_tree = FragmentTree::new(FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
    vec![FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 100.0, 40.0),
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
    Point::new(5.0, 35.0),
  );
  let (_changed, action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 35.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
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
  assert!(!has_attr(&dom, "o2", "selected"));
  assert!(
    !has_attr(&dom, "s", "data-fastr-user-validity"),
    "no-op click must not mark user-validity"
  );
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
      },
      SelectItem::Option {
        node_id: o2_dom_id,
        label: "Two".to_string(),
        value: "2".to_string(),
        selected: false,
        disabled: false,
        in_optgroup: false,
      },
      SelectItem::Option {
        node_id: o3_dom_id,
        label: "Three".to_string(),
        value: "3".to_string(),
        selected: false,
        disabled: false,
        in_optgroup: false,
      },
    ]),
    selected: vec![0],
  });

  let mut select_style = ComputedStyle::default();
  select_style.font_size = 10.0;
  select_style.root_font_size = 10.0;
  select_style.line_height = LineHeight::Length(Length::px(10.0));

  let mut select_box = BoxNode::new_replaced(
    Arc::new(select_style),
    ReplacedType::FormControl(FormControl {
      control,
      appearance: Appearance::Auto,
      placeholder_style: None,
      slider_thumb_style: None,
      slider_track_style: None,
      progress_bar_style: None,
      progress_value_style: None,
      meter_bar_style: None,
      meter_optimum_value_style: None,
      meter_suboptimum_value_style: None,
      meter_even_less_good_value_style: None,
      file_selector_button_style: None,
      disabled: false,
      focused: false,
      focus_visible: false,
      required: false,
      invalid: false,
      ime_preedit: None,
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

  // line-height=10px => 10px per row. Y=15 selects row index 1 (<option id=o2>).
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
  let (_, action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 15.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
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
  let (_, action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 15.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
    "https://x/",
    "https://x/",
  );
  assert_eq!(action, InteractionAction::None);
  assert!(has_attr(&dom, "o1", "selected"));
  assert!(!has_attr(&dom, "o2", "selected"));
  assert!(!has_attr(&dom, "o3", "selected"));
}

#[test]
fn select_keyboard_navigation_without_box_tree_changes_selection_and_skips_disabled_options() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![el(
        "select",
        vec![("id", "s")],
        vec![
          el("option", vec![("id", "o1"), ("selected", "")], vec![]),
          el("option", vec![("id", "o2"), ("disabled", "")], vec![]),
          el("option", vec![("id", "o3")], vec![]),
          el(
            "optgroup",
            vec![("id", "g"), ("disabled", "")],
            vec![el("option", vec![("id", "o4")], vec![])],
          ),
          el("option", vec![("id", "o5")], vec![]),
        ],
      )],
    )],
  )]);

  let select_dom_id = node_id(&dom, "s");

  let mut engine = InteractionEngine::new();
  engine.focus_node_id(&mut dom, Some(select_dom_id), false);

  assert_eq!(engine.interaction_state().focused, Some(select_dom_id));
  assert!(
    !engine.interaction_state().focus_visible,
    "focus should not initially be focus-visible (simulating a pointer focus)"
  );
  assert!(
    !has_attr(&dom, "s", "data-fastr-focus"),
    "focus must not be represented via DOM attrs"
  );
  assert!(
    !has_attr(&dom, "s", "data-fastr-focus-visible"),
    "focus-visible must not be represented via DOM attrs"
  );
  assert!(has_attr(&dom, "o1", "selected"));

  let (changed, action) =
    engine.key_activate(&mut dom, KeyAction::ArrowDown, "https://x/", "https://x/");
  assert!(changed);
  assert_eq!(action, InteractionAction::None);
  assert_eq!(engine.interaction_state().focused, Some(select_dom_id));
  assert!(
    engine.interaction_state().focus_visible,
    "keyboard interaction should set focus-visible"
  );
  assert!(
    !has_attr(&dom, "s", "data-fastr-focus-visible"),
    "focus-visible must not be represented via DOM attrs"
  );

  // ArrowDown should skip disabled options.
  assert!(has_attr(&dom, "o3", "selected"));
  assert!(!has_attr(&dom, "o1", "selected"));
  assert!(!has_attr(&dom, "o2", "selected"));

  // ArrowDown should also skip options inside a disabled optgroup.
  engine.key_activate(&mut dom, KeyAction::ArrowDown, "https://x/", "https://x/");
  assert!(has_attr(&dom, "o5", "selected"));
  assert!(!has_attr(&dom, "o4", "selected"));

  // ArrowUp should move back, skipping disabled options/optgroups.
  engine.key_activate(&mut dom, KeyAction::ArrowUp, "https://x/", "https://x/");
  assert!(has_attr(&dom, "o3", "selected"));

  // Home/End should jump to first/last enabled option.
  engine.key_activate(&mut dom, KeyAction::Home, "https://x/", "https://x/");
  assert!(has_attr(&dom, "o1", "selected"));
  engine.key_activate(&mut dom, KeyAction::End, "https://x/", "https://x/");
  assert!(has_attr(&dom, "o5", "selected"));
}

#[test]
fn listbox_select_click_accounts_for_element_scroll_offset() {
  let option_ids = ["o0", "o1", "o2", "o3", "o4", "o5", "o6", "o7", "o8", "o9"];
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
        let option_dom_id = node_id(&dom, id);
        SelectItem::Option {
          node_id: option_dom_id,
          label: format!("Option {idx}"),
          value: idx.to_string(),
          selected: idx == 0,
          disabled: false,
          in_optgroup: false,
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

  let mut select_style = ComputedStyle::default();
  select_style.font_size = 10.0;
  select_style.root_font_size = 10.0;
  select_style.line_height = LineHeight::Length(Length::px(10.0));

  let mut select_box = BoxNode::new_replaced(
    Arc::new(select_style),
    ReplacedType::FormControl(FormControl {
      control,
      appearance: Appearance::Auto,
      placeholder_style: None,
      slider_thumb_style: None,
      slider_track_style: None,
      progress_bar_style: None,
      progress_value_style: None,
      meter_bar_style: None,
      meter_optimum_value_style: None,
      meter_suboptimum_value_style: None,
      meter_even_less_good_value_style: None,
      file_selector_button_style: None,
      disabled: false,
      focused: false,
      focus_visible: false,
      required: false,
      invalid: false,
      ime_preedit: None,
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

  // line-height=10px => 10px per row.
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
  engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &scroll,
    Point::new(5.0, 5.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
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
  assert_eq!(engine.interaction_state().focused, Some(link_id));
  assert_eq!(engine.take_last_visited_candidate(), Some(link_id));
  assert!(engine.mark_link_visited(link_id));
  assert!(
    engine.interaction_state().is_visited_link(link_id),
    "Enter on a focused link should mark it visited"
  );
  assert!(
    engine.interaction_state().focus_visible,
    "keyboard interaction should set focus-visible"
  );
  assert!(!has_attr(&dom, "link", "data-fastr-visited"));
  assert!(!has_attr(&dom, "link", "data-fastr-focus-visible"));
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

  let (changed, action) = engine.key_activate(
    &mut dom,
    KeyAction::Tab,
    "https://x/",
    "https://example.com/base/",
  );
  assert!(changed, "Tab should focus the first focusable element");
  assert_eq!(
    action,
    InteractionAction::FocusChanged {
      node_id: Some(link_id)
    }
  );
  assert_eq!(engine.interaction_state().focused, Some(link_id));
  assert!(engine.interaction_state().focus_visible);
  assert!(!has_attr(&dom, "link", "data-fastr-focus"));
  assert!(!has_attr(&dom, "link", "data-fastr-focus-visible"));
  assert!(!has_attr(&dom, "txt", "data-fastr-focus"));
  assert!(!has_attr(&dom, "txt", "data-fastr-focus-visible"));

  let (_, action) = engine.key_activate(
    &mut dom,
    KeyAction::Tab,
    "https://x/",
    "https://example.com/base/",
  );
  assert_eq!(
    action,
    InteractionAction::FocusChanged {
      node_id: Some(input_id)
    }
  );
  assert!(!has_attr(&dom, "link", "data-fastr-focus"));
  assert!(!has_attr(&dom, "link", "data-fastr-focus-visible"));
  assert_eq!(engine.interaction_state().focused, Some(input_id));
  assert!(engine.interaction_state().focus_visible);
  assert!(!has_attr(&dom, "txt", "data-fastr-focus"));
  assert!(!has_attr(&dom, "txt", "data-fastr-focus-visible"));

  // Wrap at the end.
  let (_, action) = engine.key_activate(
    &mut dom,
    KeyAction::Tab,
    "https://x/",
    "https://example.com/base/",
  );
  assert_eq!(
    action,
    InteractionAction::FocusChanged {
      node_id: Some(link_id)
    }
  );
  assert_eq!(engine.interaction_state().focused, Some(link_id));
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
  assert!(
    !has_attr(&dom, "cb", "data-fastr-user-validity"),
    "renderer must not inject data-fastr-user-validity onto the DOM"
  );
  assert!(engine.interaction_state().has_user_validity(cb_id));
  assert_eq!(engine.interaction_state().focused, Some(cb_id));
  assert!(engine.interaction_state().focus_visible);
  assert!(!has_attr(&dom, "cb", "data-fastr-focus-visible"));
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
  assert!(
    !has_attr(&dom, "sel", "data-fastr-user-validity"),
    "renderer must not inject data-fastr-user-validity onto the DOM"
  );
  assert!(engine.interaction_state().has_user_validity(select_id));
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
  assert!(
    !has_attr(&dom, "form", "data-fastr-user-validity"),
    "renderer must not inject data-fastr-user-validity onto the DOM"
  );
  let form_dom_id = node_id(&dom, "form");
  assert!(
    engine.interaction_state().has_user_validity(form_dom_id),
    "Enter submission should mark form user validity"
  );
  assert!(engine.interaction_state().has_user_validity(input_id));
}

#[test]
fn enter_on_text_input_includes_default_submitter_name_value() {
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
            vec![("id", "q"), ("name", "q"), ("value", "abc")],
            vec![],
          ),
          el(
            "button",
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

  let input_id = node_id(&dom, "q");

  let mut engine = InteractionEngine::new();
  engine.focus_node_id(&mut dom, Some(input_id), false);

  let (_changed, action) = engine.key_activate(
    &mut dom,
    KeyAction::Enter,
    "https://example.com/doc",
    "https://example.com/base/",
  );

  assert_eq!(
    action,
    InteractionAction::Navigate {
      href: "https://example.com/search?q=abc&go=1".to_string()
    }
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
        vec![
          ("id", "r"),
          ("type", "range"),
          ("min", "0"),
          ("max", "10"),
          ("value", "0"),
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
  engine.pointer_down(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &scroll,
    Point::new(0.0, 10.0),
  );

  engine.pointer_move(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &scroll,
    Point::new(25.0, 10.0),
  );
  assert_eq!(attr_value(&dom, "r", "value").as_deref(), Some("3"));
  assert!(
    !has_attr(&dom, "r", "data-fastr-user-validity"),
    "renderer must not inject data-fastr-user-validity onto the DOM"
  );
  assert!(
    engine.interaction_state().has_user_validity(range_dom_id),
    "changing a range value should mark user validity"
  );

  engine.pointer_move(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &scroll,
    Point::new(75.0, 10.0),
  );
  assert_eq!(attr_value(&dom, "r", "value").as_deref(), Some("8"));

  // Drag beyond the right edge: clamp at max.
  engine.pointer_move(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &scroll,
    Point::new(150.0, 10.0),
  );
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
  engine.pointer_down(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &scroll,
    Point::new(0.0, 10.0),
  );
  engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(0.0, 10.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
    "https://x/",
    "https://x/",
  );
  assert_eq!(attr_value(&dom, "r", "value").as_deref(), Some("0"));

  // Near 56% should snap to the nearest step.
  engine.pointer_down(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &scroll,
    Point::new(56.0, 10.0),
  );
  engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(56.0, 10.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
    "https://x/",
    "https://x/",
  );
  assert_eq!(attr_value(&dom, "r", "value").as_deref(), Some("60"));

  // Right edge should set max.
  engine.pointer_down(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &scroll,
    Point::new(100.0, 10.0),
  );
  engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(100.0, 10.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
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
    !has_attr(&dom, "r", "data-fastr-user-validity"),
    "renderer must not inject data-fastr-user-validity onto the DOM"
  );
  assert!(
    engine.interaction_state().has_user_validity(range_dom_id),
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
fn select_home_end_keys_jump_to_first_and_last_enabled_option_box_tree_snapshot() {
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
          el("option", vec![("id", "o0"), ("disabled", "")], vec![]),
          el("option", vec![("id", "o1")], vec![]),
          el("option", vec![("id", "o2"), ("selected", "")], vec![]),
          el(
            "optgroup",
            vec![("id", "g"), ("label", "Disabled group"), ("disabled", "")],
            vec![el("option", vec![("id", "o3")], vec![])],
          ),
        ],
      )],
    )],
  )]);

  let select_dom_id = node_id(&dom, "sel");
  let o0_dom_id = node_id(&dom, "o0");
  let o1_dom_id = node_id(&dom, "o1");
  let o2_dom_id = node_id(&dom, "o2");
  let o3_dom_id = node_id(&dom, "o3");

  let control = FormControlKind::Select(SelectControl {
    multiple: false,
    size: 1,
    items: Arc::new(vec![
      SelectItem::Option {
        node_id: o0_dom_id,
        label: "Zero".to_string(),
        value: "0".to_string(),
        selected: false,
        disabled: true,
        in_optgroup: false,
      },
      SelectItem::Option {
        node_id: o1_dom_id,
        label: "One".to_string(),
        value: "1".to_string(),
        selected: false,
        disabled: false,
        in_optgroup: false,
      },
      SelectItem::Option {
        node_id: o2_dom_id,
        label: "Two".to_string(),
        value: "2".to_string(),
        selected: true,
        disabled: false,
        in_optgroup: false,
      },
      SelectItem::OptGroupLabel {
        label: "Disabled group".to_string(),
        disabled: true,
      },
      SelectItem::Option {
        node_id: o3_dom_id,
        label: "Three".to_string(),
        value: "3".to_string(),
        selected: false,
        // Disabled via optgroup.
        disabled: true,
        in_optgroup: true,
      },
    ]),
    selected: vec![2],
  });

  let mut select_box = BoxNode::new_replaced(
    default_style(),
    ReplacedType::FormControl(FormControl {
      control,
      appearance: Appearance::Auto,
      placeholder_style: None,
      slider_thumb_style: None,
      slider_track_style: None,
      progress_bar_style: None,
      progress_value_style: None,
      meter_bar_style: None,
      meter_optimum_value_style: None,
      meter_suboptimum_value_style: None,
      meter_even_less_good_value_style: None,
      file_selector_button_style: None,
      disabled: false,
      focused: false,
      focus_visible: false,
      required: false,
      invalid: false,
      ime_preedit: None,
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

  let mut engine = InteractionEngine::new();
  engine.focus_node_id(&mut dom, Some(select_dom_id), false);

  assert_eq!(
    engine.interaction_state().focused,
    Some(select_dom_id),
    "select should be focused"
  );
  assert!(
    !engine.interaction_state().focus_visible,
    "pointer focus should not set focus-visible"
  );
  assert!(!has_attr(&dom, "sel", "data-fastr-focus"));
  assert!(!has_attr(&dom, "sel", "data-fastr-focus-visible"));

  // Home should jump to the first enabled <option> (o1), skipping disabled o0.
  assert!(
    engine.key_action_with_box_tree(&mut dom, Some(&box_tree), KeyAction::Home),
    "expected Home to update select state"
  );
  assert!(has_attr(&dom, "o1", "selected"));
  assert!(!has_attr(&dom, "o2", "selected"));
  assert!(!has_attr(&dom, "o0", "selected"));
  assert!(!has_attr(&dom, "o3", "selected"));
  assert!(
    engine.interaction_state().focus_visible,
    "keyboard interaction should set focus-visible"
  );
  assert!(!has_attr(&dom, "sel", "data-fastr-focus-visible"));

  // End should jump to the last enabled <option> (o2), skipping disabled optgroup option o3.
  assert!(
    engine.key_action_with_box_tree(&mut dom, Some(&box_tree), KeyAction::End),
    "expected End to update select state"
  );
  assert!(has_attr(&dom, "o2", "selected"));
  assert!(!has_attr(&dom, "o1", "selected"));
  assert!(!has_attr(&dom, "o3", "selected"));
}

#[test]
fn select_home_end_keys_jump_to_first_and_last_enabled_option_dom_fallback() {
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
          el("option", vec![("id", "o0"), ("disabled", "")], vec![]),
          el("option", vec![("id", "o1")], vec![]),
          el("option", vec![("id", "o2"), ("selected", "")], vec![]),
          el(
            "optgroup",
            vec![("id", "g"), ("label", "Disabled group"), ("disabled", "")],
            vec![el("option", vec![("id", "o3")], vec![])],
          ),
        ],
      )],
    )],
  )]);

  let select_dom_id = node_id(&dom, "sel");

  let mut engine = InteractionEngine::new();
  engine.focus_node_id(&mut dom, Some(select_dom_id), false);

  engine.key_action(&mut dom, KeyAction::Home);
  assert!(has_attr(&dom, "o1", "selected"));
  assert!(!has_attr(&dom, "o2", "selected"));
  assert!(
    engine.interaction_state().focus_visible,
    "keyboard interaction should set focus-visible"
  );
  assert!(!has_attr(&dom, "sel", "data-fastr-focus-visible"));

  engine.key_action(&mut dom, KeyAction::End);
  assert!(has_attr(&dom, "o2", "selected"));
  assert!(!has_attr(&dom, "o1", "selected"));
  assert!(!has_attr(&dom, "o3", "selected"));
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
  engine.pointer_down(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &scroll,
    Point::new(0.0, 10.0),
  );
  engine.pointer_move(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &scroll,
    Point::new(100.0, 10.0),
  );
  engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(100.0, 10.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
    "https://x/",
    "https://x/",
  );
  assert_eq!(attr_value(&dom, "disabled", "value").as_deref(), Some("0"));
  assert!(
    !has_attr(&dom, "disabled", "data-fastr-user-validity"),
    "renderer must not inject data-fastr-user-validity onto the DOM"
  );
  assert!(
    !engine
      .interaction_state()
      .has_user_validity(disabled_dom_id),
    "disabled range must not flip user validity"
  );

  // Readonly range.
  engine.pointer_down(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &scroll,
    Point::new(0.0, 50.0),
  );
  engine.pointer_move(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &scroll,
    Point::new(100.0, 50.0),
  );
  engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(100.0, 50.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
    "https://x/",
    "https://x/",
  );
  assert_eq!(attr_value(&dom, "readonly", "value").as_deref(), Some("0"));
  assert!(
    !has_attr(&dom, "readonly", "data-fastr-user-validity"),
    "renderer must not inject data-fastr-user-validity onto the DOM"
  );
  assert!(
    !engine
      .interaction_state()
      .has_user_validity(readonly_dom_id),
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
        vec![
          ("id", "r"),
          ("type", "range"),
          ("min", "0"),
          ("max", "100"),
          ("value", "10"),
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
  engine.pointer_down(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &scroll,
    Point::new(10.0, 10.0),
  );
  let (_, action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(10.0, 10.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
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
    engine.interaction_state().focused,
    Some(range_dom_id),
    "clicking a range input should focus it"
  );
  assert!(
    !engine.interaction_state().focus_visible,
    "pointer focus should not set focus-visible"
  );
  assert!(!has_attr(&dom, "r", "data-fastr-focus"));
  assert!(!has_attr(&dom, "r", "data-fastr-focus-visible"));
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
  let (changed, action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(10.0, 10.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
    "https://x/",
    "https://x/",
  );
  assert!(changed);
  match action {
    InteractionAction::FocusChanged { node_id } => assert_eq!(node_id, Some(dom_id)),
    InteractionAction::None => {}
    other => panic!("unexpected pointer_up action: {other:?}"),
  }
  assert_eq!(engine.interaction_state().focused, Some(dom_id));
  assert!(
    !engine.interaction_state().focus_visible,
    "pointer focus should not set focus-visible"
  );
  assert!(!has_attr(&dom, "t", "data-fastr-focus"));
  assert!(!has_attr(&dom, "t", "data-fastr-focus-visible"));
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
  engine.pointer_down(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &scroll,
    Point::new(5.0, 5.0),
  );
  let (changed, _) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
    "https://x/",
    "https://x/",
  );
  assert!(changed);
  assert_eq!(engine.interaction_state().focused, Some(first_dom_id));
  assert!(
    !engine.interaction_state().focus_visible,
    "pointer focus should not set focus-visible"
  );
  assert!(!has_attr(&dom, "first", "data-fastr-focus"));
  assert!(!has_attr(&dom, "first", "data-fastr-focus-visible"));

  // Tab sequence: first -> textarea -> link -> tabindex=0 div -> last button -> wrap to first.
  assert!(engine.key_action(&mut dom, KeyAction::Tab));
  assert_eq!(
    engine.interaction_state().focused,
    Some(node_id(&dom, "ta"))
  );
  assert!(engine.interaction_state().focus_visible);
  assert!(!has_attr(&dom, "ta", "data-fastr-focus"));
  assert!(!has_attr(&dom, "ta", "data-fastr-focus-visible"));

  assert!(engine.key_action(&mut dom, KeyAction::Tab));
  assert_eq!(
    engine.interaction_state().focused,
    Some(node_id(&dom, "link"))
  );
  assert!(engine.interaction_state().focus_visible);
  assert!(!has_attr(&dom, "link", "data-fastr-focus"));
  assert!(!has_attr(&dom, "link", "data-fastr-focus-visible"));

  assert!(
    engine.key_action(&mut dom, KeyAction::Tab),
    "should skip tabindex=-1, disabled and inert descendants"
  );
  assert_eq!(
    engine.interaction_state().focused,
    Some(node_id(&dom, "tabbed"))
  );
  assert!(engine.interaction_state().focus_visible);
  assert!(!has_attr(&dom, "tabbed", "data-fastr-focus"));
  assert!(!has_attr(&dom, "tabbed", "data-fastr-focus-visible"));
  assert!(!has_attr(&dom, "skip", "data-fastr-focus"));
  assert!(!has_attr(&dom, "disabled", "data-fastr-focus"));
  assert!(!has_attr(&dom, "inert-input", "data-fastr-focus"));

  assert!(engine.key_action(&mut dom, KeyAction::Tab));
  assert_eq!(
    engine.interaction_state().focused,
    Some(node_id(&dom, "last"))
  );
  assert!(engine.interaction_state().focus_visible);
  assert!(!has_attr(&dom, "last", "data-fastr-focus"));
  assert!(!has_attr(&dom, "last", "data-fastr-focus-visible"));

  assert!(engine.key_action(&mut dom, KeyAction::Tab));
  assert_eq!(engine.interaction_state().focused, Some(first_dom_id));
  assert!(engine.interaction_state().focus_visible);
  assert!(!has_attr(&dom, "first", "data-fastr-focus"));
  assert!(!has_attr(&dom, "first", "data-fastr-focus-visible"));
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

  let btn_dom_id = node_id(&dom, "btn");
  let inp_dom_id = node_id(&dom, "inp");

  let mut engine = InteractionEngine::new();
  assert!(engine.key_action(&mut dom, KeyAction::Tab));
  assert_eq!(engine.interaction_state().focused, Some(btn_dom_id));
  assert!(engine.interaction_state().focus_visible);
  assert!(!has_attr(&dom, "btn", "data-fastr-focus"));
  assert!(!has_attr(&dom, "btn", "data-fastr-focus-visible"));

  assert!(engine.key_action(&mut dom, KeyAction::Tab));
  assert_eq!(engine.interaction_state().focused, Some(inp_dom_id));
  assert!(engine.interaction_state().focus_visible);
  assert!(!has_attr(&dom, "inp", "data-fastr-focus"));
  assert!(!has_attr(&dom, "inp", "data-fastr-focus-visible"));

  // Wrap at the end.
  assert!(engine.key_action(&mut dom, KeyAction::Tab));
  assert_eq!(engine.interaction_state().focused, Some(btn_dom_id));
}

#[test]
fn select_keyboard_navigation_changes_selection_and_skips_disabled_options() {
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
          el(
            "optgroup",
            vec![("disabled", "")],
            vec![el("option", vec![("id", "o4")], vec![])],
          ),
          el("option", vec![("id", "o5")], vec![]),
        ],
      )],
    )],
  )]);

  let select_dom_id = node_id(&dom, "sel");

  let control = FormControlKind::Select(SelectControl {
    multiple: false,
    size: 1,
    items: Arc::new(vec![
      SelectItem::Option {
        node_id: node_id(&dom, "o1"),
        label: "One".to_string(),
        value: "1".to_string(),
        selected: true,
        disabled: false,
        in_optgroup: false,
      },
      SelectItem::Option {
        node_id: node_id(&dom, "o2"),
        label: "Two".to_string(),
        value: "2".to_string(),
        selected: false,
        disabled: true,
        in_optgroup: false,
      },
      SelectItem::Option {
        node_id: node_id(&dom, "o3"),
        label: "Three".to_string(),
        value: "3".to_string(),
        selected: false,
        disabled: false,
        in_optgroup: false,
      },
      SelectItem::OptGroupLabel {
        label: "Group".to_string(),
        disabled: true,
      },
      SelectItem::Option {
        node_id: node_id(&dom, "o4"),
        label: "Four".to_string(),
        value: "4".to_string(),
        selected: false,
        disabled: true,
        in_optgroup: true,
      },
      SelectItem::Option {
        node_id: node_id(&dom, "o5"),
        label: "Five".to_string(),
        value: "5".to_string(),
        selected: false,
        disabled: false,
        in_optgroup: false,
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
      progress_bar_style: None,
      progress_value_style: None,
      meter_bar_style: None,
      meter_optimum_value_style: None,
      meter_suboptimum_value_style: None,
      meter_even_less_good_value_style: None,
      file_selector_button_style: None,
      disabled: false,
      focused: false,
      focus_visible: false,
      required: false,
      invalid: false,
      ime_preedit: None,
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

  let fragment_tree = FragmentTree::new(FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
    vec![FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 100.0, 30.0),
      select_box_id,
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
    Point::new(5.0, 5.0),
  );
  let (_, action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(5.0, 5.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
    "https://x/",
    "https://x/",
  );

  match action {
    InteractionAction::FocusChanged { node_id } => {
      assert_eq!(node_id, Some(select_dom_id));
    }
    InteractionAction::OpenSelectDropdown { select_node_id, .. } => {
      assert_eq!(select_node_id, select_dom_id);
    }
    other => panic!("expected focus/dropdown action for <select>, got {other:?}"),
  }

  assert!(
    !engine.interaction_state().focus_visible,
    "pointer focus should not set focus-visible"
  );
  assert!(!has_attr(&dom, "sel", "data-fastr-focus"));
  assert!(!has_attr(&dom, "sel", "data-fastr-focus-visible"));

  assert!(has_attr(&dom, "o1", "selected"));
  assert!(!has_attr(&dom, "o2", "selected"));
  assert!(!has_attr(&dom, "o3", "selected"));
  assert!(!has_attr(&dom, "o4", "selected"));
  assert!(!has_attr(&dom, "o5", "selected"));

  assert!(engine.key_action(&mut dom, KeyAction::ArrowDown));
  assert!(engine.interaction_state().focus_visible);
  assert!(!has_attr(&dom, "sel", "data-fastr-focus-visible"));
  assert!(!has_attr(&dom, "o1", "selected"));
  assert!(!has_attr(&dom, "o2", "selected"));
  assert!(has_attr(&dom, "o3", "selected"));

  assert!(engine.key_action(&mut dom, KeyAction::ArrowDown));
  assert!(
    has_attr(&dom, "o5", "selected"),
    "optgroup-disabled option skipped"
  );
  assert!(!has_attr(&dom, "o4", "selected"));

  assert!(engine.key_action(&mut dom, KeyAction::ArrowUp));
  assert!(has_attr(&dom, "o3", "selected"));

  assert!(engine.key_action(&mut dom, KeyAction::Home));
  assert!(has_attr(&dom, "o1", "selected"));

  assert!(engine.key_action(&mut dom, KeyAction::End));
  assert!(has_attr(&dom, "o5", "selected"));
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

  let btn_dom_id = node_id(&dom, "btn");
  let inp_dom_id = node_id(&dom, "inp");

  let mut engine = InteractionEngine::new();
  assert!(engine.key_action(&mut dom, KeyAction::ShiftTab));
  assert_eq!(engine.interaction_state().focused, Some(inp_dom_id));
  assert!(engine.interaction_state().focus_visible);
  assert!(!has_attr(&dom, "inp", "data-fastr-focus"));
  assert!(!has_attr(&dom, "inp", "data-fastr-focus-visible"));

  // Traverse backward.
  assert!(engine.key_action(&mut dom, KeyAction::ShiftTab));
  assert_eq!(engine.interaction_state().focused, Some(btn_dom_id));
  assert!(engine.interaction_state().focus_visible);
  assert!(!has_attr(&dom, "btn", "data-fastr-focus"));
  assert!(!has_attr(&dom, "btn", "data-fastr-focus-visible"));

  // Wrap backward.
  assert!(engine.key_action(&mut dom, KeyAction::ShiftTab));
  assert_eq!(engine.interaction_state().focused, Some(inp_dom_id));
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

  let area_dom_id = node_id(&dom, "area");
  let input_dom_id = node_id(&dom, "inp");

  let mut engine = InteractionEngine::new();
  assert!(engine.key_action(&mut dom, KeyAction::Tab));
  assert_eq!(engine.interaction_state().focused, Some(area_dom_id));
  assert!(engine.interaction_state().focus_visible);
  assert!(!has_attr(&dom, "area", "data-fastr-focus"));
  assert!(!has_attr(&dom, "area", "data-fastr-focus-visible"));

  assert!(engine.key_action(&mut dom, KeyAction::Tab));
  assert_eq!(engine.interaction_state().focused, Some(input_dom_id));
}

#[test]
fn document_selection_drag_creates_range_and_suppresses_click() {
  let full_text = "héllo world";
  let fragment_text = "world";

  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![el(
        "a",
        vec![("id", "link"), ("href", "foo")],
        vec![text(full_text)],
      )],
    )],
  )]);

  let text_dom_id = text_node_id(&dom, full_text);
  let mut text_box = BoxNode::new_text(default_style(), full_text.to_string());
  text_box.styled_node_id = Some(text_dom_id);
  let box_tree = BoxTree::new(BoxNode::new_block(
    default_style(),
    FormattingContextType::Block,
    vec![text_box],
  ));
  let text_box_id = find_box_id_for_styled_node(&box_tree, text_dom_id);

  let start_byte = full_text
    .find(fragment_text)
    .expect("expected substring in test text");
  let end_byte = start_byte + fragment_text.len();
  let source_range =
    TextSourceRange::new(start_byte..end_byte).expect("expected valid packed source range");

  let mut text_fragment =
    FragmentNode::new_text(Rect::from_xywh(0.0, 0.0, 200.0, 20.0), fragment_text, 16.0);
  if let crate::tree::fragment_tree::FragmentContent::Text {
    box_id,
    source_range: fragment_range,
    ..
  } = &mut text_fragment.content
  {
    *box_id = Some(text_box_id);
    *fragment_range = Some(source_range);
  } else {
    panic!("expected FragmentContent::Text");
  }

  let fragment_tree = FragmentTree::new(FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
    vec![text_fragment],
  ));

  let mut engine = InteractionEngine::new();
  engine.pointer_down(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(1.0, 10.0),
  );
  engine.pointer_move(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(199.0, 10.0),
  );
  assert!(
    engine
      .interaction_state()
      .document_selection
      .as_ref()
      .is_some_and(|sel| sel.has_highlight()),
    "dragging over text should create a document selection"
  );

  let (_changed, action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(199.0, 10.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
    "https://example.com/base/",
    "https://example.com/base/",
  );
  assert!(
    !matches!(
      action,
      InteractionAction::Navigate { .. } | InteractionAction::OpenInNewTab { .. }
    ),
    "selection drags should not activate links"
  );
  assert_eq!(
    engine.take_last_click_target(),
    None,
    "selection drags should not populate last_click_target"
  );

  assert_eq!(
    engine.clipboard_copy_with_layout(&mut dom, &box_tree, &fragment_tree),
    Some(fragment_text.to_string()),
    "clipboard copy should serialize the document selection range"
  );
}

#[test]
fn text_drag_drop_defers_default_insertion_until_apply_text_drop() {
  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![
        el("input", vec![("id", "src"), ("value", "hello")], vec![]),
        el("input", vec![("id", "dst"), ("value", "X")], vec![]),
      ],
    )],
  )]);

  let src_dom_id = node_id(&dom, "src");
  let dst_dom_id = node_id(&dom, "dst");

  let mut src_box = BoxNode::new_replaced(
    default_style(),
    ReplacedType::FormControl(text_input_form_control("hello")),
    None,
    None,
  );
  src_box.styled_node_id = Some(src_dom_id);

  let mut dst_box = BoxNode::new_replaced(
    default_style(),
    ReplacedType::FormControl(text_input_form_control("X")),
    None,
    None,
  );
  dst_box.styled_node_id = Some(dst_dom_id);

  let box_tree = BoxTree::new(BoxNode::new_block(
    default_style(),
    FormattingContextType::Block,
    vec![src_box, dst_box],
  ));
  let src_box_id = find_box_id_for_styled_node(&box_tree, src_dom_id);
  let dst_box_id = find_box_id_for_styled_node(&box_tree, dst_dom_id);

  let fragment_tree = FragmentTree::new(FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
    vec![
      FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 0.0, 160.0, 20.0), src_box_id, vec![]),
      FragmentNode::new_block_with_id(
        Rect::from_xywh(0.0, 40.0, 160.0, 20.0),
        dst_box_id,
        vec![],
      ),
    ],
  ));

  let mut engine = InteractionEngine::new();
  let _ = engine.focus_node_id(&mut dom, Some(src_dom_id), false);
  engine.set_text_selection_range(src_dom_id, 0, "hello".chars().count());

  engine.pointer_down_with_click_count(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(1.0, 10.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    1,
  );
  engine.pointer_move(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(20.0, 10.0),
  );

  let (_changed, action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(1.0, 50.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
    "https://example.com/base/",
    "https://example.com/base/",
  );

  assert_eq!(
    attr_value(&dom, "dst", "value"),
    Some("X".to_string()),
    "default drop insertion should be deferred until apply_text_drop"
  );

  let InteractionAction::TextDrop { target_dom_id, text } = action else {
    panic!("expected TextDrop action, got {action:?}");
  };
  assert_eq!(target_dom_id, dst_dom_id);
  assert_eq!(text, "hello");

  assert_eq!(
    engine.take_last_click_target(),
    None,
    "drag-drop pointer-up should suppress click target tracking"
  );

  assert!(engine.apply_text_drop(&mut dom, target_dom_id, &text));
  assert_eq!(
    attr_value(&dom, "dst", "value"),
    Some("helloX".to_string()),
    "apply_text_drop should perform the default insertion"
  );
}

#[test]
fn document_selection_drag_drop_defers_default_insertion_until_apply_text_drop() {
  let full_text = "héllo world";
  let fragment_text = "world";

  let mut dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![
        el(
          "a",
          vec![("id", "link"), ("href", "foo")],
          vec![text(full_text)],
        ),
        el(
          "textarea",
          vec![("id", "dst"), ("data-fastr-value", "X")],
          vec![],
        ),
      ],
    )],
  )]);

  let text_dom_id = text_node_id(&dom, full_text);
  let dst_dom_id = node_id(&dom, "dst");

  let mut text_box = BoxNode::new_text(default_style(), full_text.to_string());
  text_box.styled_node_id = Some(text_dom_id);

  let mut textarea_box = BoxNode::new_replaced(
    default_style(),
    ReplacedType::FormControl(textarea_form_control("X")),
    None,
    None,
  );
  textarea_box.styled_node_id = Some(dst_dom_id);

  let box_tree = BoxTree::new(BoxNode::new_block(
    default_style(),
    FormattingContextType::Block,
    vec![text_box, textarea_box],
  ));
  let text_box_id = find_box_id_for_styled_node(&box_tree, text_dom_id);
  let textarea_box_id = find_box_id_for_styled_node(&box_tree, dst_dom_id);

  let start_byte = full_text
    .find(fragment_text)
    .expect("expected substring in test text");
  let end_byte = start_byte + fragment_text.len();
  let source_range =
    TextSourceRange::new(start_byte..end_byte).expect("expected valid packed source range");

  let mut text_fragment =
    FragmentNode::new_text(Rect::from_xywh(0.0, 0.0, 200.0, 20.0), fragment_text, 16.0);
  if let crate::tree::fragment_tree::FragmentContent::Text {
    box_id,
    source_range: fragment_range,
    ..
  } = &mut text_fragment.content
  {
    *box_id = Some(text_box_id);
    *fragment_range = Some(source_range);
  } else {
    panic!("expected FragmentContent::Text");
  }

  let fragment_tree = FragmentTree::new(FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
    vec![
      text_fragment,
      FragmentNode::new_block_with_id(
        Rect::from_xywh(0.0, 40.0, 200.0, 40.0),
        textarea_box_id,
        vec![],
      ),
    ],
  ));

  let mut engine = InteractionEngine::new();

  // First, create a highlighted document selection.
  engine.pointer_down(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(1.0, 10.0),
  );
  engine.pointer_move(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(199.0, 10.0),
  );
  let _ = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(199.0, 10.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
    "https://example.com/base/",
    "https://example.com/base/",
  );

  assert!(
    engine
      .interaction_state()
      .document_selection
      .as_ref()
      .is_some_and(|sel| sel.has_highlight()),
    "expected initial drag to create a document selection"
  );

  // Start a drag-drop gesture from within the highlighted selection.
  engine.pointer_down_with_click_count(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(1.0, 10.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    1,
  );
  engine.pointer_move(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(1.0, 50.0),
  );

  let (_changed, action) = engine.pointer_up_with_scroll(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &ScrollState::default(),
    Point::new(1.0, 50.0),
    PointerButton::Primary,
    PointerModifiers::default(),
    true,
    "https://example.com/base/",
    "https://example.com/base/",
  );

  assert_eq!(
    attr_value(&dom, "dst", "data-fastr-value"),
    Some("X".to_string()),
    "default drop insertion should be deferred until apply_text_drop"
  );

  let InteractionAction::TextDrop { target_dom_id, text } = action else {
    panic!("expected TextDrop action, got {action:?}");
  };
  assert_eq!(target_dom_id, dst_dom_id);
  assert_eq!(text, fragment_text);

  assert_eq!(
    engine.take_last_click_target(),
    None,
    "drag-drop pointer-up should suppress click target tracking"
  );

  assert!(engine.apply_text_drop(&mut dom, target_dom_id, &text));
  assert_eq!(
    attr_value(&dom, "dst", "data-fastr-value"),
    Some(format!("{fragment_text}X")),
    "apply_text_drop should perform the default insertion"
  );
}
