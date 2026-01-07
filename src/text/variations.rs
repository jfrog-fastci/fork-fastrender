use std::collections::BTreeMap;

use crate::style::types::FontOpticalSizing;
use crate::style::types::FontStyle as CssFontStyle;
use crate::style::ComputedStyle;
use crate::text::font_db::LoadedFont;
use rustybuzz::Variation;
use ttf_parser::{Face, Tag};

/// A font variation setting consisting of a 4-byte axis tag and value.
#[derive(Debug, Clone, PartialEq)]
pub struct FontVariation {
  pub tag: Tag,
  pub value: f32,
}

impl FontVariation {
  pub fn new(tag: Tag, value: f32) -> Self {
    Self { tag, value }
  }
}

impl From<Variation> for FontVariation {
  fn from(v: Variation) -> Self {
    Self {
      tag: v.tag,
      value: v.value,
    }
  }
}

/// Applies variation coordinates to a parsed `ttf_parser::Face`.
///
/// The provided settings are clamped to the axis min/max values and applied in
/// deterministic tag order. Unknown axes are ignored.
pub fn apply_variations_to_face(face: &mut Face<'_>, variations: &[FontVariation]) {
  if variations.is_empty() {
    return;
  }

  let axes: Vec<_> = face.variation_axes().into_iter().collect();
  if axes.is_empty() {
    return;
  }

  let mut clamped: BTreeMap<[u8; 4], (Tag, f32)> = BTreeMap::new();
  for variation in variations {
    if let Some(axis) = axes.iter().find(|axis| axis.tag == variation.tag) {
      let value = variation.value.clamp(axis.min_value, axis.max_value);
      clamped.insert(variation.tag.to_bytes(), (variation.tag, value));
    }
  }

  for (_, (tag, value)) in clamped {
    let _ = face.set_variation(tag, value);
  }
}

/// Convenience wrapper for applying rustybuzz variations to a `ttf_parser::Face`.
pub fn apply_rustybuzz_variations(face: &mut Face<'_>, variations: &[Variation]) {
  if variations.is_empty() {
    return;
  }
  let mapped: Vec<_> = variations.iter().map(|v| FontVariation::from(*v)).collect();
  apply_variations_to_face(face, &mapped);
}

/// Converts [`FontVariation`] records to rustybuzz variation coordinates.
pub fn to_rustybuzz_variations(variations: &[FontVariation]) -> Vec<Variation> {
  variations
    .iter()
    .map(|v| Variation {
      tag: v.tag,
      value: v.value,
    })
    .collect()
}

pub(crate) fn authored_variations_from_style(style: &ComputedStyle) -> Vec<Variation> {
  style
    .font_variation_settings
    .iter()
    .map(|v| Variation {
      tag: Tag::from_bytes(&v.tag),
      value: v.value,
    })
    .collect()
}

