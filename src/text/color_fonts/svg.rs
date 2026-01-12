use super::limits::{log_glyph_limit, round_dimension, GlyphRasterLimits};
use super::{ColorFontCaches, ColorGlyphRaster, FontKey, SvgCacheKey};
use crate::paint::pixmap::new_pixmap;
use crate::style::color::Rgba;
use cssparser::{Parser, ParserInput, Token};
use crate::svg::{
  svg_markup_for_roxmltree, svg_root_view_box, svg_view_box_root_transform, SvgViewBox,
};
use regex::Regex;
use roxmltree::Document;
use std::ops::Range;
use std::sync::{Arc, Mutex, OnceLock};
use tiny_skia::Transform;

pub const MAX_SVG_GLYPH_BYTES: usize = 256 * 1024;
const MAX_SVG_GLYPH_NODES: usize = 10_000;
const MAX_SVG_GLYPH_DATA_URL_BYTES: usize = 32 * 1024;
const MAX_SVG_GLYPH_ATTRIBUTES: usize = 50_000;
const MAX_SVG_GLYPH_STYLE_BYTES: usize = 64 * 1024;
const MAX_SVG_GLYPH_URL_TOKENS: usize = 4096;
const MAX_SVG_GLYPH_CSS_NESTING_DEPTH: usize = 64;

#[derive(Clone)]
pub(super) struct ParsedSvgGlyph {
  pub tree: resvg::usvg::Tree,
  pub view_box: SvgViewBox,
  pub source_width: f32,
  pub source_height: f32,
  pub root_transform: Transform,
}

fn svg_color_signature(color: Rgba) -> u32 {
  ((color.alpha_u8() as u32) << 24)
    | ((color.r as u32) << 16)
    | ((color.g as u32) << 8)
    | color.b as u32
}

/// Render SVG-in-OpenType glyphs.
pub fn render_svg_glyph(
  face: &ttf_parser::Face<'_>,
  font_key: FontKey,
  glyph_id: ttf_parser::GlyphId,
  font_size: f32,
  text_color: Rgba,
  limits: &GlyphRasterLimits,
  caches: &Arc<Mutex<ColorFontCaches>>,
) -> Option<ColorGlyphRaster> {
  let key = SvgCacheKey::new(font_key, glyph_id.0, svg_color_signature(text_color));
  let cached = {
    let mut caches = caches
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    caches.svg_glyph(key)
  }
  .flatten();
  let parsed = if let Some(glyph) = cached {
    glyph
  } else {
    let parsed = parse_svg_glyph(face, glyph_id, text_color);
    caches
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .put_svg_glyph(key, parsed.clone());
    parsed?
  };
  rasterize_parsed_svg(
    &parsed,
    glyph_id.0 as u32,
    font_size,
    face.units_per_em() as f32,
    limits,
  )
}

pub(super) fn parse_svg_glyph(
  face: &ttf_parser::Face<'_>,
  glyph_id: ttf_parser::GlyphId,
  text_color: Rgba,
) -> Option<Arc<ParsedSvgGlyph>> {
  let svg_doc = face.glyph_svg_image(glyph_id)?;
  let svg_str = sanitize_svg_glyph(svg_doc.data)?;
  let svg_with_color = preprocess_svg_markup(svg_str, text_color);
  let markup = svg_with_color.as_deref().unwrap_or(svg_str);

  let mut options = resvg::usvg::Options::default();
  options.resources_dir = None;
  let markup_for_parse = svg_markup_for_roxmltree(markup);
  let tree = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    resvg::usvg::Tree::from_str(markup_for_parse.as_ref(), &options)
  })) {
    Ok(Ok(tree)) => tree,
    Ok(Err(_)) => return None,
    Err(_) => return None,
  };
  let size = tree.size();
  let source_width = size.width() as f32;
  let source_height = size.height() as f32;
  if source_width <= 0.0 || source_height <= 0.0 {
    return None;
  }

  let view_box = svg_root_view_box(markup).unwrap_or(SvgViewBox {
    min_x: 0.0,
    min_y: 0.0,
    width: source_width,
    height: source_height,
  });
  if view_box.width <= 0.0 || view_box.height <= 0.0 {
    return None;
  }

  let root_transform = svg_view_box_root_transform(
    markup,
    source_width,
    source_height,
    view_box.width,
    view_box.height,
  )
  .unwrap_or_else(|| {
    Transform::from_scale(
      view_box.width / source_width,
      view_box.height / source_height,
    )
  });

  Some(Arc::new(ParsedSvgGlyph {
    tree,
    view_box,
    source_width,
    source_height,
    root_transform,
  }))
}

