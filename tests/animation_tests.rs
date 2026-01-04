#[path = "animation/mod.rs"]
mod animation;

use std::collections::HashMap;

use fastrender::animation::{
  axis_scroll_state, sample_keyframes, scroll_timeline_progress, view_timeline_progress,
  AnimatedValue,
};
use fastrender::api::FastRender;
use fastrender::css::parser::parse_stylesheet;
use fastrender::css::types::{Transform as CssTransform, TranslateValue};
use fastrender::dom;
use fastrender::scroll::ScrollState;
use fastrender::style::cascade::apply_styles_with_media;
use fastrender::style::cascade::StyledNode;
use fastrender::style::media::MediaContext;
use fastrender::style::types::{
  AnimationRange, AnimationTimeline, BasicShape, FilterFunction, RangeOffset,
  ScrollFunctionTimeline, ScrollTimeline, ScrollTimelineScroller, TimelineAxis, TimelineOffset,
  ViewTimeline, ViewTimelinePhase, WritingMode,
};
use fastrender::Rgba;
use fastrender::{
  BoxNode, ComputedStyle, FragmentNode, FragmentTree, Length, Point, PreparedPaintOptions,
  RenderOptions, Size,
};

fn find_by_tag<'a>(node: &'a StyledNode, tag: &str) -> Option<&'a StyledNode> {
  if let Some(name) = node.node.tag_name() {
    if name.eq_ignore_ascii_case(tag) {
      return Some(node);
    }
  }
  for child in node.children.iter() {
    if let Some(found) = find_by_tag(child, tag) {
      return Some(found);
    }
  }
  None
}

#[test]
fn parses_timelines_and_keyframes() {
  let css = r#"
    #box {
      scroll-timeline: main block 0% 100%;
      view-timeline: viewy inline;
      animation-timeline: main, viewy;
      animation-range: 20% 80%, entry 0% exit 100%;
      animation-name: fade, move;
    }
    @keyframes fade { from { opacity: 0; } to { opacity: 1; } }
  "#;
  let html = r#"<div id="box"></div>"#;
  let dom = dom::parse_html(html).unwrap();
  let sheet = parse_stylesheet(css).unwrap();
  let styled = apply_styles_with_media(&dom, &sheet, &MediaContext::screen(800.0, 600.0));
  let div = find_by_tag(&styled, "div").expect("div present");
  assert_eq!(div.styles.animation_names.len(), 2);
  assert_eq!(div.styles.animation_names[0], "fade");
  assert_eq!(div.styles.animation_timelines.len(), 2);
  assert_eq!(div.styles.animation_ranges.len(), 2);
  assert_eq!(div.styles.scroll_timelines.len(), 1);
  assert_eq!(div.styles.view_timelines.len(), 1);

  let timeline = &div.styles.scroll_timelines[0];
  assert_eq!(timeline.name.as_deref(), Some("main"));
  assert!(matches!(timeline.start, TimelineOffset::Length(_)));
  assert!(matches!(timeline.end, TimelineOffset::Length(_)));
  let view_tl = &div.styles.view_timelines[0];
  assert_eq!(view_tl.name.as_deref(), Some("viewy"));
  assert_eq!(view_tl.axis, TimelineAxis::Inline);

  let keyframes = sheet.collect_keyframes(&MediaContext::screen(800.0, 600.0));
  assert_eq!(keyframes.len(), 1);
  assert_eq!(keyframes[0].name, "fade");
  assert_eq!(keyframes[0].keyframes.len(), 2);
}

#[test]
fn parses_animation_timeline_functions() {
  let css = r#"
    #box {
      animation-timeline: scroll(self), --foo, auto, none;
    }
  "#;
  let html = r#"<div id="box"></div>"#;
  let dom = dom::parse_html(html).unwrap();
  let sheet = parse_stylesheet(css).unwrap();
  let styled = apply_styles_with_media(&dom, &sheet, &MediaContext::screen(800.0, 600.0));
  let div = find_by_tag(&styled, "div").expect("div present");

  assert_eq!(div.styles.animation_timelines.len(), 4);
  assert_eq!(
    div.styles.animation_timelines[0],
    AnimationTimeline::Scroll(ScrollFunctionTimeline {
      scroller: ScrollTimelineScroller::SelfElement,
      axis: TimelineAxis::Block,
    })
  );
  assert!(matches!(
    div.styles.animation_timelines[1],
    AnimationTimeline::Named(ref name) if name == "--foo"
  ));
  assert!(matches!(
    div.styles.animation_timelines[2],
    AnimationTimeline::Auto
  ));
  assert!(matches!(
    div.styles.animation_timelines[3],
    AnimationTimeline::None
  ));
}

