//! Fixture HTML patching utilities.
//!
//! These helpers inject a small set of `<head>` tags into an HTML byte buffer without fully
//! parsing HTML. The patch is used by:
//! - `xtask chrome-baseline-fixtures`: Chrome baselines are rendered from a patched HTML file
//!   written into a scratch directory, so we inject `<base href=...>` to keep relative subresources
//!   resolving against the fixture directory.
//! - `render_fixtures --patch-html-for-chrome-baseline`: When comparing FastRender fixture renders
//!   against Chrome baselines, the baseline harness forces a light color scheme and white root
//!   background for determinism; this flag applies the same patch on the FastRender side so
//!   comparisons are meaningful.

/// Patch HTML fixture bytes in-place by injecting deterministic/offline tags into `<head>`.
///
/// The patch is intentionally simple and byte-oriented: we look for `<head>`, `<html>`, or a
/// doctype and insert our tags immediately after the opening token.
///
/// Important: if the input contains a doctype but omits `<html>`/`<head>`, we must not inject any
/// bytes before the doctype, or browsers may enter quirks mode. See unit tests.
pub fn patch_html_bytes(
  data: &[u8],
  base_url: Option<&str>,
  disable_js: bool,
  disable_animations: bool,
  allow_dark_mode: bool,
) -> Vec<u8> {
  const DISABLE_ANIMATIONS_STYLE: &str =
    "<style>*, *::before, *::after { animation: none !important; transition: none !important; scroll-behavior: auto !important; }</style>\n";
  const FORCE_LIGHT_META: &str = "<meta name=\"color-scheme\" content=\"light\">\n";
  const FORCE_LIGHT_STYLE: &str =
    "<style>html, body { background: white !important; color-scheme: light !important; forced-color-adjust: none !important; }</style>\n";

  let mut inserts = Vec::new();
  if let Some(base_url) = base_url {
    inserts.extend_from_slice(format!("<base href=\"{base_url}\">\n").as_bytes());
  }
  if !allow_dark_mode {
    inserts.extend_from_slice(FORCE_LIGHT_META.as_bytes());
    inserts.extend_from_slice(FORCE_LIGHT_STYLE.as_bytes());
  }
  // Enforce a deterministic/offline page load: allow only file/data subresources.
  // If JS is enabled, allow inline/file scripts for experimentation; otherwise block scripts.
  let csp = if disable_js {
    "default-src file: data:; style-src file: data: 'unsafe-inline'; script-src 'none';"
  } else {
    "default-src file: data:; style-src file: data: 'unsafe-inline'; script-src file: data: 'unsafe-inline' 'unsafe-eval';"
  };
  inserts.extend_from_slice(
    format!("<meta http-equiv=\"Content-Security-Policy\" content=\"{csp}\">\n").as_bytes(),
  );
  if disable_animations {
    inserts.extend_from_slice(DISABLE_ANIMATIONS_STYLE.as_bytes());
  }

  if inserts.is_empty() {
    return data.to_vec();
  }

  let lower = data
    .iter()
    .map(|b| b.to_ascii_lowercase())
    .collect::<Vec<_>>();

  if let Some(out) = insert_after_open_tag(data, &lower, b"<head", &inserts) {
    return out;
  }

  let wrapped = [
    b"<head>\n".as_slice(),
    inserts.as_slice(),
    b"</head>\n".as_slice(),
  ]
  .concat();
  if let Some(out) = insert_after_open_tag(data, &lower, b"<html", &wrapped) {
    return out;
  }

  // Some fixtures omit `<html>`/`<head>` but still include a `<!doctype html>` declaration. Do not
  // inject anything before the doctype because that would flip the document into quirks mode in
  // browsers and make baselines useless. Instead, inject our tags immediately after the doctype.
  if let Some(out) = insert_after_doctype(data, &lower, &inserts) {
    return out;
  }

  // Fall back to prefixing the tags; the HTML parser will usually move them into an implicit head
  // element.
  [inserts, data.to_vec()].concat()
}

