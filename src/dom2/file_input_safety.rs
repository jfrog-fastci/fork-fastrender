use super::{Document, NodeKind};

fn trim_ascii_whitespace(value: &str) -> &str {
  // HTML attribute parsing ignores leading/trailing ASCII whitespace (TAB/LF/FF/CR/SPACE) but does
  // not treat all Unicode whitespace as ignorable (e.g. NBSP).
  value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
}

/// Strip internal file-input selection state from authored markup.
///
/// This is a `dom2`-side equivalent of `crate::dom::strip_authored_file_input_state`: it ensures
/// remote markup (including `innerHTML` parsing) cannot prefill the internal state used to represent
/// user-selected files.
pub(super) fn strip_authored_file_input_state(doc: &mut Document) {
  // Keep this list in sync with `crate::dom::strip_authored_file_input_state`.
  const INTERNAL_FILE_SELECTION_ATTRS: [&str; 2] = ["data-fastr-files", "data-fastr-file-value"];

  for node in doc.nodes.iter_mut() {
    let NodeKind::Element {
      tag_name,
      attributes,
      ..
    } = &mut node.kind
    else {
      continue;
    };

    if !tag_name.eq_ignore_ascii_case("input") {
      continue;
    }

    let input_type = attributes
      .iter()
      .find(|(k, _)| k.eq_ignore_ascii_case("type"))
      .map(|(_, v)| v.as_str())
      .unwrap_or("");
    if !trim_ascii_whitespace(input_type).eq_ignore_ascii_case("file") {
      continue;
    }

    attributes.retain(|(k, _)| {
      if k.eq_ignore_ascii_case("value") {
        return false;
      }
      for &internal in &INTERNAL_FILE_SELECTION_ATTRS {
        if k.eq_ignore_ascii_case(internal) {
          return false;
        }
      }
      true
    });
  }
}

