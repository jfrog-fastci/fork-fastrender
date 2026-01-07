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
fn font_variation_settings_supports_calc_values() {
  let css = r#"div { font-variation-settings: "wght" calc(400 + 200), "wdth" calc(50 * 2); }"#;
  let html = "<div></div>";
  let dom = dom::parse_html(html).expect("parse html");
  let sheet = parse_stylesheet(css).expect("parse css");
  let styled = apply_styles_with_media(&dom, &sheet, &MediaContext::screen(800.0, 600.0));
  let div = find_by_tag(&styled, "div").expect("div present");

  assert_eq!(div.styles.font_variation_settings.len(), 2);
  assert_eq!(div.styles.font_variation_settings[0].tag, *b"wght");
  assert!((div.styles.font_variation_settings[0].value - 600.0).abs() < 0.001);
  assert_eq!(div.styles.font_variation_settings[1].tag, *b"wdth");
  assert!((div.styles.font_variation_settings[1].value - 100.0).abs() < 0.001);
}

