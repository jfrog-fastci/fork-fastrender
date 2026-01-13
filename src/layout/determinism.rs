use crate::geometry::Rect;
use crate::style::types::FootnotePolicy;
use crate::style::values::Length;
use crate::style::ComputedStyle;
use crate::text::pipeline::{RunRotation, ShapedRun};
use crate::tree::box_tree::ReplacedType;
use crate::tree::fragment_tree::{
  BlockFragmentMetadata, FragmentContent, FragmentNode, FragmentStackingContext, FragmentTree,
  FragmentationInfo, GridFragmentationInfo, GridTrackRanges, ScrollbarReservation,
  TableCollapsedBorders, TextSourceRange,
};
use sha2::{Digest, Sha256};
use std::fmt;
use std::sync::Arc;

/// Stable fingerprint of layout output.
///
/// This is intended for determinism regression tests that run the same layout under different Rayon
/// thread pool sizes. The fingerprint is:
/// - independent of pointer addresses / allocation order
/// - stable across runs for identical fragment output
/// - sensitive to layout-relevant differences (tree shape, paint order, bounds, text glyph runs)
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct LayoutFingerprint([u8; 32]);

impl LayoutFingerprint {
  pub fn as_bytes(&self) -> &[u8; 32] {
    &self.0
  }

  pub fn to_hex(&self) -> String {
    let mut out = String::with_capacity(64);
    for byte in self.0 {
      out.push_str(&format!("{byte:02x}"));
    }
    out
  }
}

impl fmt::Debug for LayoutFingerprint {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_tuple("LayoutFingerprint")
      .field(&self.to_hex())
      .finish()
  }
}

impl fmt::Display for LayoutFingerprint {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(&self.to_hex())
  }
}

struct FingerprintHasher {
  sha: Sha256,
}

impl FingerprintHasher {
  fn new(domain: &'static [u8]) -> Self {
    let mut sha = Sha256::new();
    sha.update(domain);
    sha.update(&[0u8]);
    Self { sha }
  }

  fn finish(self) -> LayoutFingerprint {
    let digest = self.sha.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    LayoutFingerprint(out)
  }

  fn write_bytes(&mut self, bytes: &[u8]) {
    self.sha.update(bytes);
  }

  fn write_u8(&mut self, value: u8) {
    self.sha.update([value]);
  }

  fn write_u16(&mut self, value: u16) {
    self.sha.update(value.to_le_bytes());
  }

  fn write_u32(&mut self, value: u32) {
    self.sha.update(value.to_le_bytes());
  }

  fn write_u64(&mut self, value: u64) {
    self.sha.update(value.to_le_bytes());
  }

  fn write_usize(&mut self, value: usize) {
    self.write_u64(value as u64);
  }

  fn write_i32(&mut self, value: i32) {
    self.sha.update(value.to_le_bytes());
  }

  fn write_bool(&mut self, value: bool) {
    self.write_u8(u8::from(value));
  }

  fn write_f32(&mut self, value: f32) {
    self.write_u32(canonical_f32_bits(value));
  }

  fn write_str(&mut self, value: &str) {
    self.write_usize(value.len());
    self.write_bytes(value.as_bytes());
  }

  fn write_option<T>(&mut self, value: Option<&T>, mut write: impl FnMut(&mut Self, &T)) {
    match value {
      Some(inner) => {
        self.write_u8(1);
        write(self, inner);
      }
      None => self.write_u8(0),
    }
  }

  fn write_slice<T>(&mut self, values: &[T], mut write: impl FnMut(&mut Self, &T)) {
    self.write_usize(values.len());
    for value in values {
      write(self, value);
    }
  }
}

fn canonical_f32_bits(value: f32) -> u32 {
  if value == 0.0 {
    // Canonicalize +0.0 / -0.0.
    return 0;
  }
  if value.is_nan() {
    // Canonicalize all NaNs to a single quiet NaN payload.
    return 0x7fc0_0000;
  }
  value.to_bits()
}

/// Produce a stable fingerprint from the fragment tree.
pub fn fragment_tree_fingerprint(tree: &FragmentTree) -> LayoutFingerprint {
  let mut hasher = FingerprintHasher::new(b"fastrender.fragment_tree_fingerprint.v1");
  hash_fragment_tree(&mut hasher, tree);
  hasher.finish()
}

fn hash_fragment_tree(hasher: &mut FingerprintHasher, tree: &FragmentTree) {
  let viewport = tree.viewport_size();
  hasher.write_f32(viewport.width);
  hasher.write_f32(viewport.height);

  hasher.write_usize(1 + tree.additional_fragments.len());
  hash_fragment_node(hasher, &tree.root);
  for root in &tree.additional_fragments {
    hash_fragment_node(hasher, root);
  }
}

