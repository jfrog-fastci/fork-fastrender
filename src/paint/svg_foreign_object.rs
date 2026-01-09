//! SVG `<foreignObject>` rendering helpers.
//!
//! Resvg cannot render HTML inside `<foreignObject>`. FastRender serializes SVG subtrees with
//! placeholder markers and captures the subtree HTML + computed styles separately. During paint we
//! render each captured HTML fragment via the normal HTML pipeline and inject the resulting pixels
//! back into the SVG as `<image href="data:image/png;base64,…">`.

use crate::api::layout_html_with_shared_resources;
use crate::fallible_vec_writer::FallibleVecWriter;
use crate::image_output::{encode_image, OutputFormat};
use crate::image_loader::ImageCache;
use crate::paint::display_list_renderer::PaintParallelism;
use crate::paint::painter::{
  paint_backend_from_env, paint_tree_with_resources_scaled_offset_backend_with_iframe_depth,
};
use crate::resource::data_url;
use crate::scroll::ScrollState;
use crate::svg::{parse_svg_view_box, SvgMeetOrSlice, SvgPreserveAspectRatio};
use crate::style::color::Rgba;
use crate::style::types::{Direction, FontStyle as CssFontStyle, Overflow, WritingMode};
use crate::text::font_loader::FontContext;
use crate::tree::box_tree::ForeignObjectInfo;
use crate::tree::fragment_tree::FragmentTree;
use crate::{Point, Rect};
use std::borrow::Cow;
use std::fmt::Write as _;
use std::io::Write as _;
use std::sync::Arc;
use tiny_skia::{Pixmap, Transform};

#[inline]
fn is_xml_whitespace(ch: char) -> bool {
  matches!(ch, '\u{0009}' | '\u{000A}' | '\u{000D}' | ' ')
}

fn trim_xml_whitespace(value: &str) -> &str {
  value.trim_matches(is_xml_whitespace)
}

fn trim_xml_whitespace_end(value: &str) -> &str {
  value.trim_end_matches(is_xml_whitespace)
}

#[inline]
fn is_ascii_whitespace_html_css(ch: char) -> bool {
  matches!(ch, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' ')
}

fn trim_ascii_whitespace_html_css(value: &str) -> &str {
  value.trim_matches(is_ascii_whitespace_html_css)
}

#[inline]
fn stable_hash64(bytes: &[u8]) -> u64 {
  // Deterministic FNV-1a hash so generated ids are stable across processes/targets.
  const OFFSET_BASIS: u64 = 0xcbf29ce484222325;
  const PRIME: u64 = 0x100000001b3;
  let mut hash = OFFSET_BASIS;
  for &b in bytes {
    hash ^= u64::from(b);
    hash = hash.wrapping_mul(PRIME);
  }
  hash
}

fn foreign_object_clip_path_id(info: &ForeignObjectInfo, idx: usize) -> String {
  let placeholder: Cow<'_, str> = if info.placeholder.is_empty() {
    Cow::Owned(format!("<!--FASTRENDER_FOREIGN_OBJECT_{}-->", idx))
  } else {
    Cow::Borrowed(info.placeholder.as_str())
  };
  let hash = stable_hash64(placeholder.as_bytes());
  format!("fastr-fo-{idx}-{hash:016x}")
}

// ForeignObject rendering constructs a synthetic HTML document containing the serialized subtree
// HTML plus a copy of the document-level CSS. Cap the total size so pathological SVGs cannot force
// multi-megabyte allocations (and potentially OOM aborts) during this nested render path.
const MAX_FOREIGN_OBJECT_DOC_BYTES: usize = 8 * 1024 * 1024;
// Extracting foreignObject HTML from SVG markup requires copying the subtree into an owned string
// so it can be fed through the HTML pipeline. Cap the total extracted markup to avoid pathological
// SVGs forcing unbounded allocations.
const MAX_FOREIGN_OBJECT_EXTRACTED_BYTES: usize = MAX_FOREIGN_OBJECT_DOC_BYTES;
const MAX_FOREIGN_OBJECTS_PER_SVG: usize = 64;

#[derive(Debug, Clone)]
pub(crate) struct ExtractedForeignObjectMarkup {
  pub placeholder: String,
  pub attributes: Vec<(String, String)>,
  pub x: Option<f32>,
  pub y: Option<f32>,
  pub width: Option<f32>,
  pub height: Option<f32>,
  pub html: String,
}

fn svg_markup_contains_foreign_object(svg: &str) -> bool {
  const NEEDLE: &[u8] = b"foreignobject";
  let bytes = svg.as_bytes();
  bytes
    .windows(NEEDLE.len())
    .any(|window| window.eq_ignore_ascii_case(NEEDLE))
}

fn parse_svg_number_attr(value: &str) -> Option<f32> {
  let value = trim_xml_whitespace(value);
  if value.is_empty() {
    return None;
  }

  // SVG numeric attributes are commonly written as `12`, `12.5`, or `12px`. Be conservative and
  // reject percentage-based lengths or unknown units.
  let bytes = value.as_bytes();
  let mut end = 0usize;
  while end < bytes.len() {
    let b = bytes[end];
    if b.is_ascii_digit() || matches!(b, b'+' | b'-' | b'.' | b'e' | b'E') {
      end += 1;
    } else {
      break;
    }
  }
  if end == 0 {
    return None;
  }
  let num = value[..end].parse::<f32>().ok()?;
  if !num.is_finite() {
    return None;
  }

  let suffix = trim_xml_whitespace(&value[end..]);
  if suffix.is_empty() || suffix.eq_ignore_ascii_case("px") {
    Some(num)
  } else {
    None
  }
}

fn parse_overflow_keyword(value: &str) -> Option<Overflow> {
  let value = trim_ascii_whitespace_html_css(value);
  if value.eq_ignore_ascii_case("visible") {
    Some(Overflow::Visible)
  } else if value.eq_ignore_ascii_case("hidden") {
    Some(Overflow::Hidden)
  } else if value.eq_ignore_ascii_case("scroll") {
    Some(Overflow::Scroll)
  } else if value.eq_ignore_ascii_case("auto") {
    Some(Overflow::Auto)
  } else if value.eq_ignore_ascii_case("clip") {
    Some(Overflow::Clip)
  } else {
    None
  }
}

fn parse_foreign_object_overflow(attrs: &[(String, String)]) -> (Overflow, Overflow) {
  let mut overflow_x = Overflow::Visible;
  let mut overflow_y = Overflow::Visible;

  for (name, value) in attrs {
    if name.eq_ignore_ascii_case("overflow") {
      if let Some(overflow) = parse_overflow_keyword(value) {
        overflow_x = overflow;
        overflow_y = overflow;
      }
    } else if name.eq_ignore_ascii_case("style") {
      for decl in value.split(';') {
        let decl = trim_ascii_whitespace_html_css(decl);
        if decl.is_empty() {
          continue;
        }
        let Some((prop, prop_value)) = decl.split_once(':') else {
          continue;
        };
        let prop = trim_ascii_whitespace_html_css(prop);
        let prop_value = trim_ascii_whitespace_html_css(prop_value);
        if prop.eq_ignore_ascii_case("overflow") {
          if let Some(overflow) = parse_overflow_keyword(prop_value) {
            overflow_x = overflow;
            overflow_y = overflow;
          }
        } else if prop.eq_ignore_ascii_case("overflow-x") {
          if let Some(overflow) = parse_overflow_keyword(prop_value) {
            overflow_x = overflow;
          }
        } else if prop.eq_ignore_ascii_case("overflow-y") {
          if let Some(overflow) = parse_overflow_keyword(prop_value) {
            overflow_y = overflow;
          }
        }
      }
    }
  }

  (overflow_x, overflow_y)
}

