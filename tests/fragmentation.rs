//! Dedicated test target for focused fragmentation regressions.
//!
//! The full integration suite is linked via `tests/integration.rs`. This target exists so automation
//! can run a small slice of fragmentation assertions without executing the entire integration test
//! harness.

#[path = "misc/fragmentation_public_api.rs"]
mod fragmentation_public_api;

use fastrender::api::FastRender;
use fastrender::style::color::Rgba;
use fastrender::tree::fragment_tree::{FragmentNode, FragmentTree};

fn fragment_roots<'a>(tree: &'a FragmentTree) -> Vec<&'a FragmentNode> {
  let mut roots = vec![&tree.root];
  roots.extend(tree.additional_fragments.iter());
  roots
}

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
fn forced_break_truncates_trailing_margins_and_preserves_leading_margins() {
  // CSS Break 3 §Adjoining Margins at Breaks:
  // - Forced breaks truncate margins *before* the break.
  // - Margins *after* the break are preserved.
  let mut renderer = FastRender::builder().paginate(100.0, 0.0).build().unwrap();

  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          #a { height: 10px; margin-bottom: 20px; background: rgb(255, 0, 0); }
          #b { height: 10px; margin-top: 20px; break-before: page; background: rgb(0, 0, 255); }
        </style>
      </head>
      <body>
        <div id="a"></div>
        <div id="b"></div>
      </body>
    </html>
  "#;

  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 200, 200).unwrap();
  let roots = fragment_roots(&tree);

  assert!(
    roots.len() >= 2,
    "forced break should create at least two fragment roots"
  );

  let red = Rgba::rgb(255, 0, 0);
  let blue = Rgba::rgb(0, 0, 255);

  let first = roots[0];
  let first_origin = (-first.bounds.x(), -first.bounds.y());
  assert!(
    find_fragment_by_background(first, first_origin, red).is_some(),
    "expected first block on the first fragment"
  );
  assert!(
    find_fragment_by_background(first, first_origin, blue).is_none(),
    "forced break should move the second block off the first fragment"
  );

  assert!(
    (first.bounds.height() - 10.0).abs() < 0.5,
    "trailing margins before the break must be truncated (got fragment height={})",
    first.bounds.height()
  );

  let second = roots
    .iter()
    .skip(1)
    .find(|root| {
      let origin = (-root.bounds.x(), -root.bounds.y());
      find_fragment_by_background(root, origin, blue).is_some()
    })
    .copied()
    .expect("second fragment root with the post-break content");
  let second_origin = (-second.bounds.x(), -second.bounds.y());
  let blue_pos = find_fragment_by_background(second, second_origin, blue).unwrap();
  assert!(
    (blue_pos.1 - 20.0).abs() < 0.5,
    "leading margins after a forced break must be preserved (got y={})",
    blue_pos.1
  );
}

