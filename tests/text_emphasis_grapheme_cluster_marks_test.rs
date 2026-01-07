use fastrender::paint::display_list::DisplayItem;
use fastrender::paint::display_list_builder::DisplayListBuilder;
use fastrender::style::color::Rgba;
use fastrender::style::types::{TextEmphasisFill, TextEmphasisShape, TextEmphasisStyle};
use fastrender::style::ComputedStyle;
use fastrender::text::font_db::{
  FontFaceMetricsOverrides, FontFaceShapingDescriptors, FontStretch, FontStyle, FontWeight, LoadedFont,
};
use fastrender::text::pipeline::{Direction, GlyphPosition, RunRotation, ShapedRun};
use fastrender::tree::fragment_tree::FragmentNode;
use fastrender::tree::fragment_tree::FragmentTree;
use fastrender::Rect;
use std::path::PathBuf;
use std::sync::Arc;

fn test_font() -> Arc<LoadedFont> {
  let font_path =
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fonts/DejaVuSans-subset.ttf");
  let data = Arc::new(std::fs::read(font_path).expect("read test font"));
  Arc::new(LoadedFont {
    id: None,
    data,
    index: 0,
    face_metrics_overrides: FontFaceMetricsOverrides::default(),
    face_settings: FontFaceShapingDescriptors::default(),
    family: "DejaVu Sans Subset".to_string(),
    weight: FontWeight::NORMAL,
    style: FontStyle::Normal,
    stretch: FontStretch::Normal,
  })
}

#[test]
fn text_emphasis_marks_follow_grapheme_clusters_even_when_harfbuzz_clusters_span_multiple_chars() {
  // CSS Text Decoration 4: emphasis marks are drawn once per typographic character unit (extended
  // grapheme cluster). HarfBuzz cluster values can span multiple grapheme clusters (e.g. ligatures),
  // so we must still emit one mark per grapheme cluster.
  //
  // Construct a synthetic shaped run where a single HarfBuzz cluster (cluster=0) spans the entire
  // string "fi". Expect two marks: one for "f" and one for "i".
  let font = test_font();
  let text = "fi".to_string();
  let advance = 20.0;
  let run = ShapedRun {
    text: text.clone(),
    start: 0,
    end: text.len(),
    glyphs: vec![GlyphPosition {
      glyph_id: 0,
      cluster: 0,
      x_offset: 0.0,
      y_offset: 0.0,
      x_advance: advance,
      y_advance: 0.0,
    }],
    direction: Direction::LeftToRight,
    level: 0,
    advance,
    font,
    font_size: 16.0,
    baseline_shift: 0.0,
    language: None,
    synthetic_bold: 0.0,
    synthetic_oblique: 0.0,
    rotation: RunRotation::None,
    palette_index: 0,
    palette_overrides: Arc::new(Vec::new()),
    palette_override_hash: 0,
    variations: Vec::new(),
    scale: 1.0,
  };

  let mut style = ComputedStyle::default();
  style.font_size = 16.0;
  style.color = Rgba::BLACK;
  style.text_emphasis_style = TextEmphasisStyle::Mark {
    fill: TextEmphasisFill::Filled,
    shape: Some(TextEmphasisShape::Dot),
  };
  style.text_emphasis_color = Some(Rgba::from_rgba8(255, 0, 0, 255));

  let runs = Arc::new(vec![run]);
  let text_fragment = FragmentNode::new_text_shaped(
    Rect::from_xywh(0.0, 0.0, 80.0, 40.0),
    text,
    20.0,
    runs,
    Arc::new(style),
  );
  let root =
    FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 80.0, 40.0), vec![text_fragment]);
  let tree = FragmentTree::new(root);

  let list = DisplayListBuilder::new().build_tree(&tree);
  let marks: usize = list
    .items()
    .iter()
    .filter_map(|item| match item {
      DisplayItem::Text(text) => text.emphasis.as_ref(),
      _ => None,
    })
    .map(|emphasis| emphasis.marks.len())
    .sum();

  assert_eq!(
    marks, 2,
    "expected two emphasis marks for two grapheme clusters in \"fi\""
  );
}