pub(super) fn rasterize_parsed_svg(
  parsed: &Arc<ParsedSvgGlyph>,
  glyph_id: u32,
  font_size: f32,
  units_per_em: f32,
  limits: &GlyphRasterLimits,
) -> Option<ColorGlyphRaster> {
  if units_per_em <= 0.0 {
    return None;
  }

  let scale = font_size / units_per_em;
  if !scale.is_finite() || scale <= 0.0 || !font_size.is_finite() {
    return None;
  }

  let width = round_dimension(parsed.view_box.width * scale)?;
  let height = round_dimension(parsed.view_box.height * scale)?;
  if let Err(err) = limits.validate(width, height) {
    log_glyph_limit("svg", glyph_id, &err);
    return None;
  }

  let mut pixmap = new_pixmap(width, height)?;
  let max_y = parsed.view_box.min_y + parsed.view_box.height;
  let glyph_transform = Transform::from_row(
    scale,
    0.0,
    0.0,
    -scale,
    -parsed.view_box.min_x * scale,
    max_y * scale,
  );
  let transform = concat_transforms(glyph_transform, parsed.root_transform);

  if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    resvg::render(&parsed.tree, transform, &mut pixmap.as_mut());
  }))
  .is_err()
  {
    return None;
  }

  let top = -max_y * scale;
  if !top.is_finite() {
    return None;
  }

  Some(ColorGlyphRaster {
    image: Arc::new(pixmap),
    left: parsed.view_box.min_x * scale,
    top,
  })
}

fn rasterize_svg_with_metrics(
  svg_with_color: &str,
  glyph_id: u32,
  font_size: f32,
  units_per_em: f32,
  limits: &GlyphRasterLimits,
) -> Option<ColorGlyphRaster> {
  let mut options = resvg::usvg::Options::default();
  options.resources_dir = None;
  let svg_for_parse = svg_markup_for_roxmltree(svg_with_color);

  let tree = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    resvg::usvg::Tree::from_str(svg_for_parse.as_ref(), &options)
  })) {
    Ok(Ok(tree)) => tree,
    Ok(Err(_)) => return None,
    Err(_) => return None,
  };
  let size = tree.size();
  let source_width = size.width() as f32;
  let source_height = size.height() as f32;
  if source_width <= 0.0 || source_height <= 0.0 {
    return None;
  }

  let view_box = svg_root_view_box(svg_with_color).unwrap_or(SvgViewBox {
    min_x: 0.0,
    min_y: 0.0,
    width: source_width,
    height: source_height,
  });
  if view_box.width <= 0.0 || view_box.height <= 0.0 {
    return None;
  }

  if units_per_em <= 0.0 {
    return None;
  }

  let scale = font_size / units_per_em;
  if !scale.is_finite() || scale <= 0.0 || !font_size.is_finite() {
    return None;
  }
  let width = round_dimension(view_box.width * scale)?;
  let height = round_dimension(view_box.height * scale)?;
  if let Err(err) = limits.validate(width, height) {
    log_glyph_limit("svg", glyph_id, &err);
    return None;
  }

  let mut pixmap = new_pixmap(width, height)?;

  // Map the root SVG viewport into the glyph viewBox while respecting preserveAspectRatio,
  // then flip the Y axis so SVG glyph coordinates (y-up) align with font coordinates.
  let view_box_transform = svg_view_box_root_transform(
    &svg_with_color,
    source_width,
    source_height,
    view_box.width,
    view_box.height,
  )
  .unwrap_or_else(|| {
    Transform::from_scale(
      view_box.width / source_width,
      view_box.height / source_height,
    )
  });

  let max_y = view_box.min_y + view_box.height;
  let glyph_transform = Transform::from_row(
    scale,
    0.0,
    0.0,
    -scale,
    -view_box.min_x * scale,
    max_y * scale,
  );
  let transform = concat_transforms(glyph_transform, view_box_transform);

  if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    resvg::render(&tree, transform, &mut pixmap.as_mut());
  }))
  .is_err()
  {
    return None;
  }

  let top = -max_y * scale;
  if !top.is_finite() {
    return None;
  }

  Some(ColorGlyphRaster {
    image: Arc::new(pixmap),
    left: view_box.min_x * scale,
    top,
  })
}