#[test]
fn preserves_single_value_animation_timeline_none() {
  let css = r#"
    #box {
      animation-timeline: none;
    }
  "#;
  let html = r#"<div id="box"></div>"#;
  let dom = dom::parse_html(html).unwrap();
  let sheet = parse_stylesheet(css).unwrap();
  let styled = apply_styles_with_media(&dom, &sheet, &MediaContext::screen(800.0, 600.0));
  let div = find_by_tag(&styled, "div").expect("div present");

  assert_eq!(div.styles.animation_timelines, vec![AnimationTimeline::None]);
}

#[test]
fn parses_animation_range_view_offsets_with_lengths() {
  let css = r#"
    #box {
      animation-range: entry 100px entry 500px;
    }
  "#;
  let html = r#"<div id="box"></div>"#;
  let dom = dom::parse_html(html).unwrap();
  let sheet = parse_stylesheet(css).unwrap();
  let styled = apply_styles_with_media(&dom, &sheet, &MediaContext::screen(800.0, 600.0));
  let div = find_by_tag(&styled, "div").expect("div present");

  assert_eq!(div.styles.animation_ranges.len(), 1);
  let range = &div.styles.animation_ranges[0];
  assert_eq!(
    range.start,
    RangeOffset::View(ViewTimelinePhase::Entry, Length::px(100.0))
  );
  assert_eq!(
    range.end,
    RangeOffset::View(ViewTimelinePhase::Entry, Length::px(500.0))
  );
}

#[test]
fn scroll_timeline_progress_tracks_scroll() {
  let timeline = ScrollTimeline::default();
  let range = AnimationRange::default();
  let progress0 = scroll_timeline_progress(&timeline, 0.0, 200.0, 100.0, &range).unwrap();
  let progress_mid = scroll_timeline_progress(&timeline, 50.0, 200.0, 100.0, &range).unwrap();
  let progress_end = scroll_timeline_progress(&timeline, 200.0, 200.0, 100.0, &range).unwrap();
  assert!((progress0 - 0.0).abs() < 1e-6);
  assert!((progress_mid - 0.25).abs() < 1e-6);
  assert!((progress_end - 1.0).abs() < 1e-6);
}

#[test]
fn scroll_timeline_progress_inactive_when_scroll_range_zero() {
  let timeline = ScrollTimeline::default();
  let range = AnimationRange::default();
  assert_eq!(
    scroll_timeline_progress(&timeline, 0.0, 0.0, 100.0, &range),
    None
  );
}

#[test]
fn view_timeline_progress_respects_entry_and_exit() {
  let timeline = ViewTimeline::default();
  let range = AnimationRange::default();
  let progress_start =
    view_timeline_progress(&timeline, 150.0, 200.0, 100.0, 50.0, &range).unwrap();
  let progress_mid = view_timeline_progress(&timeline, 150.0, 200.0, 100.0, 125.0, &range).unwrap();
  let progress_end = view_timeline_progress(&timeline, 150.0, 200.0, 100.0, 200.0, &range).unwrap();
  assert!((progress_start - 0.0).abs() < 1e-6);
  assert!((progress_mid - 0.5).abs() < 1e-6);
  assert!((progress_end - 1.0).abs() < 1e-6);
}

