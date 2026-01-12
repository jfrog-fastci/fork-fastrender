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
fn appearance_none_empty_input_auto_height_includes_line_height() {
  // Regression test for netflix.com: custom-styled `appearance: none` inputs rely on getting at
  // least one line box worth of height even when empty.
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          input {
            -webkit-appearance: none;
            appearance: none;
            display: block;
            border: 0;
            padding: 20px 16px 4px 16px;
            font-size: 20px;
            line-height: 30px;
            background: rgb(1, 2, 3);
          }
        </style>
      </head>
      <body>
        <input value="" />
      </body>
    </html>
  "#;

  let target_color = Rgba::rgb(1, 2, 3);
  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("parse HTML");
  let fragments = renderer.layout_document(&dom, 240, 180).expect("layout");

  let input_fragment =
    find_fragment_by_background(&fragments.root, target_color).expect("input fragment");

  // Padding top (20px) + padding bottom (4px) + line-height (30px).
  let height = input_fragment.bounds.height();
  assert!(
    (height - 54.0).abs() <= 0.5,
    "expected empty appearance:none input to reserve line height; got height={height} bounds={:?}",
    input_fragment.bounds,
  );
}

#[test]
fn appearance_none_empty_textarea_auto_height_includes_line_height() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          textarea {
            -webkit-appearance: none;
            appearance: none;
            display: block;
            border: 0;
            min-height: 0;
            padding: 12px 16px 8px 16px;
            font-size: 20px;
            line-height: 30px;
            background: rgb(4, 5, 6);
          }
        </style>
      </head>
      <body>
        <textarea></textarea>
      </body>
    </html>
  "#;

  let target_color = Rgba::rgb(4, 5, 6);
  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("parse HTML");
  let fragments = renderer.layout_document(&dom, 240, 180).expect("layout");

  let textarea_fragment =
    find_fragment_by_background(&fragments.root, target_color).expect("textarea fragment");

  // Padding top (12px) + padding bottom (8px) + line-height (30px).
  let height = textarea_fragment.bounds.height();
  assert!(
    (height - 50.0).abs() <= 0.5,
    "expected empty appearance:none textarea to reserve line height; got height={height} bounds={:?}",
    textarea_fragment.bounds,
  );
}