pub fn sanitize_svg_glyph_for_tests(svg_bytes: &[u8]) -> Option<&str> {
  sanitize_svg_glyph(svg_bytes)
}

#[doc(hidden)]
pub fn sanitize_preprocess_parse_svg_glyph_for_fuzzing(svg_bytes: &[u8], text_color: Rgba) -> bool {
  let Some(svg_str) = sanitize_svg_glyph(svg_bytes) else {
    return false;
  };
  let svg_with_color = preprocess_svg_markup(svg_str, text_color);
  let markup = svg_with_color.as_deref().unwrap_or(svg_str);

  let mut options = resvg::usvg::Options::default();
  options.resources_dir = None;
  let markup_for_parse = svg_markup_for_roxmltree(markup);
  let parsed = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    resvg::usvg::Tree::from_str(markup_for_parse.as_ref(), &options)
  })) {
    Ok(Ok(tree)) => tree,
    Ok(Err(_)) => return false,
    Err(_) => return false,
  };

  let size = parsed.size();
  size.width() > 0.0 && size.height() > 0.0
}

#[derive(Default)]
struct SvgSanitizeBudget {
  attributes_scanned: usize,
  style_bytes_scanned: usize,
  url_tokens_scanned: usize,
}

impl SvgSanitizeBudget {
  fn note_attribute(&mut self) -> bool {
    self.attributes_scanned = self.attributes_scanned.saturating_add(1);
    self.attributes_scanned <= MAX_SVG_GLYPH_ATTRIBUTES
  }

  fn note_style_bytes(&mut self, bytes: usize) -> bool {
    self.style_bytes_scanned = self.style_bytes_scanned.saturating_add(bytes);
    self.style_bytes_scanned <= MAX_SVG_GLYPH_STYLE_BYTES
  }

  fn note_url_token(&mut self) -> bool {
    self.url_tokens_scanned = self.url_tokens_scanned.saturating_add(1);
    self.url_tokens_scanned <= MAX_SVG_GLYPH_URL_TOKENS
  }
}

fn sanitize_svg_glyph(svg_bytes: &[u8]) -> Option<&str> {
  if svg_bytes.len() > MAX_SVG_GLYPH_BYTES {
    return None;
  }

  let svg_str = std::str::from_utf8(svg_bytes).ok()?;
  let svg_for_parse = svg_markup_for_roxmltree(svg_str);
  let doc = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    Document::parse(svg_for_parse.as_ref())
  })) {
    Ok(Ok(doc)) => doc,
    Ok(Err(_)) => return None,
    Err(_) => return None,
  };
  let mut element_count = 0usize;
  let mut budget = SvgSanitizeBudget::default();

  for node in doc.descendants().filter(|n| n.is_element()) {
    element_count += 1;
    if element_count > MAX_SVG_GLYPH_NODES {
      return None;
    }
    if svg_node_has_external_reference(&node, &mut budget) {
      return None;
    }
  }

  Some(svg_str)
}