#[test]
fn view_timeline_progress_supports_entry_length_offsets() {
  let timeline = ViewTimeline::default();
  let range = AnimationRange {
    start: RangeOffset::View(ViewTimelinePhase::Entry, Length::px(100.0)),
    end: RangeOffset::View(ViewTimelinePhase::Entry, Length::px(500.0)),
  };

  let target_start = 150.0;
  let target_end = 200.0;
  let view_size = 100.0;
  let entry = target_start - view_size;

  let progress0 = view_timeline_progress(
    &timeline,
    target_start,
    target_end,
    view_size,
    entry + 100.0,
    &range,
  )
  .unwrap();
  let progress_mid = view_timeline_progress(
    &timeline,
    target_start,
    target_end,
    view_size,
    entry + 300.0,
    &range,
  )
  .unwrap();
  let progress_end = view_timeline_progress(
    &timeline,
    target_start,
    target_end,
    view_size,
    entry + 500.0,
    &range,
  )
  .unwrap();

  assert!((progress0 - 0.0).abs() < 1e-6, "progress0={progress0}");
  assert!(
    (progress_mid - 0.5).abs() < 1e-6,
    "progress_mid={progress_mid}"
  );
  assert!(
    (progress_end - 1.0).abs() < 1e-6,
    "progress_end={progress_end}"
  );
}

#[test]
fn keyframes_sample_interpolates_opacity() {
  let sheet =
    parse_stylesheet("@keyframes fade { 0% { opacity: 0; } 100% { opacity: 1; } }").unwrap();
  let keyframes = sheet.collect_keyframes(&MediaContext::screen(800.0, 600.0));
  let rule = &keyframes[0];
  let sampled = sample_keyframes(
    rule,
    0.25,
    &ComputedStyle::default(),
    Size::new(800.0, 600.0),
    Size::new(100.0, 100.0),
  );
  let opacity = match sampled.get("opacity") {
    Some(AnimatedValue::Opacity(n)) => *n,
    other => panic!("unexpected value {other:?}"),
  };
  assert!((opacity - 0.25).abs() < 1e-6);
}

#[test]
fn keyframes_sample_inserts_implicit_boundaries_for_opacity() {
  let sheet = parse_stylesheet("@keyframes k { 50% { opacity: 0.5; } }").unwrap();
  let keyframes = sheet.collect_keyframes(&MediaContext::screen(800.0, 600.0));
  let rule = &keyframes[0];
  let base = ComputedStyle::default();
  let viewport = Size::new(800.0, 600.0);
  let element_size = Size::new(100.0, 100.0);
  let sample = |progress: f32| -> f32 {
    let sampled = sample_keyframes(rule, progress, &base, viewport, element_size);
    match sampled.get("opacity") {
      Some(AnimatedValue::Opacity(n)) => *n,
      other => panic!("unexpected value {other:?}"),
    }
  };

  assert!((sample(0.25) - 0.75).abs() < 1e-6);
  assert!((sample(0.5) - 0.5).abs() < 1e-6);
  assert!((sample(0.75) - 0.75).abs() < 1e-6);
}

#[test]
fn keyframes_sample_inserts_implicit_boundaries_for_translate() {
  let sheet = parse_stylesheet("@keyframes move { 50% { translate: 100px 0; } }").unwrap();
  let keyframes = sheet.collect_keyframes(&MediaContext::screen(800.0, 600.0));
  let rule = &keyframes[0];
  let base = ComputedStyle::default();
  let viewport = Size::new(800.0, 600.0);
  let element_size = Size::new(100.0, 100.0);

  let sample = |progress: f32| -> TranslateValue {
    let sampled = sample_keyframes(rule, progress, &base, viewport, element_size);
    match sampled.get("translate") {
      Some(AnimatedValue::Translate(v)) => *v,
      other => panic!("unexpected value {other:?}"),
    }
  };

  for (progress, expected_x) in [(0.25, 50.0), (0.5, 100.0), (0.75, 50.0)] {
    match sample(progress) {
      TranslateValue::Values { x, y, z } => {
        assert!((x.to_px() - expected_x).abs() < 1e-3, "progress={progress}");
        assert!((y.to_px() - 0.0).abs() < 1e-3, "progress={progress}");
        assert!((z.to_px() - 0.0).abs() < 1e-3, "progress={progress}");
      }
      TranslateValue::None => panic!("expected translate values"),
    }
  }
}

