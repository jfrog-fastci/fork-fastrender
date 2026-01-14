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
fn grid_text_input_width_100_percent_includes_padding_and_border() {
  // Regression test inspired by MDN's `<mdn-sidebar-filter>` shadow DOM:
  // a `width: 100%` text input spanning grid tracks with large padding should size
  // its *border box* to the grid area width (i.e. not overflow due to content-box sizing
  // or an oversized automatic minimum size).
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          *,:before,:after { box-sizing: border-box; }
          .grid {
            display: grid;
            grid-template-columns: 35px 1fr min-content;
            width: 200px;
            background: rgb(4, 5, 6);
          }
          .input {
            grid-area: 1 / 1 / -1 / -1;
            width: 100%;
            padding: 4px 67px 4px 35px;
            border: 1px solid black;
            background: rgb(1, 2, 3);
          }
          .btn {
            grid-column: 3;
            width: 20px;
            height: 10px;
          }
        </style>
      </head>
      <body>
        <div class="grid">
          <input class="input" type="text" value="" />
          <div class="btn"></div>
        </div>
      </body>
    </html>
  "#;

  let input_color = Rgba::rgb(1, 2, 3);
  let grid_color = Rgba::rgb(4, 5, 6);

  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("parse HTML");
  let fragments = renderer.layout_document(&dom, 400, 200).expect("layout");

  let input_fragment =
    find_fragment_by_background(&fragments.root, input_color).expect("input fragment");
  let grid_fragment = find_fragment_by_background(&fragments.root, grid_color).expect("grid fragment");

  let eps = 0.5;
  let grid_width = grid_fragment.bounds.width();
  let input_width = input_fragment.bounds.width();
  assert!(
    (grid_width - 200.0).abs() <= eps,
    "expected grid to have definite width 200px; got {grid_width} in {:?}",
    grid_fragment.bounds
  );
  assert!(
    (input_width - grid_width).abs() <= eps,
    "expected input border box width to match grid area width; got input_width={input_width} grid_width={grid_width} input_bounds={:?} grid_bounds={:?}",
    input_fragment.bounds,
    grid_fragment.bounds,
  );
}