fn svg_node_has_external_reference(node: &roxmltree::Node<'_, '_>, budget: &mut SvgSanitizeBudget) -> bool {
  for attr in node.attributes() {
    if !budget.note_attribute() {
      return true;
    }

    let name = attr.name();
    let value = attr.value();

    if svg_attribute_is_href(name) || svg_attribute_is_src(name) {
      if is_disallowed_svg_reference(value) {
        return true;
      }
    }

    if name.eq_ignore_ascii_case("style") {
      if !budget.note_style_bytes(value.len()) {
        return true;
      }
      if contains_disallowed_svg_css(value, CssScanPolicy::StyleAttribute, budget, 0) {
        return true;
      }
    } else if contains_disallowed_svg_css(value, CssScanPolicy::PresentationAttribute, budget, 0) {
      return true;
    }
  }

  if node.tag_name().name().eq_ignore_ascii_case("style") {
    for child in node.children().filter(|c| c.is_text()) {
      if let Some(text) = child.text() {
        if !budget.note_style_bytes(text.len()) {
          return true;
        }
        if contains_disallowed_svg_css(text, CssScanPolicy::StyleElement, budget, 0) {
          return true;
        }
      }
    }
  }

  false
}

fn svg_attribute_is_href(name: &str) -> bool {
  if name.eq_ignore_ascii_case("href") {
    return true;
  }
  name
    .rsplit_once(':')
    .is_some_and(|(_, local)| local.eq_ignore_ascii_case("href"))
}

fn svg_attribute_is_src(name: &str) -> bool {
  if name.eq_ignore_ascii_case("src") {
    return true;
  }
  name
    .rsplit_once(':')
    .is_some_and(|(_, local)| local.eq_ignore_ascii_case("src"))
}

#[inline]
fn is_ascii_whitespace_css(ch: char) -> bool {
  matches!(ch, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' ')
}

fn trim_ascii_whitespace_css(value: &str) -> &str {
  value.trim_matches(is_ascii_whitespace_css)
}

#[derive(Clone, Copy, Debug)]
enum CssScanPolicy {
  PresentationAttribute,
  StyleAttribute,
  StyleElement,
}

impl CssScanPolicy {
  fn scan_at_rules(self) -> bool {
    matches!(self, Self::StyleAttribute | Self::StyleElement)
  }
}

fn whitespace_token_is_ascii_only(ws: &str) -> bool {
  ws.bytes()
    .all(|b| matches!(b, b'\t' | b'\n' | b'\x0C' | b'\r' | b' '))
}

fn contains_disallowed_url_function(value: &str) -> bool {
  let mut budget = SvgSanitizeBudget::default();
  contains_disallowed_svg_css(value, CssScanPolicy::PresentationAttribute, &mut budget, 0)
}

fn contains_disallowed_svg_css(
  css: &str,
  policy: CssScanPolicy,
  budget: &mut SvgSanitizeBudget,
  depth: usize,
) -> bool {
  if depth >= MAX_SVG_GLYPH_CSS_NESTING_DEPTH {
    return true;
  }
  let mut input = ParserInput::new(css);
  let mut parser = Parser::new(&mut input);
  contains_disallowed_svg_css_in_parser(&mut parser, policy, budget, depth)
}

fn contains_disallowed_svg_css_in_parser<'i, 't>(
  parser: &mut Parser<'i, 't>,
  policy: CssScanPolicy,
  budget: &mut SvgSanitizeBudget,
  depth: usize,
) -> bool {
  if depth >= MAX_SVG_GLYPH_CSS_NESTING_DEPTH {
    return true;
  }

  let scan_at_rules = policy.scan_at_rules();
  let mut pending_url_ident = false;

  loop {
    let token = match parser.next_including_whitespace_and_comments() {
      Ok(token) => token,
      Err(_) => break,
    };

    match token {
      Token::WhiteSpace(ws) if whitespace_token_is_ascii_only(ws) => {
        continue;
      }
      Token::Comment(_) => {
        continue;
      }
      Token::AtKeyword(name) => {
        pending_url_ident = false;
        if scan_at_rules
          && (name.eq_ignore_ascii_case("import") || name.eq_ignore_ascii_case("font-face"))
        {
          return true;
        }
      }
      Token::UnquotedUrl(url_value) => {
        pending_url_ident = false;
        if !budget.note_url_token() {
          return true;
        }
        if is_disallowed_svg_reference(url_value.as_ref()) {
          return true;
        }
      }
      Token::BadUrl(_) => return true,
      Token::Function(ref name) if name.eq_ignore_ascii_case("url") => {
        pending_url_ident = false;
        if !budget.note_url_token() {
          return true;
        }
        if parse_and_check_url_function_args(parser) {
          return true;
        }
      }
      Token::Ident(name) => {
        pending_url_ident = name.eq_ignore_ascii_case("url");
      }
      Token::Function(_)
      | Token::ParenthesisBlock
      | Token::SquareBracketBlock
      | Token::CurlyBracketBlock => {
        let is_url_paren = pending_url_ident && matches!(token, Token::ParenthesisBlock);
        pending_url_ident = false;

        if is_url_paren {
          if !budget.note_url_token() {
            return true;
          }
          let mut bad = false;
          let parse_result = parser.parse_nested_block(|nested| {
            bad = parse_url_nested_block(nested);
            if bad {
              return Err(nested.new_custom_error(()));
            }
            Ok::<_, cssparser::ParseError<'i, ()>>(())
          });
          if parse_result.is_err() {
            return true;
          }
          if bad {
            return true;
          }
          continue;
        }

        let mut disallowed = false;
        let parse_result = parser.parse_nested_block(|nested| {
          disallowed = contains_disallowed_svg_css_in_parser(nested, policy, budget, depth + 1);
          if disallowed {
            return Err(nested.new_custom_error(()));
          }
          Ok::<_, cssparser::ParseError<'i, ()>>(())
        });
        if parse_result.is_err() {
          return true;
        }
        if disallowed {
          return true;
        }
      }
      _ => {
        pending_url_ident = false;
      }
    }

  }

  false
}

