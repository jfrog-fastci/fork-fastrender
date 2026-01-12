use std::borrow::Cow;
use std::ops::Range;

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
  const DOCTYPE_LEN: usize = "<!DOCTYPE".len();

  let bytes = input.as_bytes();
  if bytes.len() < DOCTYPE_LEN {
    return Cow::Borrowed(input);
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
    return Cow::Borrowed(input);
  }

  let mut out_bytes = bytes.to_vec();
  for range in ranges {
    for b in &mut out_bytes[range] {
      *b = b' ';
    }
  }

  // `input` is UTF-8. Replacing bytes with ASCII spaces preserves UTF-8 validity.
  match String::from_utf8(out_bytes) {
    Ok(s) => Cow::Owned(s),
    // Should be impossible, but fall back defensively.
    Err(_) => Cow::Borrowed(input),
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

