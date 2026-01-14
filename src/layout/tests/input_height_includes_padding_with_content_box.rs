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
fn input_height_includes_padding_with_content_box_box_sizing() {
  // Facebook's login form relies on the UA default box-sizing for inputs being `content-box`, so a
  // specified `height` applies to the content box and the padding/border expand the border box.
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          input {
            -webkit-appearance: none;
            appearance: none;
            width: 200px;
            height: 22px;
            padding: 14px 16px;
            border: 1px solid black;
            background: rgb(1, 2, 3);
          }
        </style>
      </head>
      <body>
        <div><input value=""></div>
      </body>
    </html>
  "#;

  let target_color = Rgba::rgb(1, 2, 3);
  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("parse HTML");
  let fragments = renderer.layout_document(&dom, 240, 180).expect("layout");

  let input_fragment =
    find_fragment_by_background(&fragments.root, target_color).expect("input fragment");

  // Content width (200px) + padding (16px * 2) + border (1px * 2) = 234px border box.
  let width = input_fragment.bounds.width();
  assert!(
    (width - 234.0).abs() <= 0.5,
    "expected input border box width to include padding/border; got {width:?} in fragment {:?}",
    input_fragment.bounds,
  );

  // Content height (22px) + padding (14px * 2) + border (1px * 2) = 52px border box.
  let height = input_fragment.bounds.height();
  assert!(
    (height - 52.0).abs() <= 0.5,
    "expected input border box height to include padding/border; got {height:?} in fragment {:?}",
    input_fragment.bounds,
  );
}

#[test]
fn input_height_includes_padding_with_border_box_when_appearance_none() {
  // Authors can still opt into `box-sizing: border-box` even when `appearance: none` disables the
  // native form-control replacement. Chromium clamps the border-box size to at least the
  // padding+border sum when the specified `height` is smaller.
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          input {
            -webkit-appearance: none;
            appearance: none;
            box-sizing: border-box;
            height: 22px;
            padding: 14px 16px;
            border: 1px solid black;
            background: rgb(1, 2, 3);
          }
        </style>
      </head>
      <body>
        <div><input value=""></div>
      </body>
    </html>
  "#;

  let target_color = Rgba::rgb(1, 2, 3);
  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("parse HTML");
  let fragments = renderer.layout_document(&dom, 240, 180).expect("layout");

  let input_fragment =
    find_fragment_by_background(&fragments.root, target_color).expect("input fragment");

  // Specified height (22px) is smaller than the vertical padding+border sum (14*2 + 1*2 = 30px),
  // so the border box clamps to 30px.
  let height = input_fragment.bounds.height();
  assert!(
    (height - 30.0).abs() <= 0.5,
    "expected border-box sized input height to clamp to padding+border; got {height:?} in fragment {:?}",
    input_fragment.bounds,
  );
}
