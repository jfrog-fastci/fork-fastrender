use fastrender::text::font_db::{FontStretch, FontStyle, FontWeight, LoadedFont};
use fastrender::text::{ClusterMap, Direction, GlyphPosition, RunRotation, ShapedRun};
use std::mem;
use std::sync::Arc;

use super::{fail_next_allocation, failed_allocs, lock_allocator};

fn synthetic_shaped_run(char_count: usize) -> ShapedRun {
  let text = "a".repeat(char_count);
  let mut glyphs = Vec::with_capacity(char_count);
  for idx in 0..char_count {
    glyphs.push(GlyphPosition {
      glyph_id: 0,
      cluster: idx as u32,
      x_offset: 0.0,
      y_offset: 0.0,
      x_advance: 0.0,
      y_advance: 0.0,
    });
  }

  ShapedRun {
    start: 0,
    end: text.len(),
    advance: char_count as f32,
    text,
    glyphs,
    direction: Direction::LeftToRight,
    level: 0,
    font: Arc::new(LoadedFont {
      id: None,
      family: "Test".into(),
      data: Arc::new(Vec::new()),
      index: 0,
      face_metrics_overrides: Default::default(),
      face_settings: Default::default(),
      weight: FontWeight::NORMAL,
      style: FontStyle::Normal,
      stretch: FontStretch::Normal,
    }),
    font_size: 16.0,
    baseline_shift: 0.0,
    language: None,
    features: Arc::from(Vec::new()),
    synthetic_bold: 0.0,
    synthetic_oblique: 0.0,
    rotation: RunRotation::None,
    palette_index: 0,
    palette_overrides: Arc::new(Vec::new()),
    palette_override_hash: 0,
    variations: Vec::new(),
    scale: 1.0,
  }
}

#[test]
fn cluster_map_survives_allocation_failure() {
  let _guard = lock_allocator();

  let char_count = 12_345usize;
  let run = synthetic_shaped_run(char_count);

  let ok = ClusterMap::from_shaped_run(&run);
  assert_eq!(ok.glyph_for_char(0), Some(0));
  assert_eq!(ok.char_for_glyph(0), Some(0));

  let alloc_size = char_count * mem::size_of::<usize>();
  let alloc_align = mem::align_of::<usize>();
  let start_failures = failed_allocs();
  fail_next_allocation(alloc_size, alloc_align);

  let failed = ClusterMap::from_shaped_run(&run);

  assert_eq!(
    failed_allocs(),
    start_failures + 1,
    "expected to trigger cluster-map allocation failure"
  );
  assert!(
    failed.glyph_for_char(0).is_none(),
    "expected cluster map to return empty mapping after allocation failure"
  );
  assert!(
    failed.char_for_glyph(0).is_none(),
    "expected cluster map to return empty mapping after allocation failure"
  );
}