fn hash_fragment_node(hasher: &mut FingerprintHasher, node: &FragmentNode) {
  hasher.write_u8(0x01); // node start marker
  hash_rect(hasher, node.bounds);
  hasher.write_option(node.logical_override.as_ref(), |hasher, rect| {
    hash_rect(hasher, *rect);
  });

  hasher.write_option(node.block_metadata.as_ref(), hash_block_metadata);
  hasher.write_option(node.baseline.as_ref(), |hasher, baseline| {
    hasher.write_f32(*baseline);
  });
  hasher.write_usize(node.fragment_index);
  hasher.write_usize(node.fragment_count);
  hasher.write_usize(node.fragmentainer_index);
  hash_fragmentainer_path(hasher, &node.fragmentainer);
  hash_slice_info(hasher, node.slice_info);
  hash_rect(hasher, node.scroll_overflow);
  hash_scrollbar_reservation(hasher, node.scrollbar_reservation);
  hash_fragment_stacking_context(hasher, node.stacking_context);
  hasher.write_option(node.fragmentation.as_ref(), hash_fragmentation_info);
  hasher.write_option(
    node.grid_fragmentation.as_deref(),
    hash_grid_fragmentation_info,
  );
  hasher.write_option(node.grid_tracks.as_deref(), hash_grid_tracks);
  hasher.write_option(node.table_borders.as_deref(), hash_table_borders);

  hasher.write_option(node.style.as_deref(), hash_style_summary);

  hash_fragment_content(hasher, &node.content);

  hasher.write_usize(node.children.len());
  for child in node.children.iter() {
    hash_fragment_node(hasher, child);
  }
  hasher.write_u8(0x02); // node end marker
}

fn hash_rect(hasher: &mut FingerprintHasher, rect: Rect) {
  hasher.write_f32(rect.x());
  hasher.write_f32(rect.y());
  hasher.write_f32(rect.width());
  hasher.write_f32(rect.height());
}

fn hash_block_metadata(hasher: &mut FingerprintHasher, meta: &BlockFragmentMetadata) {
  hasher.write_f32(meta.margin_top);
  hasher.write_f32(meta.margin_bottom);
  hasher.write_bool(meta.clipped_top);
  hasher.write_bool(meta.clipped_bottom);
}

fn hash_fragmentainer_path(
  hasher: &mut FingerprintHasher,
  path: &crate::tree::fragment_tree::FragmentainerPath,
) {
  hasher.write_usize(path.page_index);
  hasher.write_option(path.column_set_index.as_ref(), |hasher, idx| {
    hasher.write_usize(*idx);
  });
  hasher.write_option(path.column_index.as_ref(), |hasher, idx| {
    hasher.write_usize(*idx);
  });
}

fn hash_slice_info(
  hasher: &mut FingerprintHasher,
  info: crate::tree::fragment_tree::FragmentSliceInfo,
) {
  hasher.write_bool(info.is_first);
  hasher.write_bool(info.is_last);
  hasher.write_f32(info.slice_offset);
  hasher.write_f32(info.original_block_size);
}

fn hash_scrollbar_reservation(hasher: &mut FingerprintHasher, reservation: ScrollbarReservation) {
  hasher.write_f32(reservation.left);
  hasher.write_f32(reservation.right);
  hasher.write_f32(reservation.top);
  hasher.write_f32(reservation.bottom);
}

fn hash_fragment_stacking_context(hasher: &mut FingerprintHasher, ctx: FragmentStackingContext) {
  match ctx {
    FragmentStackingContext::Normal => hasher.write_u8(0),
    FragmentStackingContext::Forced { z_index } => {
      hasher.write_u8(1);
      hasher.write_i32(z_index);
    }
  }
}

fn hash_fragmentation_info(hasher: &mut FingerprintHasher, info: &FragmentationInfo) {
  hasher.write_usize(info.column_count);
  hasher.write_f32(info.column_gap);
  hasher.write_f32(info.column_width);
  hasher.write_f32(info.flow_height);
}

fn hash_grid_fragmentation_info(hasher: &mut FingerprintHasher, info: &GridFragmentationInfo) {
  hasher.write_usize(info.items.len());
  for item in &info.items {
    hasher.write_usize(item.box_id);
    hasher.write_u16(item.row_start);
    hasher.write_u16(item.row_end);
    hasher.write_u16(item.column_start);
    hasher.write_u16(item.column_end);
  }
}

fn hash_grid_tracks(hasher: &mut FingerprintHasher, tracks: &GridTrackRanges) {
  hasher.write_slice(&tracks.rows, |hasher, (start, end)| {
    hasher.write_f32(*start);
    hasher.write_f32(*end);
  });
  hasher.write_slice(&tracks.columns, |hasher, (start, end)| {
    hasher.write_f32(*start);
    hasher.write_f32(*end);
  });
}

