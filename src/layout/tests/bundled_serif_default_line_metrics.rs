use crate::tree::fragment_tree::FragmentContent;
use crate::{FastRender, FastRenderConfig, FontConfig};

fn find_text_fragment<'a>(
  fragment: &'a crate::FragmentNode,
  needle: &str,
) -> Option<&'a crate::FragmentNode> {
  let mut stack = vec![fragment];
  while let Some(node) = stack.pop() {
    if let FragmentContent::Text { text, .. } = &node.content {
      if text.contains(needle) {
        return Some(node);
      }
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  None
}

#[test]
fn bundled_serif_default_line_height_is_times_like() {
  let config = FastRenderConfig::default().with_font_sources(FontConfig::bundled_only());
  let mut renderer = FastRender::with_config(config).expect("renderer");

  // This uses default UA font-family (serif) and `line-height: normal`. Our bundled serif
  // fallback is tuned to match the common browser default line metrics (e.g. 16px -> 18px,
  // 24px -> 27px) so pages without explicit font-family/line-height don't drift vertically vs
  // Chrome.
  let html = r##"
    <html>
      <body style="margin:0">
        <h2>AAA<br>BBB</h2>
        <div><a href="#">CCC</a></div>
      </body>
    </html>
  "##;

  let dom = renderer.parse_html(html).expect("parse");
  let tree = renderer.layout_document(&dom, 400, 300).expect("layout");

  let h2_line1 = find_text_fragment(&tree.root, "AAA").expect("h2 line1 text fragment");
  let h2_line2 = find_text_fragment(&tree.root, "BBB").expect("h2 line2 text fragment");
  let link = find_text_fragment(&tree.root, "CCC").expect("link text fragment");

  for (label, fragment, expected) in [
    ("h2 line1", h2_line1, 27.0_f32),
    ("h2 line2", h2_line2, 27.0_f32),
    ("link", link, 18.0_f32),
  ] {
    let (runs, font_family) = match &fragment.content {
      FragmentContent::Text {
        shaped: Some(shaped),
        ..
      } => {
        let runs = shaped.as_ref();
        let family = runs
          .first()
          .map(|run| run.font.family.as_str())
          .unwrap_or("<missing font>");
        (runs, family)
      }
      _ => panic!("{label}: expected shaped text fragment"),
    };

    assert_eq!(
      font_family, "STIX Two Math",
      "{label}: expected default serif generic to resolve to the bundled STIX Two Math face"
    );
    assert!(
      (fragment.bounds.height() - expected).abs() < 0.05,
      "{label}: expected line fragment height {expected}, got {} (runs={})",
      fragment.bounds.height(),
      runs.len()
    );
  }
}
