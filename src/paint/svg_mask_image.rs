use crate::dom::SVG_NAMESPACE;
use crate::string_match::contains_ascii_case_insensitive;
use crate::svg::svg_markup_for_roxmltree;
use roxmltree::Document;
use std::borrow::Cow;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::fmt::Write;

fn parse_svg_fragment(fragment: &str) -> Option<Document<'_>> {
  match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| Document::parse(fragment))) {
    Ok(Ok(doc)) => Some(doc),
    Ok(Err(_)) | Err(_) => None,
  }
}

fn escape_xml_attr_value(value: &str) -> Cow<'_, str> {
  if !value.contains('&')
    && !value.contains('<')
    && !value.contains('>')
    && !value.contains('"')
    && !value.contains('\'')
  {
    return Cow::Borrowed(value);
  }
  let mut out = String::with_capacity(value.len());
  for ch in value.chars() {
    match ch {
      '&' => out.push_str("&amp;"),
      '<' => out.push_str("&lt;"),
      '>' => out.push_str("&gt;"),
      '"' => out.push_str("&quot;"),
      '\'' => out.push_str("&apos;"),
      other => out.push(other),
    }
  }
  Cow::Owned(out)
}

fn rewrite_clip_path_object_bounding_box_to_user_space_on_use(
  fragment: &str,
  clip_id: &str,
  reference_width: f32,
  reference_height: f32,
) -> Option<String> {
  if !reference_width.is_finite()
    || !reference_height.is_finite()
    || reference_width <= 0.0
    || reference_height <= 0.0
  {
    return None;
  }

  let doc = parse_svg_fragment(fragment)?;
  let root = doc.root_element();
  if !root.tag_name().name().eq_ignore_ascii_case("clipPath") {
    return None;
  }
  if !root
    .attribute("clipPathUnits")
    .is_some_and(|units| units.eq_ignore_ascii_case("objectBoundingBox"))
  {
    return None;
  }

  // The caller's coordinate system for CSS `clip-path: url(#id)` is anchored at (0,0) of the
  // element reference box. Convert `objectBoundingBox` units to `userSpaceOnUse` by applying a
  // scaling transform that maps the [0,1] box onto that reference box.
  //
  // Note: Resvg/usvg is picky about which elements it accepts inside `<clipPath>`; applying the
  // scale via the clipPath's own `transform` attribute is more reliable than wrapping contents in
  // a `<g transform="...">`.
  let mut out = String::new();
  out.push_str("<clipPath");

  let mut has_id = false;
  let mut existing_transform: Option<&str> = None;
  for attr in root.attributes() {
    let name = attr.name();
    if name.eq_ignore_ascii_case("clipPathUnits") {
      continue;
    }
    if name.eq_ignore_ascii_case("transform") {
      existing_transform = Some(attr.value());
      continue;
    }
    if name.eq_ignore_ascii_case("id") {
      has_id = true;
    }
    out.push(' ');
    out.push_str(name);
    out.push_str("=\"");
    out.push_str(&escape_xml_attr_value(attr.value()));
    out.push('"');
  }
  if !has_id {
    out.push_str(" id=\"");
    out.push_str(&escape_xml_attr_value(clip_id));
    out.push('"');
  }
  out.push_str(" clipPathUnits=\"userSpaceOnUse\"");
  let mut combined_transform = String::new();
  let _ = write!(
    &mut combined_transform,
    "scale({} {})",
    reference_width, reference_height
  );
  if let Some(existing) = existing_transform {
    let existing = existing.trim();
    if !existing.is_empty() {
      combined_transform.push(' ');
      combined_transform.push_str(existing);
    }
  }
  out.push_str(" transform=\"");
  out.push_str(&escape_xml_attr_value(&combined_transform));
  out.push_str("\">");
  for child in root.children() {
    let range = child.range();
    out.push_str(fragment.get(range)?);
  }
  out.push_str("</clipPath>");
  Some(out)
}

