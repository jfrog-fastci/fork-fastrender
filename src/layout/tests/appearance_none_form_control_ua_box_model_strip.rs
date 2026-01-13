use crate::api::FastRender;
use crate::style::values::Length;
use crate::tree::fragment_tree::FragmentNode;
use crate::Rgba;

fn find_fragment_by_background<'a>(
  node: &'a FragmentNode,
  color: Rgba,
) -> Option<&'a FragmentNode> {
  if node
    .style
    .as_ref()
    .is_some_and(|style| style.background_color == color)
  {
    return Some(node);
  }
  for child in node.children.iter() {
    if let Some(found) = find_fragment_by_background(child, color) {
      return Some(found);
    }
  }
  None
}

#[test]
fn appearance_none_checkbox_size_not_inflated_by_ua_border_or_padding() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          input {
            appearance: none;
            display: block;
            width: 40px;
            height: 20px;
            border-radius: 10px;
            background-color: rgb(1, 2, 3);
          }
        </style>
      </head>
      <body>
        <input type="checkbox">
      </body>
    </html>
  "#;

  let target_color = Rgba::rgb(1, 2, 3);
  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("parse HTML");
  let fragments = renderer.layout_document(&dom, 200, 200).expect("layout");

  let input_fragment =
    find_fragment_by_background(&fragments.root, target_color).expect("input fragment");

  let width = input_fragment.bounds.width();
  assert!(
    (width - 40.0).abs() <= 0.5,
    "expected appearance:none checkbox border box to match explicit width; got width={width} bounds={:?}",
    input_fragment.bounds,
  );

  let height = input_fragment.bounds.height();
  assert!(
    (height - 20.0).abs() <= 0.5,
    "expected appearance:none checkbox border box to match explicit height; got height={height} bounds={:?}",
    input_fragment.bounds,
  );

  let style = input_fragment.style.as_ref().expect("fragment style");
  assert_eq!(style.used_border_top_width(), Length::px(0.0));
  assert_eq!(style.used_border_right_width(), Length::px(0.0));
  assert_eq!(style.used_border_bottom_width(), Length::px(0.0));
  assert_eq!(style.used_border_left_width(), Length::px(0.0));
}

