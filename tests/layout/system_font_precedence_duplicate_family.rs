use std::fs;
use std::path::PathBuf;

use fastrender::tree::fragment_tree::FragmentContent;
use fastrender::{FastRender, FastRenderConfig, FontConfig};

fn find_text_fragment<'a>(fragment: &'a fastrender::FragmentNode, needle: &str) -> Option<&'a fastrender::FragmentNode> {
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

fn renderer_with_non_bundled_noto_sans_dir() -> (FastRender, tempfile::TempDir) {
  let dir = tempfile::tempdir().expect("temp dir");
  let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
  let font_src = manifest.join("tests/fixtures/fonts/NotoSans-subset.ttf");
  let font_dst = dir.path().join("NotoSans-subset.ttf");
  fs::copy(&font_src, &font_dst).expect("copy noto sans subset");

  // Load the font file from an explicit directory (non-bundled), then also load bundled fallbacks.
  // This creates a duplicate family name (`Noto Sans`) where one face is considered bundled and
  // carries metric overrides, and another is non-bundled. We should prefer the non-bundled face so
  // system/user-supplied metrics win (matching Chrome baselines more closely).
  let font_config = FontConfig::bundled_only().add_font_dir(dir.path());
  let config = FastRenderConfig::default().with_font_sources(font_config);
  (FastRender::with_config(config).expect("renderer"), dir)
}

#[test]
fn named_family_prefers_non_bundled_duplicate_face() {
  let (mut renderer, _dir) = renderer_with_non_bundled_noto_sans_dir();
  let html = r##"
    <html>
      <body style="margin:0; font-family: 'Noto Sans'; font-size: 16px;">
        AAA<br>BBB
      </body>
    </html>
  "##;

  let dom = renderer.parse_html(html).expect("parse");
  let tree = renderer.layout_document(&dom, 400, 200).expect("layout");

  let line1 = find_text_fragment(&tree.root, "AAA").expect("line 1 text fragment");
  let line2 = find_text_fragment(&tree.root, "BBB").expect("line 2 text fragment");

  for (label, frag) in [("line1", line1), ("line2", line2)] {
    let FragmentContent::Text {
      shaped: Some(shaped),
      ..
    } = &frag.content
    else {
      panic!("{label}: expected shaped text fragment");
    };
    let run = shaped
      .first()
      .expect("{label}: expected at least one shaped run");

    assert_eq!(run.font.family, "Noto Sans", "{label}: wrong resolved family");
    assert!(
      !run.font.face_metrics_overrides.has_metric_overrides(),
      "{label}: expected non-bundled face (no built-in metric overrides)"
    );
    assert!(
      (frag.bounds.height() - 21.0).abs() < 0.05,
      "{label}: expected line fragment height 21px (raw Noto Sans line metrics), got {}",
      frag.bounds.height()
    );
  }
}

#[test]
fn generic_sans_serif_prefers_non_bundled_duplicate_face() {
  let (mut renderer, _dir) = renderer_with_non_bundled_noto_sans_dir();
  let html = r##"
    <html>
      <body style="margin:0; font-family: sans-serif; font-size: 16px;">
        AAA<br>BBB
      </body>
    </html>
  "##;

  let dom = renderer.parse_html(html).expect("parse");
  let tree = renderer.layout_document(&dom, 400, 200).expect("layout");

  let line1 = find_text_fragment(&tree.root, "AAA").expect("line 1 text fragment");
  let line2 = find_text_fragment(&tree.root, "BBB").expect("line 2 text fragment");

  for (label, frag) in [("line1", line1), ("line2", line2)] {
    let FragmentContent::Text {
      shaped: Some(shaped),
      ..
    } = &frag.content
    else {
      panic!("{label}: expected shaped text fragment");
    };
    let run = shaped
      .first()
      .expect("{label}: expected at least one shaped run");

    assert_eq!(run.font.family, "Noto Sans", "{label}: wrong resolved family");
    assert!(
      !run.font.face_metrics_overrides.has_metric_overrides(),
      "{label}: expected non-bundled face (no built-in metric overrides)"
    );
    assert!(
      (frag.bounds.height() - 21.0).abs() < 0.05,
      "{label}: expected line fragment height 21px (raw Noto Sans line metrics), got {}",
      frag.bounds.height()
    );
  }
}
