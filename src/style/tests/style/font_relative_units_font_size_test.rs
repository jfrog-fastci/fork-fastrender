use fastrender::css::parser::parse_stylesheet;
use fastrender::dom::parse_html;
use fastrender::style::cascade::apply_style_set_with_media_target_and_imports;
use fastrender::style::media::MediaContext;
use fastrender::style::style_set::StyleSet;
use fastrender::style::types::LineHeight;
use fastrender::LengthUnit;
use std::collections::HashMap;

fn find_by_id<'a>(
  node: &'a fastrender::style::cascade::StyledNode,
  id: &str,
) -> Option<&'a fastrender::style::cascade::StyledNode> {
  if node
    .node
    .get_attribute_ref("id")
    .is_some_and(|value| value.eq_ignore_ascii_case(id))
  {
    return Some(node);
  }
  node.children.iter().find_map(|child| find_by_id(child, id))
}

#[test]
fn font_relative_units_in_font_size_and_line_height_resolve() {
  let html = r#"
    <!doctype html>
    <html>
      <body>
        <div id="parent">
          <div id="child"></div>
        </div>
      </body>
    </html>
  "#;

  let css = r#"
    #child {
      font-size: 2cap;
      line-height: 1cap;
    }
  "#;

  let dom = parse_html(html).expect("parsed html");
  let stylesheet = parse_stylesheet(css).expect("stylesheet");
  let style_set = StyleSet {
    document: stylesheet,
    shadows: HashMap::new(),
  };
  let media = MediaContext::screen(800.0, 600.0);
  let styled = apply_style_set_with_media_target_and_imports(
    &dom, &style_set, &media, None, None, None, None, None, None,
  );

  let parent = find_by_id(&styled, "parent").expect("parent element");
  assert!(
    (parent.styles.font_size - 16.0).abs() < 1e-6,
    "expected default parent font-size 16px, got {}",
    parent.styles.font_size
  );

  let child = find_by_id(&styled, "child").expect("child element");

  // cap ~= 0.7em fallback; font-size resolves against the parent font size for cycle-breaking.
  let expected_font_size = 22.4;
  assert!(
    (child.styles.font_size - expected_font_size).abs() < 1e-3,
    "expected child font-size ~{expected_font_size}px, got {}",
    child.styles.font_size
  );

  let expected_line_height = expected_font_size * 0.7;
  match &child.styles.line_height {
    LineHeight::Length(len) => {
      assert_eq!(
        len.unit,
        LengthUnit::Px,
        "expected line-height to resolve to px"
      );
      assert!(
        (len.value - expected_line_height).abs() < 1e-3,
        "expected line-height ~{expected_line_height}px, got {}",
        len.value
      );
    }
    other => panic!("expected line-height length, got {other:?}"),
  }
}
