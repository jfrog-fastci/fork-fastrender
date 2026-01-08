use std::collections::HashMap;
use std::sync::Arc;

use fastrender::animation::apply_scroll_driven_animations;
use fastrender::css::types::{Declaration, Keyframe, KeyframesRule, PropertyValue};
use fastrender::geometry::{Point, Rect, Size};
use fastrender::scroll::ScrollState;
use fastrender::style::types::{
  AnimationRange, AnimationTimeline, Overflow, ScrollFunctionTimeline, ScrollTimelineScroller,
  TimelineAxis, TransitionTimingFunction,
};
use fastrender::style::ComputedStyle;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode, FragmentTree};

fn fade_keyframes(name: &str) -> KeyframesRule {
  KeyframesRule {
    name: name.to_string(),
    keyframes: vec![
      Keyframe {
        offset: 0.0,
        declarations: vec![Declaration {
          property: "opacity".into(),
          value: PropertyValue::Number(0.0),
          contains_var: false,
          raw_value: String::new(),
          important: false,
        }],
        timing_functions: Vec::new(),
      },
      Keyframe {
        offset: 1.0,
        declarations: vec![Declaration {
          property: "opacity".into(),
          value: PropertyValue::Number(1.0),
          contains_var: false,
          raw_value: String::new(),
          important: false,
        }],
        timing_functions: Vec::new(),
      },
    ],
  }
}

#[test]
fn scroll_function_timeline_uses_self_scroll_offsets() {
  let animation_name = "fade";

  let mut style = ComputedStyle::default();
  style.overflow_y = Overflow::Auto;
  style.animation_names = vec![Some(animation_name.to_string())];
  style.animation_ranges = vec![AnimationRange::default()];
  style.animation_timelines = vec![AnimationTimeline::Scroll(ScrollFunctionTimeline {
    scroller: ScrollTimelineScroller::SelfElement,
    axis: TimelineAxis::Block,
  })];
  style.animation_timing_functions = vec![TransitionTimingFunction::Linear].into();
  let style = Arc::new(style);

  let mut scroller = FragmentNode::new_with_style(
    Rect::from_xywh(0.0, 0.0, 50.0, 100.0),
    FragmentContent::Block { box_id: Some(1) },
    vec![],
    style,
  );
  scroller.scroll_overflow = Rect::from_xywh(0.0, 0.0, 50.0, 200.0);

  let root = FragmentNode::new(
    Rect::from_xywh(0.0, 0.0, 50.0, 100.0),
    FragmentContent::Block { box_id: None },
    vec![scroller],
  );
  let mut tree = FragmentTree::with_viewport(root, Size::new(50.0, 100.0));
  tree.keyframes = HashMap::from([(animation_name.to_string(), fade_keyframes(animation_name))]);

  let scroll_state = ScrollState::from_parts(
    Point::ZERO,
    HashMap::from([(1usize, Point::new(0.0, 50.0))]),
  );

  apply_scroll_driven_animations(&mut tree, &scroll_state);

  let scroller_fragment = &tree.root.children[0];
  let opacity = scroller_fragment
    .style
    .as_ref()
    .expect("animated style present")
    .opacity;
  assert!(
    (opacity - 0.5).abs() < 0.05,
    "opacity should reflect scroll(self) progress, got {}",
    opacity
  );
}
