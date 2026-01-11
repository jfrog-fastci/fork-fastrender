use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use fastrender::{FastRender, FontConfig};

fn contains_text(fragment: &FragmentNode, needle: &str) -> bool {
  let mut stack = vec![fragment];
  while let Some(node) = stack.pop() {
    if let FragmentContent::Text { text, .. } = &node.content {
      if text.contains(needle) {
        return true;
      }
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  false
}

fn find_line_with_text_abs_y<'a>(
  node: &'a FragmentNode,
  needle: &str,
  parent_y: f32,
) -> Option<(f32, &'a FragmentNode)> {
  let abs_y = parent_y + node.bounds.y();
  if matches!(node.content, FragmentContent::Line { .. }) && contains_text(node, needle) {
    return Some((abs_y, node));
  }
  for child in node.children.iter() {
    if let Some(found) = find_line_with_text_abs_y(child, needle, abs_y) {
      return Some(found);
    }
  }
  None
}

#[test]
fn vertical_align_middle_on_inline_box_uses_own_line_height_box() {
  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("renderer");

  // Regression test for CSS 2.1 §10.8.1:
  // For inline non-replaced elements, baseline-relative `vertical-align` values operate on the
  // element's own line-height box, *not* the bounds of its aligned subtree.
  //
  // On lobste.rs, `.link { vertical-align: middle }` wraps a larger-font `<a>`. If we compute the
  // midpoint using the aligned subtree bounds, the baseline shift is too large and line boxes grow,
  // causing cumulative vertical drift down the page.
  let html = r##"
    <html>
      <head>
        <style>
          body {
            margin: 0;
            font-family: sans-serif;
            font-size: 10pt;
            line-height: 1.45em;
          }

          .wrap {
            vertical-align: middle;
          }

          .wrap a {
            font-size: 11.5pt;
            font-weight: bold;
          }
        </style>
      </head>
      <body>
        <div><span class="wrap"><a href="#">BIG</a></span></div>
      </body>
    </html>
  "##;

  let dom = renderer.parse_html(html).expect("parse");
  let tree = renderer.layout_document(&dom, 800, 200).expect("layout");

  let (_line_y, line) = find_line_with_text_abs_y(&tree.root, "BIG", 0.0).expect("BIG line");
  let line_height = line.bounds.height();

  // Before the regression fix, the line height was ~20.8px (using aligned subtree bounds as the
  // alignment box). Spec-correct behavior is ~20.47px.
  assert!(
    line_height < 20.7,
    "expected vertical-align: middle on an inline box to use its own line-height box (line_height={line_height})"
  );
}
