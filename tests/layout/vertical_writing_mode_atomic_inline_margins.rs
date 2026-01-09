use fastrender::style::display::Display;
use fastrender::tree::fragment_tree::FragmentNode;
use fastrender::{FastRender, FontConfig};

fn collect_inline_block_fragments<'a>(node: &'a FragmentNode, out: &mut Vec<&'a FragmentNode>) {
  if let Some(style) = node.style.as_deref() {
    if style.display == Display::InlineBlock {
      out.push(node);
    }
  }
  for child in node.children.iter() {
    collect_inline_block_fragments(child, out);
  }
}

#[test]
fn vertical_writing_mode_inline_block_margin_affects_inline_advance() {
  // Regression: In vertical writing modes the inline axis is vertical, so `margin-top/bottom`
  // contribute to the inline advance between atomic inline items (inline-blocks/replaced).
  //
  // Previously, inline layout always used physical left/right margins for inline advance, which
  // ignored `margin-bottom` here and caused the second inline-block to overlap the first.
  let html = r#"
    <style>
      p {
        margin: 0;
        padding: 0;
        writing-mode: vertical-lr;
        font-family: 'DejaVu Sans', sans-serif;
        font-size: 16px;
        line-height: 1;
      }
      .a, .b { display: inline-block; width: 10px; height: 10px; }
      .a { margin-bottom: 5px; }
    </style>
    <p><span class="a"></span><span class="b"></span></p>
  "#;

  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer");
  let dom = renderer.parse_html(html).expect("parse HTML");
  let fragments = renderer
    .layout_document(&dom, 200, 200)
    .expect("layout document");

  let mut blocks = Vec::new();
  collect_inline_block_fragments(&fragments.root, &mut blocks);
  assert_eq!(blocks.len(), 2);

  blocks.sort_by(|a, b| a.bounds.y().total_cmp(&b.bounds.y()));
  let first = blocks[0];
  let second = blocks[1];
  let gap = second.bounds.min_y() - first.bounds.max_y();
  assert!(
    (gap - 5.0).abs() < 0.1,
    "inline-block gap={gap:.3} expected≈5.0; first={:?} second={:?}",
    first.bounds,
    second.bounds
  );
}