fn parse_foreign_object_opacity(attrs: &[(String, String)]) -> f32 {
  let mut opacity: Option<f32> = None;
  for (name, value) in attrs {
    if name.eq_ignore_ascii_case("opacity") {
      opacity = trim_ascii_whitespace_html_css(value).parse::<f32>().ok();
    } else if name.eq_ignore_ascii_case("style") {
      for decl in value.split(';') {
        let decl = trim_ascii_whitespace_html_css(decl);
        if decl.is_empty() {
          continue;
        }
        let Some((prop, prop_value)) = decl.split_once(':') else {
          continue;
        };
        if trim_ascii_whitespace_html_css(prop).eq_ignore_ascii_case("opacity") {
          opacity = trim_ascii_whitespace_html_css(prop_value).parse::<f32>().ok();
        }
      }
    }
  }
  let opacity = opacity.unwrap_or(1.0);
  if opacity.is_finite() {
    opacity.clamp(0.0, 1.0)
  } else {
    1.0
  }
}

fn foreign_object_inner_markup(svg: &str, node: roxmltree::Node<'_, '_>) -> Option<String> {
  let mut start: Option<usize> = None;
  let mut end: Option<usize> = None;
  for child in node.children() {
    let range = child.range();
    start = Some(start.map_or(range.start, |s| s.min(range.start)));
    end = Some(end.map_or(range.end, |e| e.max(range.end)));
  }

  let Some((start, end)) = start.zip(end) else {
    return Some(String::new());
  };
  if end < start || end > svg.len() {
    return None;
  }
  let slice = svg.get(start..end)?;
  if slice.len() > MAX_FOREIGN_OBJECT_EXTRACTED_BYTES {
    return None;
  }
  Some(slice.to_string())
}

/// Parses raw SVG markup and replaces each `<foreignObject>` subtree with a placeholder comment.
///
/// This is used when rasterizing SVG images loaded from external sources (e.g. `<img src=...>`),
/// where the DOM serialization path that normally captures foreignObject metadata is not available.
pub(crate) fn extract_foreign_objects_from_svg_markup(
  svg: &str,
) -> Option<(String, Vec<ExtractedForeignObjectMarkup>)> {
  if !svg_markup_contains_foreign_object(svg) {
    return None;
  }

  let doc = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| roxmltree::Document::parse(svg)))
    .ok()
    .and_then(|doc| doc.ok())?;
  let mut nodes: Vec<roxmltree::Node<'_, '_>> = doc
    .descendants()
    .filter(|node| {
      if !node.is_element() {
        return false;
      }
      let tag = node.tag_name();
      if !tag.name().eq_ignore_ascii_case("foreignObject") {
        return false;
      }
      match tag.namespace() {
        Some("http://www.w3.org/2000/svg") | None => true,
        _ => false,
      }
    })
    .collect();

  if nodes.is_empty() {
    return None;
  }
  if nodes.len() > MAX_FOREIGN_OBJECTS_PER_SVG {
    nodes.truncate(MAX_FOREIGN_OBJECTS_PER_SVG);
  }

  // Ensure nodes are processed in document order so placeholders line up with visual ordering.
  nodes.sort_by_key(|node| node.range().start);

  let mut out_svg = String::new();
  // Reserve for the original markup plus placeholder overhead.
  let placeholder_overhead = nodes
    .len()
    .saturating_mul("<!--FASTRENDER_FOREIGN_OBJECT_-->".len() + 16);
  out_svg
    .try_reserve_exact(svg.len().saturating_add(placeholder_overhead))
    .ok()?;

  let mut extracted: Vec<ExtractedForeignObjectMarkup> = Vec::new();
  extracted.try_reserve(nodes.len()).ok()?;

  let mut cursor = 0usize;
  let mut total_extracted_bytes = 0usize;
  let mut placeholder_index = 0usize;
  for node in nodes {
    let range = node.range();
    if range.start < cursor || range.end < range.start || range.end > svg.len() {
      continue;
    }
    out_svg.push_str(svg.get(cursor..range.start)?);

    let placeholder = format!("<!--FASTRENDER_FOREIGN_OBJECT_{}-->", placeholder_index);
    placeholder_index += 1;
    out_svg.push_str(&placeholder);
    cursor = range.end;

    let mut attrs: Vec<(String, String)> = Vec::new();
    attrs.try_reserve(node.attributes().len()).ok()?;
    for attr in node.attributes() {
      attrs.push((attr.name().to_string(), attr.value().to_string()));
    }

    let html = if total_extracted_bytes >= MAX_FOREIGN_OBJECT_EXTRACTED_BYTES {
      String::new()
    } else {
      match foreign_object_inner_markup(svg, node) {
        Some(html) => {
          total_extracted_bytes = total_extracted_bytes.saturating_add(html.len());
          if total_extracted_bytes <= MAX_FOREIGN_OBJECT_EXTRACTED_BYTES {
            html
          } else {
            // Exceeded the global budget; drop the extracted markup and stop attempting further
            // nested extraction (but still strip the foreignObject nodes from the SVG).
            String::new()
          }
        }
        None => String::new(),
      }
    };

    let x = node.attribute("x").and_then(parse_svg_number_attr);
    let y = node.attribute("y").and_then(parse_svg_number_attr);
    let width = node.attribute("width").and_then(parse_svg_number_attr);
    let height = node.attribute("height").and_then(parse_svg_number_attr);

    extracted.push(ExtractedForeignObjectMarkup {
      placeholder,
      attributes: attrs,
      x,
      y,
      width,
      height,
      html,
    });
  }

  out_svg.push_str(svg.get(cursor..)?);

  Some((out_svg, extracted))
}

