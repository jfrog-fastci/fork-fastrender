use crate::api::FastRender;
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
fn submit_input_specified_size_uses_border_box() {
  // Regression test for nyu.edu: the header search button is an `<input type="submit">` with a
  // fixed (square) size. Chromium treats button-like controls as `box-sizing: border-box`, so the
  // UA padding does not expand the author-specified width/height.
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          .container { width: 45px; }
          input[type="submit"] {
            width: 45px;
            height: 45px;
            min-width: 0;
            min-height: 0;
            border: 0;
            background: rgb(1, 2, 3);
          }
        </style>
      </head>
      <body>
        <div class="container"><input type="submit" value="Go"></div>
      </body>
    </html>
  "#;

  let target_color = Rgba::rgb(1, 2, 3);
  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("parse HTML");
  let fragments = renderer.layout_document(&dom, 200, 120).expect("layout");

  let input_fragment =
    find_fragment_by_background(&fragments.root, target_color).expect("input fragment");

  let eps = 0.5;
  let width = input_fragment.bounds.width();
  let height = input_fragment.bounds.height();
  assert!(
    (width - 45.0).abs() <= eps,
    "expected submit input border box width ~= 45px, got {width:?} in fragment {:?}",
    input_fragment.bounds,
  );
  assert!(
    (height - 45.0).abs() <= eps,
    "expected submit input border box height ~= 45px, got {height:?} in fragment {:?}",
    input_fragment.bounds,
  );
}
