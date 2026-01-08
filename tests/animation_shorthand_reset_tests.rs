use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::apply_styles_with_media;
use fastrender::style::cascade::StyledNode;
use fastrender::style::media::MediaContext;

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

