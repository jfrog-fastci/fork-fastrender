//! Best-effort HTML subresource discovery for prefetching/crawling tools.
//!
//! This module intentionally uses regex-based extraction instead of a full HTML parser.
//! It is designed for developer tooling (e.g. `prefetch_assets`, `bundle_page --no-render`)
//! where "good enough" coverage is preferable to strict spec parsing.

use crate::css::loader::resolve_href;
use crate::html::image_attrs;
use memchr::memchr;
use std::collections::HashSet;
use std::ops::ControlFlow;

/// URLs discovered from an HTML document that are likely to be fetched during paint.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct HtmlAssetUrls {
  /// Image-like assets (img/src/srcset/source/srcset, video posters, icons/manifests (including
  /// `mask-icon`), and `<link rel=preload as=image>` candidates).
  pub images: Vec<String>,
  /// Embedded documents (iframes, objects, embeds).
  pub documents: Vec<String>,
}

const MAX_SRCSET_CANDIDATES: usize = 16;

// Keep discovery bounded so pathological HTML doesn't explode memory usage.
const MAX_DISCOVERED_IMAGES: usize = 4096;
const MAX_DISCOVERED_DOCUMENTS: usize = 1024;

fn parse_srcset_urls(srcset: &str, max_candidates: usize) -> Vec<String> {
  image_attrs::parse_srcset_with_limit(srcset, max_candidates)
    .into_iter()
    .map(|candidate| candidate.url)
    .collect()
}

const MAX_HTML_SCAN_BYTES: usize = 4 * 1024 * 1024;
const MAX_ATTRIBUTES_PER_TAG: usize = 128;

fn asset_scan_html(html: &str) -> &str {
  if html.len() <= MAX_HTML_SCAN_BYTES {
    return html;
  }
  let mut end = MAX_HTML_SCAN_BYTES.min(html.len());
  while end > 0 && !html.is_char_boundary(end) {
    end -= 1;
  }
  &html[..end]
}

fn for_each_attribute<'a>(
  tag: &'a str,
  mut visit: impl FnMut(&'a str, &'a str) -> ControlFlow<()>,
) {
  let bytes = tag.as_bytes();
  let mut i = 0usize;
  let mut attrs_seen = 0usize;

  // Skip the opening `<` + tag name.
  if bytes.get(i) == Some(&b'<') {
    i += 1;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
      i += 1;
    }
    while i < bytes.len() && bytes[i] != b'>' && !bytes[i].is_ascii_whitespace() {
      i += 1;
    }
  }

  while i < bytes.len() {
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
      i += 1;
    }
    if i >= bytes.len() || bytes[i] == b'>' {
      break;
    }
    // Ignore self-closing markers.
    if bytes[i] == b'/' {
      i += 1;
      continue;
    }

    let name_start = i;
    while i < bytes.len()
      && !bytes[i].is_ascii_whitespace()
      && bytes[i] != b'='
      && bytes[i] != b'>'
    {
      i += 1;
    }
    let name_end = i;
    if name_end == name_start {
      i = i.saturating_add(1);
      continue;
    }
    let name = &tag[name_start..name_end];

    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
      i += 1;
    }

    let mut value = "";
    if i < bytes.len() && bytes[i] == b'=' {
      i += 1;
      while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
      }

      if i + 1 < bytes.len()
        && bytes[i] == b'\\'
        && (bytes[i + 1] == b'"' || bytes[i + 1] == b'\'')
      {
        let quote = bytes[i + 1];
        i += 2;
        let start = i;
        while i < bytes.len() && bytes[i] != quote {
          i += 1;
        }
        value = &tag[start..i];
        if i < bytes.len() {
          i += 1;
        }
      } else if i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\'') {
        let quote = bytes[i];
        i += 1;
        let start = i;
        while i < bytes.len() && bytes[i] != quote {
          i += 1;
        }
        value = &tag[start..i];
        if i < bytes.len() {
          i += 1;
        }
      } else {
        let start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'>' {
          i += 1;
        }
        value = &tag[start..i];
      }
    }

    attrs_seen += 1;
    if let ControlFlow::Break(()) = visit(name, value) {
      break;
    }
    if attrs_seen >= MAX_ATTRIBUTES_PER_TAG {
      break;
    }
  }
}