/// Best-effort foreignObject inlining for arbitrary SVG markup strings.
///
/// Unlike `inline_svg_with_foreign_objects` (which is fed pre-extracted `ForeignObjectInfo`
/// structures from the DOM serialization pipeline), this helper parses the SVG markup directly and
/// skips individual foreignObjects that cannot be resolved (e.g. missing dimensions).
pub(crate) fn inline_svg_foreign_objects_from_markup(
  svg: &str,
  shared_css: &str,
  font_ctx: &FontContext,
  image_cache: &ImageCache,
  device_pixel_ratio: f32,
  max_iframe_depth: usize,
) -> Option<String> {
  let (placeholder_svg, extracted) = extract_foreign_objects_from_svg_markup(svg)?;
  if extracted.is_empty() {
    return Some(placeholder_svg);
  }

  let svg_doc =
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| roxmltree::Document::parse(&placeholder_svg)))
    .ok()
    .and_then(|doc| doc.ok());

  let default_style = Arc::new(crate::style::ComputedStyle::default());

  let mut out_svg = String::new();
  out_svg.try_reserve_exact(placeholder_svg.len()).ok()?;
  out_svg.push_str(&placeholder_svg);

  for (idx, extracted) in extracted.into_iter().enumerate() {
    if trim_xml_whitespace(&extracted.html).is_empty() {
      replace_placeholder_or_insert(&mut out_svg, &extracted.placeholder, "")?;
      continue;
    }

    let x = extracted.x.unwrap_or(0.0);
    let y = extracted.y.unwrap_or(0.0);
    let Some(width) = extracted.width.filter(|v| v.is_finite() && *v > 0.0) else {
      replace_placeholder_or_insert(&mut out_svg, &extracted.placeholder, "")?;
      continue;
    };
    let Some(height) = extracted.height.filter(|v| v.is_finite() && *v > 0.0) else {
      replace_placeholder_or_insert(&mut out_svg, &extracted.placeholder, "")?;
      continue;
    };

    let opacity = parse_foreign_object_opacity(&extracted.attributes);
    let (overflow_x, overflow_y) = parse_foreign_object_overflow(&extracted.attributes);

    let info = ForeignObjectInfo {
      placeholder: extracted.placeholder,
      attributes: extracted.attributes,
      x,
      y,
      width,
      height,
      opacity,
      background: None,
      html: extracted.html,
      style: Arc::clone(&default_style),
      overflow_x,
      overflow_y,
    };

    let transform_scale = foreign_object_transform_scale(svg_doc.as_ref(), &info.placeholder, &info.attributes);
    let replacement = (|| {
      let (data_url, image_bounds) = render_foreign_object_data_url(
        &info,
        shared_css,
        font_ctx,
        image_cache,
        device_pixel_ratio * transform_scale,
        max_iframe_depth,
      )?;
      foreign_object_image_tag(&info, &data_url, idx, image_bounds)
    })()
    .unwrap_or_default();
    replace_placeholder_or_insert(&mut out_svg, &info.placeholder, &replacement)?;
  }

  Some(out_svg)
}

/// Compute the device pixel ratio to use when rasterizing `<foreignObject>` HTML into a PNG.
///
/// The nested HTML is laid out in CSS px that correspond to SVG user units. When the SVG is
/// rendered at a different size (e.g. due to `viewBox` scaling or CSS object-fit), the
/// `<foreignObject>` subtree needs to be rasterized at a higher/lower DPR so the embedded PNG lands
/// at native resolution in the final SVG pixmap.
pub(crate) fn foreign_object_html_device_pixel_ratio(
  svg: &str,
  outer_device_pixel_ratio: f32,
  rendered_width_css: f32,
  rendered_height_css: f32,
  intrinsic_width_css: f32,
  intrinsic_height_css: f32,
) -> f32 {
  let outer_device_pixel_ratio = if outer_device_pixel_ratio.is_finite() && outer_device_pixel_ratio > 0.0 {
    outer_device_pixel_ratio
  } else {
    1.0
  };
  if !(rendered_width_css.is_finite()
    && rendered_height_css.is_finite()
    && rendered_width_css > 0.0
    && rendered_height_css > 0.0)
  {
    return outer_device_pixel_ratio;
  }

  let root_parse = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| roxmltree::Document::parse(svg)));
  let (view_box, preserve) = match root_parse {
    Ok(Ok(doc)) => {
      let root = doc.root_element();
      if root.tag_name().name().eq_ignore_ascii_case("svg") {
        let view_box = root.attribute("viewBox").and_then(parse_svg_view_box);
        let preserve = SvgPreserveAspectRatio::parse(root.attribute("preserveAspectRatio"));
        (view_box, Some(preserve))
      } else {
        (None, None)
      }
    }
    _ => (None, None),
  };

  let scale_factor = if let (Some(view_box), Some(preserve)) = (view_box, preserve) {
    let sx = rendered_width_css / view_box.width;
    let sy = rendered_height_css / view_box.height;
    if !(sx.is_finite() && sy.is_finite() && sx > 0.0 && sy > 0.0) {
      1.0
    } else if preserve.none {
      sx.max(sy)
    } else {
      match preserve.meet_or_slice {
        SvgMeetOrSlice::Meet => sx.min(sy),
        SvgMeetOrSlice::Slice => sx.max(sy),
      }
    }
  } else if intrinsic_width_css.is_finite()
    && intrinsic_height_css.is_finite()
    && intrinsic_width_css > 0.0
    && intrinsic_height_css > 0.0
  {
    let sx = rendered_width_css / intrinsic_width_css;
    let sy = rendered_height_css / intrinsic_height_css;
    if sx.is_finite() && sy.is_finite() && sx > 0.0 && sy > 0.0 {
      sx.max(sy)
    } else {
      1.0
    }
  } else {
    1.0
  };

  let scale_factor = if scale_factor.is_finite() && scale_factor > 0.0 {
    scale_factor
  } else {
    1.0
  };
  outer_device_pixel_ratio * scale_factor
}

fn transform_scale_factor(transform: Transform) -> f32 {
  let scale_x = (transform.sx * transform.sx + transform.ky * transform.ky).sqrt();
  let scale_y = (transform.kx * transform.kx + transform.sy * transform.sy).sqrt();
  if scale_x.is_finite() && scale_y.is_finite() && scale_x > 0.0 && scale_y > 0.0 {
    scale_x.max(scale_y)
  } else {
    1.0
  }
}

fn parse_svg_transform_attribute(value: &str) -> Option<Transform> {
  let mut combined = Transform::identity();
  for item in svgtypes::TransformListParser::from(value) {
    let item = item.ok()?;
    let t = match item {
      svgtypes::TransformListToken::Matrix { a, b, c, d, e, f } => {
        Transform::from_row(a as f32, b as f32, c as f32, d as f32, e as f32, f as f32)
      }
      svgtypes::TransformListToken::Translate { tx, ty } => {
        Transform::from_translate(tx as f32, ty as f32)
      }
      svgtypes::TransformListToken::Scale { sx, sy } => {
        Transform::from_scale(sx as f32, sy as f32)
      }
      svgtypes::TransformListToken::Rotate { angle } => Transform::from_rotate(angle as f32),
      svgtypes::TransformListToken::SkewX { angle } => {
        let tan = (angle as f32).to_radians().tan();
        Transform::from_row(1.0, 0.0, tan, 1.0, 0.0, 0.0)
      }
      svgtypes::TransformListToken::SkewY { angle } => {
        let tan = (angle as f32).to_radians().tan();
        Transform::from_row(1.0, tan, 0.0, 1.0, 0.0, 0.0)
      }
    };
    combined = combined.pre_concat(t);
  }
  Some(combined)
}

fn parse_svg_px_or_unitless_number(value: &str) -> Option<f32> {
  let trimmed = trim_xml_whitespace(value);
  if trimmed.is_empty() {
    return None;
  }

  let mut end = 0usize;
  for (idx, ch) in trimmed.char_indices() {
    if matches!(ch, '0'..='9' | '+' | '-' | '.' | 'e' | 'E') {
      end = idx + ch.len_utf8();
    } else {
      break;
    }
  }

  if end == 0 {
    return None;
  }

  let number = trimmed[..end].parse::<f32>().ok()?;
  if !number.is_finite() {
    return None;
  }

  let unit = trim_xml_whitespace(&trimmed[end..]);
  if unit.is_empty() || unit.eq_ignore_ascii_case("px") {
    Some(number)
  } else {
    None
  }
}

