use roxmltree::Document;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;

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
  let Ok(doc) = Document::parse(fragment) else {
    return HashSet::new();
  };

  let mut refs = HashSet::new();
  for node in doc.descendants() {
    if node.is_element() {
      for attr in node.attributes() {
        let name = attr.name();
        if name.eq_ignore_ascii_case("href")
          || name
            .rsplit_once(':')
            .is_some_and(|(_, local)| local.eq_ignore_ascii_case("href"))
        {
          let trimmed = attr.value().trim();
          if let Some(id) = trimmed.strip_prefix('#') {
            if !id.is_empty() {
              refs.insert(id.to_string());
            }
          }
        }
        extract_url_fragment_ids(attr.value(), &mut refs);
      }
      continue;
    }

    if node.is_text() {
      if let Some(text) = node.text() {
        extract_url_fragment_ids(text, &mut refs);
      }
    }
  }

  refs
}

fn collect_svg_fragment_ids(fragment: &str) -> HashSet<String> {
  let Ok(doc) = Document::parse(fragment) else {
    return HashSet::new();
  };

  let mut ids = HashSet::new();
  for node in doc.descendants().filter(|node| node.is_element()) {
    for attr in node.attributes() {
      if attr.name().eq_ignore_ascii_case("id") && !attr.value().is_empty() {
        ids.insert(attr.value().to_string());
      }
    }
  }
  ids
}

pub(crate) fn inline_svg_for_mask_id(
  defs: &HashMap<String, String>,
  mask_id: &str,
  width: u32,
  height: u32,
) -> Option<String> {
  if !defs.contains_key(mask_id) {
    return None;
  }

  let mut required: HashSet<String> = HashSet::new();
  let mut queue: VecDeque<String> = VecDeque::new();
  required.insert(mask_id.to_string());
  queue.push_back(mask_id.to_string());

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

  let mut include: Vec<&String> = required
    .iter()
    .filter(|id| !nested.contains(*id))
    .collect();
  include.sort();

  let mut out = String::new();
  out.push_str("<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"");
  out.push_str(&width.to_string());
  out.push_str("\" height=\"");
  out.push_str(&height.to_string());
  out.push_str("\" viewBox=\"0 0 ");
  out.push_str(&width.to_string());
  out.push(' ');
  out.push_str(&height.to_string());
  out.push_str("\"><defs>");
  for id in include {
    if let Some(serialized) = defs.get(id) {
      out.push_str(serialized);
    }
  }
  out.push_str("</defs><rect width=\"100%\" height=\"100%\" fill=\"white\" mask=\"url(#");
  out.push_str(mask_id);
  out.push_str(")\"/></svg>");
  Some(out)
}
