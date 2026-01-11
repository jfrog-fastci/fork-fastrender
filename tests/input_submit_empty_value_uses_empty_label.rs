use fastrender::api::FastRender;
use fastrender::tree::fragment_tree::FragmentNode;
use fastrender::Rgba;

fn find_fragment_by_background<'a>(node: &'a FragmentNode, color: Rgba) -> Option<&'a FragmentNode> {
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
fn input_submit_empty_value_uses_empty_label() {
  // HTML: when the `value` attribute is present on `input type=submit`, it is the label even when
  // it is the empty string. This matters because form control intrinsic sizing uses the label, and
  // the flexbox automatic minimum size clamps the used width to the intrinsic min-content size.
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          .flex { display: flex; width: 66px; }
          input {
            width: 66px;
            font-size: 100px;
            padding: 0;
            border: 0;
            background: rgb(1, 2, 3);
          }
        </style>
      </head>
      <body>
        <div class="flex"><input type="submit" value=""></div>
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
    (width - 66.0).abs() <= 0.5,
    "expected submit input with `value=\"\"` to respect explicit width; got {width:?} in fragment {:?}",
    input_fragment.bounds,
  );
}