fn foreign_object_transform_scale(
  svg_doc: Option<&roxmltree::Document<'_>>,
  placeholder: &str,
  attributes: &[(String, String)],
) -> f32 {
  let mut combined = Transform::identity();
  let mut nested_view_box_scale = 1.0f32;

  if let Some(doc) = svg_doc {
    let placeholder_trimmed = trim_xml_whitespace(placeholder);
    let needle = placeholder_trimmed
      .strip_prefix("<!--")
      .and_then(|s| s.strip_suffix("-->"))
      .unwrap_or(placeholder_trimmed);
    let needle = trim_xml_whitespace(needle);

    if !needle.is_empty() {
      let comment = doc
        .descendants()
        .find(|node| node.is_comment() && node.text().is_some_and(|t| trim_xml_whitespace(t) == needle));
      if let Some(comment) = comment {
        let mut current = comment.parent();
        while let Some(node) = current {
          if node.is_element() {
            if node.tag_name().name().eq_ignore_ascii_case("svg")
              && node.parent().is_some_and(|parent| parent.is_element())
            {
              if let Some(view_box) = node.attribute("viewBox").and_then(parse_svg_view_box) {
                let viewport_width = node
                  .attribute("width")
                  .and_then(parse_svg_px_or_unitless_number);
                let viewport_height = node
                  .attribute("height")
                  .and_then(parse_svg_px_or_unitless_number);
                if let (Some(viewport_width), Some(viewport_height)) = (viewport_width, viewport_height) {
                  let sx = viewport_width / view_box.width;
                  let sy = viewport_height / view_box.height;
                  if sx.is_finite() && sy.is_finite() && sx > 0.0 && sy > 0.0 {
                    let preserve =
                      SvgPreserveAspectRatio::parse(node.attribute("preserveAspectRatio"));
                    let scale = if preserve.none {
                      sx.max(sy)
                    } else {
                      match preserve.meet_or_slice {
                        SvgMeetOrSlice::Meet => sx.min(sy),
                        SvgMeetOrSlice::Slice => sx.max(sy),
                      }
                    };
                    if scale.is_finite() && scale > 0.0 {
                      nested_view_box_scale *= scale;
                    }
                  }
                }
              }
            }

            for attr in node.attributes() {
              if attr.name().eq_ignore_ascii_case("transform") {
                if let Some(t) = parse_svg_transform_attribute(attr.value()) {
                  combined = t.pre_concat(combined);
                }
                break;
              }
            }
          }
          current = node.parent();
        }
      }
    }
  }

  for (name, value) in attributes {
    if name.eq_ignore_ascii_case("transform") {
      if let Some(t) = parse_svg_transform_attribute(value) {
        combined = combined.pre_concat(t);
      }
      break;
    }
  }

  let view_box_scale = if nested_view_box_scale.is_finite() && nested_view_box_scale > 0.0 {
    nested_view_box_scale
  } else {
    1.0
  };

  (transform_scale_factor(combined) * view_box_scale).max(1.0)
}

fn replace_placeholder_or_insert(svg: &mut String, placeholder: &str, replacement: &str) -> Option<()> {
  if let Some(pos) = svg.find(placeholder) {
    let end = pos + placeholder.len();
    if replacement.len() > placeholder.len() {
      svg
        .try_reserve(replacement.len().saturating_sub(placeholder.len()))
        .ok()?;
    }
    svg.replace_range(pos..end, replacement);
  } else if let Some(close_pos) = svg.rfind("</svg>") {
    svg.try_reserve(replacement.len()).ok()?;
    svg.insert_str(close_pos, replacement);
  } else if let Some(close_pos) = find_self_closing_root_svg_end(svg) {
    let mut suffix = String::new();
    suffix
      .try_reserve_exact(replacement.len().saturating_add("</svg>".len() + 1))
      .ok()?;
    suffix.push('>');
    suffix.push_str(replacement);
    suffix.push_str("</svg>");

    svg.try_reserve(suffix.len().saturating_sub(2)).ok()?;
    svg.replace_range(close_pos..close_pos + 2, &suffix);
  } else {
    svg.try_reserve(replacement.len()).ok()?;
    svg.push_str(replacement);
  }
  Some(())
}

fn find_self_closing_root_svg_end(svg: &str) -> Option<usize> {
  let trimmed = trim_xml_whitespace_end(svg);
  if !trimmed.ends_with("/>") {
    return None;
  }

  // Only treat the SVG root as self-closing if it contains no other tags besides the `<svg .../>`
  // element. This avoids corrupting markup like `<svg><rect/>` where the last `/>` belongs to a
  // child element.
  let bytes = trimmed.as_bytes();
  if bytes.len() < 4 {
    return None;
  }
  let mut svg_start: Option<usize> = None;
  for i in 0..=bytes.len() - 4 {
    if bytes[i] == b'<'
      && bytes[i + 1].to_ascii_lowercase() == b's'
      && bytes[i + 2].to_ascii_lowercase() == b'v'
      && bytes[i + 3].to_ascii_lowercase() == b'g'
    {
      svg_start = Some(i);
      break;
    }
  }
  let svg_start = svg_start?;
  if bytes[svg_start + 1..].iter().any(|b| *b == b'<') {
    return None;
  }

  Some(trimmed.len() - 2)
}

pub(crate) fn inline_svg_with_foreign_objects(
  svg: &str,
  foreign_objects: &[ForeignObjectInfo],
  shared_css: &str,
  font_ctx: &FontContext,
  image_cache: &ImageCache,
  device_pixel_ratio: f32,
  max_iframe_depth: usize,
) -> Option<String> {
  let svg_doc = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| roxmltree::Document::parse(svg)))
    .ok()
    .and_then(|doc| doc.ok());
  let mut out_svg = String::new();
  out_svg.try_reserve_exact(svg.len()).ok()?;
  out_svg.push_str(svg);
  for (idx, foreign) in foreign_objects.iter().enumerate() {
    let placeholder = if foreign.placeholder.is_empty() {
      format!("<!--FASTRENDER_FOREIGN_OBJECT_{}-->", idx)
    } else {
      foreign.placeholder.clone()
    };
    let transform_scale = foreign_object_transform_scale(svg_doc.as_ref(), &placeholder, &foreign.attributes);
    let (data_url, image_bounds) = render_foreign_object_data_url(
      foreign,
      shared_css,
      font_ctx,
      image_cache,
      device_pixel_ratio * transform_scale,
      max_iframe_depth,
    )?;
    let replacement = foreign_object_image_tag(foreign, &data_url, idx, image_bounds)?;

    replace_placeholder_or_insert(&mut out_svg, &placeholder, &replacement)?;
  }

  Some(out_svg)
}

