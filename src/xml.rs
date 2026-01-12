use std::borrow::Cow;
use std::ops::Range;

/// Extracted `<!DOCTYPE ...>` information from markup that is being sanitized for `roxmltree`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExtractedDoctype {
  /// Byte offset of the `<!DOCTYPE` start in the original markup.
  pub start: usize,
  pub name: String,
  pub public_id: String,
  pub system_id: String,
}

#[derive(Debug, Clone, Copy)]
struct ParsedDoctype<'a> {
  name: &'a str,
  public_id: Option<&'a str>,
  system_id: Option<&'a str>,
}

fn is_xml_whitespace(b: u8) -> bool {
  // XML "S" production: https://www.w3.org/TR/xml/#NT-S
  matches!(b, b' ' | b'\t' | b'\r' | b'\n')
}

fn skip_ws(bytes: &[u8], i: &mut usize) {
  while bytes.get(*i).is_some_and(|b| is_xml_whitespace(*b)) {
    *i += 1;
  }
}

fn scan_name_token<'a>(bytes: &'a [u8], i: &mut usize) -> &'a str {
  let start = *i;
  while let Some(&b) = bytes.get(*i) {
    if is_xml_whitespace(b) || matches!(b, b'>' | b'[') {
      break;
    }
    *i += 1;
  }
  // Safety: `input` is valid UTF-8, and we only slice within it.
  unsafe { std::str::from_utf8_unchecked(&bytes[start..*i]) }
}

fn scan_word_token<'a>(bytes: &'a [u8], i: &mut usize) -> &'a [u8] {
  let start = *i;
  while let Some(&b) = bytes.get(*i) {
    if is_xml_whitespace(b) || matches!(b, b'>' | b'[') {
      break;
    }
    *i += 1;
  }
  &bytes[start..*i]
}

fn scan_quoted<'a>(bytes: &'a [u8], i: &mut usize) -> Option<&'a str> {
  let quote = *bytes.get(*i)?;
  if quote != b'\'' && quote != b'"' {
    return None;
  }
  *i += 1;
  let start = *i;
  while let Some(&b) = bytes.get(*i) {
    if b == quote {
      let out = &bytes[start..*i];
      *i += 1;
      // Safety: `input` is valid UTF-8 and quoted literals are subsets of it.
      return Some(unsafe { std::str::from_utf8_unchecked(out) });
    }
    *i += 1;
  }
  None
}

fn parse_doctype_decl(decl: &[u8]) -> ParsedDoctype<'_> {
  // `decl` is the byte slice of the entire doctype declaration, including `<!DOCTYPE` and the
  // closing `>`. The caller ensures it starts at `<!DOCTYPE` (case-insensitive).
  let mut i = 0usize;
  i = i.saturating_add(2);
  i = i.saturating_add("DOCTYPE".len());
  skip_ws(decl, &mut i);

  let name = scan_name_token(decl, &mut i);
  skip_ws(decl, &mut i);

  // Stop if internal subset starts immediately.
  if decl.get(i).is_some_and(|b| *b == b'[') {
    return ParsedDoctype {
      name,
      public_id: None,
      system_id: None,
    };
  }

  let keyword = scan_word_token(decl, &mut i);
  skip_ws(decl, &mut i);

  if keyword.eq_ignore_ascii_case(b"SYSTEM") {
    let system_id = scan_quoted(decl, &mut i);
    return ParsedDoctype {
      name,
      public_id: None,
      system_id,
    };
  }
  if keyword.eq_ignore_ascii_case(b"PUBLIC") {
    let public_id = scan_quoted(decl, &mut i);
    skip_ws(decl, &mut i);
    let system_id = scan_quoted(decl, &mut i);
    // Ignore malformed external IDs (must be both strings).
    if public_id.is_some() && system_id.is_some() {
      return ParsedDoctype {
        name,
        public_id,
        system_id,
      };
    }
    return ParsedDoctype {
      name,
      public_id: None,
      system_id: None,
    };
  }

  ParsedDoctype {
    name,
    public_id: None,
    system_id: None,
  }
}

/// Returns markup suitable for parsing with [`roxmltree`].
///
/// `roxmltree` deliberately rejects `<!DOCTYPE ...>` declarations, which are common in real-world
/// XML/SVG documents (e.g. output by authoring tools). When we want to parse such markup with
/// `roxmltree` while keeping source byte offsets stable, we replace the doctype declaration bytes
/// with ASCII spaces instead of deleting them.
///
/// This is a best-effort lexer: it is not a full XML tokenizer, but it does attempt to avoid
/// prematurely terminating on:
/// - `>` characters inside quoted strings.
/// - `>` characters within the `[...]` internal subset.
pub fn markup_for_roxmltree(input: &str) -> Cow<'_, str> {
  let (markup, _doctypes) = markup_for_roxmltree_with_doctypes(input);
  markup
}

