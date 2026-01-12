use fastrender::api::FastRender;
use fastrender::tree::fragment_tree::FragmentNode;
use fastrender::Rgba;

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
fn input_submit_width_uses_border_box() {
  // Chromium's UA stylesheet uses `box-sizing: border-box` for `input[type=submit]`, which makes
  // explicit `width` values apply to the border box (including padding).
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          input {
            width: 66px;
            padding: 0 7px;
            border: 0;
            background: rgb(1, 2, 3);
            font-size: 16px;
          }
        </style>
      </head>
      <body>
        <input type="submit" value="Go">
      </body>
    </html>
  "#;

  let target_color = Rgba::rgb(1, 2, 3);
  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("parse HTML");
  let fragments = renderer.layout_document(&dom, 400, 200).expect("layout");

  let input_fragment =
    find_fragment_by_background(&fragments.root, target_color).expect("input fragment");

  let width = input_fragment.bounds.width();
  assert!(
    (width - 66.0).abs() <= 0.5,
    "expected submit input border box to respect explicit width; got {width:?} in fragment {:?}",
    input_fragment.bounds,
  );
}