pub(crate) fn foreign_object_image_tag(
  info: &ForeignObjectInfo,
  data_url: &str,
  idx: usize,
  image_bounds: Rect,
) -> Option<String> {
  fn escape_upper_bound(value: &str) -> usize {
    // Worst case: every byte is escaped to a 6-byte entity (e.g. `'` -> `&apos;`).
    value.len().saturating_mul(6)
  }

  fn contains_ascii_case_insensitive(haystack: &str, needle: &str) -> bool {
    let needle = needle.as_bytes();
    let bytes = haystack.as_bytes();
    bytes
      .windows(needle.len())
      .any(|window| window.eq_ignore_ascii_case(needle))
  }

  fn write_escaped_attr_value<W: std::io::Write>(out: &mut W, value: &str) -> std::io::Result<()> {
    let bytes = value.as_bytes();
    let mut start = 0usize;
    for (idx, &b) in bytes.iter().enumerate() {
      let replacement: Option<&[u8]> = match b {
        b'&' => Some(b"&amp;"),
        b'<' => Some(b"&lt;"),
        b'"' => Some(b"&quot;"),
        b'\'' => Some(b"&apos;"),
        _ => None,
      };
      let Some(replacement) = replacement else {
        continue;
      };
      if start < idx {
        out.write_all(&bytes[start..idx])?;
      }
      out.write_all(replacement)?;
      start = idx + 1;
    }
    if start < bytes.len() {
      out.write_all(&bytes[start..])?;
    }
    Ok(())
  }

  fn write_attr_value<W: std::io::Write>(out: &mut W, name: &str, value: &str) -> std::io::Result<()> {
    out.write_all(b" ")?;
    out.write_all(name.as_bytes())?;
    out.write_all(b"=\"")?;
    write_escaped_attr_value(out, value)?;
    out.write_all(b"\"")?;
    Ok(())
  }

  let emits_computed_opacity = info.opacity < 1.0;
  let mut clip_path: Option<&str> = None;
  let mut has_filter = !info.style.filter.is_empty();
  for (name, value) in &info.attributes {
    if name.eq_ignore_ascii_case("clip-path") {
      clip_path = Some(value.as_str());
      continue;
    }
    if name.eq_ignore_ascii_case("filter") {
      has_filter = true;
    } else if name.eq_ignore_ascii_case("style") {
      has_filter |= contains_ascii_case_insensitive(value, "filter");
    }
  }

  let mut max_bytes = 512usize;
  max_bytes = max_bytes.saturating_add(escape_upper_bound(data_url));
  for (name, value) in &info.attributes {
    max_bytes = max_bytes.saturating_add(name.len().saturating_add(escape_upper_bound(value)));
  }
  if let Some(value) = clip_path {
    max_bytes = max_bytes.saturating_add(escape_upper_bound(value));
  }
  let clip_path_id =
    (info.overflow_x != Overflow::Visible || info.overflow_y != Overflow::Visible).then(|| foreign_object_clip_path_id(info, idx));
  if let Some(id) = clip_path_id.as_ref() {
    max_bytes = max_bytes.saturating_add(id.len().saturating_mul(2));
  }

  let mut out = FallibleVecWriter::new(max_bytes, "foreignObject svg image");

  if info.overflow_x == Overflow::Visible && info.overflow_y == Overflow::Visible {
    out.write_all(b"<image").ok()?;
    write!(
      out,
      " x=\"{:.6}\" y=\"{:.6}\" width=\"{:.6}\" height=\"{:.6}\"",
      image_bounds.x(),
      image_bounds.y(),
      image_bounds.width(),
      image_bounds.height()
    )
    .ok()?;
    if emits_computed_opacity {
      write!(out, " opacity=\"{:.3}\"", info.opacity.clamp(0.0, 1.0)).ok()?;
    }

    for (name, value) in &info.attributes {
      if name.eq_ignore_ascii_case("x")
        || name.eq_ignore_ascii_case("y")
        || name.eq_ignore_ascii_case("width")
        || name.eq_ignore_ascii_case("height")
        || (emits_computed_opacity && name.eq_ignore_ascii_case("opacity"))
      {
        continue;
      }
      write_attr_value(&mut out, name, value).ok()?;
    }

    out
      .write_all(b" preserveAspectRatio=\"none\"")
      .ok()?;
    write_attr_value(&mut out, "href", data_url).ok()?;
    out.write_all(b"/>").ok()?;
    return String::from_utf8(out.into_inner()).ok();
  }

  out.write_all(b"<g").ok()?;
  if let Some(value) = clip_path {
    out.write_all(b" clip-path=\"").ok()?;
    write_escaped_attr_value(&mut out, value).ok()?;
    out.write_all(b"\"").ok()?;
  }
  out.write_all(b">").ok()?;

  let mut clip_x = info.x;
  let mut clip_y = info.y;
  let mut clip_width = info.width;
  let mut clip_height = info.height;

  if info.overflow_x == Overflow::Visible {
    clip_x = image_bounds.x();
    clip_width = image_bounds.width();
  }
  if info.overflow_y == Overflow::Visible {
    clip_y = image_bounds.y();
    clip_height = image_bounds.height();
  }

  if has_filter {
    if info.overflow_x == Overflow::Visible {
      let margin = info.width;
      clip_x -= margin;
      clip_width += margin * 2.0;
    }
    if info.overflow_y == Overflow::Visible {
      let margin = info.height;
      clip_y -= margin;
      clip_height += margin * 2.0;
    }
  }

  let clip_path_id = clip_path_id.as_deref()?;
  write!(
    out,
    "<clipPath id=\"{clip_path_id}\"><rect x=\"{clip_x:.6}\" y=\"{clip_y:.6}\" width=\"{clip_width:.6}\" height=\"{clip_height:.6}\"/></clipPath>",
  )
  .ok()?;

  write!(out, "<image clip-path=\"url(#{clip_path_id})\"").ok()?;
  write!(
    out,
    " x=\"{:.6}\" y=\"{:.6}\" width=\"{:.6}\" height=\"{:.6}\"",
    image_bounds.x(),
    image_bounds.y(),
    image_bounds.width(),
    image_bounds.height()
  )
  .ok()?;
  if emits_computed_opacity {
    write!(out, " opacity=\"{:.3}\"", info.opacity.clamp(0.0, 1.0)).ok()?;
  }

  for (name, value) in &info.attributes {
    if name.eq_ignore_ascii_case("x")
      || name.eq_ignore_ascii_case("y")
      || name.eq_ignore_ascii_case("width")
      || name.eq_ignore_ascii_case("height")
      || (emits_computed_opacity && name.eq_ignore_ascii_case("opacity"))
      || name.eq_ignore_ascii_case("clip-path")
    {
      continue;
    }
    write_attr_value(&mut out, name, value).ok()?;
  }

  out
    .write_all(b" preserveAspectRatio=\"none\"")
    .ok()?;
  write_attr_value(&mut out, "href", data_url).ok()?;
  out.write_all(b"/>").ok()?;
  out.write_all(b"</g>").ok()?;

  String::from_utf8(out.into_inner()).ok()
}