fn extract_url_fragment_ids(value: &str, out: &mut HashSet<String>) {
  let bytes = value.as_bytes();
  let mut idx = 0usize;
  while idx + 4 <= bytes.len() {
    let b = bytes[idx];
    if (b == b'u' || b == b'U')
      && (bytes[idx + 1] == b'r' || bytes[idx + 1] == b'R')
      && (bytes[idx + 2] == b'l' || bytes[idx + 2] == b'L')
      && bytes[idx + 3] == b'('
    {
      idx += 4;
      while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
        idx += 1;
      }

      let mut quote: Option<u8> = None;
      if idx < bytes.len() && (bytes[idx] == b'\'' || bytes[idx] == b'"') {
        quote = Some(bytes[idx]);
        idx += 1;
        while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
          idx += 1;
        }
      }

      if idx < bytes.len() && bytes[idx] == b'#' {
        idx += 1;
        let start = idx;
        while idx < bytes.len() {
          let ch = bytes[idx];
          if ch == b')' || ch.is_ascii_whitespace() {
            break;
          }
          if quote.is_some_and(|q| q == ch) {
            break;
          }
          idx += 1;
        }
        if start < idx {
          out.insert(value[start..idx].to_string());
        }
      }

      while idx < bytes.len() && bytes[idx] != b')' {
        idx += 1;
      }
      if idx < bytes.len() {
        idx += 1;
      }
    } else {
      idx += 1;
    }
  }
}

fn collect_svg_fragment_references(fragment: &str) -> HashSet<String> {
  let Some(doc) = parse_svg_fragment(fragment) else {
    return HashSet::new();
  };

  fn trim_ascii_whitespace(value: &str) -> &str {
    value
      .trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
  }

  let mut refs = HashSet::new();
  fn walk(node: roxmltree::Node, in_svg_style: bool, out: &mut HashSet<String>) {
    if node.is_element() {
      let tag = node.tag_name();
      let is_svg = tag.namespace() == Some(SVG_NAMESPACE);
      let is_style = is_svg && tag.name().eq_ignore_ascii_case("style");
      let next_in_svg_style = in_svg_style || is_style;

      if is_svg {
        for attr in node.attributes() {
          let name = attr.name();
          if name.eq_ignore_ascii_case("href")
            || name
              .rsplit_once(':')
              .is_some_and(|(_, local)| local.eq_ignore_ascii_case("href"))
          {
            let trimmed = trim_ascii_whitespace(attr.value());
            if let Some(id) = trimmed.strip_prefix('#') {
              if !id.is_empty() {
                out.insert(id.to_string());
              }
            }
          }
          extract_url_fragment_ids(attr.value(), out);
        }
      }

      for child in node.children() {
        walk(child, next_in_svg_style, out);
      }
      return;
    }

    if node.is_text() && in_svg_style {
      if let Some(text) = node.text() {
        extract_url_fragment_ids(text, out);
      }
    }

    for child in node.children() {
      walk(child, in_svg_style, out);
    }
  }

  walk(doc.root(), false, &mut refs);

  refs
}

fn collect_svg_fragment_ids(fragment: &str) -> HashSet<String> {
  let Some(doc) = parse_svg_fragment(fragment) else {
    return HashSet::new();
  };

  let mut ids = HashSet::new();
  for node in doc
    .descendants()
    .filter(|node| node.is_element() && node.tag_name().namespace() == Some(SVG_NAMESPACE))
  {
    for attr in node.attributes() {
      if attr.name().eq_ignore_ascii_case("id") && !attr.value().is_empty() {
        ids.insert(attr.value().to_string());
      }
    }
  }
  ids
}

/// Collect raw SVG element fragments indexed by `id` from a full SVG document.
///
/// This is used to support external `clip-path: url(<svg-url>#id)` references by loading the
/// external SVG document and extracting the referenced `<clipPath>` (plus any other `id`-defined
/// elements it depends on).
///
/// The returned fragments are slices of the original markup (via `roxmltree::Node::range`) to
/// avoid lossy re-serialization. Invalid markup yields an empty map.
pub(crate) fn collect_svg_id_defs_from_svg_document(svg: &str) -> HashMap<String, String> {
  let svg_for_parse = svg_markup_for_roxmltree(svg);
  let doc = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    Document::parse(svg_for_parse.as_ref())
  })) {
    Ok(Ok(doc)) => doc,
    Ok(Err(_)) | Err(_) => return HashMap::new(),
  };

  let mut defs = HashMap::new();
  for node in doc
    .descendants()
    .filter(|node| node.is_element() && node.tag_name().namespace() == Some(SVG_NAMESPACE))
  {
    let mut id: Option<&str> = None;
    for attr in node.attributes() {
      if attr.name().eq_ignore_ascii_case("id") && !attr.value().is_empty() {
        id = Some(attr.value());
        break;
      }
    }
    let Some(id) = id else { continue };
    if defs.contains_key(id) {
      continue;
    }
    if let Some(fragment) = svg.get(node.range()) {
      defs.insert(id.to_string(), fragment.to_string());
    }
  }

  defs
}

