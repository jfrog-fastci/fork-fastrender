/// Small HTML-escaping helpers for building trusted internal HTML templates.
///
/// This is intended for browser-internal pages (e.g. `about:` pages) and internal report HTML
/// generators. It is *not* a general-purpose HTML templating engine.
///
/// Escapes the characters that can break out of text nodes/attribute values:
/// `& < > " '`.
#[must_use]
pub fn escape_html(text: &str) -> String {
  // Fast path: avoid the per-char match when there are no escapable characters.
  if !text.contains('&')
    && !text.contains('<')
    && !text.contains('>')
    && !text.contains('"')
    && !text.contains('\'')
  {
    return text.to_string();
  }

  let mut out = String::with_capacity(text.len());
  for ch in text.chars() {
    match ch {
      '&' => out.push_str("&amp;"),
      '<' => out.push_str("&lt;"),
      '>' => out.push_str("&gt;"),
      '"' => out.push_str("&quot;"),
      '\'' => out.push_str("&#39;"),
      _ => out.push(ch),
    }
  }
  out
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn escape_html_escapes_html_special_chars() {
    assert_eq!(
      escape_html("&<>\"'"),
      "&amp;&lt;&gt;&quot;&#39;".to_string()
    );
  }
}

