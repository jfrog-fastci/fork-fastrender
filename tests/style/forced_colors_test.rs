use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::apply_styles_with_media_target_and_imports;
use fastrender::style::cascade::StyledNode;
use fastrender::style::color::Rgba;
use fastrender::style::color::SystemColor;
use fastrender::style::media::ColorScheme;
use fastrender::style::media::MediaContext;
use fastrender::style::types::OutlineColor;

fn find_by_id<'a>(node: &'a StyledNode, id: &str) -> Option<&'a StyledNode> {
  if node
    .node
    .get_attribute_ref("id")
    .is_some_and(|value| value.eq_ignore_ascii_case(id))
  {
    return Some(node);
  }
  node.children.iter().find_map(|child| find_by_id(child, id))
}

fn styled_with_forced_colors(css: &str, forced: bool) -> StyledNode {
  let html = r#"<div id="t">text</div>"#;
  let dom = dom::parse_html(html).expect("parse html");
  let stylesheet = parse_stylesheet(css).expect("parse stylesheet");
  let media = MediaContext::screen(800.0, 600.0)
    .with_color_scheme(ColorScheme::Light)
    .with_forced_colors(forced);
  apply_styles_with_media_target_and_imports(
    &dom, &stylesheet, &media, None, None, None, None, None, None,
  )
}

#[test]
fn forced_colors_media_query_toggles_rules() {
  let css = r#"
    #t { font-size: 10px; }
    @media (forced-colors: active) {
      #t { font-size: 20px; }
    }
  "#;

  let unforced = styled_with_forced_colors(css, false);
  let forced = styled_with_forced_colors(css, true);

  assert_eq!(
    find_by_id(&unforced, "t").expect("node").styles.font_size,
    10.0
  );
  assert_eq!(
    find_by_id(&forced, "t").expect("node").styles.font_size,
    20.0
  );
}

#[test]
fn system_colors_use_forced_palette_when_forced_colors_active() {
  let css = r#"
    #t {
      forced-color-adjust: none;
      color: CanvasText;
      background-color: Canvas;
    }
  "#;

  let unforced = styled_with_forced_colors(css, false);
  let forced = styled_with_forced_colors(css, true);

  let expected_unforced_text = SystemColor::CanvasText.to_rgba(false, false);
  let expected_unforced_bg = SystemColor::Canvas.to_rgba(false, false);
  let expected_forced_text = SystemColor::CanvasText.to_rgba(false, true);
  let expected_forced_bg = SystemColor::Canvas.to_rgba(false, true);

  let unforced_node = find_by_id(&unforced, "t").expect("node");
  assert_eq!(unforced_node.styles.color, expected_unforced_text);
  assert_eq!(unforced_node.styles.background_color, expected_unforced_bg);

  let forced_node = find_by_id(&forced, "t").expect("node");
  assert_eq!(forced_node.styles.color, expected_forced_text);
  assert_eq!(forced_node.styles.background_color, expected_forced_bg);
}

#[test]
fn forced_color_adjust_none_preserves_authored_non_system_colors() {
  let css = r#"
    #t {
      forced-color-adjust: none;
      color: rgb(10, 20, 30);
      background-color: rgb(40, 50, 60);
      border: 1px solid rgb(70, 80, 90);
      outline: 2px solid rgb(100, 110, 120);
    }
  "#;

  let styled = styled_with_forced_colors(css, true);
  let node = find_by_id(&styled, "t").expect("node");

  assert_eq!(node.styles.color, Rgba::rgb(10, 20, 30));
  assert_eq!(node.styles.background_color, Rgba::rgb(40, 50, 60));
  assert_eq!(node.styles.border_top_color, Rgba::rgb(70, 80, 90));
  assert_eq!(node.styles.border_right_color, Rgba::rgb(70, 80, 90));
  assert_eq!(node.styles.border_bottom_color, Rgba::rgb(70, 80, 90));
  assert_eq!(node.styles.border_left_color, Rgba::rgb(70, 80, 90));

  assert_eq!(
    node.styles.outline_color,
    OutlineColor::Color(Rgba::rgb(100, 110, 120))
  );
}

#[test]
fn forced_color_adjust_auto_overrides_authored_colors() {
  let css = r#"
    #t {
      color: rgb(10, 20, 30);
      background-color: rgb(40, 50, 60);
      border: 1px solid rgb(70, 80, 90);
      outline: 2px solid rgb(100, 110, 120);
    }
  "#;

  let styled = styled_with_forced_colors(css, true);
  let node = find_by_id(&styled, "t").expect("node");

  let canvas = SystemColor::Canvas.to_rgba(false, true);
  let canvas_text = SystemColor::CanvasText.to_rgba(false, true);
  let button_border = SystemColor::ButtonBorder.to_rgba(false, true);
  let highlight = SystemColor::Highlight.to_rgba(false, true);

  assert_eq!(node.styles.color, canvas_text);
  assert_eq!(node.styles.background_color, canvas);
  assert_eq!(node.styles.border_top_color, button_border);
  assert_eq!(node.styles.border_right_color, button_border);
  assert_eq!(node.styles.border_bottom_color, button_border);
  assert_eq!(node.styles.border_left_color, button_border);
  assert_eq!(node.styles.outline_color, OutlineColor::Color(highlight));
}

#[test]
fn system_color_text_is_not_overridden_by_forced_colors_policy() {
  let css = r#"
    #t { color: LinkText; }
  "#;

  let styled = styled_with_forced_colors(css, true);
  let node = find_by_id(&styled, "t").expect("node");
  assert_eq!(node.styles.color, SystemColor::LinkText.to_rgba(false, true));
}