fn parse_and_check_url_function_args<'i, 't>(
  parser: &mut Parser<'i, 't>,
) -> bool {
  let mut disallowed = false;
  let parse_result = parser.parse_nested_block(|nested| {
    disallowed = parse_url_nested_block(nested);
    if disallowed {
      return Err(nested.new_custom_error(()));
    }
    Ok::<_, cssparser::ParseError<'i, ()>>(())
  });
  if parse_result.is_err() {
    return true;
  }
  disallowed
}

fn parse_url_nested_block<'i, 't>(
  nested: &mut Parser<'i, 't>,
) -> bool {
  let mut arg: Option<cssparser::CowRcStr<'i>> = None;
  let mut saw_non_trivia = false;

  while !nested.is_exhausted() {
    let token = match nested.next_including_whitespace_and_comments() {
      Ok(token) => token,
      Err(_) => return true,
    };

    match token {
      Token::WhiteSpace(ws) if whitespace_token_is_ascii_only(ws) => {}
      Token::Comment(_) => {}
      Token::QuotedString(s) | Token::UnquotedUrl(s) | Token::Ident(s) => {
        arg = Some(s.clone());
        saw_non_trivia = true;
        break;
      }
      Token::BadUrl(_) => return true,
      _ => {
        saw_non_trivia = true;
      }
    }
  }

  if let Some(arg) = arg {
    return is_disallowed_svg_reference(arg.as_ref());
  }

  saw_non_trivia
}

