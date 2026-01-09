//! User agent stylesheet tests

#![allow(clippy::len_zero)]

use fastrender::FastRender;
use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::apply_styles_with_media;
use fastrender::style::cascade::StyledNode;
use fastrender::style::media::MediaContext;
use fastrender::style::types::TextAlign;

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

#[test]
fn test_user_agent_styles() {
  let html = r#"
<!DOCTYPE html>
<html>
<head><title>Test</title></head>
<body>
    <h1>Heading 1</h1>
    <h2>Heading 2</h2>
    <p>Paragraph</p>
    <div>Block div</div>
    <span>Inline span</span>
    <strong>Bold text</strong>
    <em>Italic text</em>
</body>
</html>
    "#;

  let mut renderer = FastRender::new().unwrap();
  let result = renderer.render_to_png(html, 800, 600);

  assert!(
    result.is_ok(),
    "Should render successfully with user-agent styles"
  );

  let png = result.unwrap();
  assert!(png.len() > 0, "Should produce non-empty PNG");
}

#[test]
fn test_user_agent_form_elements() {
  let html = r#"
<!DOCTYPE html>
<html>
<body>
    <form>
        <input type="text" value="test">
        <button>Click</button>
        <input type="submit" value="Submit">
    </form>
</body>
</html>
    "#;

  let mut renderer = FastRender::new().unwrap();
  let result = renderer.render_to_png(html, 800, 600);

  assert!(
    result.is_ok(),
    "Should render form elements with user-agent styles"
  );
}

#[test]
fn test_user_agent_margins() {
  let html = r#"
<!DOCTYPE html>
<html>
<body>
    <h1>Test</h1>
</body>
</html>
    "#;

  let mut renderer = FastRender::new().unwrap();
  let result = renderer.render_to_png(html, 800, 600);

  assert!(result.is_ok(), "Should apply user-agent margins correctly");
}

#[test]
fn test_user_agent_input_button_text_align_defaults_to_center() {
  let dom = dom::parse_html(
    r#"
      <input id="submit" type="submit" value="Submit">
      <input id="text" type="text" value="Hello">
    "#,
  )
  .unwrap();
  let stylesheet = parse_stylesheet("").unwrap();

  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));
  let submit = find_by_id(&styled, "submit").expect("submit input");
  let text = find_by_id(&styled, "text").expect("text input");

  assert_eq!(submit.styles.text_align, TextAlign::Center);
  assert_eq!(text.styles.text_align, TextAlign::Start);
}
