use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::apply_styles_with_media;
use fastrender::style::cascade::StyledNode;
use fastrender::style::media::MediaContext;
use fastrender::style::types::TimelineAxis;

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
fn scroll_timeline_name_none_clears_list() {
  let css = r#"
    #box { scroll-timeline-name: none; }
  "#;
  let html = r#"<div id="box"></div>"#;
  let dom = dom::parse_html(html).unwrap();
  let sheet = parse_stylesheet(css).unwrap();
  let styled = apply_styles_with_media(&dom, &sheet, &MediaContext::screen(800.0, 600.0));
  let div = find_by_tag(&styled, "div").expect("div present");
  assert_eq!(div.styles.scroll_timelines.len(), 0);
}

#[test]
fn scroll_timeline_name_none_clears_existing_names() {
  let css = r#"
    #box {
      scroll-timeline: main inline;
      scroll-timeline-name: none;
    }
  "#;
  let html = r#"<div id="box"></div>"#;
  let dom = dom::parse_html(html).unwrap();
  let sheet = parse_stylesheet(css).unwrap();
  let styled = apply_styles_with_media(&dom, &sheet, &MediaContext::screen(800.0, 600.0));
  let div = find_by_tag(&styled, "div").expect("div present");
  assert_eq!(div.styles.scroll_timelines.len(), 1);
  assert_eq!(div.styles.scroll_timelines[0].name, None);
  assert_eq!(div.styles.scroll_timelines[0].axis, TimelineAxis::Inline);
}

#[test]
fn view_timeline_name_none_clears_list() {
  let css = r#"
    #box { view-timeline-name: none; }
  "#;
  let html = r#"<div id="box"></div>"#;
  let dom = dom::parse_html(html).unwrap();
  let sheet = parse_stylesheet(css).unwrap();
  let styled = apply_styles_with_media(&dom, &sheet, &MediaContext::screen(800.0, 600.0));
  let div = find_by_tag(&styled, "div").expect("div present");
  assert_eq!(div.styles.view_timelines.len(), 0);
}

#[test]
fn view_timeline_name_none_clears_existing_names() {
  let css = r#"
    #box {
      view-timeline: viewy inline;
      view-timeline-name: none;
    }
  "#;
  let html = r#"<div id="box"></div>"#;
  let dom = dom::parse_html(html).unwrap();
  let sheet = parse_stylesheet(css).unwrap();
  let styled = apply_styles_with_media(&dom, &sheet, &MediaContext::screen(800.0, 600.0));
  let div = find_by_tag(&styled, "div").expect("div present");
  assert_eq!(div.styles.view_timelines.len(), 1);
  assert_eq!(div.styles.view_timelines[0].name, None);
  assert_eq!(div.styles.view_timelines[0].axis, TimelineAxis::Inline);
}

#[test]
fn scroll_timeline_name_rejects_css_wide_keywords_in_lists() {
  let css = r#"
    #box {
      scroll-timeline: main inline;
      scroll-timeline-name: inherit, foo;
    }
  "#;
  let html = r#"<div id="box"></div>"#;
  let dom = dom::parse_html(html).unwrap();
  let sheet = parse_stylesheet(css).unwrap();
  let styled = apply_styles_with_media(&dom, &sheet, &MediaContext::screen(800.0, 600.0));
  let div = find_by_tag(&styled, "div").expect("div present");
  assert_eq!(div.styles.scroll_timelines.len(), 1);
  assert_eq!(div.styles.scroll_timelines[0].name.as_deref(), Some("main"));
  assert_eq!(div.styles.scroll_timelines[0].axis, TimelineAxis::Inline);
}

#[test]
fn view_timeline_name_rejects_css_wide_keywords_in_lists() {
  let css = r#"
    #box {
      view-timeline: viewy inline;
      view-timeline-name: revert, foo;
    }
  "#;
  let html = r#"<div id="box"></div>"#;
  let dom = dom::parse_html(html).unwrap();
  let sheet = parse_stylesheet(css).unwrap();
  let styled = apply_styles_with_media(&dom, &sheet, &MediaContext::screen(800.0, 600.0));
  let div = find_by_tag(&styled, "div").expect("div present");
  assert_eq!(div.styles.view_timelines.len(), 1);
  assert_eq!(div.styles.view_timelines[0].name.as_deref(), Some("viewy"));
  assert_eq!(div.styles.view_timelines[0].axis, TimelineAxis::Inline);
}

#[test]
fn scroll_timeline_rejects_css_wide_keywords_in_lists() {
  let css = r#"
    #box {
      scroll-timeline: main inline 0% 100%;
      scroll-timeline: main inline 0% 100%, inherit inline;
    }
  "#;
  let html = r#"<div id="box"></div>"#;
  let dom = dom::parse_html(html).unwrap();
  let sheet = parse_stylesheet(css).unwrap();
  let styled = apply_styles_with_media(&dom, &sheet, &MediaContext::screen(800.0, 600.0));
  let div = find_by_tag(&styled, "div").expect("div present");
  assert_eq!(div.styles.scroll_timelines.len(), 1);
  assert_eq!(div.styles.scroll_timelines[0].name.as_deref(), Some("main"));
  assert_eq!(div.styles.scroll_timelines[0].axis, TimelineAxis::Inline);
}

#[test]
fn view_timeline_rejects_css_wide_keywords_in_lists() {
  let css = r#"
    #box {
      view-timeline: viewy inline;
      view-timeline: viewy inline, inherit inline;
    }
  "#;
  let html = r#"<div id="box"></div>"#;
  let dom = dom::parse_html(html).unwrap();
  let sheet = parse_stylesheet(css).unwrap();
  let styled = apply_styles_with_media(&dom, &sheet, &MediaContext::screen(800.0, 600.0));
  let div = find_by_tag(&styled, "div").expect("div present");
  assert_eq!(div.styles.view_timelines.len(), 1);
  assert_eq!(div.styles.view_timelines[0].name.as_deref(), Some("viewy"));
  assert_eq!(div.styles.view_timelines[0].axis, TimelineAxis::Inline);
}
