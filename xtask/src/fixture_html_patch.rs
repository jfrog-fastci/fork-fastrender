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

use base64::Engine as _;
use image::ImageFormat;
use std::collections::HashMap;
use std::io::Cursor;
use std::path::PathBuf;
use url::Url;

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
  const HIDE_SCROLLBARS_STYLE: &str = "<style>* { scrollbar-width: none !important; }</style>\n";
  const FORCE_LIGHT_META: &str = "<meta name=\"color-scheme\" content=\"light\">\n";
  const FORCE_LIGHT_STYLE: &str =
    // Important: set `position`/`z-index` on `body` so it forms a stacking context.
    //
    // Many real-world pages use `position:absolute; z-index:-1` for "background" images. If `body`
    // does not establish a stacking context, those negative z-index stacking contexts can escape up
    // to the root stacking context and end up painting *behind* the forced `body` background,
    // effectively disappearing in baseline screenshots.
    "<style>html, body { background: white !important; color-scheme: light !important; forced-color-adjust: none !important; } body { position: relative !important; z-index: 0 !important; }</style>\n";

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
    // Many fixtures include `decoding=async` (quoted or unquoted) on `<img>` elements. In headless
    // screenshot mode, Chrome can capture before those async decodes have finished, producing
    // blank thumbnails in the baseline PNGs. Force synchronous decode so screenshots are more
    // representative.
    out = replace_all_bytes(&out, br#"decoding="async""#, br#"decoding="sync""#);
    out = replace_all_bytes(&out, br#"decoding='async'"#, br#"decoding='sync'"#);
    out = replace_all_bytes_with_ascii_boundaries(&out, b"decoding=async", b"decoding=sync");

    // Chrome's native lazy-loading (`loading="lazy"`) can defer image fetch/decoding and, for
    // animated images, delay animation start. In headless screenshot mode this can make baselines
    // timing-sensitive (e.g. GIF frame mismatches) while FastRender currently loads images eagerly.
    // Rewrite lazy-loading hints to eager so Chrome baselines stay deterministic and better aligned
    // with FastRender fixture renders.
    out = replace_all_bytes(&out, br#"loading="lazy""#, br#"loading="eager""#);
    out = replace_all_bytes(&out, br#"loading='lazy'"#, br#"loading='eager'"#);
    out = replace_all_bytes_with_ascii_boundaries(&out, b"loading=lazy", b"loading=eager");

    if let Some(base_url) = base_url {
      out = rewrite_gif_image_srcs_to_static_png_data_urls(&out, base_url);
    }

    // When JavaScript execution is disabled, `<noscript>` fallback content should become active.
    //
    // Our fixture baselines disable scripts via CSP (`script-src 'none'`). Chromium still parses the
    // document with "scripting enabled" semantics in this mode, which suppresses `<noscript>`
    // fallback content (notably the common `head`-injected stylesheet link pattern and lazyload
    // fallbacks). That can leave large sites effectively unstyled in both Chrome baselines and
    // FastRender fixture renders.
    //
    // Promote `<noscript>` content by unwrapping the tags at the HTML byte level. This keeps the
    // patch deterministic/offline while making JS-disabled baselines closer to what users expect
    // when scripts do not run.
    out = unwrap_noscript_tags(&out);
  }

  out
}

fn unwrap_noscript_tags(data: &[u8]) -> Vec<u8> {
  let lower: Vec<u8> = data.iter().map(|b| b.to_ascii_lowercase()).collect();
  let mut out = Vec::with_capacity(data.len());
  let mut idx = 0usize;

  fn tag_boundary_ok(after: Option<&u8>) -> bool {
    matches!(
      after,
      Some(b'>') | Some(b' ') | Some(b'\n') | Some(b'\r') | Some(b'\t') | Some(b'/')
    )
  }

  while idx < data.len() {
    // Strip `<noscript ...>` open tags.
    if idx + 9 <= lower.len() && lower[idx..].starts_with(b"<noscript") {
      if tag_boundary_ok(lower.get(idx + 9)) {
        if let Some(end_rel) = lower[idx..].iter().position(|&b| b == b'>') {
          idx += end_rel + 1;
          continue;
        }
      }
    }

    // Strip `</noscript>` close tags.
    if idx + 10 <= lower.len() && lower[idx..].starts_with(b"</noscript") {
      if tag_boundary_ok(lower.get(idx + 10)) {
        if let Some(end_rel) = lower[idx..].iter().position(|&b| b == b'>') {
          idx += end_rel + 1;
          continue;
        }
      }
    }

    out.push(data[idx]);
    idx += 1;
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

fn rewrite_gif_image_srcs_to_static_png_data_urls(html: &[u8], base_url: &str) -> Vec<u8> {
  let base = match Url::parse(base_url) {
    Ok(url) => url,
    Err(_) => return html.to_vec(),
  };
  if base.scheme() != "file" {
    return html.to_vec();
  }

  let lower = html
    .iter()
    .map(|b| b.to_ascii_lowercase())
    .collect::<Vec<_>>();
  let mut cache: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();

  let mut out = Vec::with_capacity(html.len());
  // Avoid quadratic scans when only one of the tags exists (e.g. pages with many `<img>` tags but
  // no `<source>`). Scan once for `<`, then check whether it's an `<img`/`<source` tag we care
  // about.
  let mut cursor = 0usize;
  let mut scan = 0usize;

  const IMG: &[u8] = b"<img";
  const SOURCE: &[u8] = b"<source";
  while scan < lower.len() {
    let Some(rel) = lower[scan..].iter().position(|&b| b == b'<') else {
      break;
    };
    let pos = scan + rel;

    let tag_len = if lower[pos..].starts_with(IMG) {
      IMG.len()
    } else if lower[pos..].starts_with(SOURCE) {
      SOURCE.len()
    } else {
      scan = pos + 1;
      continue;
    };

    let after = lower.get(pos + tag_len);
    let boundary_ok = matches!(
      after,
      Some(b'>') | Some(b' ') | Some(b'\n') | Some(b'\r') | Some(b'\t') | Some(b'/')
    );
    if !boundary_ok {
      scan = pos + 1;
      continue;
    }

    let Some(end_rel) = lower[pos..].iter().position(|&b| b == b'>') else {
      break;
    };
    let end = pos + end_rel + 1;

    out.extend_from_slice(&html[cursor..pos]);
    out.extend_from_slice(&rewrite_image_tag_gif_urls_to_data_urls(
      &html[pos..end],
      &lower[pos..end],
      &base,
      &mut cache,
    ));
    cursor = end;
    scan = end;
  }

  out.extend_from_slice(&html[cursor..]);
  out
}

fn rewrite_image_tag_gif_urls_to_data_urls(
  tag: &[u8],
  tag_lower: &[u8],
  base: &Url,
  cache: &mut HashMap<Vec<u8>, Vec<u8>>,
) -> Vec<u8> {
  let mut out = Vec::with_capacity(tag.len());
  let mut last_copied = 0usize;
  let mut mutated = false;

  let mut i = 0usize;
  while i < tag.len() {
    // Find the start of the next attribute name.
    while i < tag.len() && !is_attr_name_start(tag[i]) {
      i += 1;
    }
    let name_start = i;
    while i < tag.len() && is_attr_name_char(tag[i]) {
      i += 1;
    }
    let name_end = i;
    if name_start == name_end {
      break;
    }

    while i < tag.len() && tag[i].is_ascii_whitespace() {
      i += 1;
    }
    if i >= tag.len() || tag[i] != b'=' {
      continue;
    }
    i += 1;
    while i < tag.len() && tag[i].is_ascii_whitespace() {
      i += 1;
    }
    if i >= tag.len() {
      break;
    }

    let name = &tag_lower[name_start..name_end];

    let quote = match tag[i] {
      b'"' | b'\'' => {
        let q = tag[i];
        i += 1;
        Some(q)
      }
      _ => None,
    };
    let value_start = i;
    let value_end = match quote {
      Some(q) => match tag[value_start..].iter().position(|&b| b == q) {
        Some(rel) => value_start + rel,
        None => break,
      },
      None => {
        let mut end = value_start;
        while end < tag.len() && !tag[end].is_ascii_whitespace() && tag[end] != b'>' {
          end += 1;
        }
        end
      }
    };
    let raw_value = &tag[value_start..value_end];

    // Advance i past the value (and closing quote when present) so the next iteration starts at
    // the next attribute.
    i = value_end;
    if let Some(q) = quote {
      if i < tag.len() && tag[i] == q {
        i += 1;
      }
    }

    let replacement = if name == b"src" {
      if !src_is_gif(raw_value) {
        continue;
      }
      gif_url_to_png_data_url_cached(raw_value, base, cache)
    } else if name == b"srcset" {
      rewrite_srcset_gif_urls_to_data_urls(raw_value, base, cache)
    } else {
      continue;
    };

    let Some(replacement) = replacement else {
      continue;
    };
    mutated = true;
    out.extend_from_slice(&tag[last_copied..value_start]);
    out.extend_from_slice(&replacement);
    last_copied = value_end;
  }

  if !mutated {
    return tag.to_vec();
  }
  out.extend_from_slice(&tag[last_copied..]);
  out
}

fn gif_url_to_png_data_url_cached(
  raw_value: &[u8],
  base: &Url,
  cache: &mut HashMap<Vec<u8>, Vec<u8>>,
) -> Option<Vec<u8>> {
  if let Some(cached) = cache.get(raw_value) {
    return Some(cached.clone());
  }
  let data_url = gif_src_to_png_data_url(raw_value, base)?;
  cache.insert(raw_value.to_vec(), data_url.clone());
  Some(data_url)
}

fn rewrite_srcset_gif_urls_to_data_urls(
  raw_value: &[u8],
  base: &Url,
  cache: &mut HashMap<Vec<u8>, Vec<u8>>,
) -> Option<Vec<u8>> {
  let srcset = std::str::from_utf8(raw_value).ok()?.trim();
  if srcset.is_empty() {
    return None;
  }

  let mut rewritten_any = false;
  let mut out = String::new();
  let mut first_out = true;
  for candidate in srcset.split(',') {
    let candidate = candidate.trim();
    if candidate.is_empty() {
      continue;
    }
    let mut parts = candidate.split_whitespace();
    let url = parts.next().unwrap_or_default();
    if url.is_empty() {
      continue;
    }
    // Preserve the original descriptor substring without allocating an intermediate Vec/join string.
    // (The srcset grammar treats runs of ASCII whitespace equivalently.)
    let descriptor = candidate.get(url.len()..).unwrap_or("").trim();

    let url_bytes = url.as_bytes();
    let replacement_url = if src_is_gif(url_bytes) {
      // Avoid an extra allocation when converting the generated data URL bytes into a `String`.
      // (`String::from_utf8` reuses the underlying `Vec<u8>` allocation on success.)
      gif_url_to_png_data_url_cached(url_bytes, base, cache)
        .and_then(|data_url| String::from_utf8(data_url).ok())
    } else {
      None
    };

    use std::borrow::Cow;
    let (final_url, did_rewrite): (Cow<'_, str>, bool) = match replacement_url {
      Some(url) => (Cow::Owned(url), true),
      None => (Cow::Borrowed(url), false),
    };
    rewritten_any |= did_rewrite;

    if !first_out {
      out.try_reserve(2).ok()?;
      out.push_str(", ");
    }
    first_out = false;

    let extra = final_url
      .len()
      .checked_add(if descriptor.is_empty() {
        0
      } else {
        descriptor.len().checked_add(1)?
      })?;
    out.try_reserve(extra).ok()?;
    out.push_str(&final_url);
    if !descriptor.is_empty() {
      out.push(' ');
      out.push_str(descriptor);
    }
  }

  if !rewritten_any {
    return None;
  }
  Some(out.into_bytes())
}

fn is_attr_name_start(b: u8) -> bool {
  b.is_ascii_alphabetic() || b == b':' || b == b'_'
}

fn is_attr_name_char(b: u8) -> bool {
  is_attr_name_start(b) || b.is_ascii_digit() || b == b'-' || b == b'.'
}

fn src_is_gif(raw_value: &[u8]) -> bool {
  let end = raw_value
    .iter()
    .position(|&b| b == b'?' || b == b'#')
    .unwrap_or(raw_value.len());
  let slice = &raw_value[..end];
  if slice.len() < 4 {
    return false;
  }
  let suffix = &slice[slice.len() - 4..];
  suffix
    .iter()
    .zip(b".gif")
    .all(|(a, b)| a.to_ascii_lowercase() == *b)
}

fn gif_src_to_png_data_url(raw_value: &[u8], base: &Url) -> Option<Vec<u8>> {
  let raw = std::str::from_utf8(raw_value).ok()?.trim();
  if raw.is_empty() {
    return None;
  }
  if raw.starts_with("data:") {
    return None;
  }

  let end = raw.find(|c| matches!(c, '?' | '#')).unwrap_or(raw.len());
  let raw = raw[..end].trim();
  if raw.is_empty() {
    return None;
  }

  let resolved_url = Url::parse(raw).ok().or_else(|| base.join(raw).ok())?;
  if resolved_url.scheme() != "file" {
    return None;
  }
  let path: PathBuf = resolved_url.to_file_path().ok()?;
  let bytes = std::fs::read(path).ok()?;
  let image = image::load_from_memory_with_format(&bytes, ImageFormat::Gif).ok()?;
  let mut png_buf = Cursor::new(Vec::new());
  image.write_to(&mut png_buf, ImageFormat::Png).ok()?;
  let png = png_buf.into_inner();

  let encoded = base64::engine::general_purpose::STANDARD.encode(png);
  Some(format!("data:image/png;base64,{encoded}").into_bytes())
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
  use base64::Engine as _;
  use tempfile::tempdir;
  use url::Url;

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
      output_str.contains("position: relative !important")
        && output_str.contains("z-index: 0 !important"),
      "patched HTML should ensure body establishes a stacking context (prevents negative z-index content from disappearing)"
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
    let input = b"<!doctype html><html><head></head><body><img decoding=\"async\" src=\"x\"><img decoding='async' src=\"y\"><img decoding=async src=\"z\"></body></html>";
    let output = patch_html_bytes(input, None, true, false, true);
    let output_str = String::from_utf8_lossy(&output);
    assert!(
      output_str.contains("decoding=\"sync\""),
      "patched HTML should force decoding=sync when JS is disabled; got: {output_str}"
    );
    assert!(
      output_str.contains("decoding='sync'"),
      "patched HTML should force decoding=sync for single-quoted attributes when JS is disabled; got: {output_str}"
    );
    assert!(
      output_str.contains("decoding=sync"),
      "patched HTML should force decoding=sync for unquoted attributes when JS is disabled; got: {output_str}"
    );
    assert!(
      !output_str.contains("decoding=\"async\"")
        && !output_str.contains("decoding='async'")
        && !output_str.contains("decoding=async"),
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

  #[test]
  fn patch_html_rewrites_gif_imgs_to_static_png_data_urls_when_js_disabled() {
    let dir = tempdir().expect("tempdir");
    let gif_path = dir.path().join("x.gif");
    // A minimal 1x1 GIF (`GIF89a`), base64-encoded to keep the test self-contained.
    let gif_bytes = base64::engine::general_purpose::STANDARD
      .decode("R0lGODdhAQABAIAAAP///////ywAAAAAAQABAAACAkQBADs=")
      .expect("decode gif base64");
    std::fs::write(&gif_path, gif_bytes).expect("write gif");

    let base_url = Url::from_directory_path(dir.path()).expect("base url");
    let input = b"<!doctype html><html><head></head><body><picture><source srcset=\"x.gif 1x\"><img src=\"x.gif\"></picture></body></html>";
    let output = patch_html_bytes(input, Some(base_url.as_str()), true, false, true);
    let output_str = String::from_utf8_lossy(&output);

    assert!(
      output_str.contains("data:image/png;base64,"),
      "expected GIF src to be rewritten to a PNG data URL; got: {output_str}"
    );
    assert!(
      !output_str.contains("x.gif"),
      "expected original GIF src to be removed; got: {output_str}"
    );
    assert!(
      output_str.contains("1x"),
      "expected srcset descriptor to be preserved; got: {output_str}"
    );

    let prefix = "data:image/png;base64,";
    let start = output_str.find(prefix).expect("expected PNG data URL") + prefix.len();
    let rest = &output_str[start..];
    let end = rest
      .find(|c: char| !matches!(c, 'A'..='Z' | 'a'..='z' | '0'..='9' | '+' | '/' | '='))
      .unwrap_or(rest.len());
    let b64 = &rest[..end];
    let png = base64::engine::general_purpose::STANDARD
      .decode(b64.as_bytes())
      .expect("decode png base64");
    assert!(
      png.starts_with(b"\x89PNG\r\n\x1a\n"),
      "expected rewritten data URL to decode to a PNG; got bytes: {png:?}"
    );
  }

  #[test]
  fn patch_html_rewrites_multiple_gif_srcset_candidates() {
    let dir = tempdir().expect("tempdir");
    let gif_bytes = base64::engine::general_purpose::STANDARD
      .decode("R0lGODdhAQABAIAAAP///////ywAAAAAAQABAAACAkQBADs=")
      .expect("decode gif base64");
    std::fs::write(dir.path().join("a.gif"), &gif_bytes).expect("write a.gif");
    std::fs::write(dir.path().join("b.gif"), &gif_bytes).expect("write b.gif");

    let base_url = Url::from_directory_path(dir.path()).expect("base url");
    let input = b"<!doctype html><html><head></head><body><img srcset=\"a.gif 1x, b.gif 2x\" src=\"a.gif\"></body></html>";
    let output = patch_html_bytes(input, Some(base_url.as_str()), true, false, true);
    let output_str = String::from_utf8_lossy(&output);

    assert!(
      output_str.contains("data:image/png;base64,"),
      "expected GIF srcset URLs to be rewritten to PNG data URLs; got: {output_str}"
    );
    assert!(
      !output_str.contains("a.gif") && !output_str.contains("b.gif"),
      "expected original GIF URLs to be removed; got: {output_str}"
    );
    assert!(
      output_str.contains("2x"),
      "expected srcset descriptors to be preserved; got: {output_str}"
    );
  }

  #[test]
  fn patch_html_unwraps_noscript_when_js_disabled() {
    let input = b"<!doctype html><html><head><noscript><link rel=\"stylesheet\" href=\"a.css\"></noscript></head><body><noscript><div id=\"fallback\">ok</div></noscript></body></html>";
    let output = patch_html_bytes(input, None, true, false, true);
    let output_str = String::from_utf8_lossy(&output);
    assert!(
      !output_str.to_ascii_lowercase().contains("<noscript"),
      "expected <noscript> wrappers to be removed when JS is disabled; got: {output_str}"
    );
    assert!(
      output_str.contains("href=\"a.css\""),
      "expected noscript contents to be preserved; got: {output_str}"
    );
    assert!(
      output_str.contains("id=\"fallback\""),
      "expected body noscript contents to be preserved; got: {output_str}"
    );
  }

  #[test]
  fn patch_html_preserves_noscript_when_js_enabled() {
    let input =
      b"<!doctype html><html><head><noscript><div id=\"x\">ok</div></noscript></head><body></body></html>";
    let output = patch_html_bytes(input, None, false, false, true);
    let output_str = String::from_utf8_lossy(&output);
    assert!(
      output_str.to_ascii_lowercase().contains("<noscript"),
      "expected <noscript> to remain when JS is enabled; got: {output_str}"
    );
  }
}