#[test]
fn keyframes_interpolate_colors_and_currentcolor() {
  let sheet = parse_stylesheet(
    "@keyframes tint { from { background-color: currentColor; } to { background-color: rgb(0, 0, 255); } }",
  )
  .unwrap();
  let keyframes = sheet.collect_keyframes(&MediaContext::screen(800.0, 600.0));
  let rule = &keyframes[0];
  let mut base = ComputedStyle::default();
  base.color = Rgba::new(255, 0, 0, 1.0);
  let sampled = sample_keyframes(
    rule,
    0.5,
    &base,
    Size::new(800.0, 600.0),
    Size::new(200.0, 200.0),
  );
  let color = match sampled.get("background-color") {
    Some(AnimatedValue::Color(c)) => *c,
    other => panic!("unexpected value {other:?}"),
  };
  assert!(color.r > 120 && color.r < 140, "r={}", color.r);
  assert!(color.b > 120 && color.b < 140, "b={}", color.b);
  assert_eq!(color.g, 0);
}

#[test]
fn keyframes_interpolate_transform_lists() {
  let sheet = parse_stylesheet(
    "@keyframes move { from { transform: translateX(0px); } to { transform: translateX(100px); } }",
  )
  .unwrap();
  let keyframes = sheet.collect_keyframes(&MediaContext::screen(800.0, 600.0));
  let rule = &keyframes[0];
  let sampled = sample_keyframes(
    rule,
    0.5,
    &ComputedStyle::default(),
    Size::new(800.0, 600.0),
    Size::new(120.0, 80.0),
  );
  let transform = match sampled.get("transform") {
    Some(AnimatedValue::Transform(t)) => t,
    other => panic!("unexpected value {other:?}"),
  };
  assert_eq!(transform.len(), 1);
  match &transform[0] {
    CssTransform::TranslateX(len) => assert!((len.to_px() - 50.0).abs() < 1e-3),
    other => panic!("unexpected transform {other:?}"),
  }
}

#[test]
fn keyframes_interpolate_filters() {
  let sheet =
    parse_stylesheet("@keyframes blur { from { filter: blur(0px); } to { filter: blur(10px); } }")
      .unwrap();
  let keyframes = sheet.collect_keyframes(&MediaContext::screen(800.0, 600.0));
  let rule = &keyframes[0];
  let sampled = sample_keyframes(
    rule,
    0.5,
    &ComputedStyle::default(),
    Size::new(800.0, 600.0),
    Size::new(100.0, 100.0),
  );
  let filters = match sampled.get("filter") {
    Some(AnimatedValue::Filter(f)) => f,
    other => panic!("unexpected value {other:?}"),
  };
  assert_eq!(filters.len(), 1);
  match &filters[0] {
    FilterFunction::Blur(len) => assert!((len.to_px() - 5.0).abs() < 1e-3),
    other => panic!("unexpected filter {other:?}"),
  }
}

#[test]
fn keyframes_interpolate_box_shadows() {
  let sheet = parse_stylesheet(
    "@keyframes shadow { from { box-shadow: none; } to { box-shadow: 10px 0px 0px 0px rgba(255, 0, 0, 1); } }",
  )
  .unwrap();
  let keyframes = sheet.collect_keyframes(&MediaContext::screen(800.0, 600.0));
  let rule = &keyframes[0];
  let sampled = sample_keyframes(
    rule,
    0.5,
    &ComputedStyle::default(),
    Size::new(800.0, 600.0),
    Size::new(100.0, 100.0),
  );
  let shadows = match sampled.get("box-shadow") {
    Some(AnimatedValue::BoxShadow(shadows)) => shadows,
    other => panic!("unexpected value {other:?}"),
  };
  assert_eq!(shadows.len(), 1);
  let shadow = &shadows[0];
  assert!((shadow.offset_x.to_px() - 5.0).abs() < 1e-3);
  assert!((shadow.offset_y.to_px() - 0.0).abs() < 1e-3);
  assert_eq!(shadow.color.r, 255);
  assert_eq!(shadow.color.g, 0);
  assert_eq!(shadow.color.b, 0);
  assert!((shadow.color.a - 0.5).abs() < 1e-6);
}