/// Like [`markup_for_roxmltree`], but also extracts doctype metadata.
///
/// Parsing rules are pragmatic (not a full DTD parser):
/// - After `<!DOCTYPE` (case-insensitive), parse the name token.
/// - If `SYSTEM` then parse 1 quoted string → `system_id`.
/// - If `PUBLIC` then parse 2 quoted strings → `public_id`, `system_id`.
/// - Ignore internal subset; ignore malformed external ids (leave ids empty).
pub(crate) fn markup_for_roxmltree_with_doctypes(
  input: &str,
) -> (Cow<'_, str>, Vec<ExtractedDoctype>) {
  const DOCTYPE_LEN: usize = "<!DOCTYPE".len();

  let bytes = input.as_bytes();
  if bytes.len() < DOCTYPE_LEN {
    return (Cow::Borrowed(input), Vec::new());
  }

  let mut ranges: Vec<Range<usize>> = Vec::new();
  let mut search_start = 0usize;

  while search_start + DOCTYPE_LEN <= bytes.len() {
    let mut found = None;
    for i in search_start..=(bytes.len() - DOCTYPE_LEN) {
      if bytes[i] == b'<'
        && bytes.get(i + 1) == Some(&b'!')
        && bytes
          .get(i + 2..i + 9)
          .is_some_and(|needle| needle.eq_ignore_ascii_case(b"DOCTYPE"))
      {
        found = Some(i);
        break;
      }
    }
    let Some(start) = found else {
      break;
    };

    // Scan to the end of the doctype declaration.
    //
    // The internal subset is wrapped in brackets (`[...]`); while in the internal subset, `>` does
    // not end the doctype. We also ignore `>` while inside quotes to avoid terminating on URL/ID
    // strings containing `>`.
    let mut i = start + 2;
    let mut bracket_depth = 0u32;
    let mut quote: Option<u8> = None;
    while i < bytes.len() {
      let b = bytes[i];
      if let Some(q) = quote {
        if b == q {
          quote = None;
        }
        i += 1;
        continue;
      }

      match b {
        b'\'' | b'"' => quote = Some(b),
        b'[' => bracket_depth = bracket_depth.saturating_add(1),
        b']' => bracket_depth = bracket_depth.saturating_sub(1),
        b'>' if bracket_depth == 0 => {
          i += 1;
          break;
        }
        _ => {}
      }
      i += 1;
    }

    if i <= start || i > bytes.len() {
      break;
    }
    ranges.push(start..i);
    search_start = i;
  }

  if ranges.is_empty() {
    return (Cow::Borrowed(input), Vec::new());
  }

  let mut doctypes: Vec<ExtractedDoctype> = Vec::with_capacity(ranges.len());
  for range in &ranges {
    let decl = &bytes[range.start..range.end];
    let parsed = parse_doctype_decl(decl);
    doctypes.push(ExtractedDoctype {
      start: range.start,
      name: parsed.name.to_string(),
      public_id: parsed.public_id.unwrap_or("").to_string(),
      system_id: parsed.system_id.unwrap_or("").to_string(),
    });
  }

  let mut out_bytes = bytes.to_vec();
  for range in ranges {
    for b in &mut out_bytes[range] {
      *b = b' ';
    }
  }

  // `input` is UTF-8. Replacing bytes with ASCII spaces preserves UTF-8 validity.
  match String::from_utf8(out_bytes) {
    Ok(s) => (Cow::Owned(s), doctypes),
    // Should be impossible, but fall back defensively.
    Err(_) => (Cow::Borrowed(input), doctypes),
  }
}

#[cfg(test)]
mod tests {
  use super::markup_for_roxmltree;
  use roxmltree::Document;

  fn assert_blanks_doctype(input: &str, root_snippet: &str) {
    assert!(
      Document::parse(input).is_err(),
      "expected roxmltree to reject a doctype"
    );

    let output = markup_for_roxmltree(input);
    let output = output.as_ref();

    assert_eq!(output.len(), input.len(), "output length must be preserved");
    assert!(
      !output.to_ascii_lowercase().contains("<!doctype"),
      "doctype marker should be blanked out"
    );

    let root_index = output
      .find(root_snippet)
      .unwrap_or_else(|| panic!("expected output to contain `{root_snippet}`"));
    assert!(
      output
        .as_bytes()
        .iter()
        .take(root_index)
        .all(|b| *b == b' '),
      "expected everything before root snippet to be spaces"
    );

    Document::parse(output).unwrap();
  }

  #[test]
  fn blanks_system_doctype() {
    let input = r#"<!DOCTYPE root SYSTEM "http://example.com/test.dtd"><root/>"#;
    assert_blanks_doctype(input, "<root/>");
  }

  #[test]
  fn blanks_public_doctype() {
    let input = concat!(
      r#"<!DOCTYPE root PUBLIC "-//W3C//DTD XHTML 1.0 Strict//EN" "#,
      r#""http://www.w3.org/TR/xhtml1/DTD/xhtml1-strict.dtd">"#,
      "<root/>",
    );
    assert_blanks_doctype(input, "<root/>");
  }

  #[test]
  fn doctype_with_gt_inside_quotes() {
    let input = r#"<!DoCtYpE root SYSTEM "http://example.com/with>quote"><root/>"#;
    assert_blanks_doctype(input, "<root/>");
  }

  #[test]
  fn doctype_with_internal_subset_brackets() {
    let input = r#"<!DOCTYPE root [<!ENTITY a "b">]><root/>"#;
    assert_blanks_doctype(input, "<root/>");
  }

  #[test]
  fn blanks_multiple_doctypes() {
    let input = r#"<!DOCTYPE root><!DOCTYPE root SYSTEM "x"><root/>"#;
    assert_blanks_doctype(input, "<root/>");
  }

  #[test]
  fn preserves_utf8_validity_and_length() {
    let input = r#"<!DOCTYPE root SYSTEM "x"><root>π</root>"#;
    assert_blanks_doctype(input, "<root>π</root>");
  }
}
