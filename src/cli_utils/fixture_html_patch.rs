//! Fixture HTML patching utilities.
//!
//! These helpers inject a small set of `<head>` tags into an HTML byte buffer without fully
//! parsing HTML. The patch is used by:
//! - `xtask chrome-baseline-fixtures`: Chrome baselines are rendered from a patched HTML file
//!   written into a scratch directory, so we inject `<base href=...>` to keep relative subresources
//!   resolving against the fixture directory.
//! - `render_fixtures --patch-html-for-chrome-baseline`: When comparing FastRender fixture renders
//!   against Chrome baselines, the baseline harness forces a light color scheme and white root
//!   background for determinism and hides scrollbars; this flag applies the same patch on the
//!   FastRender side so comparisons are meaningful.

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
  // Chrome fixture baselines use host/system fonts, while FastRender fixture renders typically run
  // with bundled fonts for determinism. That mismatch can dominate pixel diffs even when layout is
  // otherwise correct.
  //
  // To make fixture diffs more actionable, alias a small set of common "system" families to the
  // deterministic bundled fonts shipped with the repo. The relative URLs assume the baseline patch
  // sets `<base href=".../tests/pages/fixtures/<stem>/">` (see callers); the `../../../` prefix
  // walks back up to the `tests/` directory.
  const BUNDLED_FONT_ALIASES_STYLE: &str = r#"<style>
@font-face {
  font-family: "Liberation Sans";
  src: url("../../../fonts/RobotoFlex-VF.ttf") format("truetype");
  font-weight: 100 1000;
  font-style: normal;
}
 @font-face {
   font-family: "DejaVu Sans";
   src: url("../../../fonts/RobotoFlex-VF.ttf") format("truetype");
   font-weight: 100 1000;
   font-style: normal;
 }
 @font-face {
   font-family: Verdana;
   src: url("../../../fonts/RobotoFlex-VF.ttf") format("truetype");
   font-weight: 100 1000;
   font-style: normal;
 }
 @font-face {
   font-family: Geneva;
   src: url("../../../fonts/RobotoFlex-VF.ttf") format("truetype");
   font-weight: 100 1000;
   font-style: normal;
 }
@font-face {
  font-family: Tahoma;
  src: url("../../../fonts/RobotoFlex-VF.ttf") format("truetype");
  font-weight: 100 1000;
  font-style: normal;
}
 @font-face {
   font-family: Arial;
   src: url("../../../fonts/RobotoFlex-VF.ttf") format("truetype");
   font-weight: 100 1000;
   font-style: normal;
}
@font-face {
  font-family: Helvetica;
  src: url("../../../fonts/RobotoFlex-VF.ttf") format("truetype");
  font-weight: 100 1000;
  font-style: normal;
}
@font-face {
  font-family: "Helvetica Neue";
  src: url("../../../fonts/RobotoFlex-VF.ttf") format("truetype");
  font-weight: 100 1000;
  font-style: normal;
}
@font-face {
  font-family: Roboto;
  src: url("../../../fonts/RobotoFlex-VF.ttf") format("truetype");
  font-weight: 100 1000;
  font-style: normal;
}
@font-face {
  font-family: "Segoe UI";
  src: url("../../../fonts/RobotoFlex-VF.ttf") format("truetype");
  font-weight: 100 1000;
  font-style: normal;
}
@font-face {
  font-family: Ubuntu;
  src: url("../../../fonts/RobotoFlex-VF.ttf") format("truetype");
  font-weight: 100 1000;
  font-style: normal;
}
 @font-face {
   font-family: Cantarell;
   src: url("../../../fonts/RobotoFlex-VF.ttf") format("truetype");
   font-weight: 100 1000;
   font-style: normal;
 }
