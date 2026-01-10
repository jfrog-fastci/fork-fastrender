use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use fastrender::{FastRender, FontConfig};

fn find_first_block_with_line_children<'a>(node: &'a FragmentNode) -> Option<&'a FragmentNode> {
  if matches!(node.content, FragmentContent::Block { .. })
    && node
      .children
      .iter()
      .any(|child| matches!(child.content, FragmentContent::Line { .. }))
  {
    return Some(node);
  }

  for child in node.children.iter() {
    if let Some(found) = find_first_block_with_line_children(child) {
      return Some(found);
    }
  }
  None
}

#[test]
fn inline_block_subpixel_overflow_does_not_force_wrap() {
  // Regression: Atomic inline items (inline-block / replaced) can overflow the available line width
  // by a subpixel amount due to text shaping, while their containing blocks have pixel-snapped
  // widths (e.g. from flex/grid intrinsic sizing probes). Treat a <0.5px overflow as fitting so we
  // don't spuriously wrap content like footer link lists.
  let html = r#"
    <style>
      .flex { display: flex; width: 400px }
      .row { font-size: 0px; line-height: 0px }
      .row span { display: inline-block; height: 10px }
    </style>
    <div class="flex"><div class="row">
      <span style="width: 50px"></span> <span style="width: 50.49px"></span>
    </div></div>
  "#;

  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer");
  let dom = renderer.parse_html(html).expect("parse HTML");
  let fragments = renderer
    .layout_document(&dom, 800, 200)
    .expect("layout document");

  let row = find_first_block_with_line_children(&fragments.root)
    .expect("expected a block fragment with line children");

  let line_count = row
    .children
    .iter()
    .filter(|child| matches!(child.content, FragmentContent::Line { .. }))
    .count();

  assert_eq!(
    line_count, 1,
    "expected subpixel overflow to stay on a single line"
  );
}