fn svg_ids_to_inline(defs: &HashMap<String, String>, root_id: &str) -> Option<Vec<String>> {
  if !defs.contains_key(root_id) {
    return None;
  }

  let mut required: HashSet<String> = HashSet::new();
  let mut queue: VecDeque<String> = VecDeque::new();
  required.insert(root_id.to_string());
  queue.push_back(root_id.to_string());

  while let Some(id) = queue.pop_front() {
    let Some(fragment) = defs.get(&id) else {
      continue;
    };
    for reference in collect_svg_fragment_references(fragment) {
      if !defs.contains_key(&reference) {
        continue;
      }
      if required.insert(reference.clone()) {
        queue.push_back(reference);
      }
    }
  }

  let mut nested: HashSet<String> = HashSet::new();
  for id in required.iter() {
    let Some(fragment) = defs.get(id) else {
      continue;
    };
    for contained_id in collect_svg_fragment_ids(fragment) {
      if contained_id != *id && required.contains(&contained_id) {
        nested.insert(contained_id);
      }
    }
  }

  let mut include: Vec<String> = required
    .into_iter()
    .filter(|id| !nested.contains(id))
    .collect();
  include.sort();
  Some(include)
}

/// Computes a `<defs>...</defs>` element containing document-level SVG fragments that the given SVG
/// markup references via `href="#id"` / `url(#id)`.
///
/// This is used when FastRender rasterizes an inline `<svg>` subtree as a standalone SVG document.
/// Fragment-only references cannot resolve outside of that serialized subtree, so we inline the
/// missing fragments from the document-wide `defs` map.
///
/// The returned `<defs>` string:
/// - includes all referenced ids that are missing from `svg_fragment`,
/// - includes transitive references (e.g. gradients referenced by an inlined `<symbol>`), and
/// - suppresses nested defs so duplicate ids are not emitted twice.
pub(crate) fn defs_injection_for_svg_fragment(
  defs: &HashMap<String, String>,
  svg_fragment: &str,
) -> Option<String> {
  if defs.is_empty() || svg_fragment.is_empty() {
    return None;
  }

  // Avoid parsing unless it looks like there are fragment references.
  // This must be case-insensitive because HTML/SVG serialization preserves attribute casing.
  if !contains_ascii_case_insensitive(svg_fragment, "href")
    && !contains_ascii_case_insensitive(svg_fragment, "url(")
  {
    return None;
  }

  let local_ids = collect_svg_fragment_ids(svg_fragment);
  let refs = collect_svg_fragment_references(svg_fragment);
  if refs.is_empty() {
    return None;
  }

  let mut required: HashSet<String> = HashSet::new();
  let mut queue: VecDeque<String> = VecDeque::new();
  for id in refs {
    if local_ids.contains(&id) {
      continue;
    }
    if defs.contains_key(&id) && required.insert(id.clone()) {
      queue.push_back(id);
    }
  }

  while let Some(id) = queue.pop_front() {
    let Some(fragment) = defs.get(&id) else {
      continue;
    };
    for reference in collect_svg_fragment_references(fragment) {
      if local_ids.contains(&reference) {
        continue;
      }
      if !defs.contains_key(&reference) {
        continue;
      }
      if required.insert(reference.clone()) {
        queue.push_back(reference);
      }
    }
  }

  if required.is_empty() {
    return None;
  }

  let mut nested: HashSet<String> = HashSet::new();
  for id in required.iter() {
    let Some(fragment) = defs.get(id) else {
      continue;
    };
    for contained_id in collect_svg_fragment_ids(fragment) {
      if contained_id != *id && required.contains(&contained_id) {
        nested.insert(contained_id);
      }
    }
  }

  let mut include: Vec<String> = required
    .into_iter()
    .filter(|id| !nested.contains(id))
    .collect();
  include.sort();
  if include.is_empty() {
    return None;
  }

  let mut out = String::new();
  out.push_str("<defs>");
  for id in include {
    if let Some(serialized) = defs.get(&id) {
      out.push_str(serialized);
    }
  }
  out.push_str("</defs>");
  Some(out)
}

