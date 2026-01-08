use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::apply_styles_with_media;
use fastrender::style::cascade::StyledNode;
use fastrender::style::media::MediaContext;
use fastrender::style::types::RangeOffset;

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
fn animation_range_ignores_invalid_comma_list() {
  let css = r#"
    #box {
      animation-range: 20% 80%;
      animation-range: entry 0% exit 100%, inherit;
    }
  "#;
  let html = r#"<div id="box"></div>"#;
  let dom = dom::parse_html(html).unwrap();
  let sheet = parse_stylesheet(css).unwrap();
  let styled = apply_styles_with_media(&dom, &sheet, &MediaContext::screen(800.0, 600.0));
  let div = find_by_tag(&styled, "div").expect("div present");

  assert_eq!(div.styles.animation_ranges.len(), 1);
  assert_eq!(div.styles.animation_ranges[0].start, RangeOffset::Progress(0.2));
  assert_eq!(div.styles.animation_ranges[0].end, RangeOffset::Progress(0.8));
}

#[test]
fn animation_range_start_ignores_invalid_comma_list() {
  let css = r#"
    #box {
      animation-range: 20% 80%;
      animation-range-start: entry 0%, inherit;
    }
  "#;
  let html = r#"<div id="box"></div>"#;
  let dom = dom::parse_html(html).unwrap();
  let sheet = parse_stylesheet(css).unwrap();
  let styled = apply_styles_with_media(&dom, &sheet, &MediaContext::screen(800.0, 600.0));
  let div = find_by_tag(&styled, "div").expect("div present");

  assert_eq!(div.styles.animation_ranges.len(), 1);
  assert_eq!(div.styles.animation_ranges[0].start, RangeOffset::Progress(0.2));
  assert_eq!(div.styles.animation_ranges[0].end, RangeOffset::Progress(0.8));
}

#[test]
fn animation_range_end_ignores_invalid_comma_list() {
  let css = r#"
    #box {
      animation-range: 20% 80%;
      animation-range-end: exit 100%, revert;
    }
  "#;
  let html = r#"<div id="box"></div>"#;
  let dom = dom::parse_html(html).unwrap();
  let sheet = parse_stylesheet(css).unwrap();
  let styled = apply_styles_with_media(&dom, &sheet, &MediaContext::screen(800.0, 600.0));
  let div = find_by_tag(&styled, "div").expect("div present");

  assert_eq!(div.styles.animation_ranges.len(), 1);
  assert_eq!(div.styles.animation_ranges[0].start, RangeOffset::Progress(0.2));
  assert_eq!(div.styles.animation_ranges[0].end, RangeOffset::Progress(0.8));
}