fn render_foreign_object_data_url(
  info: &ForeignObjectInfo,
  shared_css: &str,
  font_ctx: &FontContext,
  image_cache: &ImageCache,
  device_pixel_ratio: f32,
  max_iframe_depth: usize,
) -> Option<(String, Rect)> {
  let width = info.width.max(1.0).round() as u32;
  let height = info.height.max(1.0).round() as u32;
  if width == 0 || height == 0 {
    return None;
  }

  let device_pixel_ratio = if device_pixel_ratio.is_finite() && device_pixel_ratio > 0.0 {
    device_pixel_ratio
  } else {
    1.0
  };
  let html = build_foreign_object_document(info, shared_css, width, height)?;
  // ForeignObject "background" comes from the SVG element's computed CSS, not the nested HTML.
  // Render on a transparent canvas and apply the background via the `<body>` inline style so:
  // - document-level shared CSS (e.g. `body { background: white }`) cannot override it
  // - semi-transparent colors are only composited once
  let background = Rgba::TRANSPARENT;
  let context = image_cache
    .resource_context()
    .map(|mut ctx| {
      if ctx.iframe_depth_remaining.is_none() {
        ctx.iframe_depth_remaining = Some(max_iframe_depth);
      }
      ctx
    });
  let policy = context
    .as_ref()
    .map(|c| c.policy.clone())
    .unwrap_or_default();

  let fragment_tree = layout_html_with_shared_resources(
    &html,
    width,
    height,
    font_ctx,
    image_cache,
    Arc::clone(image_cache.fetcher()),
    image_cache.base_url(),
    device_pixel_ratio,
    policy,
    context.clone(),
    max_iframe_depth,
  )
  .ok()?;

  let bounds = fragment_tree.content_size();
  let viewport_width = width as f32;
  let viewport_height = height as f32;
  let (mut min_x, mut max_x) = (bounds.min_x(), bounds.max_x());
  let (mut min_y, mut max_y) = (bounds.min_y(), bounds.max_y());
  if info.overflow_x != Overflow::Visible {
    min_x = 0.0;
    max_x = viewport_width;
  }
  if info.overflow_y != Overflow::Visible {
    min_y = 0.0;
    max_y = viewport_height;
  }

  if !min_x.is_finite() || !max_x.is_finite() {
    min_x = 0.0;
    max_x = viewport_width;
  }
  if !min_y.is_finite() || !max_y.is_finite() {
    min_y = 0.0;
    max_y = viewport_height;
  }

  if max_x <= min_x {
    min_x = 0.0;
    max_x = viewport_width;
  }
  if max_y <= min_y {
    min_y = 0.0;
    max_y = viewport_height;
  }

  let origin_x = min_x.floor();
  let origin_y = min_y.floor();
  let paint_width = (max_x.ceil() - origin_x).max(1.0) as u32;
  let paint_height = (max_y.ceil() - origin_y).max(1.0) as u32;

  let mut paint_tree = FragmentTree::new(fragment_tree.root.clone());
  paint_tree.additional_fragments = fragment_tree.additional_fragments.clone();
  paint_tree.keyframes = fragment_tree.keyframes.clone();
  paint_tree.svg_filter_defs = fragment_tree.svg_filter_defs.clone();
  paint_tree.svg_id_defs = fragment_tree.svg_id_defs.clone();
  paint_tree.scroll_metadata = fragment_tree.scroll_metadata.clone();

  let mut paint_font_ctx = font_ctx.clone();
  paint_font_ctx.set_resource_context(context.clone());
  let mut paint_image_cache = image_cache.clone();
  paint_image_cache.set_resource_context(context.clone());

  let offset = Point::new(-origin_x, -origin_y);
  let pixmap = paint_tree_with_resources_scaled_offset_backend_with_iframe_depth(
    &paint_tree,
    paint_width,
    paint_height,
    background,
    paint_font_ctx,
    paint_image_cache,
    device_pixel_ratio,
    offset,
    PaintParallelism::default(),
    &ScrollState::default(),
    paint_backend_from_env(),
    max_iframe_depth,
  )
  .ok()?;

  let data_url = pixmap_to_data_url(pixmap)?;
  let scale_x = info.width / width.max(1) as f32;
  let scale_y = info.height / height.max(1) as f32;
  let image_bounds = Rect::from_xywh(
    info.x + origin_x * scale_x,
    info.y + origin_y * scale_y,
    paint_width as f32 * scale_x,
    paint_height as f32 * scale_y,
  );

  Some((data_url, image_bounds))
}

fn escape_style_end_tags<W: std::io::Write>(css: &str, out: &mut W) -> std::io::Result<()> {
  const STYLE: [u8; 5] = [b's', b't', b'y', b'l', b'e'];
  let bytes = css.as_bytes();

  let has_sequence = bytes.windows(7).any(|window| {
    window[0] == b'<'
      && window[1] == b'/'
      && window[2..]
        .iter()
        .zip(STYLE.iter())
        .all(|(b, expected)| b.to_ascii_lowercase() == *expected)
  });

  if !has_sequence {
    out.write_all(bytes)?;
    return Ok(());
  }

  let mut idx = 0;
  while idx < bytes.len() {
    if idx + 7 <= bytes.len() && bytes[idx] == b'<' && bytes[idx + 1] == b'/' {
      if STYLE
        .iter()
        .enumerate()
        .all(|(offset, expected)| bytes[idx + 2 + offset].to_ascii_lowercase() == *expected)
      {
        out.write_all(b"<\\/style")?;
        idx += 7;
        continue;
      }
    }

    out.write_all(&bytes[idx..idx + 1])?;
    idx += 1;
  }

  Ok(())
}

fn build_foreign_object_document(
  info: &ForeignObjectInfo,
  shared_css: &str,
  width: u32,
  height: u32,
) -> Option<String> {
  let mut html = FallibleVecWriter::new(MAX_FOREIGN_OBJECT_DOC_BYTES, "foreignObject html");
  write!(
    html,
    "<!DOCTYPE html><html style=\"margin:0;padding:0;width:{width}px;height:{height}px;background:transparent !important;\"><head><meta charset=\"utf-8\">",
  )
  .ok()?;
  if !trim_ascii_whitespace_html_css(shared_css).is_empty() {
    html.write_all(b"<style>").ok()?;
    escape_style_end_tags(shared_css, &mut html).ok()?;
    html.write_all(b"</style>").ok()?;
  }
  html.write_all(b"</head><body style=\"").ok()?;
  html
    .write_all(foreign_object_body_style(info).as_bytes())
    .ok()?;
  html.write_all(b"\">").ok()?;
  html.write_all(info.html.as_bytes()).ok()?;
  html.write_all(b"</body></html>").ok()?;
  String::from_utf8(html.into_inner()).ok()
}

fn foreign_object_body_style(info: &ForeignObjectInfo) -> String {
  let width = info.width.max(1.0).round() as u32;
  let height = info.height.max(1.0).round() as u32;
  let mut style = format!(
    "margin:0;padding:0;width:{width}px;height:{height}px;display:block;box-sizing:border-box;"
  );
  style.push_str("border:none !important;box-shadow:none !important;outline:none !important;");
  if let Some(bg) = info.background {
    style.push_str("background:");
    style.push_str(&format_css_color(bg));
    style.push_str(" !important;");
  } else {
    style.push_str("background:transparent !important;");
  }
  let overflow_keyword = |overflow: Overflow| match overflow {
    Overflow::Visible => "visible",
    Overflow::Hidden => "hidden",
    Overflow::Scroll => "scroll",
    Overflow::Auto => "auto",
    Overflow::Clip => "clip",
  };
  let _ = write!(
    &mut style,
    "overflow-x:{};overflow-y:{};",
    overflow_keyword(info.overflow_x),
    overflow_keyword(info.overflow_y)
  );
  style.push_str("color:");
  style.push_str(&format_css_color(info.style.color));
  style.push(';');

  if !info.style.font_family.is_empty() {
    let families: Vec<String> = info
      .style
      .font_family
      .iter()
      .map(|f| {
        if f.contains(' ') && !(f.starts_with('"') && f.ends_with('"')) {
          format!("\"{}\"", f)
        } else {
          f.clone()
        }
      })
      .collect();
    style.push_str("font-family:");
    style.push_str(&families.join(", "));
    style.push(';');
  }

  let _ = write!(
    &mut style,
    "font-size:{:.2}px;font-weight:{};",
    info.style.font_size,
    info.style.font_weight.to_u16()
  );

  match info.style.font_style {
    CssFontStyle::Italic => style.push_str("font-style: italic;"),
    CssFontStyle::Oblique(Some(angle)) => {
      let _ = write!(&mut style, "font-style: oblique {}deg;", angle);
    }
    CssFontStyle::Oblique(None) => style.push_str("font-style: oblique;"),
    CssFontStyle::Normal => {}
  }

  if info.style.direction == Direction::Rtl {
    style.push_str("direction: rtl;");
  }

  if info.style.writing_mode != WritingMode::HorizontalTb {
    style.push_str("writing-mode:");
    style.push_str(writing_mode_keyword(info.style.writing_mode));
    style.push(';');
  }

  style
}