pub(crate) fn collect_variations_for_face(
  face: &Face<'_>,
  style: &ComputedStyle,
  font: &LoadedFont,
  font_size: f32,
  authored_variations: &[Variation],
) -> Vec<Variation> {
  let axes: Vec<_> = face.variation_axes().into_iter().collect();
  if axes.is_empty() {
    return authored_variations.to_vec();
  }

  let mut variations: Vec<Variation> = Vec::new();
  let wght_tag = Tag::from_bytes(b"wght");
  let wdth_tag = Tag::from_bytes(b"wdth");
  let opsz_tag = Tag::from_bytes(b"opsz");
  let ital_tag = Tag::from_bytes(b"ital");
  let slnt_tag = Tag::from_bytes(b"slnt");

  let axis_bounds: BTreeMap<Tag, (f32, f32)> = axes
    .iter()
    .map(|axis| (axis.tag, (axis.min_value, axis.max_value)))
    .collect();

  let mut push_variation = |variations: &mut Vec<Variation>, tag: Tag, value: f32| {
    let Some((min, max)) = axis_bounds.get(&tag).copied() else {
      return;
    };
    let clamped = value.clamp(min, max);
    variations.retain(|v| v.tag != tag);
    variations.push(Variation {
      tag,
      value: clamped,
    });
  };

  // --------------------------------------------------------------------------
  // CSS Fonts 4: Feature and variation precedence
  // --------------------------------------------------------------------------
  // 2) Apply font matching variations (font-weight/font-width/font-style).
  // --------------------------------------------------------------------------
  let requested_angle = match style.font_style {
    CssFontStyle::Normal => 0.0,
    CssFontStyle::Italic => crate::text::pipeline::DEFAULT_OBLIQUE_ANGLE_DEG,
    CssFontStyle::Oblique(angle) => {
      angle.unwrap_or(crate::text::pipeline::DEFAULT_OBLIQUE_ANGLE_DEG)
    }
  };
  let has_slnt_axis = axis_bounds.contains_key(&slnt_tag);
  let has_ital_axis = axis_bounds.contains_key(&ital_tag);

  // Clamp style-derived values to the matched @font-face descriptors when available.
  let wght_base = {
    let base = font.weight.value() as f32;
    if let Some((min, max)) = font.face_settings.weight_range {
      base.clamp(min as f32, max as f32)
    } else {
      base
    }
  };
  let wdth_base = {
    if let Some((min, max)) = font.face_settings.stretch_range {
      style.font_stretch.to_percentage().clamp(min, max)
    } else {
      font.stretch.to_percentage()
    }
  };

  if axis_bounds.contains_key(&wght_tag) {
    push_variation(&mut variations, wght_tag, wght_base);
  }
  if axis_bounds.contains_key(&wdth_tag) {
    push_variation(&mut variations, wdth_tag, wdth_base);
  }

  let clamped_oblique = match font.face_settings.style.as_ref() {
    Some(crate::css::types::FontFaceStyle::Oblique { range }) => {
      let (start, end) = range.unwrap_or((
        crate::text::pipeline::DEFAULT_OBLIQUE_ANGLE_DEG,
        crate::text::pipeline::DEFAULT_OBLIQUE_ANGLE_DEG,
      ));
      requested_angle.clamp(start, end)
    }
    _ => requested_angle,
  };

  // Apply at most one of ital/slnt (CSS Fonts 4 §#feature-variation-precedence).
  match font.style {
    crate::text::font_db::FontStyle::Normal => {
      if has_ital_axis {
        push_variation(&mut variations, ital_tag, 0.0);
      } else if has_slnt_axis {
        push_variation(&mut variations, slnt_tag, 0.0);
      }
    }
    crate::text::font_db::FontStyle::Italic => {
      if has_ital_axis {
        push_variation(&mut variations, ital_tag, 1.0);
      } else if has_slnt_axis {
        push_variation(
          &mut variations,
          slnt_tag,
          -crate::text::pipeline::DEFAULT_OBLIQUE_ANGLE_DEG,
        );
      }
    }
    crate::text::font_db::FontStyle::Oblique => {
      if has_slnt_axis {
        push_variation(&mut variations, slnt_tag, -clamped_oblique);
      } else if has_ital_axis {
        push_variation(&mut variations, ital_tag, 1.0);
      }
    }
  }

  // --------------------------------------------------------------------------
  // 5) Apply named instance axis values.
  // --------------------------------------------------------------------------
  if let Some(instance_name) = font.face_settings.font_named_instance.as_deref() {
    if let Some(instance_coords) = named_instance_coords(face, instance_name, axes.as_slice()) {
      for (tag, value) in instance_coords {
        push_variation(&mut variations, tag, value);
      }
    }
  }

  // --------------------------------------------------------------------------
  // 6) Apply @font-face font-variation-settings descriptor.
  // --------------------------------------------------------------------------
  if let Some(settings) = font.face_settings.font_variation_settings.as_deref() {
    for setting in settings {
      let tag = Tag::from_bytes(&setting.tag);
      push_variation(&mut variations, tag, setting.value);
    }
  }

  // --------------------------------------------------------------------------
  // 9) Apply font-optical-sizing-derived opsz after @font-face descriptors.
  // --------------------------------------------------------------------------
  if axis_bounds.contains_key(&opsz_tag)
    && matches!(style.font_optical_sizing, FontOpticalSizing::Auto)
  {
    // `size-adjust` scales the used font size for this face, so auto optical sizing should use
    // the same effective size that will be used when scaling glyph advances.
    push_variation(
      &mut variations,
      opsz_tag,
      font_size * font.face_metrics_overrides.size_adjust,
    );
  }

  // --------------------------------------------------------------------------
  // 12) Apply author font-variation-settings property last.
  // --------------------------------------------------------------------------
  for authored in authored_variations {
    push_variation(&mut variations, authored.tag, authored.value);
  }

  variations
}