#[test]
fn keyframes_interpolate_text_shadows() {
  let sheet = parse_stylesheet(
    "@keyframes shadow { from { text-shadow: none; } to { text-shadow: 10px 0px 0px rgba(255, 0, 0, 1); } }",
  )
  .unwrap();
  let keyframes = sheet.collect_keyframes(&MediaContext::screen(800.0, 600.0));
  let rule = &keyframes[0];
  let sampled = sample_keyframes(
    rule,
    0.5,
    &ComputedStyle::default(),
    Size::new(800.0, 600.0),
    Size::new(100.0, 100.0),
  );
  let shadows = match sampled.get("text-shadow") {
    Some(AnimatedValue::TextShadow(shadows)) => shadows,
    other => panic!("unexpected value {other:?}"),
  };
  assert_eq!(shadows.len(), 1);
  let shadow = &shadows[0];
  assert!((shadow.offset_x.to_px() - 5.0).abs() < 1e-3);
  assert!((shadow.offset_y.to_px() - 0.0).abs() < 1e-3);
  let color = shadow.color.expect("resolved color");
  assert_eq!(color.r, 255);
  assert_eq!(color.g, 0);
  assert_eq!(color.b, 0);
  assert!((color.a - 0.5).abs() < 1e-6);
}

#[test]
fn clip_path_mismatches_fall_back_to_discrete() {
  let sheet = parse_stylesheet(
    "@keyframes mask { from { clip-path: inset(0%); } to { clip-path: circle(50%); } }",
  )
  .unwrap();
  let keyframes = sheet.collect_keyframes(&MediaContext::screen(800.0, 600.0));
  let rule = &keyframes[0];
  let sample_shape = |progress: f32| -> BasicShape {
    let sampled = sample_keyframes(
      rule,
      progress,
      &ComputedStyle::default(),
      Size::new(400.0, 300.0),
      Size::new(100.0, 100.0),
    );
    match sampled.get("clip-path") {
      Some(AnimatedValue::ClipPath(path)) => match path {
        fastrender::style::types::ClipPath::BasicShape(shape, _) => shape.as_ref().clone(),
        other => panic!("unexpected clip-path {other:?}"),
      },
      other => panic!("unexpected clip-path value {other:?}"),
    }
  };

  match sample_shape(0.25) {
    BasicShape::Inset {
      top,
      right,
      bottom,
      left,
      ..
    } => {
      assert_eq!(top.to_px(), 0.0);
      assert_eq!(right.to_px(), 0.0);
      assert_eq!(bottom.to_px(), 0.0);
      assert_eq!(left.to_px(), 0.0);
    }
    other => panic!("expected inset fallback, got {other:?}"),
  }

  match sample_shape(0.5) {
    BasicShape::Circle { .. } => {}
    other => panic!("expected circle fallback, got {other:?}"),
  }
}

#[test]
fn inline_axis_uses_writing_mode_direction() {
  let timeline = ScrollTimeline {
    axis: TimelineAxis::Inline,
    ..ScrollTimeline::default()
  };
  let range = AnimationRange::default();
  let (scroll_pos, scroll_range, view_size) = axis_scroll_state(
    timeline.axis,
    WritingMode::VerticalRl,
    10.0,
    30.0,
    100.0,
    200.0,
    100.0,
    400.0,
  );
  let progress =
    scroll_timeline_progress(&timeline, scroll_pos, scroll_range, view_size, &range).unwrap();
  assert!((scroll_pos - 30.0).abs() < 1e-6);
  assert!((scroll_range - 200.0).abs() < 1e-6);
  assert!(progress > 0.0 && progress < 1.0);
}

#[test]
fn nested_scroll_timelines_progress_independently() {
  let outer = ScrollTimeline {
    axis: TimelineAxis::Block,
    ..ScrollTimeline::default()
  };
  let inner = ScrollTimeline {
    axis: TimelineAxis::Inline,
    ..ScrollTimeline::default()
  };
  let range = AnimationRange::default();

  let (outer_pos, outer_range, outer_size) = axis_scroll_state(
    outer.axis,
    WritingMode::HorizontalTb,
    0.0,
    120.0,
    240.0,
    240.0,
    400.0,
    700.0,
  );
  let (inner_pos, inner_range, inner_size) = axis_scroll_state(
    inner.axis,
    WritingMode::HorizontalTb,
    80.0,
    0.0,
    180.0,
    180.0,
    360.0,
    360.0,
  );

  let outer_progress =
    scroll_timeline_progress(&outer, outer_pos, outer_range, outer_size, &range).unwrap();
  let inner_progress =
    scroll_timeline_progress(&inner, inner_pos, inner_range, inner_size, &range).unwrap();

  assert!(
    (outer_progress - 0.3).abs() < 0.05,
    "outer progress {outer_progress}"
  );
  assert!(
    (inner_progress - 0.44).abs() < 0.05,
    "inner progress {inner_progress}"
  );
  assert!((outer_progress - inner_progress).abs() > 0.05);
}