fn link_rel_is_image_asset(rel_value: &str, as_value: Option<&str>) -> bool {
  let mut has_preload = false;
  for token in rel_value.split_whitespace() {
    if token.eq_ignore_ascii_case("icon")
      || token.eq_ignore_ascii_case("apple-touch-icon")
      || token.eq_ignore_ascii_case("apple-touch-icon-precomposed")
      || token.eq_ignore_ascii_case("manifest")
      || token.eq_ignore_ascii_case("mask-icon")
    {
      return true;
    }
    if token.eq_ignore_ascii_case("preload") {
      has_preload = true;
    }
  }

  if has_preload {
    return as_value
      .map(|value| value.trim().eq_ignore_ascii_case("image"))
      .unwrap_or(false);
  }

  false
}

fn link_rel_is_preload_image(rel_value: &str, as_value: Option<&str>) -> bool {
  rel_value
    .split_ascii_whitespace()
    .any(|token| token.eq_ignore_ascii_case("preload"))
    && as_value
      .map(|value| value.trim().eq_ignore_ascii_case("image"))
      .unwrap_or(false)
}

/// Best-effort extraction of subresource URLs from raw HTML.
///
/// All returned URLs are resolved against `base_url` using [`resolve_href`]. Discovery is
/// deterministic (input-order, first occurrence wins) and bounded (per-category caps and
/// `srcset` candidate limits).
pub fn discover_html_asset_urls(html: &str, base_url: &str) -> HtmlAssetUrls {
  discover_html_asset_urls_with_srcset_limit(html, base_url, MAX_SRCSET_CANDIDATES)
}

