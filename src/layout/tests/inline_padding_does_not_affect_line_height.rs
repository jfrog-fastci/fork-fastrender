use crate::tree::fragment_tree::{FragmentContent, FragmentNode};
use crate::{FastRender, FontConfig};

fn find_first_line<'a>(node: &'a FragmentNode) -> Option<&'a FragmentNode> {
  if matches!(node.content, FragmentContent::Line { .. }) {
    return Some(node);
  }
  node.children.iter().find_map(find_first_line)
}

fn find_first_inline<'a>(node: &'a FragmentNode) -> Option<&'a FragmentNode> {
  if matches!(node.content, FragmentContent::Inline { .. }) {
    return Some(node);
  }
  node.children.iter().find_map(find_first_inline)
}

#[test]
fn inline_padding_does_not_expand_line_box_height() {
  // Per CSS 2.1, inline padding/borders do not affect line box height; they overflow instead.
  //
  // Regression: the inline box's border box is positioned around the element's content area
  // (font metrics), not the line-height strut. When the line-height introduces enough leading,
  // vertical padding should fit *inside* the line box rather than always overflowing above it.
  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer");
  let dom = renderer
    .parse_html(
      r#"<div style="font-size:16px;line-height:60px"><span style="padding-top:10px;padding-bottom:10px">A</span></div>"#,
    )
    .expect("parse HTML");
  let fragments = renderer
    .layout_document(&dom, 200, 200)
    .expect("layout document");

  let line = find_first_line(&fragments.root).expect("expected a line fragment");
  assert!(
    (line.bounds.height() - 60.0).abs() < 0.01,
    "line box height should match the authored line-height: got {}",
    line.bounds.height()
  );

  let inline = find_first_inline(line).expect("expected an inline fragment");
  assert!(
    inline.bounds.height() < line.bounds.height(),
    "inline border box should not automatically include the full line-height strut: got {} (line height {})",
    inline.bounds.height(),
    line.bounds.height(),
  );
  assert!(
    inline.bounds.y() >= 0.0,
    "padded inline boxes should fit within the line box when half-leading exceeds padding: got y={}",
    inline.bounds.y()
  );
}
