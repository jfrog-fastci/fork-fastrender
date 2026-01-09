use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::apply_styles_with_media;
use fastrender::style::cascade::StyledNode;
use fastrender::style::media::MediaContext;
use fastrender::Rgba;

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
fn input_invalid_changes_computed_style_for_type_mismatch() {
  let html = r#"
    <input id="bad" type="email" value="not-an-email">
    <input id="ok" type="email" value="a@b.com">
  "#;
  let css = r#"
    input { color: rgb(0 0 255); }
    input:invalid { color: rgb(255 0 0); }
  "#;
  let dom = dom::parse_html(html).expect("parse html");
  let stylesheet = parse_stylesheet(css).expect("parse stylesheet");
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  assert_eq!(
    find_by_id(&styled, "bad").expect("bad email").styles.color,
    Rgba::rgb(255, 0, 0)
  );
  assert_eq!(
    find_by_id(&styled, "ok").expect("ok email").styles.color,
    Rgba::rgb(0, 0, 255)
  );
}

#[test]
fn in_range_and_out_of_range_match_number_min_max() {
  let html = r#"
    <input id="oor" type="number" value="15" min="1" max="10">
    <input id="ir" type="number" value="5" min="1" max="10">
  "#;
  let css = r#"
    input { color: rgb(0 0 255); }
    input:out-of-range { color: rgb(255 0 0); }
    input:in-range { color: rgb(0 255 0); }
  "#;
  let dom = dom::parse_html(html).expect("parse html");
  let stylesheet = parse_stylesheet(css).expect("parse stylesheet");
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  assert_eq!(
    find_by_id(&styled, "oor").expect("out of range").styles.color,
    Rgba::rgb(255, 0, 0)
  );
  assert_eq!(
    find_by_id(&styled, "ir").expect("in range").styles.color,
    Rgba::rgb(0, 255, 0)
  );
}

#[test]
fn placeholder_shown_matches_empty_placeholder_controls() {
  let html = r#"
    <input id="empty" placeholder="Email" required>
    <input id="filled" placeholder="Email" required value="x">
  "#;
  let css = r#"
    input { color: rgb(0 0 255); }
    input:placeholder-shown { color: rgb(255 0 0); }
  "#;
  let dom = dom::parse_html(html).expect("parse html");
  let stylesheet = parse_stylesheet(css).expect("parse stylesheet");
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  assert_eq!(
    find_by_id(&styled, "empty").expect("empty placeholder").styles.color,
    Rgba::rgb(255, 0, 0)
  );
  assert_eq!(
    find_by_id(&styled, "filled").expect("filled placeholder").styles.color,
    Rgba::rgb(0, 0, 255)
  );
}

