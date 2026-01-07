//! SVG `<foreignObject>` rendering helpers.
//!
//! Resvg cannot render HTML inside `<foreignObject>`. FastRender serializes SVG subtrees with
//! placeholder markers and captures the subtree HTML + computed styles separately. During paint we
//! render each captured HTML fragment via the normal HTML pipeline and inject the resulting pixels
//! back into the SVG as `<image href="data:image/png;base64,…">`.

use crate::api::render_html_with_shared_resources;
use crate::image_output::{encode_image, OutputFormat};
use crate::image_loader::ImageCache;
use crate::style::color::Rgba;
use crate::style::types::{Direction, FontStyle as CssFontStyle, Overflow, WritingMode};
use crate::text::font_loader::FontContext;
use crate::tree::box_tree::ForeignObjectInfo;
use base64::Engine;
use std::fmt::Write as _;
use std::sync::Arc;
use tiny_skia::Pixmap;

fn replace_placeholder_or_insert(svg: &mut String, placeholder: &str, replacement: &str) {
  if let Some(pos) = svg.find(placeholder) {
    let end = pos + placeholder.len();
    svg.replace_range(pos..end, replacement);
  } else if let Some(close_pos) = svg.rfind("</svg>") {
    svg.insert_str(close_pos, replacement);
  } else if let Some(close_pos) = find_self_closing_root_svg_end(svg) {
    let suffix = format!(">{replacement}</svg>");
    svg.replace_range(close_pos..close_pos + 2, &suffix);
  } else {
    svg.push_str(replacement);
  }
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
  max_iframe_depth: usize,
) -> Option<String> {
  let mut svg = svg.to_string();
  for (idx, foreign) in foreign_objects.iter().enumerate() {
    let data_url =
      render_foreign_object_data_url(foreign, shared_css, font_ctx, image_cache, max_iframe_depth)?;
    let replacement = foreign_object_image_tag(foreign, &data_url, idx);
    let placeholder = if foreign.placeholder.is_empty() {
      format!("<!--FASTRENDER_FOREIGN_OBJECT_{}-->", idx)
    } else {
      foreign.placeholder.clone()
    };

    replace_placeholder_or_insert(&mut svg, &placeholder, &replacement);
  }

  Some(svg)
}

pub(crate) fn foreign_object_image_tag(info: &ForeignObjectInfo, data_url: &str, idx: usize) -> String {
  let mut parts: Vec<String> = Vec::new();
  parts.push(format!("x=\"{:.6}\"", info.x));
  parts.push(format!("y=\"{:.6}\"", info.y));
  parts.push(format!("width=\"{:.6}\"", info.width));
  parts.push(format!("height=\"{:.6}\"", info.height));
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

  if has_filter {
    if info.overflow_x == Overflow::Visible {
      let margin = info.width;
      clip_x = info.x - margin;
      clip_width = info.width + margin * 2.0;
    }
    if info.overflow_y == Overflow::Visible {
      let margin = info.height;
      clip_y = info.y - margin;
      clip_height = info.height + margin * 2.0;
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
  max_iframe_depth: usize,
) -> Option<String> {
  let width = info.width.max(1.0).round() as u32;
  let height = info.height.max(1.0).round() as u32;
  if width == 0 || height == 0 {
    return None;
  }

  let html = build_foreign_object_document(info, shared_css, width, height);
  // ForeignObject "background" comes from the SVG element's computed CSS, not the nested HTML.
  // Render on a transparent canvas and apply the background via the `<body>` inline style so:
  // - document-level shared CSS (e.g. `body { background: white }`) cannot override it
  // - semi-transparent colors are only composited once
  let background = Rgba::TRANSPARENT;
  let context = image_cache.resource_context();
  let policy = context
    .as_ref()
    .map(|c| c.policy.clone())
    .unwrap_or_default();
  let pixmap = render_html_with_shared_resources(
    &html,
    width,
    height,
    background,
    font_ctx,
    image_cache,
    Arc::clone(image_cache.fetcher()),
    image_cache.base_url(),
    1.0,
    policy,
    context,
    max_iframe_depth,
  )
  .ok()?;

  pixmap_to_data_url(pixmap)
}

fn escape_style_end_tags(css: &str) -> String {
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
    return css.to_string();
  }

  let mut out = Vec::with_capacity(bytes.len());
  let mut idx = 0;
  while idx < bytes.len() {
    if idx + 7 <= bytes.len() && bytes[idx] == b'<' && bytes[idx + 1] == b'/' {
      if STYLE
        .iter()
        .enumerate()
        .all(|(offset, expected)| bytes[idx + 2 + offset].to_ascii_lowercase() == *expected)
      {
        out.extend_from_slice(b"<\\/style");
        idx += 7;
        continue;
      }
    }

    out.push(bytes[idx]);
    idx += 1;
  }

  String::from_utf8(out).unwrap_or_default()
}

fn build_foreign_object_document(
  info: &ForeignObjectInfo,
  shared_css: &str,
  width: u32,
  height: u32,
) -> String {
  let mut html = format!(
    "<!DOCTYPE html><html style=\"margin:0;padding:0;width:{width}px;height:{height}px;background:transparent !important;\"><head><meta charset=\"utf-8\">"
  );
  if !shared_css.trim().is_empty() {
    let sanitized_css = escape_style_end_tags(shared_css);
    html.push_str("<style>");
    html.push_str(&sanitized_css);
    html.push_str("</style>");
  }
  html.push_str("</head><body style=\"");
  html.push_str(&foreign_object_body_style(info));
  html.push_str("\">");
  html.push_str(&info.html);
  html.push_str("</body></html>");
  html
}

fn foreign_object_body_style(info: &ForeignObjectInfo) -> String {
  let width = info.width.max(1.0).round() as u32;
  let height = info.height.max(1.0).round() as u32;
  let mut style = format!(
    "margin:0;padding:0;width:{width}px;height:{height}px;display:block;box-sizing:border-box;"
  );
  style.push_str("background:transparent !important;border:none !important;box-shadow:none !important;outline:none !important;");
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

  if let Some(bg) = info.background {
    style.push_str("background:");
    style.push_str(&format_css_color(bg));
    style.push_str(" !important;");
  }

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

  Some(format!(
    "data:image/png;base64,{}",
    base64::engine::general_purpose::STANDARD.encode(buf)
  ))
}

#[cfg(test)]
mod tests {
  use super::{pixmap_to_data_url, replace_placeholder_or_insert};
  use base64::Engine;

  #[test]
  fn replaces_placeholder_when_present() {
    let mut svg = "<svg><!--P--></svg>".to_string();
    replace_placeholder_or_insert(&mut svg, "<!--P-->", "<image/>");
    assert_eq!(svg, "<svg><image/></svg>");
  }

  #[test]
  fn inserts_before_closing_tag_when_placeholder_missing() {
    let mut svg = "<svg></svg>".to_string();
    replace_placeholder_or_insert(&mut svg, "<!--P-->", "<image/>");
    assert_eq!(svg, "<svg><image/></svg>");
  }

  #[test]
  fn appends_when_svg_has_no_closing_tag() {
    let mut svg = "<svg>".to_string();
    replace_placeholder_or_insert(&mut svg, "<!--P-->", "<image/>");
    assert_eq!(svg, "<svg><image/>");
  }

  #[test]
  fn expands_self_closing_root_svg_when_placeholder_missing() {
    let mut svg = "<svg/>".to_string();
    replace_placeholder_or_insert(&mut svg, "<!--P-->", "<image/>");
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
}
