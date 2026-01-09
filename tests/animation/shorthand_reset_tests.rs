use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::apply_styles_with_media;
use fastrender::style::cascade::StyledNode;
use fastrender::style::media::MediaContext;
use fastrender::style::types::{AnimationTimeline, RangeOffset, ViewTimelinePhase};
use fastrender::Length;

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

fn find_by_id<'a>(node: &'a StyledNode, id: &str) -> Option<&'a StyledNode> {
  if node.node.get_attribute_ref("id") == Some(id) {
    return Some(node);
  }
  for child in node.children.iter() {
    if let Some(found) = find_by_id(child, id) {
      return Some(found);
    }
  }
  None
}

#[test]
fn animation_shorthand_resets_timeline_and_range() {
  let css = r#"
    #box {
      animation-timeline: foo;
      animation-range: entry 0% exit 100%;
      animation: fade 1s linear;
    }
  "#;
  let html = r#"<div id="box"></div>"#;
  let dom = dom::parse_html(html).unwrap();
  let sheet = parse_stylesheet(css).unwrap();
  let styled = apply_styles_with_media(&dom, &sheet, &MediaContext::screen(800.0, 600.0));
  let div = find_by_tag(&styled, "div").expect("div present");

  assert_eq!(div.styles.animation_names.len(), 1);
  assert_eq!(div.styles.animation_names[0].as_deref(), Some("fade"));
  assert!(div.styles.animation_timelines.is_empty());
  assert!(div.styles.animation_ranges.is_empty());
}

#[test]
fn invalid_animation_shorthand_does_not_reset_timeline_or_range() {
  let css = r#"
    #box {
      animation-timeline: foo;
      animation-range: entry 0% exit 100%;
      animation: fade 1s linear,;
    }
  "#;
  let html = r#"<div id="box"></div>"#;
  let dom = dom::parse_html(html).unwrap();
  let sheet = parse_stylesheet(css).unwrap();
  let styled = apply_styles_with_media(&dom, &sheet, &MediaContext::screen(800.0, 600.0));
  let div = find_by_tag(&styled, "div").expect("div present");

  assert_eq!(div.styles.animation_names.len(), 0);
  assert_eq!(div.styles.animation_timelines.len(), 1);
  assert_eq!(div.styles.animation_ranges.len(), 1);
}

#[test]
fn animation_shorthand_initial_resets_timeline_and_range() {
  let css = r#"
    #box {
      animation-timeline: foo;
      animation-range: entry 0% exit 100%;
      animation: initial;
    }
  "#;
  let html = r#"<div id="box"></div>"#;
  let dom = dom::parse_html(html).unwrap();
  let sheet = parse_stylesheet(css).unwrap();
  let styled = apply_styles_with_media(&dom, &sheet, &MediaContext::screen(800.0, 600.0));
  let div = find_by_tag(&styled, "div").expect("div present");

  assert_eq!(div.styles.animation_names.len(), 0);
  assert!(div.styles.animation_timelines.is_empty());
  assert!(div.styles.animation_ranges.is_empty());
}

#[test]
fn animation_shorthand_inherit_includes_timeline_and_range() {
  let css = r#"
    #parent {
      animation-timeline: foo;
      animation-range: entry 0% exit 100%;
    }
    #child {
      animation: inherit;
    }
  "#;
  let html = r#"<div id="parent"><div id="child"></div></div>"#;
  let dom = dom::parse_html(html).unwrap();
  let sheet = parse_stylesheet(css).unwrap();
  let styled = apply_styles_with_media(&dom, &sheet, &MediaContext::screen(800.0, 600.0));
  let child = find_by_id(&styled, "child").expect("child present");

  assert_eq!(child.styles.animation_timelines.len(), 1);
  assert_eq!(
    child.styles.animation_timelines[0],
    AnimationTimeline::Named("foo".to_string())
  );
  assert_eq!(child.styles.animation_ranges.len(), 1);
  assert_eq!(
    child.styles.animation_ranges[0].start,
    RangeOffset::View(ViewTimelinePhase::Entry, Length::percent(0.0))
  );
  assert_eq!(
    child.styles.animation_ranges[0].end,
    RangeOffset::View(ViewTimelinePhase::Exit, Length::percent(100.0))
  );
}