fn read_u16_be(data: &[u8], offset: usize) -> Option<u16> {
  data
    .get(offset..offset + 2)
    .map(|b| u16::from_be_bytes([b[0], b[1]]))
}

fn read_i32_be(data: &[u8], offset: usize) -> Option<i32> {
  data
    .get(offset..offset + 4)
    .map(|b| i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

fn read_fixed(data: &[u8], offset: usize) -> Option<f32> {
  let raw = read_i32_be(data, offset)?;
  Some(raw as f32 / 65536.0)
}

fn case_fold(value: &str) -> String {
  let mut out = String::with_capacity(value.len());
  for ch in value.chars() {
    match ch {
      '\u{00DF}' | '\u{1E9E}' => out.push_str("ss"),
      '\u{03C2}' => out.push('\u{03C3}'),
      '\u{212A}' => out.push('k'),
      '\u{212B}' => out.push('\u{00E5}'),
      _ => {
        for lower in ch.to_lowercase() {
          out.push(lower);
        }
      }
    }
  }
  out
}

fn named_instance_coords(
  face: &Face<'_>,
  target_name: &str,
  axes: &[ttf_parser::VariationAxis],
) -> Option<Vec<(Tag, f32)>> {
  let target = case_fold(target_name.trim());
  if target.is_empty() {
    return None;
  }

  let fvar = face.raw_face().table(Tag::from_bytes(b"fvar"))?;
  if fvar.len() < 16 {
    return None;
  }

  let axis_count = read_u16_be(fvar, 8)? as usize;
  let axis_size = read_u16_be(fvar, 10)? as usize;
  let instance_count = read_u16_be(fvar, 12)? as usize;
  let instance_size = read_u16_be(fvar, 14)? as usize;
  let axes_offset = read_u16_be(fvar, 4)? as usize;
  let instances_offset = axes_offset.checked_add(axis_count.checked_mul(axis_size)?)?;

  let axis_count = axis_count.min(axes.len());
  for instance_idx in 0..instance_count {
    let offset = instances_offset.checked_add(instance_idx.checked_mul(instance_size)?)?;
    let name_id = read_u16_be(fvar, offset)?; // subfamilyNameID
    let coords_offset = offset.checked_add(4)?;
    if coords_offset.checked_add(axis_count.checked_mul(4)?)? > fvar.len() {
      continue;
    }

    if !face
      .names()
      .into_iter()
      .filter(|name| name.name_id == name_id)
      .filter_map(|name| name.to_string())
      .any(|name| case_fold(&name) == target)
    {
      continue;
    }

    let mut coords = Vec::new();
    for axis_idx in 0..axis_count {
      let value = read_fixed(fvar, coords_offset + axis_idx * 4)?;
      let tag = axes[axis_idx].tag;
      coords.push((tag, value));
    }
    return Some(coords);
  }

  None
}

const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

#[inline]
fn f32_to_canonical_bits(value: f32) -> u32 {
  if value == 0.0 {
    0.0f32.to_bits()
  } else {
    value.to_bits()
  }
}

fn fnv1a_extend(mut hash: u64, byte: u8) -> u64 {
  hash ^= byte as u64;
  hash.wrapping_mul(FNV_PRIME)
}

/// Stable variation hash used for cache keys.
pub fn variation_hash(variations: &[Variation]) -> u64 {
  let mut entries: Vec<([u8; 4], u32)> = variations
    .iter()
    .map(|v| (v.tag.to_bytes(), f32_to_canonical_bits(v.value)))
    .collect();
  entries.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));

  let mut hash = FNV_OFFSET_BASIS;
  for (tag, value_bits) in entries {
    for byte in tag {
      hash = fnv1a_extend(hash, byte);
    }
    for byte in value_bits.to_be_bytes() {
      hash = fnv1a_extend(hash, byte);
    }
  }
  hash
}

