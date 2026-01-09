use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::apply_styles_with_media;
use fastrender::style::cascade::StyledNode;
use fastrender::style::color::Rgba;
use fastrender::style::media::MediaContext;

fn find_by_id<'a>(node: &'a StyledNode, id: &str) -> Option<&'a StyledNode> {
  if node
    .node
    .get_attribute_ref("id")
    .map(|value| value == id)
    .unwrap_or(false)
  {
    return Some(node);
  }
  for child in node.children.iter() {
    if let Some(found) = find_by_id(child, id) {
      return Some(found);
    }
  }
  None
}

fn render_color(css: &str) -> Rgba {
  let dom = dom::parse_html(r#"<div id="t"></div>"#).unwrap();
  let stylesheet = parse_stylesheet(css).unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));
  find_by_id(&styled, "t").expect("#t").styles.color
}

#[test]
fn relative_rgb_identity_matches_origin_color() {
  let css = r#"#t { color: rgb(from red r g b); }"#;
  assert_eq!(render_color(css), Rgba::RED);
}

#[test]
fn relative_rgb_allows_channel_swizzling_and_calc() {
  // Start from rgb(10 20 30), then output:
  // - red: b (30)
  // - green: calc(g / 2) (10)
  // - blue: r (10)
  let css = r#"#t { color: rgb(from rgb(10 20 30) b calc(g / 2) r); }"#;
  assert_eq!(render_color(css), Rgba::rgb(30, 10, 10));
}

#[test]
fn supports_query_accepts_relative_rgb() {
  let css = r#"
    #t { color: rgb(10 20 30); }
    @supports (color: rgb(from red r g b)) {
      #t { color: rgb(1 2 3); }
    }
  "#;
  assert_eq!(render_color(css), Rgba::rgb(1, 2, 3));
}

