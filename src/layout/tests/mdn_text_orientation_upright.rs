use crate::style::types::{TextOrientation, WritingMode};
use crate::text::RunRotation;
use crate::tree::fragment_tree::{FragmentContent, FragmentNode};
use crate::{FastRender, FontConfig};

fn find_first_text_fragment(node: &FragmentNode) -> Option<&FragmentNode> {
  node
    .iter_fragments()
    .find(|frag| matches!(frag.content, FragmentContent::Text { .. }))
}

#[test]
fn mdn_text_orientation_upright_live_sample_produces_unrotated_runs_and_vertical_geometry() {
  // Mirrors the MDN `text-orientation` live sample:
  //
  //   p { writing-mode: vertical-rl; text-orientation: upright; }
  //
  // We keep the document minimal (no external fixtures) so that once MDN live-sample iframes
  // are rendered, this remains a focused correctness signal for vertical-rl + upright.
  let html = r#"<html><body style="margin:0"><p style="margin:0; writing-mode: vertical-rl; text-orientation: upright; font-family:'DejaVu Sans', sans-serif; font-size:20px; line-height:1">Lorem ipsum dolor sit amet</p></body></html>"#;

  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer");

  let dom = renderer.parse_html(html).expect("parse HTML");
  let fragments = renderer
    .layout_document(&dom, 400, 400)
    .expect("layout document");

  let frag = find_first_text_fragment(&fragments.root).expect("expected at least one text fragment");
  let style = frag.style.as_ref().expect("fragment style");

  assert_eq!(
    style.writing_mode,
    WritingMode::VerticalRl,
    "expected text fragment to inherit `writing-mode: vertical-rl`"
  );
  assert_eq!(
    style.text_orientation,
    TextOrientation::Upright,
    "expected text fragment to inherit `text-orientation: upright`"
  );

  let shaped_runs = match &frag.content {
    FragmentContent::Text {
      shaped: Some(shaped),
      ..
    } => shaped.as_ref(),
    FragmentContent::Text { shaped: None, .. } => {
      panic!("expected shaped runs on text fragment");
    }
    other => panic!("expected FragmentContent::Text, got {other:?}"),
  };

  assert!(
    !shaped_runs.is_empty(),
    "expected shaped runs for text fragment"
  );
  for run in shaped_runs.iter() {
    assert_eq!(
      run.rotation,
      RunRotation::None,
      "expected upright text to shape with no rotation; got {:?} for run {:?}",
      run.rotation,
      run.text
    );
  }

  // In vertical writing mode the text fragment's inline advance runs along the physical Y axis,
  // while the block axis is horizontal (X). The fragment should therefore be a narrow column
  // (block-size ≈ 1em / line-height) with a tall inline advance.
  let expected_block_size = 20.0;
  assert!(
    (frag.bounds.width() - expected_block_size).abs() < 0.5,
    "expected vertical text fragment to have ~1em block-size width; got {:.3} (bounds={:?})",
    frag.bounds.width(),
    frag.bounds
  );

  let expected_inline_advance: f32 = shaped_runs.iter().map(|run| run.advance).sum();
  assert!(
    (frag.bounds.height() - expected_inline_advance).abs() < 0.5,
    "expected vertical text fragment height to match shaped inline advance; expected {:.3}, got {:.3} (bounds={:?})",
    expected_inline_advance,
    frag.bounds.height(),
    frag.bounds
  );
  assert!(
    frag.bounds.height() > frag.bounds.width(),
    "expected vertical text fragment to be taller than it is wide (bounds={:?})",
    frag.bounds
  );
}