/// Normalized variation coordinates for a font face.
///
/// [`ordered`] follows the axis order returned by [`Face::variation_axes`], while
/// [`by_tag`] allows quick lookups by axis tag.
#[derive(Debug, Clone, Default)]
pub struct NormalizedCoords {
  pub ordered: Vec<f32>,
  pub by_tag: BTreeMap<Tag, f32>,
}

/// Computes normalized coordinates for each variation axis in the face.
///
/// Values are clamped to each axis' min/max, normalized against the default per the
/// OpenType spec, and limited to the [-1.0, 1.0] range.
pub fn normalized_coords(face: &Face<'_>, variations: &[Variation]) -> NormalizedCoords {
  let axes: Vec<_> = face.variation_axes().into_iter().collect();
  if axes.is_empty() {
    return NormalizedCoords::default();
  }

  let mut by_tag = BTreeMap::new();
  let ordered = axes
    .iter()
    .map(|axis| {
      let requested = variations
        .iter()
        .find(|v| v.tag == axis.tag)
        .map(|v| v.value)
        .unwrap_or(axis.def_value);
      let clamped = requested.clamp(axis.min_value, axis.max_value);
      let normalized = if clamped < axis.def_value {
        if axis.def_value == axis.min_value {
          0.0
        } else {
          (clamped - axis.def_value) / (axis.def_value - axis.min_value)
        }
      } else if clamped > axis.def_value {
        if axis.max_value == axis.def_value {
          0.0
        } else {
          (clamped - axis.def_value) / (axis.max_value - axis.def_value)
        }
      } else {
        0.0
      }
      .clamp(-1.0, 1.0);
      by_tag.insert(axis.tag, normalized);
      normalized
    })
    .collect();

  NormalizedCoords { ordered, by_tag }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::text::font_loader::FontContext;

  #[test]
  fn applying_variations_is_stable_on_non_variable_faces() {
    let ctx = FontContext::new();
    let Some(font) = ctx.get_sans_serif() else {
      return;
    };
    let Ok(cached_face) = font.as_cached_face() else {
      return;
    };
    let mut face = cached_face.clone_face();
    let Some(glyph) = face.glyph_index('A') else {
      return;
    };

    let before = face.glyph_bounding_box(glyph);
    let variations = vec![
      FontVariation {
        tag: Tag::from_bytes(b"wght"),
        value: 900.0,
      },
      FontVariation {
        tag: Tag::from_bytes(b"wdth"),
        value: 10.0,
      },
    ];
    apply_variations_to_face(&mut face, &variations);
    let after_first = face.glyph_bounding_box(glyph);

    let mut reversed = variations;
    reversed.reverse();
    apply_variations_to_face(&mut face, &reversed);
    let after_second = face.glyph_bounding_box(glyph);

    assert_eq!(before, after_first);
    assert_eq!(after_first, after_second);
  }

  #[test]
  fn variation_hash_canonicalizes_negative_zero() {
    let tag = Tag::from_bytes(b"wght");
    let positive = [Variation { tag, value: 0.0 }];
    let negative = [Variation { tag, value: -0.0 }];

    assert_eq!(variation_hash(&positive), variation_hash(&negative));
  }
}