fn insert_after_open_tag(
  data: &[u8],
  lower: &[u8],
  tag: &[u8],
  insertion: &[u8],
) -> Option<Vec<u8>> {
  let mut search_start = 0usize;
  while let Some(pos) = lower[search_start..]
    .windows(tag.len())
    .position(|window| window == tag)
    .map(|rel| rel + search_start)
  {
    let after = lower.get(pos + tag.len());
    let boundary_ok = matches!(
      after,
      Some(b'>') | Some(b' ') | Some(b'\n') | Some(b'\r') | Some(b'\t') | Some(b'/')
    );
    if !boundary_ok {
      search_start = pos + tag.len();
      continue;
    }

    let end = lower[pos..].iter().position(|&b| b == b'>')? + pos + 1;
    let mut out = Vec::with_capacity(data.len() + insertion.len() + 1);
    out.extend_from_slice(&data[..end]);
    out.extend_from_slice(b"\n");
    out.extend_from_slice(insertion);
    out.extend_from_slice(&data[end..]);
    return Some(out);
  }
  None
}

fn insert_after_doctype(data: &[u8], lower: &[u8], insertion: &[u8]) -> Option<Vec<u8>> {
  const DOCTYPE: &[u8] = b"<!doctype";
  let mut search_start = 0usize;
  while let Some(pos) = lower[search_start..]
    .windows(DOCTYPE.len())
    .position(|window| window == DOCTYPE)
    .map(|rel| rel + search_start)
  {
    let after = lower.get(pos + DOCTYPE.len());
    let boundary_ok = matches!(
      after,
      Some(b'>') | Some(b' ') | Some(b'\n') | Some(b'\r') | Some(b'\t')
    );
    if !boundary_ok {
      search_start = pos + DOCTYPE.len();
      continue;
    }

    let end = lower[pos..].iter().position(|&b| b == b'>')? + pos + 1;
    let mut out = Vec::with_capacity(data.len() + insertion.len() + 1);
    out.extend_from_slice(&data[..end]);
    out.extend_from_slice(b"\n");
    out.extend_from_slice(insertion);
    out.extend_from_slice(&data[end..]);
    return Some(out);
  }
  None
}

#[cfg(test)]
mod tests {
  use super::patch_html_bytes;

  #[test]
  fn patch_html_keeps_doctype_first_when_head_missing() {
    let input = b"<!doctype html>\n<meta charset=\"utf-8\">\n<body>Hello</body>\n";
    let output = patch_html_bytes(input, Some("file:///tmp/fixture/"), true, true, false);
    assert!(
      output.starts_with(b"<!doctype html>"),
      "doctype must remain the first token to avoid quirks mode"
    );

    let output_str = String::from_utf8_lossy(&output);
    assert!(
      output_str.contains("<meta name=\"color-scheme\" content=\"light\">"),
      "patched HTML should force a deterministic light color scheme"
    );
    assert!(
      output_str.contains("background: white !important"),
      "patched HTML should force a white background"
    );
    assert!(
      output_str.contains("Content-Security-Policy"),
      "patched HTML should include CSP injection"
    );
    assert!(
      output_str.contains("<base href=\"file:///tmp/fixture/\">"),
      "patched HTML should include base href injection"
    );
    assert!(
      output_str.contains("animation: none !important"),
      "patched HTML should disable animations for deterministic baselines"
    );
  }

  #[test]
  fn patch_html_can_opt_out_of_animation_disabling() {
    let input = b"<!doctype html><html><head></head><body>Hello</body></html>";
    let output = patch_html_bytes(input, Some("file:///tmp/fixture/"), true, false, false);
    let output_str = String::from_utf8_lossy(&output);
    assert!(
      !output_str.contains("animation: none !important"),
      "opt-out should omit the animation-disabling CSS"
    );
    assert!(
      !output_str.contains("transition: none !important"),
      "opt-out should omit the transition-disabling CSS"
    );
  }
}

