use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::sync::Arc;

use super::{InteractionEngine, KeyAction};
use crate::cli_utils::prng::SplitMix64;
use crate::dom::enumerate_dom_ids;
use crate::dom::{DomNode, DomNodeType, HTML_NAMESPACE};
use crate::geometry::{Point, Rect};
use crate::interaction::fragment_tree_with_scroll;
use crate::interaction::scroll_wheel::{apply_wheel_scroll_at_point, ScrollWheelInput};
use crate::scroll::ScrollState;
use crate::style::display::FormattingContextType;
use crate::style::types::{LineHeight, Overflow};
use crate::style::ComputedStyle;
use crate::tree::box_tree::{BoxNode, BoxTree};
use crate::tree::fragment_tree::{FragmentContent, FragmentNode, FragmentTree};
use crate::ui::messages::{PointerButton, PointerModifiers};
use selectors::context::QuirksMode;

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

fn txt(value: &str) -> DomNode {
  DomNode {
    node_type: DomNodeType::Text {
      content: value.to_string(),
    },
    children: Vec::new(),
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

fn node_id(root: &DomNode, html_id: &str) -> usize {
  let ids = enumerate_dom_ids(root);
  let node = find_by_id(root, html_id).expect("node");
  ids.get(&(node as *const DomNode)).copied().expect("id present")
}

fn default_style() -> Arc<ComputedStyle> {
  let mut style = ComputedStyle::default();
  // Avoid `line-height: normal` which consults full font metrics and is unnecessary for fuzz
  // invariants.
  style.line_height = LineHeight::Number(1.0);
  Arc::new(style)
}

fn scroller_style() -> Arc<ComputedStyle> {
  let mut style = ComputedStyle::default();
  style.overflow_y = Overflow::Scroll;
  style.line_height = LineHeight::Number(1.0);
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

#[derive(Clone)]
struct SharedFixture {
  dom: DomNode,
  box_tree: BoxTree,
  fragment_tree: FragmentTree,
}

fn build_fixture() -> SharedFixture {
  let dom = doc(vec![el(
    "html",
    vec![("id", "html")],
    vec![el(
      "body",
      vec![("id", "body")],
      vec![
        el(
          "div",
          vec![("id", "scroller")],
          vec![
            el("input", vec![("id", "txt"), ("value", "")], vec![]),
            el("textarea", vec![("id", "ta")], vec![txt("hello")]),
            el("a", vec![("id", "link"), ("href", "/foo")], vec![txt("link")]),
            el(
              "input",
              vec![
                ("id", "range"),
                ("type", "range"),
                ("min", "0"),
                ("max", "100"),
                ("value", "50"),
              ],
              vec![],
            ),
            el(
              "select",
              vec![("id", "sel")],
              vec![
                el("option", vec![("id", "o1"), ("value", "a"), ("selected", "")], vec![txt("A")]),
                el("option", vec![("id", "o2"), ("value", "b")], vec![txt("B")]),
              ],
            ),
          ],
        ),
        // Extra content beyond the viewport so viewport scrolling is exercised too.
        el("div", vec![("id", "tail")], vec![txt("tail")]),
      ],
    )],
  )]);

  let scroller_dom_id = node_id(&dom, "scroller");
  let input_dom_id = node_id(&dom, "txt");
  let textarea_dom_id = node_id(&dom, "ta");
  let link_dom_id = node_id(&dom, "link");
  let range_dom_id = node_id(&dom, "range");
  let select_dom_id = node_id(&dom, "sel");
  let tail_dom_id = node_id(&dom, "tail");

  let style = default_style();

  let mut input_box = BoxNode::new_block(style.clone(), FormattingContextType::Block, vec![]);
  input_box.styled_node_id = Some(input_dom_id);
  let mut textarea_box = BoxNode::new_block(style.clone(), FormattingContextType::Block, vec![]);
  textarea_box.styled_node_id = Some(textarea_dom_id);
  let mut link_box = BoxNode::new_block(style.clone(), FormattingContextType::Block, vec![]);
  link_box.styled_node_id = Some(link_dom_id);
  let mut range_box = BoxNode::new_block(style.clone(), FormattingContextType::Block, vec![]);
  range_box.styled_node_id = Some(range_dom_id);
  let mut select_box = BoxNode::new_block(style.clone(), FormattingContextType::Block, vec![]);
  select_box.styled_node_id = Some(select_dom_id);
  let mut tail_box = BoxNode::new_block(style.clone(), FormattingContextType::Block, vec![]);
  tail_box.styled_node_id = Some(tail_dom_id);

  let mut scroller_box = BoxNode::new_block(
    style.clone(),
    FormattingContextType::Block,
    vec![input_box, textarea_box, link_box, range_box, select_box],
  );
  scroller_box.styled_node_id = Some(scroller_dom_id);

  let box_tree = BoxTree::new(BoxNode::new_block(
    style,
    FormattingContextType::Block,
    vec![scroller_box, tail_box],
  ));

  let scroller_box_id = find_box_id_for_styled_node(&box_tree, scroller_dom_id);
  let input_box_id = find_box_id_for_styled_node(&box_tree, input_dom_id);
  let textarea_box_id = find_box_id_for_styled_node(&box_tree, textarea_dom_id);
  let link_box_id = find_box_id_for_styled_node(&box_tree, link_dom_id);
  let range_box_id = find_box_id_for_styled_node(&box_tree, range_dom_id);
  let select_box_id = find_box_id_for_styled_node(&box_tree, select_dom_id);
  let tail_box_id = find_box_id_for_styled_node(&box_tree, tail_dom_id);

  // Root viewport is 200x200. The `tail` fragment is positioned below that so viewport scrolling has
  // a non-zero range.
  let root = FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 200.0, 200.0),
    vec![
      FragmentNode::new_with_style(
        Rect::from_xywh(0.0, 0.0, 200.0, 100.0),
        FragmentContent::Block {
          box_id: Some(scroller_box_id),
        },
        vec![
          FragmentNode::new_block_with_id(
            Rect::from_xywh(10.0, 10.0, 180.0, 20.0),
            input_box_id,
            vec![],
          ),
          FragmentNode::new_block_with_id(
            Rect::from_xywh(10.0, 40.0, 180.0, 40.0),
            textarea_box_id,
            vec![],
          ),
          FragmentNode::new_block_with_id(
            Rect::from_xywh(10.0, 110.0, 180.0, 20.0),
            link_box_id,
            vec![],
          ),
          FragmentNode::new_block_with_id(
            Rect::from_xywh(10.0, 140.0, 180.0, 20.0),
            range_box_id,
            vec![],
          ),
          FragmentNode::new_block_with_id(
            Rect::from_xywh(10.0, 170.0, 180.0, 30.0),
            select_box_id,
            vec![],
          ),
          // Tail content inside the scroller to ensure it can scroll.
          FragmentNode::new_block(Rect::from_xywh(0.0, 280.0, 1.0, 1.0), vec![]),
        ],
        scroller_style(),
      ),
      FragmentNode::new_block_with_id(
        Rect::from_xywh(0.0, 420.0, 200.0, 20.0),
        tail_box_id,
        vec![],
      ),
      FragmentNode::new_block(Rect::from_xywh(0.0, 800.0, 1.0, 1.0), vec![]),
    ],
  );
  let fragment_tree = FragmentTree::new(root);

  SharedFixture {
    dom,
    box_tree,
    fragment_tree,
  }
}

fn rand_i32(rng: &mut SplitMix64, min: i32, max_inclusive: i32) -> i32 {
  debug_assert!(min <= max_inclusive);
  let span = (max_inclusive - min + 1) as usize;
  min + rng.next_usize(span) as i32
}

fn rand_viewport_point(rng: &mut SplitMix64) -> Point {
  // Include some out-of-bounds coordinates to exercise "miss" paths and sentinel behaviour.
  let x = rand_i32(rng, -50, 250) as f32;
  let y = rand_i32(rng, -50, 250) as f32;
  Point::new(x, y)
}

fn rand_pointer_modifiers(rng: &mut SplitMix64) -> PointerModifiers {
  let mut mods = PointerModifiers::NONE;
  if (rng.next_u64() & 1) != 0 {
    mods = mods | PointerModifiers::CTRL;
  }
  if (rng.next_u64() & 1) != 0 {
    mods = mods | PointerModifiers::SHIFT;
  }
  if (rng.next_u64() & 1) != 0 {
    mods = mods | PointerModifiers::ALT;
  }
  if (rng.next_u64() & 1) != 0 {
    mods = mods | PointerModifiers::META;
  }
  mods
}

fn rand_key_action(rng: &mut SplitMix64) -> KeyAction {
  const KEYS: &[KeyAction] = &[
    KeyAction::Backspace,
    KeyAction::Delete,
    KeyAction::Enter,
    KeyAction::Tab,
    KeyAction::ShiftTab,
    KeyAction::Space,
    KeyAction::ShiftSpace,
    KeyAction::ArrowLeft,
    KeyAction::ArrowRight,
    KeyAction::ShiftArrowLeft,
    KeyAction::ShiftArrowRight,
    KeyAction::ShiftArrowUp,
    KeyAction::ShiftArrowDown,
    KeyAction::ArrowUp,
    KeyAction::ArrowDown,
    KeyAction::Home,
    KeyAction::End,
    KeyAction::ShiftHome,
    KeyAction::ShiftEnd,
    KeyAction::SelectAll,
  ];
  KEYS[rng.next_usize(KEYS.len())]
}

fn rand_text(rng: &mut SplitMix64) -> String {
  const CHOICES: &[&str] = &["a", "b", "c", " ", "あ", "😀"];
  let len = 1 + rng.next_usize(3);
  let mut out = String::new();
  for _ in 0..len {
    out.push_str(CHOICES[rng.next_usize(CHOICES.len())]);
  }
  out
}

fn check_invariants(
  engine: &InteractionEngine,
  dom: &mut DomNode,
  scroll: &ScrollState,
  seed: u64,
  step: usize,
  event: &str,
) {
  if let Err(err) = catch_unwind(AssertUnwindSafe(|| engine.assert_invariants(dom, scroll))) {
    eprintln!("interaction invariant failed: seed={seed} step={step} event={event}");
    resume_unwind(err);
  }
}

fn apply_event(
  seed: u64,
  step: usize,
  event: &str,
  f: impl FnOnce(),
) {
  if let Err(err) = catch_unwind(AssertUnwindSafe(f)) {
    eprintln!("interaction event panicked: seed={seed} step={step} event={event}");
    resume_unwind(err);
  }
}

#[test]
fn deterministic_engine_fuzz_invariants() {
  // Keep runtime CI-friendly: a few hundred short sequences.
  const SEQUENCES: usize = 200;
  const STEPS: usize = 40;
  const BASE_SEED: u64 = 0xD1A5_71C0_6EED_1234;

  let fixture = build_fixture();

  for seq in 0..SEQUENCES {
    let seed = BASE_SEED.wrapping_add(seq as u64);
    let mut rng = SplitMix64::new(seed);

    let mut dom = fixture.dom.clone();
    let box_tree = &fixture.box_tree;
    let fragment_tree = &fixture.fragment_tree;

    let mut engine = InteractionEngine::new();
    let mut scroll = ScrollState::default();

    check_invariants(&engine, &mut dom, &scroll, seed, 0, "init");

    for step in 0..STEPS {
      let choice = rng.next_usize(100);
      match choice {
        0..=19 => {
          let viewport_point = rand_viewport_point(&mut rng);
          apply_event(seed, step, "pointer_move", || {
            let scrolled_tree = fragment_tree_with_scroll(fragment_tree, &scroll);
            let _ = engine.pointer_move(
              &mut dom,
              box_tree,
              &scrolled_tree,
              &scroll,
              viewport_point,
            );
          });
          check_invariants(&engine, &mut dom, &scroll, seed, step, "pointer_move");
        }
        20..=39 => {
          let viewport_point = rand_viewport_point(&mut rng);
          apply_event(seed, step, "pointer_down", || {
            let scrolled_tree = fragment_tree_with_scroll(fragment_tree, &scroll);
            let _ = engine.pointer_down(
              &mut dom,
              box_tree,
              &scrolled_tree,
              &scroll,
              viewport_point,
            );
          });
          check_invariants(&engine, &mut dom, &scroll, seed, step, "pointer_down");
        }
        40..=59 => {
          let viewport_point = rand_viewport_point(&mut rng);
          let button = if (rng.next_u64() & 1) != 0 {
            PointerButton::Primary
          } else {
            PointerButton::Secondary
          };
          let modifiers = rand_pointer_modifiers(&mut rng);
          apply_event(seed, step, "pointer_up", || {
            let scrolled_tree = fragment_tree_with_scroll(fragment_tree, &scroll);
            let _ = engine.pointer_up_with_scroll(
              &mut dom,
              box_tree,
              &scrolled_tree,
              &scroll,
              viewport_point,
              button,
              modifiers,
              true,
              "https://example.com/doc",
              "https://example.com/base/",
            );
          });
          check_invariants(&engine, &mut dom, &scroll, seed, step, "pointer_up");
        }
        60..=74 => {
          let key = rand_key_action(&mut rng);
          apply_event(seed, step, "key_action", || {
            let _ = engine.key_action_with_box_tree(&mut dom, Some(box_tree), key);
          });
          check_invariants(&engine, &mut dom, &scroll, seed, step, "key_action");
        }
        75..=82 => {
          let text = rand_text(&mut rng);
          apply_event(seed, step, "text_input", || {
            let _ = engine.text_input(&mut dom, &text);
          });
          check_invariants(&engine, &mut dom, &scroll, seed, step, "text_input");
        }
        83..=86 => {
          let text = rand_text(&mut rng);
          apply_event(seed, step, "ime_preedit", || {
            let _ = engine.ime_preedit(&mut dom, &text, None);
          });
          check_invariants(&engine, &mut dom, &scroll, seed, step, "ime_preedit");
        }
        87..=90 => {
          let text = rand_text(&mut rng);
          apply_event(seed, step, "ime_commit", || {
            let _ = engine.ime_commit(&mut dom, &text);
          });
          check_invariants(&engine, &mut dom, &scroll, seed, step, "ime_commit");
        }
        91..=92 => {
          apply_event(seed, step, "ime_cancel", || {
            let _ = engine.ime_cancel(&mut dom);
          });
          check_invariants(&engine, &mut dom, &scroll, seed, step, "ime_cancel");
        }
        _ => {
          // Scroll wheel deltas are expressed in CSS px.
          let viewport_point = rand_viewport_point(&mut rng);
          let page_point = viewport_point.translate(scroll.viewport);
          let delta_x = rand_i32(&mut rng, -60, 60) as f32;
          let delta_y = rand_i32(&mut rng, -120, 120) as f32;
          apply_event(seed, step, "scroll_wheel", || {
            scroll = apply_wheel_scroll_at_point(
              fragment_tree,
              &scroll,
              fragment_tree.viewport_size(),
              page_point,
              ScrollWheelInput { delta_x, delta_y },
            );
          });
          check_invariants(&engine, &mut dom, &scroll, seed, step, "scroll_wheel");
        }
      }
    }
  }
}
