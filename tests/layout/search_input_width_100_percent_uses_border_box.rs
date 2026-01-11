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
fn search_input_width_100_percent_uses_border_box() {
  // Chrome's UA stylesheet treats `input[type=search]` as `box-sizing: border-box`.
  //
  // This matters for patterns like Berkeley's skyline search field:
  // `width: 100%` + padding should not overflow the containing block.
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          .container { width: 210px; }
          input {
            width: 100%;
            padding: 14px 48px 14px 16px;
            border: 0;
            background: rgb(1, 2, 3);
            font-size: 16px;
          }
        </style>
      </head>
      <body>
        <div class="container"><input type="search" value=""></div>
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
    (width - 210.0).abs() <= 0.5,
    "expected search input border box to fit container width; got {width:?} in fragment {:?}",
    input_fragment.bounds,
  );
}