fn is_disallowed_svg_reference(target: &str) -> bool {
  match classify_svg_reference(target) {
    SvgReference::Empty | SvgReference::Fragment => false,
    SvgReference::DataUrl { too_large } => too_large,
    SvgReference::External => true,
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SvgReference {
  Empty,
  Fragment,
  DataUrl { too_large: bool },
  External,
}

fn classify_svg_reference(target: &str) -> SvgReference {
  let trimmed = trim_ascii_whitespace_css(target);
  if trimmed.is_empty() {
    return SvgReference::Empty;
  }
  if trimmed.starts_with('#') {
    return SvgReference::Fragment;
  }
  if starts_with_case_insensitive(trimmed, "data:") {
    return SvgReference::DataUrl {
      too_large: trimmed.len() > MAX_SVG_GLYPH_DATA_URL_BYTES,
    };
  }
  SvgReference::External
}

fn starts_with_case_insensitive(value: &str, prefix: &str) -> bool {
  value
    .get(..prefix.len())
    .map(|s| s.eq_ignore_ascii_case(prefix))
    .unwrap_or(false)
}

fn preprocess_svg_markup(svg: &str, text_color: Rgba) -> Option<String> {
  let svg_for_parse = svg_markup_for_roxmltree(svg);
  let doc = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    Document::parse(svg_for_parse.as_ref())
  })) {
    Ok(Ok(doc)) => doc,
    Ok(Err(_)) => return None,
    Err(_) => return None,
  };
  let root = doc.root_element();
  let color_css = format_css_color(text_color);

  let mut replacements: Vec<(Range<usize>, String)> = Vec::new();
  let mut root_has_style = false;
  let mut root_has_color_attribute = false;

  for node in doc.descendants().filter(|node| node.is_element()) {
    let is_root = node.id() == root.id();
    for attr in node.attributes() {
      let mut new_value = None;
      let name = attr.name();
      if name.eq_ignore_ascii_case("fill") || name.eq_ignore_ascii_case("stroke") {
        new_value = replace_context_paint(attr.value());
      } else if name.eq_ignore_ascii_case("style") {
        new_value = rewrite_style_attribute(attr.value(), is_root.then_some(color_css.as_str()));
        if is_root {
          root_has_style = true;
        }
      } else if is_root && name.eq_ignore_ascii_case("color") {
        root_has_color_attribute = true;
        if attr.value() != color_css {
          new_value = Some(color_css.clone());
        }
      }

      if let Some(value) = new_value {
        replacements.push((attr.range_value(), value));
      }
    }
  }

  if !root_has_style && !root_has_color_attribute {
    if let Some(insert_at) = find_root_style_insertion(svg, root.range()) {
      replacements.push((
        insert_at..insert_at,
        format!(r#" style="color:{}""#, color_css),
      ));
    }
  }

  if replacements.is_empty() {
    return None;
  }

  replacements.sort_by(|a, b| b.0.start.cmp(&a.0.start));
  let mut output = svg.to_string();
  for (range, value) in replacements {
    output.replace_range(range, &value);
  }
  Some(output)
}

fn rewrite_style_attribute(style: &str, inject_color: Option<&str>) -> Option<String> {
  let mut declarations = Vec::new();
  let mut changed = false;
  let mut saw_color = false;

  for raw in style.split(';') {
    let raw = trim_ascii_whitespace_css(raw);
    if raw.is_empty() {
      continue;
    }
    if let Some((name, value)) = raw.split_once(':') {
      let name = trim_ascii_whitespace_css(name);
      let mut value = trim_ascii_whitespace_css(value).to_string();
      if let Some(replaced) = replace_context_paint(&value) {
        value = replaced;
        changed = true;
      }

      if name.eq_ignore_ascii_case("color") {
        saw_color = true;
        if let Some(color) = inject_color {
          if value != color {
            value = color.to_string();
            changed = true;
          }
        }
      }

      declarations.push(format!("{}:{}", name, value));
    } else {
      declarations.push(raw.to_string());
    }
  }

  if let Some(color) = inject_color {
    if !saw_color {
      declarations.push(format!("color:{}", color));
      changed = true;
    }
  }

  if changed {
    Some(declarations.join(";"))
  } else {
    None
  }
}

fn replace_context_paint(value: &str) -> Option<String> {
  static CONTEXT_RE: OnceLock<Regex> = OnceLock::new();
  let re = CONTEXT_RE.get_or_init(|| Regex::new("(?i)context-(fill|stroke)").unwrap());
  if re.is_match(value) {
    Some(re.replace_all(value, "currentColor").into_owned())
  } else {
    None
  }
}