/// Best-effort extraction of subresource URLs from raw HTML.
///
/// All returned URLs are resolved against `base_url` using [`resolve_href`]. Discovery is
/// deterministic (input-order, first occurrence wins) and bounded (per-category caps and
/// `srcset` candidate limits).
pub fn discover_html_asset_urls_with_srcset_limit(
  html: &str,
  base_url: &str,
  max_srcset_candidates: usize,
) -> HtmlAssetUrls {
  let html = asset_scan_html(html);
  let bytes = html.as_bytes();
  let max_srcset_candidates = max_srcset_candidates.min(MAX_SRCSET_CANDIDATES);

  let mut out = HtmlAssetUrls::default();
  let mut seen_images: HashSet<String> = HashSet::new();
  let mut seen_documents: HashSet<String> = HashSet::new();

  let mut push_image = |out: &mut HtmlAssetUrls, seen_images: &mut HashSet<String>, raw: &str| {
    if out.images.len() >= MAX_DISCOVERED_IMAGES {
      return;
    }
    if let Some(resolved) = resolve_href(base_url, raw) {
      if seen_images.insert(resolved.clone()) {
        out.images.push(resolved);
      }
    }
  };
  let mut push_document =
    |out: &mut HtmlAssetUrls, seen_documents: &mut HashSet<String>, raw: &str| {
    if out.documents.len() >= MAX_DISCOVERED_DOCUMENTS {
      return;
    }
    if let Some(resolved) = resolve_href(base_url, raw) {
      if seen_documents.insert(resolved.clone()) {
        out.documents.push(resolved);
      }
    }
  };

  let mut template_depth: usize = 0;
  let mut i: usize = 0;

  while let Some(rel) = memchr(b'<', &bytes[i..]) {
    if out.images.len() >= MAX_DISCOVERED_IMAGES && out.documents.len() >= MAX_DISCOVERED_DOCUMENTS {
      break;
    }

    let tag_start = i + rel;

    if bytes
      .get(tag_start..tag_start + 4)
      .is_some_and(|head| head == b"<!--")
    {
      let end = super::find_bytes(bytes, tag_start + 4, b"-->")
        .map(|pos| pos + 3)
        .unwrap_or(bytes.len());
      i = end;
      continue;
    }

    if bytes
      .get(tag_start..tag_start + 9)
      .is_some_and(|head| head.eq_ignore_ascii_case(b"<![cdata["))
    {
      let end = super::find_bytes(bytes, tag_start + 9, b"]]>")
        .map(|pos| pos + 3)
        .unwrap_or(bytes.len());
      i = end;
      continue;
    }

    if bytes
      .get(tag_start + 1)
      .is_some_and(|b| *b == b'!' || *b == b'?')
    {
      let Some(end) = super::find_tag_end(bytes, tag_start) else {
        break;
      };
      i = end;
      continue;
    }

    let Some(tag_end) = super::find_tag_end(bytes, tag_start) else {
      break;
    };

    let Some((is_end, name_start, name_end)) = super::parse_tag_name_range(bytes, tag_start, tag_end)
    else {
      i = tag_start + 1;
      continue;
    };
    let name = &bytes[name_start..name_end];

    let raw_text_tag: Option<&'static [u8]> = if !is_end && name.eq_ignore_ascii_case(b"script") {
      Some(b"script")
    } else if !is_end && name.eq_ignore_ascii_case(b"style") {
      Some(b"style")
    } else if !is_end && name.eq_ignore_ascii_case(b"textarea") {
      Some(b"textarea")
    } else if !is_end && name.eq_ignore_ascii_case(b"title") {
      Some(b"title")
    } else if !is_end && name.eq_ignore_ascii_case(b"xmp") {
      Some(b"xmp")
    } else {
      None
    };

    if !is_end && name.eq_ignore_ascii_case(b"plaintext") {
      break;
    }

    if name.eq_ignore_ascii_case(b"template") {
      if is_end {
        if template_depth > 0 {
          template_depth -= 1;
        }
      } else {
        template_depth += 1;
      }
    }

    if template_depth == 0 && !is_end {
      let tag = &html[tag_start..tag_end];
      if name.eq_ignore_ascii_case(b"img") {
        let mut src: Option<&str> = None;
        let mut srcset: Option<&str> = None;
        for_each_attribute(tag, |attr, value| {
          if attr.eq_ignore_ascii_case("src") {
            src = Some(value);
          } else if attr.eq_ignore_ascii_case("srcset") {
            srcset = Some(value);
          }
          ControlFlow::Continue(())
        });

        if let Some(raw) = src {
          push_image(&mut out, &mut seen_images, raw);
        }
        if let Some(raw_srcset) = srcset {
          for candidate in parse_srcset_urls(raw_srcset, max_srcset_candidates) {
            push_image(&mut out, &mut seen_images, &candidate);
          }
        }
      } else if name.eq_ignore_ascii_case(b"source") {
        let mut srcset: Option<&str> = None;
        for_each_attribute(tag, |attr, value| {
          if attr.eq_ignore_ascii_case("srcset") {
            srcset = Some(value);
            return ControlFlow::Break(());
          }
          ControlFlow::Continue(())
        });
        if let Some(raw_srcset) = srcset {
          for candidate in parse_srcset_urls(raw_srcset, max_srcset_candidates) {
            push_image(&mut out, &mut seen_images, &candidate);
          }
        }
      } else if name.eq_ignore_ascii_case(b"video") {
        let mut poster: Option<&str> = None;
        for_each_attribute(tag, |attr, value| {
          if attr.eq_ignore_ascii_case("poster") {
            poster = Some(value);
            return ControlFlow::Break(());
          }
          ControlFlow::Continue(())
        });
        if let Some(raw) = poster {
          push_image(&mut out, &mut seen_images, raw);
        }
      } else if name.eq_ignore_ascii_case(b"iframe") {
        let mut src: Option<&str> = None;
        for_each_attribute(tag, |attr, value| {
          if attr.eq_ignore_ascii_case("src") {
            src = Some(value);
            return ControlFlow::Break(());
          }
          ControlFlow::Continue(())
        });
        if let Some(raw) = src {
          push_document(&mut out, &mut seen_documents, raw);
        }
      } else if name.eq_ignore_ascii_case(b"object") {
        let mut data: Option<&str> = None;
        for_each_attribute(tag, |attr, value| {
          if attr.eq_ignore_ascii_case("data") {
            data = Some(value);
            return ControlFlow::Break(());
          }
          ControlFlow::Continue(())
        });
        if let Some(raw) = data {
          push_document(&mut out, &mut seen_documents, raw);
        }
      } else if name.eq_ignore_ascii_case(b"embed") {
        let mut src: Option<&str> = None;
        for_each_attribute(tag, |attr, value| {
          if attr.eq_ignore_ascii_case("src") {
            src = Some(value);
            return ControlFlow::Break(());
          }
          ControlFlow::Continue(())
        });
        if let Some(raw) = src {
          push_document(&mut out, &mut seen_documents, raw);
        }
      } else if name.eq_ignore_ascii_case(b"link") {
        let mut rel: Option<&str> = None;
        let mut href: Option<&str> = None;
        let mut as_value: Option<&str> = None;
        let mut imagesrcset: Option<&str> = None;
        for_each_attribute(tag, |attr, value| {
          if attr.eq_ignore_ascii_case("rel") {
            rel = Some(value);
          } else if attr.eq_ignore_ascii_case("href") {
            href = Some(value);
          } else if attr.eq_ignore_ascii_case("as") {
            as_value = Some(value);
          } else if attr.eq_ignore_ascii_case("imagesrcset") {
            imagesrcset = Some(value);
          }
          ControlFlow::Continue(())
        });

        let rel = rel.unwrap_or("");
        if !rel.is_empty() && link_rel_is_image_asset(rel, as_value) {
          if let Some(href) = href {
            if !href.is_empty() {
              push_image(&mut out, &mut seen_images, href);
            }
          }
          if link_rel_is_preload_image(rel, as_value) {
            if let Some(imagesrcset) = imagesrcset {
              if !imagesrcset.is_empty() {
                for candidate in parse_srcset_urls(imagesrcset, MAX_SRCSET_CANDIDATES) {
                  push_image(&mut out, &mut seen_images, &candidate);
                }
              }
            }
          }
        }
      }
    }

    if let Some(tag) = raw_text_tag {
      i = super::find_raw_text_element_end(bytes, tag_end, tag);
      continue;
    }

    i = tag_end;
  }

  out
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn discovers_img_sources_and_srcset_candidates() {
    let html = r#"
      <img src="img.png">
      <img srcset="a1.png 1x, a2.png 2x">
      <picture><source srcset="s1.png 1x, s2.png 2x"></picture>
    "#;
    let out = discover_html_asset_urls(html, "https://example.com/dir/page.html");
    assert_eq!(
      out.images,
      vec![
        "https://example.com/dir/img.png",
        "https://example.com/dir/a1.png",
        "https://example.com/dir/a2.png",
        "https://example.com/dir/s1.png",
        "https://example.com/dir/s2.png",
      ]
      .into_iter()
      .map(str::to_string)
      .collect::<Vec<_>>()
    );
  }

  #[test]
  fn caps_srcset_candidates() {
    let srcset = (0..20)
      // Density descriptors must be positive; use 1x.. for deterministic ordering.
      .map(|i| format!("img{i}.png {}x", i + 1))
      .collect::<Vec<_>>()
      .join(", ");
    let html = format!(r#"<img srcset="{srcset}">"#);
    let out = discover_html_asset_urls(&html, "https://example.com/");
    assert_eq!(out.images.len(), MAX_SRCSET_CANDIDATES);
    assert_eq!(out.images[0], "https://example.com/img0.png");
    assert_eq!(
      out.images[MAX_SRCSET_CANDIDATES - 1],
      "https://example.com/img15.png"
    );
  }

  #[test]
  fn honors_custom_srcset_limit() {
    let html = r#"<img srcset="img0.png 1x, img1.png 2x">"#;
    let out = discover_html_asset_urls_with_srcset_limit(html, "https://example.com/", 1);
    assert_eq!(out.images, vec!["https://example.com/img0.png".to_string()]);
  }

  #[test]
  fn discovers_srcset_candidates_with_commas_inside_urls() {
    let html = r#"<img srcset="https://img.example/master/w_2560,c_limit/foo.jpg 2560w, https://img.example/master/w_1280,c_limit/foo.jpg 1280w">"#;
    let out = discover_html_asset_urls(html, "https://example.com/");
    assert_eq!(
      out.images,
      vec![
        "https://img.example/master/w_2560,c_limit/foo.jpg",
        "https://img.example/master/w_1280,c_limit/foo.jpg",
      ]
      .into_iter()
      .map(str::to_string)
      .collect::<Vec<_>>()
    );
  }

  #[test]
  fn discovers_iframe_object_embed_documents() {
    let html = r#"
      <iframe src="frame.html"></iframe>
      <object data="/obj.html"></object>
      <embed src='embed.html'>
    "#;
    let out = discover_html_asset_urls(html, "https://example.com/base/");
    assert_eq!(
      out.documents,
      vec![
        "https://example.com/base/frame.html",
        "https://example.com/obj.html",
        "https://example.com/base/embed.html",
      ]
      .into_iter()
      .map(str::to_string)
      .collect::<Vec<_>>()
    );
  }

  #[test]
  fn discovers_video_posters() {
    let html = r#"<video poster="/poster.jpg"></video>"#;
    let out = discover_html_asset_urls(html, "https://example.com/page.html");
    assert_eq!(
      out.images,
      vec!["https://example.com/poster.jpg".to_string()]
    );
  }

  #[test]
  fn discovers_icons_and_manifest_link_rel() {
    let html = r#"
      <link rel="stylesheet" href="style.css">
      <link rel="icon" href="favicon.ico">
      <link rel="apple-touch-icon" href="/touch.png">
      <link href="favicon2.ico" rel="shortcut icon">
      <link href="manifest.json" rel="manifest">
      <link rel="mask-icon" href="/mask.svg">
      <link rel="preload" as="image" href="preload.png">
      <link rel="preload" as="script" href="preload.js">
    "#;
    let out = discover_html_asset_urls(html, "https://example.com/base/page.html");
    assert_eq!(
      out.images,
      vec![
        "https://example.com/base/favicon.ico",
        "https://example.com/touch.png",
        "https://example.com/base/favicon2.ico",
        "https://example.com/base/manifest.json",
        "https://example.com/mask.svg",
        "https://example.com/base/preload.png",
      ]
      .into_iter()
      .map(str::to_string)
      .collect::<Vec<_>>()
    );
  }

  #[test]
  fn discovers_preload_imagesrcset_candidates() {
    let html =
      r#"<link rel="preload" as="image" href="fallback.png" imagesrcset="a1.png 1x, a2.png 2x">"#;
    let out = discover_html_asset_urls(html, "https://example.com/base/page.html");
    assert_eq!(
      out.images,
      vec![
        "https://example.com/base/fallback.png",
        "https://example.com/base/a1.png",
        "https://example.com/base/a2.png",
      ]
      .into_iter()
      .map(str::to_string)
      .collect::<Vec<_>>()
    );
  }

  #[test]
  fn discovers_preload_imagesrcset_without_href() {
    let html = r#"<link rel="preload" as="image" imagesrcset="a1.png 1x, a2.png 2x">"#;
    let out = discover_html_asset_urls(html, "https://example.com/base/page.html");
    assert_eq!(
      out.images,
      vec![
        "https://example.com/base/a1.png",
        "https://example.com/base/a2.png",
      ]
      .into_iter()
      .map(str::to_string)
      .collect::<Vec<_>>()
    );
  }

  #[test]
  fn ignores_assets_inside_template() {
    let html = r#"
      <img src="live.png">
      <template>
        <img src="inert.png">
        <iframe src="inert.html"></iframe>
      </template>
      <iframe src="live.html"></iframe>
    "#;
    let out = discover_html_asset_urls(html, "https://example.com/base/");
    assert_eq!(
      out.images,
      vec!["https://example.com/base/live.png".to_string()]
    );
    assert_eq!(
      out.documents,
      vec!["https://example.com/base/live.html".to_string()]
    );
  }

  #[test]
  fn ignores_assets_inside_rawtext_elements() {
    let html = r#"
      <script>var s = '<img src="bad.png"><iframe src="bad.html"></iframe>';</script>
      <style>/* <img src="also-bad.png"> */</style>
      <img src="good.png">
      <iframe src="good.html"></iframe>
    "#;
    let out = discover_html_asset_urls(html, "https://example.com/base/");
    assert_eq!(
      out.images,
      vec!["https://example.com/base/good.png".to_string()]
    );
    assert_eq!(
      out.documents,
      vec!["https://example.com/base/good.html".to_string()]
    );
  }

  #[test]
  fn does_not_match_data_attributes() {
    let html = r#"
      <img data-src="lazy.png">
      <iframe data-src="frame.html"></iframe>
      <link data-href="bad.ico" rel="icon" href="good.ico">
    "#;
    let out = discover_html_asset_urls(html, "https://example.com/base/");
    assert_eq!(
      out.images,
      vec!["https://example.com/base/good.ico".to_string()]
    );
    assert!(
      out.documents.is_empty(),
      "data-src iframe should not be discovered"
    );
  }
}
