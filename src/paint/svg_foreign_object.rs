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
use std::fmt::Write as _;
use std::io::Write as _;
use std::sync::Arc;
use tiny_skia::{Pixmap, Transform};

// ForeignObject rendering constructs a synthetic HTML document containing the serialized subtree
// HTML plus a copy of the document-level CSS. Cap the total size so pathological SVGs cannot force
// multi-megabyte allocations (and potentially OOM aborts) during this nested render path.
const MAX_FOREIGN_OBJECT_DOC_BYTES: usize = 8 * 1024 * 1024;

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

fn foreign_object_transform_scale(
  svg_doc: Option<&roxmltree::Document<'_>>,
  placeholder: &str,
  attributes: &[(String, String)],
) -> f32 {
  let mut combined = Transform::identity();

  if let Some(doc) = svg_doc {
    let needle = placeholder
      .trim()
      .strip_prefix("<!--")
      .and_then(|s| s.strip_suffix("-->"))
      .unwrap_or_else(|| placeholder.trim())
      .trim();

    if !needle.is_empty() {
      let comment = doc
        .descendants()
        .find(|node| node.is_comment() && node.text().is_some_and(|t| t.trim() == needle));
      if let Some(comment) = comment {
        let mut current = comment.parent();
        while let Some(node) = current {
          if node.is_element() {
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

  transform_scale_factor(combined).max(1.0)
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
  let trimmed = svg.trim_end();
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
    let replacement = foreign_object_image_tag(foreign, &data_url, idx, image_bounds);

    replace_placeholder_or_insert(&mut out_svg, &placeholder, &replacement)?;
  }

  Some(out_svg)
}

pub(crate) fn foreign_object_image_tag(
  info: &ForeignObjectInfo,
  data_url: &str,
  idx: usize,
  image_bounds: Rect,
) -> String {
  let mut parts: Vec<String> = Vec::new();
  parts.push(format!("x=\"{:.6}\"", image_bounds.x()));
  parts.push(format!("y=\"{:.6}\"", image_bounds.y()));
  parts.push(format!("width=\"{:.6}\"", image_bounds.width()));
  parts.push(format!("height=\"{:.6}\"", image_bounds.height()));
  let emits_computed_opacity = info.opacity < 1.0;
  if emits_computed_opacity {
    parts.push(format!("opacity=\"{:.3}\"", info.opacity.clamp(0.0, 1.0)));
  }

  let mut clip_path: Option<&str> = None;
  let mut has_filter = !info.style.filter.is_empty();
  for (name, value) in &info.attributes {
    if name.eq_ignore_ascii_case("x")
      || name.eq_ignore_ascii_case("y")
      || name.eq_ignore_ascii_case("width")
      || name.eq_ignore_ascii_case("height")
      || (emits_computed_opacity && name.eq_ignore_ascii_case("opacity"))
    {
      continue;
    }
    if name.eq_ignore_ascii_case("clip-path") {
      clip_path = Some(value.as_str());
      continue;
    }
    if name.eq_ignore_ascii_case("filter") {
      has_filter = true;
    } else if name.eq_ignore_ascii_case("style") {
      has_filter |= value.to_ascii_lowercase().contains("filter");
    }
    parts.push(format!("{}=\"{}\"", name, escape_attr_value(value)));
  }

  parts.push("preserveAspectRatio=\"none\"".to_string());
  parts.push(format!("href=\"{}\"", escape_attr_value(data_url)));

  if info.overflow_x == Overflow::Visible && info.overflow_y == Overflow::Visible {
    if let Some(value) = clip_path {
      parts.push(format!("clip-path=\"{}\"", escape_attr_value(value)));
    }
    return format!("<image {attrs}/>", attrs = parts.join(" "));
  }

  let group_clip_path = clip_path
    .map(|value| format!(" clip-path=\"{}\"", escape_attr_value(value)))
    .unwrap_or_default();
  let clip_id = format!("fastr-fo-{}", idx);
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
  let clip = format!(
    "<clipPath id=\"{}\"><rect x=\"{:.6}\" y=\"{:.6}\" width=\"{:.6}\" height=\"{:.6}\"/></clipPath>",
    clip_id,
    clip_x,
    clip_y,
    clip_width,
    clip_height
  );

  format!(
    "<g{group_clip_path}>{clip}<image clip-path=\"url(#{clip_id})\" {attrs}/></g>",
    clip = clip,
    clip_id = clip_id,
    group_clip_path = group_clip_path,
    attrs = parts.join(" ")
  )
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
  if !shared_css.trim().is_empty() {
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

fn escape_attr_value(value: &str) -> String {
  value
    .replace('&', "&amp;")
    .replace('<', "&lt;")
    .replace('"', "&quot;")
    .replace('\'', "&apos;")
}

fn pixmap_to_data_url(pixmap: Pixmap) -> Option<String> {
  let buf = encode_image(&pixmap, OutputFormat::Png).ok()?;
  data_url::encode_base64_data_url("image/png", &buf)
}

#[cfg(test)]
mod tests {
  use super::{foreign_object_html_device_pixel_ratio, pixmap_to_data_url, replace_placeholder_or_insert};
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
}  