@font-face {
  font-family: "DejaVu Serif";
  src: url("../../../fixtures/fonts/STIXTwoMath-Regular.otf") format("opentype");
  font-weight: 100 1000;
  font-style: normal;
}
@font-face {
  font-family: "Times New Roman";
  src: url("../../../fixtures/fonts/STIXTwoMath-Regular.otf") format("opentype");
  font-weight: 100 1000;
  font-style: normal;
}
@font-face {
  font-family: Times;
  src: url("../../../fixtures/fonts/STIXTwoMath-Regular.otf") format("opentype");
  font-weight: 100 1000;
  font-style: normal;
}
@font-face {
  font-family: "Courier New";
  src: url("../../../fixtures/fonts/NotoSansMono-subset.ttf") format("truetype");
  font-weight: 100 1000;
  font-style: normal;
}
@font-face {
  font-family: Courier;
  src: url("../../../fixtures/fonts/NotoSansMono-subset.ttf") format("truetype");
  font-weight: 100 1000;
  font-style: normal;
}
@font-face {
  font-family: "DejaVu Sans Mono";
  src: url("../../../fixtures/fonts/NotoSansMono-subset.ttf") format("truetype");
  font-weight: 100 1000;
  font-style: normal;
}
@font-face {
  font-family: "Liberation Mono";
  src: url("../../../fixtures/fonts/NotoSansMono-subset.ttf") format("truetype");
  font-weight: 100 1000;
  font-style: normal;
}
</style>
"#;
  const DISABLE_ANIMATIONS_STYLE: &str =
    "<style>*, *::before, *::after { animation: none !important; transition: none !important; scroll-behavior: auto !important; }</style>\n";
  // Chrome baselines are rendered with `--hide-scrollbars`, which removes scrollbar gutters from
  // layout. FastRender reserves classic scrollbar space during layout (15px for `auto`), so inject
  // `scrollbar-width: none` to keep our layout viewport aligned with Chrome.
  const HIDE_SCROLLBARS_STYLE: &str =
    "<style>* { scrollbar-width: none !important; }</style>\n";
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
  inserts.extend_from_slice(BUNDLED_FONT_ALIASES_STYLE.as_bytes());
  inserts.extend_from_slice(HIDE_SCROLLBARS_STYLE.as_bytes());
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

  let mut out = if let Some(out) = insert_after_open_tag(data, &lower, b"<head", &inserts) {
    out
  } else {
    let wrapped = [
      b"<head>\n".as_slice(),
      inserts.as_slice(),
      b"</head>\n".as_slice(),
    ]
    .concat();
    if let Some(out) = insert_after_open_tag(data, &lower, b"<html", &wrapped) {
      out
    } else if let Some(out) = insert_after_doctype(data, &lower, &inserts) {
      out
    } else {
      // Fall back to prefixing the tags; the HTML parser will usually move them into an implicit
      // head element.
      [inserts, data.to_vec()].concat()
    }
  };

  if disable_js {
    // Many fixtures include `decoding="async"` on `<img>` elements. In headless screenshot mode,
    // Chrome can capture before those async decodes have finished, producing blank thumbnails in
    // the baseline PNGs. Force synchronous decode so screenshots are more representative.
    out = replace_all_bytes(&out, br#"decoding="async""#, br#"decoding="sync""#);

    // Chrome's native lazy-loading (`loading="lazy"`) can defer image fetch/decoding and, for
    // animated images, delay animation start. In headless screenshot mode this can make baselines
    // timing-sensitive (e.g. GIF frame mismatches) while FastRender currently loads images eagerly.
    // Rewrite lazy-loading hints to eager so Chrome baselines stay deterministic and better aligned
    // with FastRender fixture renders.
    out = replace_all_bytes(&out, br#"loading="lazy""#, br#"loading="eager""#);
    out = replace_all_bytes(&out, br#"loading='lazy'"#, br#"loading='eager'"#);
    out = replace_all_bytes_with_ascii_boundaries(&out, b"loading=lazy", b"loading=eager");
  }

  out
}

fn replace_all_bytes(haystack: &[u8], needle: &[u8], replacement: &[u8]) -> Vec<u8> {
  if needle.is_empty() {
    return haystack.to_vec();
  }
  let mut out = Vec::with_capacity(haystack.len());
  let mut start = 0usize;
  while let Some(pos) = haystack[start..]
    .windows(needle.len())
    .position(|window| window == needle)
  {
    let idx = start + pos;
    out.extend_from_slice(&haystack[start..idx]);
    out.extend_from_slice(replacement);
    start = idx + needle.len();
  }
  out.extend_from_slice(&haystack[start..]);
  out
}

fn replace_all_bytes_with_ascii_boundaries(
  haystack: &[u8],
  needle: &[u8],
  replacement: &[u8],
) -> Vec<u8> {
  if needle.is_empty() {
    return haystack.to_vec();
  }
  let mut out = Vec::with_capacity(haystack.len());
  let mut start = 0usize;
  while let Some(pos) = haystack[start..]
    .windows(needle.len())
    .position(|window| window == needle)
  {
    let idx = start + pos;
    let before = idx.checked_sub(1).and_then(|i| haystack.get(i));
    let after = haystack.get(idx + needle.len());
    let boundary_before_ok = before.is_none_or(|b| ascii_boundary(*b));
    let boundary_after_ok = after.is_none_or(|b| ascii_boundary(*b));
    if !(boundary_before_ok && boundary_after_ok) {
      // Skip this match and continue searching past it.
      out.extend_from_slice(&haystack[start..idx + needle.len()]);
      start = idx + needle.len();
      continue;
    }
    out.extend_from_slice(&haystack[start..idx]);
    out.extend_from_slice(replacement);
    start = idx + needle.len();
  }
  out.extend_from_slice(&haystack[start..]);
  out
}

fn ascii_boundary(b: u8) -> bool {
  matches!(b, b'>' | b' ' | b'\n' | b'\r' | b'\t' | b'/')
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
    assert!(
      output_str.contains("@font-face"),
      "patched HTML should alias common system fonts to bundled fonts"
    );
    assert!(
      output_str.contains("RobotoFlex-VF.ttf"),
      "patched HTML should reference the bundled Roboto Flex font"
    );
    assert!(
      output_str.contains("DejaVu Sans"),
      "patched HTML should alias common Linux default fonts to bundled fonts"
    );
    assert!(
      output_str.contains("Verdana"),
      "patched HTML should alias common legacy sans-serif fonts to bundled fonts"
    );
    assert!(
      output_str.contains("Geneva"),
      "patched HTML should alias common legacy sans-serif fonts to bundled fonts"
    );
    assert!(
      output_str.contains("Tahoma"),
      "patched HTML should alias common Windows system fonts to bundled fonts"
    );
    assert!(
      output_str.contains("NotoSansMono-subset.ttf"),
      "patched HTML should reference the bundled monospace font"
    );
    assert!(
      output_str.contains("DejaVu Serif"),
      "patched HTML should alias common Linux default serif fonts to bundled fonts"
    );
    assert!(
      output_str.contains("DejaVu Sans Mono"),
      "patched HTML should alias common Linux default monospace fonts to bundled fonts"
    );
  }

  #[test]
  fn patch_html_forces_sync_img_decoding_when_js_disabled() {
    let input = b"<!doctype html><html><head></head><body><img decoding=\"async\" src=\"x\"></body></html>";
    let output = patch_html_bytes(input, None, true, false, true);
    let output_str = String::from_utf8_lossy(&output);
    assert!(
      output_str.contains("decoding=\"sync\""),
      "patched HTML should force decoding=sync when JS is disabled; got: {output_str}"
    );
    assert!(
      !output_str.contains("decoding=\"async\""),
      "patched HTML should rewrite decoding=async when JS is disabled; got: {output_str}"
    );
  }

  #[test]
  fn patch_html_forces_eager_loading_when_js_disabled() {
    let input = b"<!doctype html><html><head></head><body><img loading=\"lazy\" src=\"x\"><img loading='lazy' src=\"y\"><img loading=lazy src=\"z\"></body></html>";
    let output = patch_html_bytes(input, None, true, false, true);
    let output_str = String::from_utf8_lossy(&output);
    assert!(
      output_str.contains("loading=\"eager\""),
      "expected loading=\"lazy\" to be rewritten when JS is disabled; got: {output_str}"
    );
    assert!(
      output_str.contains("loading='eager'"),
      "expected loading='lazy' to be rewritten when JS is disabled; got: {output_str}"
    );
    assert!(
      output_str.contains("loading=eager"),
      "expected loading=lazy to be rewritten when JS is disabled; got: {output_str}"
    );
    assert!(
      !output_str.contains("loading=\"lazy\"")
        && !output_str.contains("loading='lazy'")
        && !output_str.contains("loading=lazy"),
      "expected loading=lazy variants to be removed when JS is disabled; got: {output_str}"
    );
  }

  #[test]
  fn patch_html_preserves_loading_lazy_when_js_enabled() {
    let input =
      b"<!doctype html><html><head></head><body><img loading=\"lazy\" src=\"x\"></body></html>";
    let output = patch_html_bytes(input, None, false, false, true);
    let output_str = String::from_utf8_lossy(&output);
    assert!(
      output_str.contains("loading=\"lazy\""),
      "expected loading=\"lazy\" to remain when JS is enabled; got: {output_str}"
    );
    assert!(
      !output_str.contains("loading=\"eager\""),
      "expected loading to remain lazy when JS is enabled; got: {output_str}"
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
    assert!(
      output_str.contains("scrollbar-width: none"),
      "patched HTML should hide scrollbars to keep layout viewport aligned with Chrome"
    );
    assert!(
      output_str.contains("@font-face"),
      "patched HTML should alias common system fonts to bundled fonts"
    );
  }
}