fn writing_mode_keyword(mode: WritingMode) -> &'static str {
  match mode {
    WritingMode::HorizontalTb => "horizontal-tb",
    WritingMode::VerticalRl => "vertical-rl",
    WritingMode::VerticalLr => "vertical-lr",
    WritingMode::SidewaysRl => "sideways-rl",
    WritingMode::SidewaysLr => "sideways-lr",
  }
}

fn format_css_color(color: Rgba) -> String {
  format!(
    "rgba({},{},{},{:.3})",
    color.r,
    color.g,
    color.b,
    color.a.clamp(0.0, 1.0)
  )
}

fn pixmap_to_data_url(pixmap: Pixmap) -> Option<String> {
  let buf = encode_image(&pixmap, OutputFormat::Png).ok()?;
  data_url::encode_base64_data_url("image/png", &buf)
}

#[cfg(test)]
mod tests {
  use super::{
    foreign_object_html_device_pixel_ratio, foreign_object_image_tag, pixmap_to_data_url, replace_placeholder_or_insert,
  };
  use base64::Engine;

  #[test]
  fn replaces_placeholder_when_present() {
    let mut svg = "<svg><!--P--></svg>".to_string();
    replace_placeholder_or_insert(&mut svg, "<!--P-->", "<image/>").expect("replace");
    assert_eq!(svg, "<svg><image/></svg>");
  }

  #[test]
  fn inserts_before_closing_tag_when_placeholder_missing() {
    let mut svg = "<svg></svg>".to_string();
    replace_placeholder_or_insert(&mut svg, "<!--P-->", "<image/>").expect("replace");
    assert_eq!(svg, "<svg><image/></svg>");
  }

  #[test]
  fn appends_when_svg_has_no_closing_tag() {
    let mut svg = "<svg>".to_string();
    replace_placeholder_or_insert(&mut svg, "<!--P-->", "<image/>").expect("replace");
    assert_eq!(svg, "<svg><image/>");
  }

  #[test]
  fn expands_self_closing_root_svg_when_placeholder_missing() {
    let mut svg = "<svg/>".to_string();
    replace_placeholder_or_insert(&mut svg, "<!--P-->", "<image/>").expect("replace");
    assert_eq!(svg, "<svg><image/></svg>");
  }

  #[test]
  fn non_ascii_whitespace_expands_self_closing_root_svg_does_not_trim_nbsp_suffix() {
    let nbsp = '\u{00A0}';
    let mut svg = format!("<svg/>{nbsp}");
    replace_placeholder_or_insert(&mut svg, "<!--P-->", "<image/>").expect("replace");
    assert_eq!(svg, format!("<svg/>{nbsp}<image/>"));
  }

  #[test]
  fn pixmap_data_url_unpremultiplies_alpha() {
    let mut pixmap = tiny_skia::Pixmap::new(1, 1).expect("pixmap");
    pixmap.data_mut()[..4].copy_from_slice(&[128, 0, 0, 128]);

    let data_url = pixmap_to_data_url(pixmap).expect("data url");
    let encoded = data_url
      .strip_prefix("data:image/png;base64,")
      .expect("prefix");
    let png = base64::engine::general_purpose::STANDARD
      .decode(encoded)
      .expect("decode base64");
    let decoded = image::load_from_memory_with_format(&png, image::ImageFormat::Png)
      .expect("decode png")
      .to_rgba8();
    let px = decoded.get_pixel(0, 0).0;
    assert_eq!(px[1], 0);
    assert_eq!(px[2], 0);
    assert_eq!(px[3], 128);
    assert!(
      px[0] >= 254,
      "expected nearly full red channel after unpremultiplication, got {px:?}"
    );
  }

  #[test]
  fn foreign_object_document_escapes_style_end_tags_inside_css() {
    use crate::tree::box_tree::ForeignObjectInfo;
    use crate::ComputedStyle;
    use crate::Overflow;
    use std::sync::Arc;

    let foreign = ForeignObjectInfo {
      placeholder: "<!--FASTRENDER_FOREIGN_OBJECT_0-->".to_string(),
      attributes: Vec::new(),
      x: 0.0,
      y: 0.0,
      width: 1.0,
      height: 1.0,
      opacity: 1.0,
      background: None,
      html: "<div xmlns=\"http://www.w3.org/1999/xhtml\"></div>".to_string(),
      style: Arc::new(ComputedStyle::default()),
      overflow_x: Overflow::Visible,
      overflow_y: Overflow::Visible,
    };

    let shared_css =
      "body{color:red;}/*</STYLE><img src=\"https://example.com/evil.png\">*/";
    let doc = super::build_foreign_object_document(&foreign, shared_css, 1, 1).expect("doc");

    assert_eq!(
      doc.match_indices("</style>").count(),
      1,
      "expected a single closing style tag in the generated document"
    );

    let start = doc.find("<style>").expect("style start") + "<style>".len();
    let end = doc.find("</style>").expect("style end");
    let style_content = &doc[start..end];
    assert!(
      !style_content.to_ascii_lowercase().contains("</style"),
      "style content unexpectedly contains raw </style> sequence"
    );
    assert!(
      style_content.contains("<\\/style"),
      "expected escaped closing tag in style content, got {style_content:?}"
    );
  }

  #[test]
  fn foreign_object_document_rejects_oversized_payloads() {
    use crate::tree::box_tree::ForeignObjectInfo;
    use crate::ComputedStyle;
    use crate::Overflow;
    use std::sync::Arc;

    let foreign = ForeignObjectInfo {
      placeholder: "<!--FASTRENDER_FOREIGN_OBJECT_0-->".to_string(),
      attributes: Vec::new(),
      x: 0.0,
      y: 0.0,
      width: 1.0,
      height: 1.0,
      opacity: 1.0,
      background: None,
      html: "<div xmlns=\"http://www.w3.org/1999/xhtml\"></div>".to_string(),
      style: Arc::new(ComputedStyle::default()),
      overflow_x: Overflow::Visible,
      overflow_y: Overflow::Visible,
    };

    let shared_css = "a".repeat(super::MAX_FOREIGN_OBJECT_DOC_BYTES);
    assert!(super::build_foreign_object_document(&foreign, &shared_css, 1, 1).is_none());
  }