fn find_root_style_insertion(svg: &str, root_range: Range<usize>) -> Option<usize> {
  let start = root_range.start;
  let slice = svg.get(start..)?;
  let mut end = slice.find('>')? + start;
  if end > start && svg.as_bytes().get(end - 1) == Some(&b'/') {
    end -= 1;
  }
  Some(end)
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

fn concat_transforms(a: Transform, b: Transform) -> Transform {
  Transform::from_row(
    a.sx * b.sx + a.kx * b.ky,
    a.ky * b.sx + a.sy * b.ky,
    a.sx * b.kx + a.kx * b.sy,
    a.ky * b.kx + a.sy * b.sy,
    a.sx * b.tx + a.kx * b.ty + a.tx,
    a.ky * b.tx + a.sy * b.ty + a.ty,
  )
}

#[cfg(test)]
mod tests {
  use super::super::limits::GlyphRasterLimits;
  use super::{
    classify_svg_reference, contains_disallowed_url_function, preprocess_svg_markup,
    rasterize_svg_with_metrics, sanitize_svg_glyph_for_tests, SvgReference, MAX_SVG_GLYPH_DATA_URL_BYTES,
  };
  use crate::style::color::Rgba;

  #[test]
  fn svg_glyph_rasterization_respects_limits() {
    let svg = r#"<svg width="10000" height="10000" viewBox="0 0 10000 10000"></svg>"#;
    let limits = GlyphRasterLimits::new(1024, 1024_u64 * 1024_u64);
    assert!(rasterize_svg_with_metrics(svg, 1, 50.0, 1.0, &limits).is_none());
  }

  #[test]
  fn svg_glyph_helpers_reject_invalid_markup_without_panicking() {
    let invalid = b"<svg><";
    let sanitized = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      sanitize_svg_glyph_for_tests(invalid)
    }));
    assert!(sanitized.is_ok(), "sanitize_svg_glyph panicked");
    assert!(sanitized.unwrap().is_none());

    let preprocess = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      preprocess_svg_markup("<svg><", Rgba::rgb(1, 2, 3))
    }));
    assert!(preprocess.is_ok(), "preprocess_svg_markup panicked");
  }

  #[test]
  fn non_ascii_whitespace_svg_reference_classification_does_not_trim_nbsp() {
    assert_eq!(classify_svg_reference(" "), SvgReference::Empty);
    assert_eq!(classify_svg_reference("\u{00A0}"), SvgReference::External);
    assert_eq!(classify_svg_reference(" \u{00A0} "), SvgReference::External);
    assert!(
      contains_disallowed_url_function("url(\u{00A0})"),
      "NBSP should not be treated as CSS whitespace when classifying url() targets"
    );
  }

  #[test]
  fn svg_sanitizer_allows_fragment_url_references() {
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg"><path fill="url(#grad)"/></svg>"#;
    assert!(sanitize_svg_glyph_for_tests(svg.as_bytes()).is_some());
  }

  #[test]
  fn svg_sanitizer_rejects_external_urls_even_with_css_escapes() {
    let svg =
      r#"<svg xmlns="http://www.w3.org/2000/svg"><path fill="u\72l(https://example.com/res)"/></svg>"#;
    assert!(sanitize_svg_glyph_for_tests(svg.as_bytes()).is_none());
  }

  #[test]
  fn svg_sanitizer_rejects_external_urls_even_with_css_comments() {
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg"><path fill="url/*comment*/(https://example.com/res)"/></svg>"#;
    assert!(sanitize_svg_glyph_for_tests(svg.as_bytes()).is_none());
  }

  #[test]
  fn svg_sanitizer_rejects_oversized_data_urls() {
    let data = format!("data:{}", "a".repeat(MAX_SVG_GLYPH_DATA_URL_BYTES + 1));
    let svg = format!(
      r#"<svg xmlns="http://www.w3.org/2000/svg"><path fill="url({data})"/></svg>"#
    );
    assert!(sanitize_svg_glyph_for_tests(svg.as_bytes()).is_none());
  }

  #[test]
  fn svg_sanitizer_rejects_css_import_rules() {
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg"><style>@import url(https://example.com/res);</style></svg>"#;
    assert!(sanitize_svg_glyph_for_tests(svg.as_bytes()).is_none());
  }

  #[test]
  fn svg_sanitizer_rejects_css_font_face_rules() {
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg"><style>@font-face { src: url(https://example.com/res); }</style></svg>"#;
    assert!(sanitize_svg_glyph_for_tests(svg.as_bytes()).is_none());
  }

  #[test]
  fn svg_sanitizer_handles_namespaced_hrefs() {
    let external = r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink"><image xlink:href="https://example.com/res"/></svg>"#;
    assert!(sanitize_svg_glyph_for_tests(external.as_bytes()).is_none());

    let fragment = r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink"><use xlink:href="#glyph"/></svg>"##;
    assert!(sanitize_svg_glyph_for_tests(fragment.as_bytes()).is_some());
  }
}
