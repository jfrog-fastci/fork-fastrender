use fastrender::animation::scroll_timeline_progress;
use fastrender::css::types::{Declaration, PropertyValue};
use fastrender::style::properties::{apply_declaration_with_base, DEFAULT_VIEWPORT};
use fastrender::style::types::{AnimationRange, RangeOffset, ScrollTimeline};
use fastrender::style::ComputedStyle;
use fastrender::Length;

#[test]
fn animation_range_parses_length_offsets() {
  let mut styles = ComputedStyle::default();
  let parent = ComputedStyle::default();
  let decl = Declaration {
    property: "animation-range".into(),
    value: PropertyValue::Keyword("200px 300px".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut styles,
    &decl,
    &parent,
    &ComputedStyle::default(),
    None,
    parent.font_size,
    parent.root_font_size,
    DEFAULT_VIEWPORT,
    false,
  );

  assert_eq!(
    styles.animation_ranges,
    vec![AnimationRange {
      start: RangeOffset::Length(Length::px(200.0)),
      end: RangeOffset::Length(Length::px(300.0)),
    }]
  );
}

#[test]
fn scroll_timeline_progress_respects_length_animation_range() {
  let timeline = ScrollTimeline::default();
  let range = AnimationRange {
    start: RangeOffset::Length(Length::px(200.0)),
    end: RangeOffset::Length(Length::px(300.0)),
  };

  let progress = scroll_timeline_progress(&timeline, 250.0, 1000.0, 100.0, &range).unwrap();
  assert!((progress - 0.5).abs() < 1e-6, "progress={progress}");
}

