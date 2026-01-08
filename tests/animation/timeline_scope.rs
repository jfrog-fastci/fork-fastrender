use std::collections::HashMap;
use std::sync::Arc;

use fastrender::animation::apply_scroll_driven_animations;
use fastrender::css::types::{Declaration, Keyframe, KeyframesRule, PropertyValue};
use fastrender::geometry::{Point, Rect, Size};
use fastrender::scroll::ScrollState;
use fastrender::style::types::{
  AnimationRange, AnimationTimeline, ScrollTimeline, TimelineAxis, TimelineScopeProperty,
  TransitionTimingFunction,
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

fn animated_style(animation: &str, timeline: &str) -> Arc<ComputedStyle> {
  let mut style = ComputedStyle::default();
  style.animation_names = vec![Some(animation.to_string())];
  style.animation_ranges = vec![AnimationRange::default()];
  style.animation_timelines = vec![AnimationTimeline::Named(timeline.to_string())];
  style.animation_timing_functions = vec![TransitionTimingFunction::Linear].into();
  Arc::new(style)
}

fn scroll_timeline_style(name: &str) -> Arc<ComputedStyle> {
  let mut style = ComputedStyle::default();
  style.scroll_timelines = vec![ScrollTimeline {
    name: Some(name.to_string()),
    axis: TimelineAxis::Block,
    ..ScrollTimeline::default()
  }];
  Arc::new(style)
}

#[test]
fn timeline_scope_promotes_named_timeline_to_siblings() {
  let animation_name = "fade";
  let timeline_name = "--scroller";

  let mut parent_style = ComputedStyle::default();
  parent_style.timeline_scope = TimelineScopeProperty::Names(vec![timeline_name.to_string()]);
  let parent_style = Arc::new(parent_style);

  let animated = FragmentNode::new_with_style(
    Rect::from_xywh(0.0, 0.0, 50.0, 20.0),
    FragmentContent::Block { box_id: None },
    vec![],
    animated_style(animation_name, timeline_name),
  );

  let mut scroller = FragmentNode::new_with_style(
    Rect::from_xywh(0.0, 0.0, 50.0, 100.0),
    FragmentContent::Block { box_id: Some(1) },
    vec![],
    scroll_timeline_style(timeline_name),
  );
  scroller.scroll_overflow = Rect::from_xywh(0.0, 0.0, 50.0, 200.0);

  let root = FragmentNode::new_with_style(
    Rect::from_xywh(0.0, 0.0, 50.0, 100.0),
    FragmentContent::Block { box_id: None },
    vec![animated, scroller],
    parent_style,
  );
  let mut tree = FragmentTree::with_viewport(root, Size::new(50.0, 100.0));
  tree
    .keyframes
    .insert(animation_name.to_string(), fade_keyframes(animation_name));

  let scroll_state = ScrollState::from_parts(
    Point::ZERO,
    HashMap::from([(1usize, Point::new(0.0, 50.0))]),
  );
  apply_scroll_driven_animations(&mut tree, &scroll_state);

  let opacity = tree.root.children[0]
    .style
    .as_ref()
    .expect("animated style present")
    .opacity;
  assert!(
    (opacity - 0.5).abs() < 0.05,
    "expected sibling promotion to resolve progress ≈0.5, got {opacity}"
  );
}

#[test]
fn timeline_scope_blocks_ancestor_timelines_inside_subtree() {
  let animation_name = "fade";
  let timeline_name = "--x";

  let mut outer_style = ComputedStyle::default();
  outer_style.scroll_timelines = vec![ScrollTimeline {
    name: Some(timeline_name.to_string()),
    axis: TimelineAxis::Block,
    ..ScrollTimeline::default()
  }];
  let outer_style = Arc::new(outer_style);

  let mut inner_style = ComputedStyle::default();
  inner_style.timeline_scope = TimelineScopeProperty::Names(vec![timeline_name.to_string()]);
  let inner_style = Arc::new(inner_style);

  let animated = FragmentNode::new_with_style(
    Rect::from_xywh(0.0, 0.0, 50.0, 20.0),
    FragmentContent::Block { box_id: None },
    vec![],
    animated_style(animation_name, timeline_name),
  );

  let mut inner_scroller = FragmentNode::new_with_style(
    Rect::from_xywh(0.0, 0.0, 50.0, 100.0),
    FragmentContent::Block { box_id: Some(2) },
    vec![],
    scroll_timeline_style(timeline_name),
  );
  inner_scroller.scroll_overflow = Rect::from_xywh(0.0, 0.0, 50.0, 200.0);

  let inner_root = FragmentNode::new_with_style(
    Rect::from_xywh(0.0, 0.0, 50.0, 100.0),
    FragmentContent::Block { box_id: None },
    vec![animated, inner_scroller],
    inner_style,
  );

  let mut outer_root = FragmentNode::new_with_style(
    Rect::from_xywh(0.0, 0.0, 50.0, 100.0),
    FragmentContent::Block { box_id: Some(1) },
    vec![inner_root],
    outer_style,
  );
  outer_root.scroll_overflow = Rect::from_xywh(0.0, 0.0, 50.0, 300.0);

  let mut tree = FragmentTree::with_viewport(outer_root, Size::new(50.0, 100.0));
  tree
    .keyframes
    .insert(animation_name.to_string(), fade_keyframes(animation_name));

  let scroll_state = ScrollState::from_parts(
    Point::ZERO,
    HashMap::from([
      (1usize, Point::new(0.0, 50.0)), // progress 0.25 (range 200)
      (2usize, Point::new(0.0, 75.0)), // progress 0.75 (range 100)
    ]),
  );
  apply_scroll_driven_animations(&mut tree, &scroll_state);

  let opacity = tree.root.children[0].children[0]
    .style
    .as_ref()
    .expect("animated style present")
    .opacity;
  assert!(
    (opacity - 0.75).abs() < 0.05,
    "expected inner promoted timeline (0.75), got {opacity}"
  );
}

#[test]
fn timeline_scope_blocks_descendant_timelines_outside_subtree() {
  let animation_name = "fade";
  let timeline_name = "--x";

  let mut outer_style = ComputedStyle::default();
  outer_style.timeline_scope = TimelineScopeProperty::Names(vec![timeline_name.to_string()]);
  let outer_style = Arc::new(outer_style);

  let outside = FragmentNode::new_with_style(
    Rect::from_xywh(0.0, 0.0, 50.0, 20.0),
    FragmentContent::Block { box_id: None },
    vec![],
    animated_style(animation_name, timeline_name),
  );

  let mut inner_style = ComputedStyle::default();
  inner_style.timeline_scope = TimelineScopeProperty::Names(vec![timeline_name.to_string()]);
  let inner_style = Arc::new(inner_style);

  let mut inner_scroller = FragmentNode::new_with_style(
    Rect::from_xywh(0.0, 0.0, 50.0, 100.0),
    FragmentContent::Block { box_id: Some(1) },
    vec![],
    scroll_timeline_style(timeline_name),
  );
  inner_scroller.scroll_overflow = Rect::from_xywh(0.0, 0.0, 50.0, 200.0);

  let inner_root = FragmentNode::new_with_style(
    Rect::from_xywh(0.0, 0.0, 50.0, 100.0),
    FragmentContent::Block { box_id: None },
    vec![inner_scroller],
    inner_style,
  );

  let root = FragmentNode::new_with_style(
    Rect::from_xywh(0.0, 0.0, 50.0, 100.0),
    FragmentContent::Block { box_id: None },
    vec![outside, inner_root],
    outer_style,
  );

  let mut tree = FragmentTree::with_viewport(root, Size::new(50.0, 100.0));
  tree
    .keyframes
    .insert(animation_name.to_string(), fade_keyframes(animation_name));

  let scroll_state = ScrollState::from_parts(
    Point::ZERO,
    HashMap::from([(1usize, Point::new(0.0, 50.0))]),
  );
  apply_scroll_driven_animations(&mut tree, &scroll_state);

  let opacity = tree.root.children[0]
    .style
    .as_ref()
    .expect("animated style present")
    .opacity;
  assert!(
    opacity > 0.95,
    "expected outer lookup to be blocked (inactive), got {opacity}"
  );
}

#[test]
fn timeline_scope_ambiguous_name_declares_inactive() {
  let animation_name = "fade";
  let timeline_name = "--x";

  let mut ancestor_style = ComputedStyle::default();
  ancestor_style.scroll_timelines = vec![ScrollTimeline {
    name: Some(timeline_name.to_string()),
    axis: TimelineAxis::Block,
    ..ScrollTimeline::default()
  }];
  let ancestor_style = Arc::new(ancestor_style);

  let mut scope_style = ComputedStyle::default();
  scope_style.timeline_scope = TimelineScopeProperty::Names(vec![timeline_name.to_string()]);
  let scope_style = Arc::new(scope_style);

  let animated = FragmentNode::new_with_style(
    Rect::from_xywh(0.0, 0.0, 50.0, 20.0),
    FragmentContent::Block { box_id: None },
    vec![],
    animated_style(animation_name, timeline_name),
  );

  let mut scroller_one = FragmentNode::new_with_style(
    Rect::from_xywh(0.0, 0.0, 50.0, 100.0),
    FragmentContent::Block { box_id: Some(2) },
    vec![],
    scroll_timeline_style(timeline_name),
  );
  scroller_one.scroll_overflow = Rect::from_xywh(0.0, 0.0, 50.0, 200.0);

  let mut scroller_two = FragmentNode::new_with_style(
    Rect::from_xywh(0.0, 0.0, 50.0, 100.0),
    FragmentContent::Block { box_id: Some(3) },
    vec![],
    scroll_timeline_style(timeline_name),
  );
  scroller_two.scroll_overflow = Rect::from_xywh(0.0, 0.0, 50.0, 200.0);

  let scope_root = FragmentNode::new_with_style(
    Rect::from_xywh(0.0, 0.0, 50.0, 100.0),
    FragmentContent::Block { box_id: None },
    vec![animated, scroller_one, scroller_two],
    scope_style,
  );

  let mut root = FragmentNode::new_with_style(
    Rect::from_xywh(0.0, 0.0, 50.0, 100.0),
    FragmentContent::Block { box_id: Some(1) },
    vec![scope_root],
    ancestor_style,
  );
  root.scroll_overflow = Rect::from_xywh(0.0, 0.0, 50.0, 200.0);

  let mut tree = FragmentTree::with_viewport(root, Size::new(50.0, 100.0));
  tree
    .keyframes
    .insert(animation_name.to_string(), fade_keyframes(animation_name));

  let scroll_state = ScrollState::from_parts(
    Point::ZERO,
    HashMap::from([
      (1usize, Point::new(0.0, 50.0)), // ancestor progress 0.5 (range 100)
      (2usize, Point::new(0.0, 25.0)),
      (3usize, Point::new(0.0, 75.0)),
    ]),
  );
  apply_scroll_driven_animations(&mut tree, &scroll_state);

  let opacity = tree.root.children[0].children[0]
    .style
    .as_ref()
    .expect("animated style present")
    .opacity;
  assert!(
    opacity > 0.95,
    "expected ambiguity to declare inactive and block ancestor timeline, got {opacity}"
  );
}

#[test]
fn timeline_scope_all_ambiguous_name_declares_inactive() {
  let animation_name = "fade";
  let timeline_name = "--x";

  let mut ancestor_style = ComputedStyle::default();
  ancestor_style.scroll_timelines = vec![ScrollTimeline {
    name: Some(timeline_name.to_string()),
    axis: TimelineAxis::Block,
    ..ScrollTimeline::default()
  }];
  let ancestor_style = Arc::new(ancestor_style);

  let mut scope_style = ComputedStyle::default();
  scope_style.timeline_scope = TimelineScopeProperty::All;
  let scope_style = Arc::new(scope_style);

  let animated = FragmentNode::new_with_style(
    Rect::from_xywh(0.0, 0.0, 50.0, 20.0),
    FragmentContent::Block { box_id: None },
    vec![],
    animated_style(animation_name, timeline_name),
  );

  let mut scroller_one = FragmentNode::new_with_style(
    Rect::from_xywh(0.0, 0.0, 50.0, 100.0),
    FragmentContent::Block { box_id: Some(2) },
    vec![],
    scroll_timeline_style(timeline_name),
  );
  scroller_one.scroll_overflow = Rect::from_xywh(0.0, 0.0, 50.0, 200.0);

  let mut scroller_two = FragmentNode::new_with_style(
    Rect::from_xywh(0.0, 0.0, 50.0, 100.0),
    FragmentContent::Block { box_id: Some(3) },
    vec![],
    scroll_timeline_style(timeline_name),
  );
  scroller_two.scroll_overflow = Rect::from_xywh(0.0, 0.0, 50.0, 200.0);

  let scope_root = FragmentNode::new_with_style(
    Rect::from_xywh(0.0, 0.0, 50.0, 100.0),
    FragmentContent::Block { box_id: None },
    vec![animated, scroller_one, scroller_two],
    scope_style,
  );

  let mut root = FragmentNode::new_with_style(
    Rect::from_xywh(0.0, 0.0, 50.0, 100.0),
    FragmentContent::Block { box_id: Some(1) },
    vec![scope_root],
    ancestor_style,
  );
  root.scroll_overflow = Rect::from_xywh(0.0, 0.0, 50.0, 200.0);

  let mut tree = FragmentTree::with_viewport(root, Size::new(50.0, 100.0));
  tree
    .keyframes
    .insert(animation_name.to_string(), fade_keyframes(animation_name));

  let scroll_state = ScrollState::from_parts(
    Point::ZERO,
    HashMap::from([
      (1usize, Point::new(0.0, 50.0)),
      (2usize, Point::new(0.0, 25.0)),
      (3usize, Point::new(0.0, 75.0)),
    ]),
  );
  apply_scroll_driven_animations(&mut tree, &scroll_state);

  let opacity = tree.root.children[0].children[0]
    .style
    .as_ref()
    .expect("animated style present")
    .opacity;
  assert!(
    opacity > 0.95,
    "expected `timeline-scope: all` ambiguity to declare inactive, got {opacity}"
  );
}