pub(crate) fn svg_root_start_tag_end(svg_fragment: &str) -> Option<usize> {
  // Allow leading whitespace / XML declarations by searching for the first `<svg` start tag.
  let bytes = svg_fragment.as_bytes();
  let mut start = None;
  let mut i = 0usize;
  while i + 4 <= bytes.len() {
    if bytes[i] == b'<'
      && bytes[i + 1].to_ascii_lowercase() == b's'
      && bytes[i + 2].to_ascii_lowercase() == b'v'
      && bytes[i + 3].to_ascii_lowercase() == b'g'
    {
      start = Some(i);
      break;
    }
    i += 1;
  }
  let start = start?;

  // Find the end of the root element's start tag, ensuring `>` within quoted attributes does not
  // terminate the scan early.
  let mut quote: Option<u8> = None;
  let mut i = start;
  while i < bytes.len() {
    let b = bytes[i];
    if let Some(q) = quote {
      if b == q {
        quote = None;
      }
    } else if b == b'"' || b == b'\'' {
      quote = Some(b);
    } else if b == b'>' {
      return Some(i + 1);
    }
    i += 1;
  }
  None
}

pub(crate) fn inline_svg_for_mask_id(
  defs: &HashMap<String, String>,
  mask_id: &str,
  width: u32,
  height: u32,
) -> Option<String> {
  inline_svg_for_mask_id_with_view_box(
    defs,
    mask_id,
    width.max(1) as f32,
    height.max(1) as f32,
    width.max(1),
    height.max(1),
  )
}