fn pixel(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let px = pixmap.pixel(x, y).unwrap();
  (px.red(), px.green(), px.blue(), px.alpha())
}

fn red_pixels(pixmap: &tiny_skia::Pixmap) -> usize {
  let mut count = 0usize;
  for y in 0..pixmap.height() {
    for x in 0..pixmap.width() {
      let p = pixmap.pixel(x, y).unwrap();
      if p.red() == 255 && p.green() == 0 && p.blue() == 0 && p.alpha() == 255 {
        count += 1;
      }
    }
  }
  count
}

fn find_box_id_by_dom_id(node: &BoxNode, id: &str) -> Option<usize> {
  if node.debug_info.as_ref().and_then(|info| info.id.as_deref()) == Some(id) {
    return Some(node.id);
  }
  node
    .children
    .iter()
    .find_map(|child| find_box_id_by_dom_id(child, id))
}

fn find_fragment_by_box_id<'a>(tree: &'a FragmentTree, box_id: usize) -> Option<&'a FragmentNode> {
  fn rec<'a>(node: &'a FragmentNode, box_id: usize) -> Option<&'a FragmentNode> {
    if node.box_id() == Some(box_id) {
      return Some(node);
    }
    node.children.iter().find_map(|child| rec(child, box_id))
  }

  rec(&tree.root, box_id).or_else(|| {
    tree
      .additional_fragments
      .iter()
      .find_map(|frag| rec(frag, box_id))
  })
}

#[test]
fn scroll_self_timeline_becomes_inactive_when_element_cannot_scroll() {
  let mut renderer = FastRender::new().expect("renderer");
  let options = RenderOptions::new().with_viewport(100, 100);

  let html_inactive = r#"
    <style>
      html, body { margin: 0; }
      #scroller {
        overflow-y: auto;
        height: 100px;
        width: 100px;
        background: rgb(255, 0, 0);
        animation-timeline: scroll(self);
        animation-name: fade;
      }
      @keyframes fade { from { opacity: 0; } to { opacity: 1; } }
    </style>
    <div id="scroller"><div style="height: 100px;"></div></div>
  "#;

  let prepared = renderer
    .prepare_html(html_inactive, options)
    .expect("prepare inactive");
  let scroller_id =
    find_box_id_by_dom_id(&prepared.box_tree().root, "scroller").expect("scroller box_id");
  let scroll_state = ScrollState::from_parts(
    Point::ZERO,
    HashMap::from([(scroller_id, Point::new(0.0, 0.0))]),
  );
  let pixmap = prepared
    .paint_with_options(
      PreparedPaintOptions::new()
        .with_scroll_state(scroll_state)
        .with_background(Rgba::new(0, 0, 0, 1.0)),
    )
    .expect("paint inactive");

  // With scroll range == 0, scroll(self) should be inactive and the animation should not apply.
  assert_eq!(pixel(&pixmap, 10, 10), (255, 0, 0, 255));
}

