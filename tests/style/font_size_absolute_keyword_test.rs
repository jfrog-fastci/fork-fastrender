use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::{apply_styles_with_media, StyledNode};
use fastrender::style::media::MediaContext;

fn find_styled_by_id<'a>(node: &'a StyledNode, id: &str) -> Option<&'a StyledNode> {
  if node
    .node
    .get_attribute_ref("id")
    .is_some_and(|value| value.eq_ignore_ascii_case(id))
  {
    return Some(node);
  }

  node
    .children
    .iter()
    .find_map(|child| find_styled_by_id(child, id))
}

#[test]
fn font_size_absolute_keywords_match_chrome_defaults() {
  let dom = dom::parse_html(
    r#"
      <!doctype html>
      <div id="xx-small">xx-small</div>
      <div id="x-small">x-small</div>
      <div id="small">small</div>
      <div id="medium">medium</div>
      <div id="large">large</div>
      <div id="x-large">x-large</div>
      <div id="xx-large">xx-large</div>
      <div id="xxx-large">xxx-large</div>
    "#,
  )
  .expect("parse html");

  let stylesheet = parse_stylesheet(
    r#"
      #xx-small { font-size: xx-small; }
      #x-small { font-size: x-small; }
      #small { font-size: small; }
      #medium { font-size: medium; }
      #large { font-size: large; }
      #x-large { font-size: x-large; }
      #xx-large { font-size: xx-large; }
      #xxx-large { font-size: xxx-large; }
    "#,
  )
  .expect("stylesheet parses");

  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  let expect = [
    ("xx-small", 9.0),
    ("x-small", 10.0),
    ("small", 13.0),
    ("medium", 16.0),
    ("large", 18.0),
    ("x-large", 24.0),
    ("xx-large", 32.0),
    ("xxx-large", 48.0),
  ];
  for (id, expected) in expect {
    let node = find_styled_by_id(&styled, id).unwrap();
    assert_eq!(
      node.styles.font_size, expected,
      "expected font-size keyword '{id}' to map to {expected}px"
    );
  }
}
