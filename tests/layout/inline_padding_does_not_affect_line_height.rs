use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use fastrender::{FastRender, FontConfig};

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
  // Regresses Techmeme.com's navbar, where padded <a> elements were incorrectly increasing the
  // line height of their containing block.
  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer");
  let dom = renderer
    .parse_html(
      r#"<div style="font-size:16px;line-height:32px"><span style="padding-top:10px;padding-bottom:10px">A</span></div>"#,
    )
    .expect("parse HTML");
  let fragments = renderer
    .layout_document(&dom, 200, 200)
    .expect("layout document");

  let line = find_first_line(&fragments.root).expect("expected a line fragment");
  assert!(
    (line.bounds.height() - 32.0).abs() < 0.01,
    "line box height should match the authored line-height: got {}",
    line.bounds.height()
  );

  let inline = find_first_inline(line).expect("expected an inline fragment");
  assert!(
    (inline.bounds.height() - 52.0).abs() < 0.01,
    "inline border box should still include padding: got {}",
    inline.bounds.height()
  );
  assert!(
    (inline.bounds.y() + 10.0).abs() < 0.01,
    "padded inline boxes should overflow above the line box: got y={}",
    inline.bounds.y()
  );
}