#[test]
fn scroll_self_timeline_progress_tracks_element_scroll_offsets() {
  let mut renderer = FastRender::new().expect("renderer");
  let options = RenderOptions::new().with_viewport(100, 100);

  let html = r#"
    <style>
      html, body { margin: 0; }
      #scroller {
        overflow-y: auto;
        height: 100px;
        width: 100px;
        background: rgb(255, 0, 0);
        animation-timeline: scroll(self);
        animation-name: fade;
      }
      @keyframes fade { from { opacity: 0; } to { opacity: 1; } }
    </style>
    <div id="scroller"><div style="height: 200px;"></div></div>
  "#;

  let prepared = renderer.prepare_html(html, options).expect("prepare");
  let scroller_id =
    find_box_id_by_dom_id(&prepared.box_tree().root, "scroller").expect("scroller box_id");
  let scroller_frag =
    find_fragment_by_box_id(prepared.fragment_tree(), scroller_id).expect("scroller fragment");
  let max_scroll =
    (scroller_frag.scroll_overflow.height() - scroller_frag.bounds.height()).max(0.0);
  assert!(
    max_scroll > 0.0,
    "expected non-zero scroll range for scroll(self) test"
  );

  let paint = |scroll_y: f32| {
    let scroll_state = ScrollState::from_parts(
      Point::ZERO,
      HashMap::from([(scroller_id, Point::new(0.0, scroll_y))]),
    );
    prepared.paint_with_options(
      PreparedPaintOptions::new()
        .with_scroll_state(scroll_state)
        .with_background(Rgba::new(0, 0, 0, 1.0)),
    )
  };

  let pixmap_top = paint(0.0).expect("paint at top");
  // With scroll(self) active and at progress 0, opacity is 0 so we should see the background.
  assert_eq!(pixel(&pixmap_top, 10, 10), (0, 0, 0, 255));

  let pixmap_bottom = paint(max_scroll).expect("paint at bottom");
  // With scroll(self) at max scroll, progress should be ~1 so opacity is 1.
  assert_eq!(pixel(&pixmap_bottom, 10, 10), (255, 0, 0, 255));
}

#[test]
fn view_timeline_animation_range_entry_length_offsets_move_pixels() {
  let mut renderer = FastRender::new().expect("renderer");
  let html = r#"
    <style>
      html, body { margin: 0; background: rgb(0, 0, 0); }
      .spacer { height: 100px; }
      #target {
        width: 40px;
        height: 800px;
        background: rgb(255, 0, 0);
        view-timeline: --t block;
        animation-timeline: --t;
        animation-range: entry 100px entry 500px;
        animation-name: slide;
        animation-timing-function: linear;
        animation-fill-mode: both;
      }
      @keyframes slide { from { transform: translateX(0px); } to { transform: translateX(50px); } }
    </style>
    <div class="spacer"></div>
    <div id="target"></div>
    <div class="spacer"></div>
  "#;

  let pixmap_start = renderer
    .render_html_with_scroll(html, 100, 100, 0.0, 100.0)
    .expect("render start");
  assert_eq!(pixel(&pixmap_start, 2, 10), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap_start, 60, 10), (0, 0, 0, 255));

  let pixmap_mid = renderer
    .render_html_with_scroll(html, 100, 100, 0.0, 300.0)
    .expect("render mid");
  assert_eq!(pixel(&pixmap_mid, 22, 10), (0, 0, 0, 255));
  assert_eq!(pixel(&pixmap_mid, 30, 10), (255, 0, 0, 255));

  let pixmap_end = renderer
    .render_html_with_scroll(html, 100, 100, 0.0, 500.0)
    .expect("render end");
  assert_eq!(pixel(&pixmap_end, 30, 10), (0, 0, 0, 255));
  assert_eq!(pixel(&pixmap_end, 60, 10), (255, 0, 0, 255));
}

#[test]
fn view_timeline_fill_mode_none_does_not_apply_before_range_start() {
  let mut renderer = FastRender::new().expect("renderer");
  let html = r#"
    <style>
      html, body { margin: 0; background: rgb(0, 0, 0); }
      .spacer { height: 100px; }
      #target {
        width: 40px;
        height: 800px;
        background: rgb(255, 0, 0);
        view-timeline: --t block;
        animation-timeline: --t;
        animation-range: entry 100px entry 500px;
        animation-name: slide;
        animation-timing-function: linear;
        animation-fill-mode: none;
      }
      @keyframes slide { from { transform: translateX(50px); } to { transform: translateX(100px); } }
    </style>
    <div class="spacer"></div>
    <div id="target"></div>
    <div class="spacer"></div>
  "#;

  // entry == 0 for this setup, so this scroll position is before `entry 100px` but the element is
  // already visible in the viewport.
  let pixmap_before = renderer
    .render_html_with_scroll(html, 100, 100, 0.0, 50.0)
    .expect("render before");
  assert_eq!(pixel(&pixmap_before, 2, 60), (255, 0, 0, 255));

  let pixmap_start = renderer
    .render_html_with_scroll(html, 100, 100, 0.0, 100.0)
    .expect("render at start");
  assert_eq!(pixel(&pixmap_start, 2, 60), (0, 0, 0, 255));
  assert_eq!(pixel(&pixmap_start, 60, 60), (255, 0, 0, 255));
}