fn hash_table_borders(hasher: &mut FingerprintHasher, borders: &TableCollapsedBorders) {
  hasher.write_usize(borders.column_count);
  hasher.write_usize(borders.row_count);
  hasher.write_slice(&borders.column_line_positions, |hasher, pos| {
    hasher.write_f32(*pos);
  });
  hasher.write_slice(&borders.row_line_positions, |hasher, pos| {
    hasher.write_f32(*pos);
  });
  hasher.write_slice(&borders.vertical_borders, hash_collapsed_border_segment);
  hasher.write_slice(&borders.horizontal_borders, hash_collapsed_border_segment);
  hasher.write_slice(&borders.corner_borders, hash_collapsed_border_segment);
  hasher.write_slice(&borders.vertical_line_base, |hasher, v| {
    hasher.write_f32(*v)
  });
  hasher.write_slice(&borders.horizontal_line_base, |hasher, v| {
    hasher.write_f32(*v)
  });
  hash_rect(hasher, borders.paint_bounds);
  hasher.write_option(borders.header_rows.as_ref(), |hasher, (start, end)| {
    hasher.write_usize(*start);
    hasher.write_usize(*end);
  });
  hasher.write_option(borders.footer_rows.as_ref(), |hasher, (start, end)| {
    hasher.write_usize(*start);
    hasher.write_usize(*end);
  });
  hasher.write_bool(borders.fragment_local);
}

fn hash_collapsed_border_segment(
  hasher: &mut FingerprintHasher,
  seg: &crate::tree::fragment_tree::CollapsedBorderSegment,
) {
  hasher.write_f32(seg.width);
  hasher.write_str(&format!("{:?}", seg.style));
  hasher.write_u8(seg.color.r);
  hasher.write_u8(seg.color.g);
  hasher.write_u8(seg.color.b);
  hasher.write_f32(seg.color.a);
}

fn hash_style_summary(hasher: &mut FingerprintHasher, style: &ComputedStyle) {
  hasher.write_option(style.z_index.as_ref(), |hasher, z| {
    hasher.write_i32(*z);
  });
  hash_length_opt(hasher, style.margin_top.as_ref());
  hash_length_opt(hasher, style.margin_right.as_ref());
  hash_length_opt(hasher, style.margin_bottom.as_ref());
  hash_length_opt(hasher, style.margin_left.as_ref());
  hash_length(hasher, style.padding_top);
  hash_length(hasher, style.padding_right);
  hash_length(hasher, style.padding_bottom);
  hash_length(hasher, style.padding_left);
  hash_length(hasher, style.border_top_width);
  hash_length(hasher, style.border_right_width);
  hash_length(hasher, style.border_bottom_width);
  hash_length(hasher, style.border_left_width);
}

fn hash_length_opt(hasher: &mut FingerprintHasher, value: Option<&Length>) {
  match value {
    Some(len) => {
      hasher.write_u8(1);
      hash_length(hasher, *len);
    }
    None => hasher.write_u8(0),
  }
}

fn hash_length(hasher: &mut FingerprintHasher, length: Length) {
  if let Some(calc) = length.calc {
    hasher.write_u8(1);
    hasher.write_str(&calc.to_css());
  } else {
    hasher.write_u8(0);
    hasher.write_str(length.unit.as_str());
    hasher.write_f32(length.value);
  }
}

fn hash_fragment_content(hasher: &mut FingerprintHasher, content: &FragmentContent) {
  match content {
    FragmentContent::Block { box_id } => {
      hasher.write_u8(0);
      hash_box_id(hasher, *box_id);
    }
    FragmentContent::Inline {
      box_id,
      fragment_index,
    } => {
      hasher.write_u8(1);
      hash_box_id(hasher, *box_id);
      hasher.write_usize(*fragment_index);
    }
    FragmentContent::Text {
      text,
      box_id,
      source_range,
      baseline_offset,
      shaped,
      is_marker,
      emphasis_offset,
      ..
    } => {
      hasher.write_u8(2);
      hasher.write_str(text);
      hash_box_id(hasher, *box_id);
      hasher.write_option(source_range.as_ref(), hash_text_source_range);
      hasher.write_f32(*baseline_offset);
      hasher.write_bool(*is_marker);
      hasher.write_f32(emphasis_offset.over);
      hasher.write_f32(emphasis_offset.under);
      hasher.write_option(shaped.as_deref(), hash_shaped_runs);
    }
    FragmentContent::Line { baseline } => {
      hasher.write_u8(3);
      hasher.write_f32(*baseline);
    }
    FragmentContent::Replaced {
      replaced_type,
      box_id,
    } => {
      hasher.write_u8(4);
      hash_box_id(hasher, *box_id);
      hash_replaced_type(hasher, replaced_type);
    }
    FragmentContent::RunningAnchor { name, snapshot } => {
      hasher.write_u8(5);
      hasher.write_str(name);
      hash_fragment_node(hasher, snapshot.as_ref());
    }
    FragmentContent::FootnoteAnchor { snapshot, policy } => {
      hasher.write_u8(6);
      hash_fragment_node(hasher, snapshot.as_ref());
      hasher.write_u8(match policy {
        FootnotePolicy::Auto => 0,
        FootnotePolicy::Line => 1,
        FootnotePolicy::Block => 2,
      });
    }
  }
}