  #[test]
  fn foreign_object_png_dimensions_scale_with_device_pixel_ratio() {
    use crate::image_loader::ImageCache;
    use crate::text::font_loader::FontContext;
    use crate::tree::box_tree::ForeignObjectInfo;
    use crate::ComputedStyle;
    use crate::Overflow;
    use std::sync::Arc;

    let foreign = ForeignObjectInfo {
      placeholder: "<!--FASTRENDER_FOREIGN_OBJECT_0-->".to_string(),
      attributes: Vec::new(),
      x: 0.0,
      y: 0.0,
      width: 1.0,
      height: 1.0,
      opacity: 1.0,
      background: None,
      html: "<div xmlns=\"http://www.w3.org/1999/xhtml\"></div>".to_string(),
      style: Arc::new(ComputedStyle::default()),
      overflow_x: Overflow::Visible,
      overflow_y: Overflow::Visible,
    };

    let font_ctx = FontContext::new();
    let image_cache = ImageCache::new();
    let svg = "<svg><!--FASTRENDER_FOREIGN_OBJECT_0--></svg>";
    let resolved = super::inline_svg_with_foreign_objects(
      svg,
      &[foreign],
      "",
      &font_ctx,
      &image_cache,
      2.0,
      0,
    )
    .expect("resolved svg");

    let href_prefix = "href=\"data:image/png;base64,";
    let href_start = resolved.find(href_prefix).expect("href attribute") + href_prefix.len();
    let href_end = resolved[href_start..].find('"').expect("closing quote") + href_start;
    let encoded = &resolved[href_start..href_end];
    let png = base64::engine::general_purpose::STANDARD
      .decode(encoded)
      .expect("decode base64");
    let decoded = image::load_from_memory_with_format(&png, image::ImageFormat::Png)
      .expect("decode png");
    assert_eq!(decoded.width(), 2);
    assert_eq!(decoded.height(), 2);
  }

  #[test]
  fn inlines_foreign_object_from_raw_markup() {
    use crate::image_loader::ImageCache;
    use crate::text::font_loader::FontContext;

    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="20">
      <foreignObject x="0" y="0" width="20" height="20">
        <div xmlns="http://www.w3.org/1999/xhtml" style="width:20px;height:20px;background:red"></div>
      </foreignObject>
    </svg>"#;
    let font_ctx = FontContext::new();
    let image_cache = ImageCache::new();

    let resolved = super::inline_svg_foreign_objects_from_markup(svg, "", &font_ctx, &image_cache, 1.0, 0)
      .expect("resolved svg");

    assert!(
      resolved.contains("data:image/png;base64,"),
      "expected injected PNG data URL, got {resolved:?}"
    );
  }

  #[test]
  fn rasterizes_inlined_foreign_object_svg() {
    use crate::image_loader::ImageCache;
    use crate::text::font_loader::FontContext;

    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="20">
      <foreignObject x="0" y="0" width="20" height="20">
        <div xmlns="http://www.w3.org/1999/xhtml" style="width:20px;height:20px;background:red"></div>
      </foreignObject>
    </svg>"#;
    let font_ctx = FontContext::new();
    let image_cache = ImageCache::new();

    let resolved = super::inline_svg_foreign_objects_from_markup(svg, "", &font_ctx, &image_cache, 1.0, 0)
      .expect("resolved svg");
    let pixmap = image_cache
      .render_svg_pixmap_at_size(&resolved, 20, 20, "inline-svg", 1.0)
      .expect("render pixmap");
    let px = pixmap.pixel(10, 10).expect("center px");
    assert_eq!((px.red(), px.green(), px.blue(), px.alpha()), (255, 0, 0, 255));
  }

  #[test]
  fn foreign_object_raster_dpr_accounts_for_view_box_scale() {
    let svg = r#"<svg width="160" height="160" viewBox="0 0 16 16" xmlns="http://www.w3.org/2000/svg"></svg>"#;
    let dpr = foreign_object_html_device_pixel_ratio(svg, 1.0, 160.0, 160.0, 160.0, 160.0);
    assert!((dpr - 10.0).abs() < 0.01, "expected dpr ~10, got {dpr}");
  }

  #[test]
  fn foreign_object_transform_scale_accounts_for_ancestor_and_element_transforms() {
    let svg =
      r#"<svg xmlns="http://www.w3.org/2000/svg"><g transform="scale(2)"><!--FASTRENDER_FOREIGN_OBJECT_0--></g></svg>"#;
    let doc = roxmltree::Document::parse(svg).expect("parse svg");
    let attrs = vec![(
      "transform".to_string(),
      "translate(0 0) scale(3)".to_string(),
    )];
    let scale =
      super::foreign_object_transform_scale(Some(&doc), "<!--FASTRENDER_FOREIGN_OBJECT_0-->", &attrs);
    assert!((scale - 6.0).abs() < 0.01, "expected scale ~6, got {scale}");
  }

  #[test]
  fn foreign_object_transform_scale_accounts_for_nested_svg_view_box_scale() {
    let svg = r#"<svg width="160" height="160" viewBox="0 0 80 80" xmlns="http://www.w3.org/2000/svg"><svg width="80" height="80" viewBox="0 0 8 8"><!--FASTRENDER_FOREIGN_OBJECT_0--></svg></svg>"#;
    let doc = roxmltree::Document::parse(svg).expect("parse svg");
    let scale =
      super::foreign_object_transform_scale(Some(&doc), "<!--FASTRENDER_FOREIGN_OBJECT_0-->", &[]);
    assert!((scale - 10.0).abs() < 0.01, "expected scale ~10, got {scale}");
  }

  fn clip_path_id_from_image_tag(tag: &str) -> &str {
    let prefix = "<clipPath id=\"";
    let start = tag.find(prefix).expect("clipPath start") + prefix.len();
    let end = start + tag[start..].find('"').expect("clipPath id end");
    &tag[start..end]
  }

  fn default_foreign_object(placeholder: &str) -> crate::tree::box_tree::ForeignObjectInfo {
    use crate::ComputedStyle;
    use crate::Overflow;
    use std::sync::Arc;

    crate::tree::box_tree::ForeignObjectInfo {
      placeholder: placeholder.to_string(),
      attributes: Vec::new(),
      x: 0.0,
      y: 0.0,
      width: 10.0,
      height: 10.0,
      opacity: 1.0,
      background: None,
      html: "<div xmlns=\"http://www.w3.org/1999/xhtml\"></div>".to_string(),
      style: Arc::new(ComputedStyle::default()),
      overflow_x: Overflow::Hidden,
      overflow_y: Overflow::Hidden,
    }
  }

  #[test]
  fn foreign_object_clip_path_id_changes_with_placeholder() {
    let image_bounds = crate::Rect::from_xywh(0.0, 0.0, 10.0, 10.0);
    let first = default_foreign_object("<!--P1-->");
    let second = default_foreign_object("<!--P2-->");

    let first_tag = foreign_object_image_tag(&first, "data:image/png;base64,AAAA", 0, image_bounds).expect("tag");
    let second_tag = foreign_object_image_tag(&second, "data:image/png;base64,AAAA", 0, image_bounds).expect("tag");

    let first_id = clip_path_id_from_image_tag(&first_tag);
    let second_id = clip_path_id_from_image_tag(&second_tag);
    assert_ne!(first_id, second_id);
  }

  #[test]
  fn foreign_object_clip_path_id_is_unique_and_consistently_applied() {
    let image_bounds = crate::Rect::from_xywh(0.0, 0.0, 10.0, 10.0);
    let foreign = default_foreign_object("<!--P1-->");

    let tag = foreign_object_image_tag(&foreign, "data:image/png;base64,AAAA", 0, image_bounds).expect("tag");
    let id = clip_path_id_from_image_tag(&tag);

    assert!(
      id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
      "expected svg-safe id, got {id:?}"
    );

    assert_eq!(tag.match_indices(&format!("<clipPath id=\"{id}\">")).count(), 1);
    assert_eq!(tag.match_indices(&format!("clip-path=\"url(#{id})\"")).count(), 1);
  }
}