#[test]
fn scroll_timeline_drives_animation_during_render() {
  let mut renderer = FastRender::new().expect("renderer");
  let html = r#"
    <style>
      html, body { margin: 0; height: 100%; }
      body { background: black; scroll-timeline: main block; }
      .box { display: block; position: sticky; top: 0; left: 0; width: 100px; height: 100px; background: red; animation-timeline: main; animation-name: fade; }
      @keyframes fade { from { opacity: 0; } to { opacity: 1; } }
    </style>
    <div class="box"></div>
    <div style="height: 300px;"></div>
  "#;

  // Ensure content exceeds the viewport so scroll progress can advance.
  let dom = renderer.parse_html(html).expect("parse html");
  let tree = renderer.layout_document(&dom, 100, 100).expect("layout");
  let content_height = tree.content_size().height();
  assert!(
    content_height > 100.0,
    "content height must exceed viewport: {content_height}"
  );
  let max_scroll = (content_height - tree.viewport_size().height).max(0.0);
  assert!(
    max_scroll > 0.0,
    "expected scrollable range, got {max_scroll}"
  );
  let timeline_check = ScrollTimeline::default();
  let (pos, range, view_size) = axis_scroll_state(
    timeline_check.axis,
    WritingMode::HorizontalTb,
    0.0,
    max_scroll,
    tree.viewport_size().width,
    tree.viewport_size().height,
    tree.content_size().width(),
    tree.content_size().height(),
  );
  let prog = scroll_timeline_progress(
    &timeline_check,
    pos,
    range,
    view_size,
    &AnimationRange::default(),
  )
  .unwrap();
  assert!(
    prog > 0.9,
    "expected near-complete progress ({} / {}) -> {prog}",
    pos,
    range
  );

  let pixmap_top = renderer
    .render_html_with_scroll(html, 100, 100, 0.0, 0.0)
    .expect("render top");
  assert_eq!(pixel(&pixmap_top, 50, 50), (0, 0, 0, 255));
  assert_eq!(
    red_pixels(&pixmap_top),
    0,
    "no red content when progress at start"
  );

  let pixmap_bottom = renderer
    .render_html_with_scroll(html, 100, 100, 0.0, max_scroll)
    .expect("render bottom");
  assert_eq!(pixel(&pixmap_bottom, 50, 50), (255, 0, 0, 255));
  assert!(
    red_pixels(&pixmap_bottom) > 0,
    "red content should appear when fully scrolled"
  );
}

#[test]
fn transform_animation_moves_pixels() {
  let mut renderer = FastRender::new().expect("renderer");
  let html = r#"
    <style>
      html, body { margin: 0; height: 100%; }
      body { background: black; scroll-timeline: main block; }
      .box { display: block; position: sticky; top: 0; left: 0; width: 40px; height: 40px; background: red; animation-timeline: main; animation-name: slide; }
      @keyframes slide { from { transform: translateX(0px); } to { transform: translateX(50px); } }
    </style>
    <div class="box"></div>
    <div style="height: 300px;"></div>
  "#;

  let dom = renderer.parse_html(html).expect("parse html");
  let tree = renderer.layout_document(&dom, 120, 120).expect("layout");
  let max_scroll = (tree.content_size().height() - tree.viewport_size().height).max(0.0);
  let pixmap_top = renderer
    .render_html_with_scroll(html, 120, 120, 0.0, 0.0)
    .expect("render top");
  assert_eq!(pixel(&pixmap_top, 10, 10), (255, 0, 0, 255));

  let pixmap_bottom = renderer
    .render_html_with_scroll(html, 120, 120, 0.0, max_scroll)
    .expect("render bottom");
  assert_eq!(pixel(&pixmap_bottom, 10, 10), (0, 0, 0, 255));
  assert_eq!(pixel(&pixmap_bottom, 60, 10), (255, 0, 0, 255));
}
