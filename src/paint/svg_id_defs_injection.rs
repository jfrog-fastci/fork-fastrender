use crate::dom::SVG_NAMESPACE;
use roxmltree::Document;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;

fn parse_svg_fragment(fragment: &str) -> Option<Document<'_>> {
  match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| Document::parse(fragment))) {
    Ok(Ok(doc)) => Some(doc),
    Ok(Err(_)) | Err(_) => None,
  }
}

fn trim_ascii_whitespace(value: &str) -> &str {
  value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
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
    if let Some(value) = node
      .attributes()
      .find(|attr| attr.name().eq_ignore_ascii_case("id"))
      .map(|attr| attr.value())
      .filter(|v| !v.is_empty())
    {
      ids.insert(value.to_string());
    }
  }
  ids
}

fn contains_xlink_prefix(value: &str) -> bool {
  const NEEDLE: &[u8] = b"xlink:";
  value
    .as_bytes()
    .windows(NEEDLE.len())
    .any(|window| window.eq_ignore_ascii_case(NEEDLE))
}

fn find_svg_root_start_tag_bounds(svg: &str) -> Option<(usize, usize)> {
  const NEEDLE: &[u8] = b"<svg";
  let bytes = svg.as_bytes();
  if bytes.len() < NEEDLE.len() {
    return None;
  }
  let mut start = None;
  for idx in 0..=bytes.len() - NEEDLE.len() {
    if bytes[idx..idx + NEEDLE.len()].eq_ignore_ascii_case(NEEDLE) {
      start = Some(idx);
      break;
    }
  }
  let start = start?;

  let mut quote: Option<u8> = None;
  let mut idx = start + NEEDLE.len();
  while idx < bytes.len() {
    let b = bytes[idx];
    if let Some(q) = quote {
      if b == q {
        quote = None;
      }
    } else if b == b'\'' || b == b'"' {
      quote = Some(b);
    } else if b == b'>' {
      return Some((start, idx + 1));
    }
    idx += 1;
  }
  None
}

fn start_tag_has_xmlns_xlink(start_tag: &str) -> bool {
  const NEEDLE: &[u8] = b"xmlns:xlink";
  start_tag
    .as_bytes()
    .windows(NEEDLE.len())
    .any(|window| window.eq_ignore_ascii_case(NEEDLE))
}

fn svg_ids_to_inline(
  defs: &HashMap<String, String>,
  root_ids: impl IntoIterator<Item = String>,
  already_defined: &HashSet<String>,
) -> Vec<String> {
  let mut required: HashSet<String> = HashSet::new();
  let mut queue: VecDeque<String> = VecDeque::new();

  for root in root_ids {
    if already_defined.contains(&root) {
      continue;
    }
    if defs.contains_key(&root) && required.insert(root.clone()) {
      queue.push_back(root);
    }
  }

  while let Some(id) = queue.pop_front() {
    let Some(fragment) = defs.get(&id) else {
      continue;
    };
    for reference in collect_svg_fragment_references(fragment) {
      if already_defined.contains(&reference) {
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
  include
}

/// Injects same-document SVG `<defs>` into an inline SVG subtree so fragment references (e.g.
/// `<use href="#id">`) resolve across sibling `<svg>` roots.
///
/// Returns `None` when no defs need to be injected or parsing fails.
pub(crate) fn inject_svg_id_defs_raw(
  svg: &str,
  defs: &HashMap<String, String>,
) -> Option<(String, usize)> {
  if defs.is_empty() {
    return None;
  }

  let defined_ids = collect_svg_fragment_ids(svg);
  let referenced_ids = collect_svg_fragment_references(svg);
  let missing_roots = referenced_ids
    .into_iter()
    .filter(|id| !defined_ids.contains(id))
    .collect::<Vec<_>>();

  let include = svg_ids_to_inline(defs, missing_roots, &defined_ids);
  if include.is_empty() {
    return None;
  }

  let mut defs_body = String::new();
  for id in include {
    if let Some(fragment) = defs.get(&id) {
      defs_body.push_str(fragment);
    }
  }
  if defs_body.is_empty() {
    return None;
  }

  let needs_xlink = contains_xlink_prefix(&defs_body);
  let (start_tag_start, start_tag_end) = find_svg_root_start_tag_bounds(svg)?;
  let start_tag = svg.get(start_tag_start..start_tag_end)?;

  let mut extra_root_attr = "";
  if needs_xlink && !start_tag_has_xmlns_xlink(start_tag) {
    extra_root_attr = " xmlns:xlink=\"http://www.w3.org/1999/xlink\"";
  }

  let attr_len = extra_root_attr.len();
  let defs_wrapper_len = "<defs></defs>".len() + defs_body.len();

  let mut out = String::with_capacity(svg.len() + attr_len + defs_wrapper_len);

  if extra_root_attr.is_empty() {
    out.push_str(&svg[..start_tag_end]);
  } else {
    // Insert before `>` (or before `/>`) of the root start tag.
    let mut insert_at = start_tag_end - 1;
    if insert_at > start_tag_start && svg.as_bytes()[insert_at - 1] == b'/' {
      insert_at -= 1;
    }
    out.push_str(&svg[..insert_at]);
    out.push_str(extra_root_attr);
    out.push_str(&svg[insert_at..start_tag_end]);
  }

  let insert_pos = start_tag_end + attr_len;

  out.push_str("<defs>");
  out.push_str(&defs_body);
  out.push_str("</defs>");
  out.push_str(&svg[start_tag_end..]);

  Some((out, insert_pos))
}
