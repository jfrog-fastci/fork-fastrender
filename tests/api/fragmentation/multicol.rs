//! Multi-column fragmentation regressions.

use fastrender::api::FastRender;
use fastrender::style::color::Rgba;
use fastrender::tree::fragment_tree::FragmentNode;

fn find_fragment_by_background(
  node: &FragmentNode,
  origin: (f32, f32),
  color: Rgba,
) -> Option<(f32, f32)> {
  let abs_x = origin.0 + node.bounds.x();
  let abs_y = origin.1 + node.bounds.y();
  if node
    .style
    .as_ref()
    .is_some_and(|style| style.background_color == color)
  {
    return Some((abs_x, abs_y));
  }
  for child in node.children.iter() {
    if let Some(found) = find_fragment_by_background(child, (abs_x, abs_y), color) {
      return Some(found);
    }
  }
  None
}

#[test]
fn forced_column_break_preserves_leading_margins() {
  // Regression test for CSS Break 3 §Adjoining Margins at Breaks, in a multi-column context:
  // - Forced breaks truncate margins *before* the break.
  // - Margins *after* the break are preserved.
  let mut renderer = FastRender::new().unwrap();
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          .multi { width: 200px; column-count: 2; column-gap: 0; }
          #a { height: 20px; background: rgb(255, 0, 0); }
          #b { height: 20px; margin-top: 20px; break-before: column; background: rgb(0, 0, 255); }
        </style>
      </head>
      <body>
        <div class="multi">
          <div id="a"></div>
          <div id="b"></div>
        </div>
      </body>
    </html>
  "#;

  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 200, 200).unwrap();

  let red = Rgba::rgb(255, 0, 0);
  let blue = Rgba::rgb(0, 0, 255);

  let origin = (-tree.root.bounds.x(), -tree.root.bounds.y());
  let red_pos = find_fragment_by_background(&tree.root, origin, red).expect("red fragment");
  let blue_pos = find_fragment_by_background(&tree.root, origin, blue).expect("blue fragment");

  assert!(
    blue_pos.0 > red_pos.0 + 1.0,
    "forced column break should move the second block to a later column (red={red_pos:?}, blue={blue_pos:?})"
  );
  assert!(
    (blue_pos.1 - 20.0).abs() < 0.5,
    "forced column breaks should preserve the leading margin of the next fragment (got y={})",
    blue_pos.1
  );
}