pub(crate) fn inline_svg_for_mask_id_with_view_box(
  defs: &HashMap<String, String>,
  mask_id: &str,
  view_width: f32,
  view_height: f32,
  render_width: u32,
  render_height: u32,
) -> Option<String> {
  if !view_width.is_finite() || !view_height.is_finite() || view_width <= 0.0 || view_height <= 0.0
  {
    return None;
  }
  if render_width == 0 || render_height == 0 {
    return None;
  }

  let include = svg_ids_to_inline(defs, mask_id)?;

  let mut out = String::new();
  out.push_str("<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"");
  out.push_str(&render_width.to_string());
  out.push_str("\" height=\"");
  out.push_str(&render_height.to_string());
  out.push_str("\" viewBox=\"0 0 ");
  out.push_str(&view_width.to_string());
  out.push(' ');
  out.push_str(&view_height.to_string());
  out.push_str("\"><defs>");
  for id in include {
    if let Some(serialized) = defs.get(&id) {
      out.push_str(serialized);
    }
  }
  out.push_str("</defs><rect width=\"100%\" height=\"100%\" fill=\"white\" mask=\"url(#");
  out.push_str(mask_id);
  out.push_str(")\"/></svg>");
  Some(out)
}

pub(crate) fn inline_svg_for_clip_path_id_with_view_box(
  defs: &HashMap<String, String>,
  clip_id: &str,
  view_width: f32,
  view_height: f32,
  render_width: u32,
  render_height: u32,
) -> Option<String> {
  if !view_width.is_finite() || !view_height.is_finite() || view_width <= 0.0 || view_height <= 0.0
  {
    return None;
  }
  if render_width == 0 || render_height == 0 {
    return None;
  }

  let include = svg_ids_to_inline(defs, clip_id)?;

  let mut out = String::new();
  out.push_str("<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"");
  out.push_str(&render_width.to_string());
  out.push_str("\" height=\"");
  out.push_str(&render_height.to_string());
  out.push_str("\" viewBox=\"0 0 ");
  out.push_str(&view_width.to_string());
  out.push(' ');
  out.push_str(&view_height.to_string());
  out.push_str("\"><defs>");
  for id in include {
    if let Some(serialized) = defs.get(&id) {
      out.push_str(serialized);
    }
  }
  out.push_str("</defs><rect width=\"100%\" height=\"100%\" fill=\"white\" clip-path=\"url(#");
  out.push_str(clip_id);
  out.push_str(")\"/></svg>");
  Some(out)
}

pub(crate) fn inline_svg_for_clip_path_id(
  defs: &HashMap<String, String>,
  clip_id: &str,
  width: u32,
  height: u32,
) -> Option<String> {
  inline_svg_for_clip_path_id_with_view_box(
    defs,
    clip_id,
    width.max(1) as f32,
    height.max(1) as f32,
    width.max(1),
    height.max(1),
  )
}

/// Inline an SVG that rasterizes `clip-path: url(#id)` over an arbitrary viewBox origin.
///
/// This is used to render clip paths over a larger mask surface (e.g. the stacking context bounds)
/// without changing the clipPath coordinate system, which remains anchored at (0,0) of the
/// reference box passed in by the caller.
pub(crate) fn inline_svg_for_clip_path_id_with_view_box_offset(
  defs: &HashMap<String, String>,
  clip_id: &str,
  reference_width: f32,
  reference_height: f32,
  viewbox_x: f32,
  viewbox_y: f32,
  view_width: f32,
  view_height: f32,
  render_width: u32,
  render_height: u32,
) -> Option<String> {
  if !viewbox_x.is_finite() || !viewbox_y.is_finite() {
    return None;
  }
  if !view_width.is_finite() || !view_height.is_finite() || view_width <= 0.0 || view_height <= 0.0
  {
    return None;
  }
  if render_width == 0 || render_height == 0 {
    return None;
  }

  let include = svg_ids_to_inline(defs, clip_id)?;
  let rewritten_clip_path = defs.get(clip_id).and_then(|fragment| {
    rewrite_clip_path_object_bounding_box_to_user_space_on_use(
      fragment,
      clip_id,
      reference_width,
      reference_height,
    )
  });

  let mut out = String::new();
  out.push_str("<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"");
  out.push_str(&render_width.to_string());
  out.push_str("\" height=\"");
  out.push_str(&render_height.to_string());
  let _ = write!(
    &mut out,
    "\" viewBox=\"{} {} {} {}\"><defs>",
    viewbox_x, viewbox_y, view_width, view_height
  );
  for id in include {
    if id == clip_id {
      if let Some(rewritten) = rewritten_clip_path.as_ref() {
        out.push_str(rewritten);
        continue;
      }
    }
    if let Some(serialized) = defs.get(&id) {
      out.push_str(serialized);
    }
  }
  out.push_str("</defs>");
  let _ = write!(
    &mut out,
    "<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" fill=\"white\" clip-path=\"url(#{})\"/></svg>",
    viewbox_x, viewbox_y, view_width, view_height, clip_id
  );
  Some(out)
}

#[cfg(test)]
mod tests {
  use super::{
    collect_svg_fragment_ids, collect_svg_fragment_references,
    collect_svg_id_defs_from_svg_document, inline_svg_for_clip_path_id,
    inline_svg_for_clip_path_id_with_view_box, inline_svg_for_mask_id,
    inline_svg_for_mask_id_with_view_box,
  };
  use std::collections::HashMap;

  #[test]
  fn svg_mask_image_helpers_do_not_panic_on_invalid_markup() {
    let invalid = "<svg><";

    let refs = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      collect_svg_fragment_references(invalid)
    }));
    assert!(refs.is_ok());
    assert!(refs.unwrap().is_empty());

    let ids = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      collect_svg_fragment_ids(invalid)
    }));
    assert!(ids.is_ok());
    assert!(ids.unwrap().is_empty());

    let defs = HashMap::from([
      ("mask".to_string(), invalid.to_string()),
      ("clip".to_string(), invalid.to_string()),
    ]);
    let inlined = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      inline_svg_for_mask_id(&defs, "mask", 10, 10)
    }));
    assert!(inlined.is_ok());
    assert!(inlined.unwrap().is_some());

    let inlined = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      inline_svg_for_clip_path_id(&defs, "clip", 10, 10)
    }));
    assert!(inlined.is_ok());
    assert!(inlined.unwrap().is_some());

    let collected = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      collect_svg_id_defs_from_svg_document(invalid)
    }));
    assert!(collected.is_ok());
    assert!(collected.unwrap().is_empty());
  }

  #[test]
  fn svg_mask_image_helpers_collect_ids_and_references() {
    let fragment = r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink"><defs><mask id="m"></mask><g id="ref"></g><use xlink:href="#ref" fill="url(#m)"/></defs></svg>"##;

    let refs = collect_svg_fragment_references(fragment);
    assert!(refs.contains("ref"));
    assert!(refs.contains("m"));

    let ids = collect_svg_fragment_ids(fragment);
    assert!(ids.contains("m"));
    assert!(ids.contains("ref"));
  }

  #[test]
  fn svg_mask_image_helpers_do_not_trim_non_ascii_whitespace_in_href_refs() {
    let nbsp = "\u{00A0}";
    let fragment = format!(
      r##"<svg xmlns="http://www.w3.org/2000/svg"><use href="#ref{}"/></svg>"##,
      nbsp
    );

    let refs = collect_svg_fragment_references(&fragment);
    assert!(
      refs.contains(&format!("ref{nbsp}")),
      "expected href refs to preserve NBSP, got {refs:?}"
    );
    assert!(
      !refs.contains("ref"),
      "href refs should not trim non-ASCII whitespace like NBSP"
    );
  }

  #[test]
  fn svg_clip_path_image_inlines_transitive_references() {
    let clip = r##"<clipPath xmlns="http://www.w3.org/2000/svg" id="clip"><use href="#shape"/><path d="M0 0H10V10Z" fill="url(#paint)"/></clipPath>"##;
    let shape = r##"<rect xmlns="http://www.w3.org/2000/svg" id="shape" width="10" height="10"/>"##;
    let paint = r##"<linearGradient xmlns="http://www.w3.org/2000/svg" id="paint"><stop offset="0" stop-color="white"/></linearGradient>"##;
    let defs = HashMap::from([
      ("clip".to_string(), clip.to_string()),
      ("shape".to_string(), shape.to_string()),
      ("paint".to_string(), paint.to_string()),
    ]);

    let svg = inline_svg_for_clip_path_id(&defs, "clip", 12, 12).expect("expected svg");

    assert!(svg.contains(clip));
    assert!(svg.contains(shape));
    assert!(svg.contains(paint));
    assert!(svg.contains("clip-path=\"url(#clip)\""));
  }

  #[test]
  fn svg_clip_path_image_collects_url_refs_from_style_text() {
    let clip = r##"<clipPath xmlns="http://www.w3.org/2000/svg" id="clip"><style>.x{fill:url(#paint)}</style><rect class="x" width="10" height="10"/></clipPath>"##;
    let paint = r##"<linearGradient xmlns="http://www.w3.org/2000/svg" id="paint"><stop offset="0" stop-color="white"/></linearGradient>"##;
    let defs = HashMap::from([
      ("clip".to_string(), clip.to_string()),
      ("paint".to_string(), paint.to_string()),
    ]);

    let svg = inline_svg_for_clip_path_id(&defs, "clip", 10, 10).expect("expected svg");
    assert!(
      svg.contains(paint),
      "expected style url(#paint) reference to include paint def"
    );
  }

  #[test]
  fn svg_clip_path_image_avoids_emitting_nested_defs_twice() {
    let clip = r##"<clipPath xmlns="http://www.w3.org/2000/svg" id="clip"><use href="#nested"/><g id="nested"><metadata>EXTRA</metadata></g></clipPath>"##;
    let nested =
      r##"<g xmlns="http://www.w3.org/2000/svg" id="nested"><metadata>EXTRA</metadata></g>"##;
    let defs = HashMap::from([
      ("clip".to_string(), clip.to_string()),
      ("nested".to_string(), nested.to_string()),
    ]);

    let svg = inline_svg_for_clip_path_id(&defs, "clip", 10, 10).expect("expected svg");
    assert_eq!(
      svg.matches("EXTRA").count(),
      1,
      "expected nested def to only appear once in output: {svg}"
    );
  }

  #[test]
  fn svg_clip_path_image_supports_separate_view_box_and_render_size() {
    let clip = r##"<clipPath xmlns="http://www.w3.org/2000/svg" id="clip"><rect x="0" y="0" width="10" height="10"/></clipPath>"##;
    let defs = HashMap::from([("clip".to_string(), clip.to_string())]);

    let svg = inline_svg_for_clip_path_id_with_view_box(&defs, "clip", 10.5, 20.25, 21, 41)
      .expect("expected svg");

    assert!(
      svg.contains("width=\"21\""),
      "expected render width in root <svg>, got: {svg}"
    );
    assert!(
      svg.contains("height=\"41\""),
      "expected render height in root <svg>, got: {svg}"
    );
    assert!(
      svg.contains("viewBox=\"0 0 10.5 20.25\""),
      "expected viewBox to use unscaled CSS dimensions, got: {svg}"
    );
  }

  #[test]
  fn svg_mask_image_supports_separate_view_box_and_render_size() {
    let mask = r##"<mask xmlns="http://www.w3.org/2000/svg" id="mask"><rect width="100%" height="100%" fill="white"/></mask>"##;
    let defs = HashMap::from([("mask".to_string(), mask.to_string())]);

    let svg = inline_svg_for_mask_id_with_view_box(&defs, "mask", 10.5, 20.25, 21, 41)
      .expect("expected svg");

    assert!(
      svg.contains("width=\"21\""),
      "expected render width in root <svg>, got: {svg}"
    );
    assert!(
      svg.contains("height=\"41\""),
      "expected render height in root <svg>, got: {svg}"
    );
    assert!(
      svg.contains("viewBox=\"0 0 10.5 20.25\""),
      "expected viewBox to use unscaled CSS dimensions, got: {svg}"
    );
  }
}