fn hash_box_id(hasher: &mut FingerprintHasher, box_id: Option<usize>) {
  match box_id {
    Some(id) => {
      hasher.write_u8(1);
      hasher.write_usize(id);
    }
    None => hasher.write_u8(0),
  }
}

fn hash_text_source_range(hasher: &mut FingerprintHasher, range: &TextSourceRange) {
  hasher.write_usize(range.start());
  hasher.write_usize(range.end());
}

fn hash_shaped_runs(hasher: &mut FingerprintHasher, runs: &Vec<ShapedRun>) {
  hasher.write_usize(runs.len());
  for run in runs {
    hasher.write_usize(run.start);
    hasher.write_usize(run.end);
    hasher.write_u8(match run.direction {
      crate::text::pipeline::Direction::LeftToRight => 0,
      crate::text::pipeline::Direction::RightToLeft => 1,
    });
    hasher.write_u8(run.level);
    hasher.write_f32(run.advance);
    hasher.write_f32(run.font_size);
    hasher.write_f32(run.baseline_shift);
    hasher.write_f32(run.synthetic_bold);
    hasher.write_f32(run.synthetic_oblique);
    hash_run_rotation(hasher, run.rotation);
    hasher.write_u8(u8::from(run.vertical));
    hasher.write_u16(run.palette_index);
    hasher.write_u64(run.palette_override_hash);
    hasher.write_f32(run.scale);

    hash_loaded_font_summary(hasher, &run.font);

    hasher.write_usize(run.glyphs.len());
    for glyph in &run.glyphs {
      hasher.write_u32(glyph.glyph_id);
      hasher.write_u32(glyph.cluster);
      hasher.write_f32(glyph.x_offset);
      hasher.write_f32(glyph.y_offset);
      hasher.write_f32(glyph.x_advance);
      hasher.write_f32(glyph.y_advance);
    }
  }
}

fn hash_loaded_font_summary(
  hasher: &mut FingerprintHasher,
  font: &Arc<crate::text::font_db::LoadedFont>,
) {
  // Avoid hashing raw font bytes; we only need enough stable metadata to detect fallback changes.
  hasher.write_str(&font.family);
  hasher.write_u32(font.index);
  hasher.write_str(&format!("{:?}", font.weight));
  hasher.write_str(&format!("{:?}", font.style));
  hasher.write_str(&format!("{:?}", font.stretch));
}

fn hash_run_rotation(hasher: &mut FingerprintHasher, rotation: RunRotation) {
  hasher.write_u8(match rotation {
    RunRotation::None => 0,
    RunRotation::Ccw90 => 1,
    RunRotation::Cw90 => 2,
  });
}

fn hash_replaced_type(hasher: &mut FingerprintHasher, replaced: &ReplacedType) {
  match replaced {
    ReplacedType::Image { src, alt, .. } => {
      hasher.write_u8(0);
      hasher.write_str(src);
      hasher.write_option(alt.as_ref(), |hasher, alt| hasher.write_str(alt));
    }
    ReplacedType::Video { src, .. } => {
      hasher.write_u8(1);
      hasher.write_str(src);
    }
    ReplacedType::Audio { src, .. } => {
      hasher.write_u8(2);
      hasher.write_str(src);
    }
    ReplacedType::Canvas => hasher.write_u8(3),
    ReplacedType::Svg { .. } => hasher.write_u8(4),
    ReplacedType::Iframe { src, srcdoc, .. } => {
      hasher.write_u8(5);
      hasher.write_str(src);
      hasher.write_option(srcdoc.as_ref(), |hasher, doc| hasher.write_str(doc));
    }
    ReplacedType::Embed { src } => {
      hasher.write_u8(6);
      hasher.write_str(src);
    }
    ReplacedType::Object { data } => {
      hasher.write_u8(7);
      hasher.write_str(data);
    }
    ReplacedType::Math(_) => hasher.write_u8(8),
    ReplacedType::FormControl(control) => {
      hasher.write_u8(9);
      hasher.write_str(&control.control.snapshot_label());
    }
  }
}
