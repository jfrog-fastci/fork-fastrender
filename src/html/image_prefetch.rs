//! Best-effort HTML image URL discovery for cache warming.
//!
//! This module is used by developer tooling (e.g. `prefetch_assets`) to find
//! image URLs in HTML without executing layout. Selection is aligned with the
//! renderer's responsive image algorithm (`srcset`/`sizes`/`picture`) so the
//! warmed cache matches what paint would request.

use crate::css::loader::resolve_href_with_base;
use crate::css::parser::tokenize_rel_list;
use crate::dom::{
  img_src_is_placeholder, DomNode, COMPAT_IMG_SRCSET_DATA_ATTR_CANDIDATES,
  COMPAT_IMG_SRC_DATA_ATTR_CANDIDATES, COMPAT_SIZES_DATA_ATTR_CANDIDATES,
  COMPAT_SOURCE_SRCSET_DATA_ATTR_CANDIDATES,
};
use crate::html::image_attrs::{parse_sizes, parse_srcset};
use crate::html::images::{
  image_sources_with_fallback, is_supported_image_mime, select_image_source, ImageSelectionContext,
};
use crate::resource::is_data_url;
use crate::style::media::MediaContext;
use crate::style::media::MediaQuery;
use crate::tree::box_tree::{CrossOriginAttribute, PictureSource, SizesList, SrcsetDescriptor};
use std::collections::HashSet;

/// Hard limits for image prefetch discovery.
#[derive(Debug, Clone, Copy)]
pub struct ImagePrefetchLimits {
  /// Maximum number of image-like elements to consider per document.
  pub max_image_elements: usize,
  /// Maximum number of URLs to emit per image element (primary + fallbacks).
  pub max_urls_per_element: usize,
}

impl Default for ImagePrefetchLimits {
  fn default() -> Self {
    Self {
      max_image_elements: 150,
      max_urls_per_element: 2,
    }
  }
}

/// Result of HTML image prefetch discovery.
#[derive(Debug, Clone)]
pub struct ImagePrefetchDiscovery {
  /// Number of image elements walked (bounded by `max_image_elements`).
  pub image_elements: usize,
  /// Discovered URLs, in deterministic priority order.
  pub urls: Vec<String>,
  /// True when discovery stopped early due to `max_image_elements`.
  pub limited: bool,
}

/// A discovered image request, paired with the element's `crossorigin` state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImagePrefetchRequest {
  pub url: String,
  pub crossorigin: CrossOriginAttribute,
}

/// Result of HTML image prefetch discovery including `crossorigin` metadata.
#[derive(Debug, Clone)]
pub struct ImagePrefetchRequestDiscovery {
  /// Number of image elements walked (bounded by `max_image_elements`).
  pub image_elements: usize,
  /// Discovered requests, in deterministic priority order.
  pub requests: Vec<ImagePrefetchRequest>,
  /// True when discovery stopped early due to `max_image_elements`.
  pub limited: bool,
}

const WIDTH_DESCRIPTOR_SECONDARY_SLOT_SCALE: f32 = 0.75;

const IMG_SRC_DATA_ATTR_FALLBACKS: [&str; 5] = [
  "data-src",
  "data-lazy-src",
  "data-original",
  "data-original-src",
  "data-gl-src",
];

const IMG_SRCSET_DATA_ATTR_FALLBACKS: [&str; 3] =
  ["data-srcset", "data-lazy-srcset", "data-gl-srcset"];

fn trim_ascii_whitespace(value: &str) -> &str {
  // HTML defines "ASCII whitespace" as: U+0009 TAB, U+000A LF, U+000C FF, U+000D CR, U+0020 SPACE.
  value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
}

fn get_non_empty_attr<'a>(node: &'a DomNode, name: &str) -> Option<&'a str> {
  node
    .get_attribute_ref(name)
    .filter(|value| !trim_ascii_whitespace(value).is_empty())
}

fn get_first_non_empty_attr<'a>(node: &'a DomNode, names: &[&str]) -> Option<&'a str> {
  names.iter().find_map(|name| get_non_empty_attr(node, name))
}

fn get_img_src_attr<'a>(node: &'a DomNode) -> Option<&'a str> {
  if let Some(src) = get_non_empty_attr(node, "src") {
    if !img_src_is_placeholder(src) {
      return Some(src);
    }
  }

  get_first_non_empty_attr(node, &IMG_SRC_DATA_ATTR_FALLBACKS)
    .or_else(|| get_first_non_empty_attr(node, &COMPAT_IMG_SRC_DATA_ATTR_CANDIDATES))
}

fn get_img_srcset_attr<'a>(node: &'a DomNode) -> Option<&'a str> {
  get_non_empty_attr(node, "srcset")
    .or_else(|| get_first_non_empty_attr(node, &IMG_SRCSET_DATA_ATTR_FALLBACKS))
    .or_else(|| get_first_non_empty_attr(node, &COMPAT_IMG_SRCSET_DATA_ATTR_CANDIDATES))
}

fn get_source_srcset_attr<'a>(node: &'a DomNode) -> Option<&'a str> {
  get_non_empty_attr(node, "srcset")
    .or_else(|| get_first_non_empty_attr(node, &COMPAT_SOURCE_SRCSET_DATA_ATTR_CANDIDATES))
}

fn get_sizes_attr<'a>(node: &'a DomNode) -> Option<&'a str> {
  get_non_empty_attr(node, "sizes")
    .or_else(|| get_first_non_empty_attr(node, &COMPAT_SIZES_DATA_ATTR_CANDIDATES))
}

fn normalize_mime_type(value: &str) -> Option<String> {
  let base = trim_ascii_whitespace(value.split(';').next().unwrap_or(""));
  if base.is_empty() {
    None
  } else {
    Some(base.to_ascii_lowercase())
  }
}

fn resolve_prefetch_url(ctx: ImageSelectionContext<'_>, raw: &str) -> Option<String> {
  let resolved = resolve_href_with_base(ctx.base_url, raw)?;
  if is_data_url(&resolved) {
    return None;
  }
  Some(resolved)
}

fn parse_crossorigin_attr(node: &DomNode) -> CrossOriginAttribute {
  match node.get_attribute_ref("crossorigin") {
    None => CrossOriginAttribute::None,
    Some(value) => {
      let value = trim_ascii_whitespace(value);
      if value.eq_ignore_ascii_case("use-credentials") {
        CrossOriginAttribute::UseCredentials
      } else {
        // Empty, `anonymous`, and unknown tokens are treated as `anonymous`.
        CrossOriginAttribute::Anonymous
      }
    }
  }
}

fn push_unique_url(
  ctx: ImageSelectionContext<'_>,
  seen: &mut HashSet<String>,
  out: &mut Vec<String>,
  raw: &str,
) {
  let Some(resolved) = resolve_prefetch_url(ctx, raw) else {
    return;
  };
  if seen.insert(resolved.clone()) {
    out.push(resolved);
  }
}

fn push_unique_request(
  ctx: ImageSelectionContext<'_>,
  crossorigin: CrossOriginAttribute,
  seen: &mut HashSet<(String, CrossOriginAttribute)>,
  out: &mut Vec<ImagePrefetchRequest>,
  raw: &str,
) {
  let Some(resolved) = resolve_prefetch_url(ctx, raw) else {
    return;
  };
  if seen.insert((resolved.clone(), crossorigin)) {
    out.push(ImagePrefetchRequest {
      url: resolved,
      crossorigin,
    });
  }
}

fn picture_source_matches(source: &PictureSource, ctx: ImageSelectionContext<'_>) -> bool {
  if source.srcset.is_empty() {
    return false;
  }

  if let Some(mime) = &source.mime_type {
    if !is_supported_image_mime(mime) {
      return false;
    }
  }

  match &source.media {
    Some(queries) => ctx
      .media_context
      .map(|m| m.evaluate_list(queries))
      .unwrap_or(true),
    None => true,
  }
}

fn select_picture_source<'a>(
  sources: &'a [PictureSource],
  ctx: ImageSelectionContext<'_>,
) -> Option<&'a PictureSource> {
  sources
    .iter()
    .find(|source| picture_source_matches(source, ctx))
}

fn estimate_source_size(sizes: Option<&SizesList>, ctx: ImageSelectionContext<'_>) -> Option<f32> {
  let viewport = ctx.viewport?;
  let font_size = ctx.font_size.unwrap_or(16.0);
  let root_font_size = ctx.root_font_size.unwrap_or(font_size);
  let media_ctx = ctx.media_context.cloned().unwrap_or_else(|| {
    MediaContext::screen(viewport.width, viewport.height)
      .with_device_pixel_ratio(ctx.device_pixel_ratio)
      .with_env_overrides()
  });

  let resolved = if let Some(list) = sizes {
    list.evaluate(&media_ctx, viewport, font_size, root_font_size)
  } else {
    viewport.width
  };

  resolved
    .is_finite()
    .then_some(resolved)
    .filter(|v| *v > 0.0)
}

fn srcset_has_width_descriptors(srcset: &[crate::tree::box_tree::SrcsetCandidate]) -> bool {
  srcset
    .iter()
    .any(|c| matches!(c.descriptor, SrcsetDescriptor::Width(_) | SrcsetDescriptor::WidthHeight { .. }))
}

fn link_rel_is_preload_image(rel_tokens: &[String], as_attr: Option<&str>) -> bool {
  rel_tokens.iter().any(|t| t.eq_ignore_ascii_case("preload"))
    && as_attr
      .map(|v| trim_ascii_whitespace(v).eq_ignore_ascii_case("image"))
      .unwrap_or(false)
}

fn picture_sources_and_fallback_img<'a>(
  picture: &'a DomNode,
) -> Option<(Vec<PictureSource>, &'a DomNode)> {
  let mut sources: Vec<PictureSource> = Vec::new();

  for child in &picture.children {
    let Some(tag) = child.tag_name() else {
      continue;
    };

    if tag.eq_ignore_ascii_case("source") {
      let Some(srcset_attr) = get_source_srcset_attr(child) else {
        continue;
      };
      let parsed_srcset = parse_srcset(srcset_attr);
      if parsed_srcset.is_empty() {
        continue;
      }

      let sizes = get_sizes_attr(child).and_then(parse_sizes);
      let media = child
        .get_attribute_ref("media")
        .and_then(|m| MediaQuery::parse_list(m).ok());
      let mime_type = child
        .get_attribute_ref("type")
        .and_then(normalize_mime_type);

      sources.push(PictureSource {
        srcset: parsed_srcset,
        sizes,
        media,
        mime_type,
      });
      continue;
    }

    if tag.eq_ignore_ascii_case("img") {
      return Some((sources, child));
    }
  }

  None
}

fn push_prefetch_selection(
  ctx: ImageSelectionContext<'_>,
  picture_sources: &[PictureSource],
  img_src: &str,
  img_srcset: &[crate::tree::box_tree::SrcsetCandidate],
  img_sizes: Option<&SizesList>,
  limits: ImagePrefetchLimits,
  seen_urls: &mut HashSet<String>,
  urls: &mut Vec<String>,
) {
  if limits.max_urls_per_element == 0 {
    return;
  }

  let srcset_to_consider = select_picture_source(picture_sources, ctx)
    .map(|source| source.srcset.as_slice())
    .unwrap_or(img_srcset);
  let uses_width_descriptors = srcset_has_width_descriptors(srcset_to_consider);

  if uses_width_descriptors {
    let sizes_for_estimate = select_picture_source(picture_sources, ctx)
      .and_then(|source| source.sizes.as_ref())
      .or(img_sizes);

    let Some(source_size) = estimate_source_size(sizes_for_estimate, ctx) else {
      // Fallback to the renderer-aligned selection which will pick a candidate based on `sizes`
      // evaluation when slot widths are unknown.
      for selected in
        image_sources_with_fallback(img_src, img_srcset, img_sizes, picture_sources, ctx)
          .into_iter()
          .take(limits.max_urls_per_element)
      {
        push_unique_url(ctx, seen_urls, urls, selected.url);
      }
      return;
    };

    let mut emitted = 0usize;
    for slot_width in [
      source_size,
      source_size * WIDTH_DESCRIPTOR_SECONDARY_SLOT_SCALE,
    ] {
      if emitted >= limits.max_urls_per_element {
        break;
      }
      if !slot_width.is_finite() || slot_width <= 0.0 {
        continue;
      }

      let selection_ctx = ImageSelectionContext {
        slot_width: Some(slot_width),
        ..ctx
      };
      let selected = select_image_source(
        img_src,
        img_srcset,
        img_sizes,
        picture_sources,
        selection_ctx,
      );
      if trim_ascii_whitespace(selected.url).is_empty() {
        continue;
      }
      let before_len = urls.len();
      push_unique_url(ctx, seen_urls, urls, selected.url);
      if urls.len() != before_len {
        emitted += 1;
      }
    }

    // Ensure a plain `src` is still captured when we didn't fill the cap (e.g. malformed srcset).
    if emitted < limits.max_urls_per_element && !trim_ascii_whitespace(img_src).is_empty() {
      push_unique_url(ctx, seen_urls, urls, img_src);
    }
    return;
  }

  for selected in image_sources_with_fallback(img_src, img_srcset, img_sizes, picture_sources, ctx)
    .into_iter()
    .take(limits.max_urls_per_element)
  {
    push_unique_url(ctx, seen_urls, urls, selected.url);
  }
}

fn push_prefetch_selection_with_crossorigin(
  ctx: ImageSelectionContext<'_>,
  crossorigin: CrossOriginAttribute,
  picture_sources: &[PictureSource],
  img_src: &str,
  img_srcset: &[crate::tree::box_tree::SrcsetCandidate],
  img_sizes: Option<&SizesList>,
  limits: ImagePrefetchLimits,
  seen_urls: &mut HashSet<(String, CrossOriginAttribute)>,
  urls: &mut Vec<ImagePrefetchRequest>,
) {
  if limits.max_urls_per_element == 0 {
    return;
  }

  let srcset_to_consider = select_picture_source(picture_sources, ctx)
    .map(|source| source.srcset.as_slice())
    .unwrap_or(img_srcset);
  let uses_width_descriptors = srcset_has_width_descriptors(srcset_to_consider);

  if uses_width_descriptors {
    let sizes_for_estimate = select_picture_source(picture_sources, ctx)
      .and_then(|source| source.sizes.as_ref())
      .or(img_sizes);

    let Some(source_size) = estimate_source_size(sizes_for_estimate, ctx) else {
      // Fallback to the renderer-aligned selection which will pick a candidate based on `sizes`
      // evaluation when slot widths are unknown.
      for selected in
        image_sources_with_fallback(img_src, img_srcset, img_sizes, picture_sources, ctx)
          .into_iter()
          .take(limits.max_urls_per_element)
      {
        push_unique_request(ctx, crossorigin, seen_urls, urls, selected.url);
      }
      return;
    };

    let mut emitted = 0usize;
    for slot_width in [
      source_size,
      source_size * WIDTH_DESCRIPTOR_SECONDARY_SLOT_SCALE,
    ] {
      if emitted >= limits.max_urls_per_element {
        break;
      }
      if !slot_width.is_finite() || slot_width <= 0.0 {
        continue;
      }

      let selection_ctx = ImageSelectionContext {
        slot_width: Some(slot_width),
        ..ctx
      };
      let selected = select_image_source(
        img_src,
        img_srcset,
        img_sizes,
        picture_sources,
        selection_ctx,
      );
      if trim_ascii_whitespace(selected.url).is_empty() {
        continue;
      }
      let before_len = urls.len();
      push_unique_request(ctx, crossorigin, seen_urls, urls, selected.url);
      if urls.len() != before_len {
        emitted += 1;
      }
    }

    // Ensure a plain `src` is still captured when we didn't fill the cap (e.g. malformed srcset).
    if emitted < limits.max_urls_per_element && !trim_ascii_whitespace(img_src).is_empty() {
      push_unique_request(ctx, crossorigin, seen_urls, urls, img_src);
    }
    return;
  }

  for selected in image_sources_with_fallback(img_src, img_srcset, img_sizes, picture_sources, ctx)
    .into_iter()
    .take(limits.max_urls_per_element)
  {
    push_unique_request(ctx, crossorigin, seen_urls, urls, selected.url);
  }
}

/// Discover image URLs in a DOM tree using the renderer's responsive image selection.
///
/// This intentionally does **not** execute layout, so slot widths are treated as
/// unknown and `sizes` evaluation uses the viewport fallback logic.
pub fn discover_image_prefetch_urls(
  dom: &DomNode,
  ctx: ImageSelectionContext<'_>,
  limits: ImagePrefetchLimits,
) -> ImagePrefetchDiscovery {
  let mut urls: Vec<String> = Vec::new();
  let mut seen_urls: HashSet<String> = HashSet::new();
  let mut image_elements = 0usize;
  let mut limited = false;

  if limits.max_image_elements == 0 || limits.max_urls_per_element == 0 {
    return ImagePrefetchDiscovery {
      image_elements,
      urls,
      limited,
    };
  }

  let mut stack: Vec<&DomNode> = vec![dom];
  while let Some(node) = stack.pop() {
    if image_elements >= limits.max_image_elements {
      limited = true;
      break;
    }

    if node.template_contents_are_inert() {
      continue;
    }

    let mut descend = true;
    if let Some(tag) = node.tag_name() {
      if tag.eq_ignore_ascii_case("picture") {
        if let Some((picture_sources, img)) = picture_sources_and_fallback_img(node) {
          if image_elements >= limits.max_image_elements {
            limited = true;
            break;
          }
          image_elements += 1;

          let img_src = get_img_src_attr(img).unwrap_or("");
          let img_srcset = get_img_srcset_attr(img)
            .map(parse_srcset)
            .unwrap_or_default();
          let img_sizes = get_sizes_attr(img).and_then(parse_sizes);

          push_prefetch_selection(
            ctx,
            &picture_sources,
            img_src,
            &img_srcset,
            img_sizes.as_ref(),
            limits,
            &mut seen_urls,
            &mut urls,
          );
          if image_elements >= limits.max_image_elements {
            limited = true;
            break;
          }

          // `<picture>` sources/img children are consumed by `picture_sources_and_fallback_img`.
          descend = false;
        }
      } else if tag.eq_ignore_ascii_case("img") {
        if image_elements >= limits.max_image_elements {
          limited = true;
          break;
        }
        let img_src = get_img_src_attr(node).unwrap_or("");
        let has_src = !trim_ascii_whitespace(img_src).is_empty();
        let img_srcset_attr = get_img_srcset_attr(node);
        let has_srcset = img_srcset_attr.is_some();
        if has_src || has_srcset {
          image_elements += 1;

          let img_srcset = img_srcset_attr.map(parse_srcset).unwrap_or_default();
          let img_sizes = get_sizes_attr(node).and_then(parse_sizes);

          push_prefetch_selection(
            ctx,
            &[],
            img_src,
            &img_srcset,
            img_sizes.as_ref(),
            limits,
            &mut seen_urls,
            &mut urls,
          );
          if image_elements >= limits.max_image_elements {
            limited = true;
            break;
          }
        }
      } else if tag.eq_ignore_ascii_case("video") {
        let poster = node
          .get_attribute_ref("poster")
          .filter(|value| !trim_ascii_whitespace(value).is_empty())
          .or_else(|| {
            node
              .get_attribute_ref("gnt-gl-ps")
              .filter(|value| !trim_ascii_whitespace(value).is_empty())
          });
        if let Some(poster) = poster {
          if image_elements >= limits.max_image_elements {
            limited = true;
            break;
          }
          image_elements += 1;
          push_unique_url(ctx, &mut seen_urls, &mut urls, poster);
          if image_elements >= limits.max_image_elements {
            limited = true;
            break;
          }
        }
      } else if tag.eq_ignore_ascii_case("link") {
        if let Some(rel_attr) = node.get_attribute_ref("rel") {
          let rel_tokens = tokenize_rel_list(rel_attr);
          if !rel_tokens.is_empty() {
            let href = trim_ascii_whitespace(node.get_attribute_ref("href").unwrap_or(""));
            let as_attr = node.get_attribute_ref("as");

            let media_matches = match node.get_attribute_ref("media") {
              Some(media) => MediaQuery::parse_list(media)
                .ok()
                .map(|list| {
                  ctx
                    .media_context
                    .map(|m| m.evaluate_list(&list))
                    .unwrap_or(true)
                })
                .unwrap_or(true),
              None => true,
            };
            if !media_matches {
              descend = false;
            } else if link_rel_is_preload_image(&rel_tokens, as_attr) {
              let parsed_srcset = node
                .get_attribute_ref("imagesrcset")
                .map(parse_srcset)
                .unwrap_or_default();
              let parsed_sizes = node.get_attribute_ref("imagesizes").and_then(parse_sizes);
              if href.is_empty() && parsed_srcset.is_empty() {
                descend = false;
              } else {
                if image_elements >= limits.max_image_elements {
                  limited = true;
                  break;
                }
                image_elements += 1;
                push_prefetch_selection(
                  ctx,
                  &[],
                  href,
                  &parsed_srcset,
                  parsed_sizes.as_ref(),
                  limits,
                  &mut seen_urls,
                  &mut urls,
                );
                if image_elements >= limits.max_image_elements {
                  limited = true;
                  break;
                }
              }
            }
          }
        }
      }
    }

    if !descend {
      continue;
    }

    for child in node.traversal_children().iter().rev() {
      stack.push(child);
    }
  }

  ImagePrefetchDiscovery {
    image_elements,
    urls,
    limited,
  }
}

/// Discover image URLs in a DOM tree using the renderer's responsive image selection, emitting
/// the element's `crossorigin` state for each discovered request.
///
/// This intentionally does **not** execute layout, so slot widths are treated as unknown and
/// `sizes` evaluation uses the viewport fallback logic.
pub fn discover_image_prefetch_requests(
  dom: &DomNode,
  ctx: ImageSelectionContext<'_>,
  limits: ImagePrefetchLimits,
) -> ImagePrefetchRequestDiscovery {
  let mut requests: Vec<ImagePrefetchRequest> = Vec::new();
  let mut seen_requests: HashSet<(String, CrossOriginAttribute)> = HashSet::new();
  let mut image_elements = 0usize;
  let mut limited = false;

  if limits.max_image_elements == 0 || limits.max_urls_per_element == 0 {
    return ImagePrefetchRequestDiscovery {
      image_elements,
      requests,
      limited,
    };
  }

  let mut stack: Vec<&DomNode> = vec![dom];
  while let Some(node) = stack.pop() {
    if image_elements >= limits.max_image_elements {
      limited = true;
      break;
    }

    if node.template_contents_are_inert() {
      continue;
    }

    let mut descend = true;
    if let Some(tag) = node.tag_name() {
      if tag.eq_ignore_ascii_case("picture") {
        if let Some((picture_sources, img)) = picture_sources_and_fallback_img(node) {
          if image_elements >= limits.max_image_elements {
            limited = true;
            break;
          }
          image_elements += 1;

          let crossorigin = parse_crossorigin_attr(img);
          let img_src = get_img_src_attr(img).unwrap_or("");
          let img_srcset = get_img_srcset_attr(img)
            .map(parse_srcset)
            .unwrap_or_default();
          let img_sizes = get_sizes_attr(img).and_then(parse_sizes);

          push_prefetch_selection_with_crossorigin(
            ctx,
            crossorigin,
            &picture_sources,
            img_src,
            &img_srcset,
            img_sizes.as_ref(),
            limits,
            &mut seen_requests,
            &mut requests,
          );
          if image_elements >= limits.max_image_elements {
            limited = true;
            break;
          }

          descend = false;
        }
      } else if tag.eq_ignore_ascii_case("img") {
        if image_elements >= limits.max_image_elements {
          limited = true;
          break;
        }
        let img_src = get_img_src_attr(node).unwrap_or("");
        let has_src = !trim_ascii_whitespace(img_src).is_empty();
        let img_srcset_attr = get_img_srcset_attr(node);
        let has_srcset = img_srcset_attr.is_some();
        if has_src || has_srcset {
          image_elements += 1;

          let crossorigin = parse_crossorigin_attr(node);
          let img_srcset = img_srcset_attr.map(parse_srcset).unwrap_or_default();
          let img_sizes = get_sizes_attr(node).and_then(parse_sizes);

          push_prefetch_selection_with_crossorigin(
            ctx,
            crossorigin,
            &[],
            img_src,
            &img_srcset,
            img_sizes.as_ref(),
            limits,
            &mut seen_requests,
            &mut requests,
          );
          if image_elements >= limits.max_image_elements {
            limited = true;
            break;
          }
        }
      } else if tag.eq_ignore_ascii_case("video") {
        let poster = node
          .get_attribute_ref("poster")
          .filter(|value| !trim_ascii_whitespace(value).is_empty())
          .or_else(|| {
            node
              .get_attribute_ref("gnt-gl-ps")
              .filter(|value| !trim_ascii_whitespace(value).is_empty())
          });
        if let Some(poster) = poster {
          if image_elements >= limits.max_image_elements {
            limited = true;
            break;
          }
          image_elements += 1;
          push_unique_request(
            ctx,
            CrossOriginAttribute::None,
            &mut seen_requests,
            &mut requests,
            poster,
          );
          if image_elements >= limits.max_image_elements {
            limited = true;
            break;
          }
        }
      } else if tag.eq_ignore_ascii_case("link") {
        if let Some(rel_attr) = node.get_attribute_ref("rel") {
          let rel_tokens = tokenize_rel_list(rel_attr);
          if !rel_tokens.is_empty() {
            let href = trim_ascii_whitespace(node.get_attribute_ref("href").unwrap_or(""));
            let as_attr = node.get_attribute_ref("as");

            let media_matches = match node.get_attribute_ref("media") {
              Some(media) => MediaQuery::parse_list(media)
                .ok()
                .map(|list| {
                  ctx
                    .media_context
                    .map(|m| m.evaluate_list(&list))
                    .unwrap_or(true)
                })
                .unwrap_or(true),
              None => true,
            };
            if !media_matches {
              descend = false;
            } else if link_rel_is_preload_image(&rel_tokens, as_attr) {
              let parsed_srcset = node
                .get_attribute_ref("imagesrcset")
                .map(parse_srcset)
                .unwrap_or_default();
              let parsed_sizes = node.get_attribute_ref("imagesizes").and_then(parse_sizes);
              if href.is_empty() && parsed_srcset.is_empty() {
                descend = false;
              } else {
                if image_elements >= limits.max_image_elements {
                  limited = true;
                  break;
                }
                image_elements += 1;
                let crossorigin = parse_crossorigin_attr(node);
                push_prefetch_selection_with_crossorigin(
                  ctx,
                  crossorigin,
                  &[],
                  href,
                  &parsed_srcset,
                  parsed_sizes.as_ref(),
                  limits,
                  &mut seen_requests,
                  &mut requests,
                );
                if image_elements >= limits.max_image_elements {
                  limited = true;
                  break;
                }
              }
            }
          }
        }
      }
    }

    if !descend {
      continue;
    }

    for child in node.traversal_children().iter().rev() {
      stack.push(child);
    }
  }

  ImagePrefetchRequestDiscovery {
    image_elements,
    requests,
    limited,
  }
}

#[cfg(test)]
mod tests {
  use super::{
    discover_image_prefetch_requests, discover_image_prefetch_urls, ImagePrefetchLimits,
  };
  use crate::dom::{parse_html, DomNode, DomNodeType};
  use crate::geometry::Size;
  use crate::html::images::ImageSelectionContext;
  use crate::style::media::MediaContext;
  use crate::tree::box_tree::CrossOriginAttribute;
  use selectors::context::QuirksMode;
  use url::Url;

  fn media_ctx_for(viewport: (f32, f32), dpr: f32) -> MediaContext {
    MediaContext::screen(viewport.0, viewport.1)
      .with_device_pixel_ratio(dpr)
      .with_env_overrides()
  }

  fn ctx_for<'a>(
    viewport: (f32, f32),
    dpr: f32,
    media_ctx: &'a MediaContext,
    base_url: &'a str,
  ) -> ImageSelectionContext<'a> {
    ImageSelectionContext {
      device_pixel_ratio: dpr,
      slot_width: None,
      viewport: Some(Size::new(viewport.0, viewport.1)),
      media_context: Some(media_ctx),
      font_size: None,
      root_font_size: None,
      base_url: Some(base_url),
    }
  }

  #[test]
  fn discover_image_prefetch_urls_deep_dom_does_not_overflow_stack() {
    const DEPTH: usize = 20_000;

    let mut node = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "div".to_string(),
        namespace: String::new(),
        attributes: Vec::new(),
      },
      children: Vec::new(),
    };

    for _ in 0..DEPTH {
      node = DomNode {
        node_type: DomNodeType::Element {
          tag_name: "div".to_string(),
          namespace: String::new(),
          attributes: Vec::new(),
        },
        children: vec![node],
      };
    }

    let dom = DomNode {
      node_type: DomNodeType::Document {
        quirks_mode: QuirksMode::NoQuirks,
      },
      children: vec![node],
    };

    let media_ctx = media_ctx_for((800.0, 600.0), 1.0);
    let ctx = ctx_for((800.0, 600.0), 1.0, &media_ctx, "https://example.com/");
    let out = discover_image_prefetch_urls(&dom, ctx, ImagePrefetchLimits::default());
    assert_eq!(out.image_elements, 0);
    assert!(out.urls.is_empty());
    assert!(!out.limited);
  }

  #[test]
  fn discover_image_prefetch_urls_does_not_trim_non_ascii_whitespace() {
    let nbsp = "\u{00A0}";
    let html = format!(r#"<img src=" {nbsp} ">"#);
    let dom = parse_html(&html).unwrap();

    let media_ctx = media_ctx_for((800.0, 600.0), 1.0);
    let ctx = ctx_for((800.0, 600.0), 1.0, &media_ctx, "https://example.com/");
    let out = discover_image_prefetch_urls(
      &dom,
      ctx,
      ImagePrefetchLimits {
        max_image_elements: 10,
        max_urls_per_element: 2,
      },
    );

    assert_eq!(out.image_elements, 1);
    assert!(!out.limited);

    let expected = Url::parse("https://example.com/")
      .unwrap()
      .join(nbsp)
      .unwrap()
      .to_string();
    assert_eq!(out.urls, vec![expected]);
  }

  #[test]
  fn discover_image_prefetch_urls_does_not_trim_non_ascii_whitespace_in_type_attr() {
    let nbsp = "\u{00A0}";
    let html = format!(
      r#"
      <picture>
        <source type="{nbsp}image/png" srcset="bad.png 1x">
        <source type="image/png" srcset="good.png 1x">
        <img src="fallback.jpg">
      </picture>
      "#
    );
    let dom = parse_html(&html).unwrap();

    let media_ctx = media_ctx_for((800.0, 600.0), 1.0);
    let ctx = ctx_for((800.0, 600.0), 1.0, &media_ctx, "https://example.com/");
    let out = discover_image_prefetch_urls(&dom, ctx, ImagePrefetchLimits::default());

    assert_eq!(
      out.urls[0],
      Url::parse("https://example.com/").unwrap().join("good.png").unwrap().to_string()
    );
  }

  #[test]
  fn selects_picture_source_matching_media_and_caps_urls() {
    let html = r#"
      <picture>
        <source media="(max-width: 600px)" srcset="small.jpg 1x, small@2x.jpg 2x">
        <source media="(min-width: 601px)" srcset="large.jpg 1x, large@2x.jpg 2x">
        <img src="fallback.jpg" srcset="fallback1.jpg 1x, fallback2.jpg 2x">
      </picture>
    "#;
    let dom = parse_html(html).unwrap();

    let media_ctx = media_ctx_for((500.0, 800.0), 2.0);
    let ctx = ctx_for((500.0, 800.0), 2.0, &media_ctx, "https://example.com/");
    let out = discover_image_prefetch_urls(
      &dom,
      ctx,
      ImagePrefetchLimits {
        max_image_elements: 10,
        max_urls_per_element: 2,
      },
    );

    assert_eq!(out.image_elements, 1);
    assert!(!out.limited);
    assert_eq!(
      out.urls,
      vec![
        "https://example.com/small@2x.jpg".to_string(),
        "https://example.com/fallback2.jpg".to_string(),
      ]
    );
  }

  #[test]
  fn caps_images_per_page_in_dom_order() {
    let html = r#"
      <img src="a.jpg">
      <img src="b.jpg">
    "#;
    let dom = parse_html(html).unwrap();

    let media_ctx = media_ctx_for((1200.0, 800.0), 1.0);
    let ctx = ctx_for((1200.0, 800.0), 1.0, &media_ctx, "https://example.com/");
    let out = discover_image_prefetch_urls(
      &dom,
      ctx,
      ImagePrefetchLimits {
        max_image_elements: 1,
        max_urls_per_element: 3,
      },
    );

    assert_eq!(out.image_elements, 1);
    assert!(out.limited);
    assert_eq!(out.urls, vec!["https://example.com/a.jpg".to_string()]);
  }

  #[test]
  fn ignores_images_inside_unused_declarative_shadow_templates() {
    let html = r#"
      <div id="host">
        <template shadowroot="open"><slot></slot></template>
        <template shadowroot="closed"><img src="bad.jpg"></template>
      </div>
      <img src="good.jpg">
    "#;
    let dom = parse_html(html).unwrap();

    let media_ctx = media_ctx_for((800.0, 600.0), 1.0);
    let ctx = ctx_for((800.0, 600.0), 1.0, &media_ctx, "https://example.com/");
    let out = discover_image_prefetch_urls(
      &dom,
      ctx,
      ImagePrefetchLimits {
        max_image_elements: 10,
        max_urls_per_element: 2,
      },
    );

    assert_eq!(out.image_elements, 1);
    assert!(!out.limited);
    assert_eq!(out.urls, vec!["https://example.com/good.jpg".to_string()]);
  }

  #[test]
  fn selects_link_preload_imagesrcset() {
    let html = r#"
      <link rel="preload" as="image"
        href="fallback.jpg"
        imagesrcset="a1.jpg 1x, a2.jpg 2x">
    "#;
    let dom = parse_html(html).unwrap();

    let media_ctx = media_ctx_for((800.0, 600.0), 2.0);
    let ctx = ctx_for((800.0, 600.0), 2.0, &media_ctx, "https://example.com/");
    let out = discover_image_prefetch_urls(
      &dom,
      ctx,
      ImagePrefetchLimits {
        max_image_elements: 10,
        max_urls_per_element: 2,
      },
    );

    assert_eq!(out.image_elements, 1);
    assert!(!out.limited);
    assert_eq!(
      out.urls,
      vec![
        "https://example.com/a2.jpg".to_string(),
        "https://example.com/fallback.jpg".to_string(),
      ]
    );
  }

  #[test]
  fn discovers_img_src_from_data_gl_src() {
    let html = r#"<img data-gl-src="a.jpg">"#;
    let dom = parse_html(html).unwrap();

    let media_ctx = media_ctx_for((800.0, 600.0), 1.0);
    let ctx = ctx_for((800.0, 600.0), 1.0, &media_ctx, "https://example.com/");
    let out = discover_image_prefetch_urls(
      &dom,
      ctx,
      ImagePrefetchLimits {
        max_image_elements: 10,
        max_urls_per_element: 2,
      },
    );

    assert_eq!(out.image_elements, 1);
    assert!(!out.limited);
    assert_eq!(out.urls, vec!["https://example.com/a.jpg".to_string()]);
  }

  #[test]
  fn discovers_img_src_from_data_src() {
    let html = r#"<img data-src="a.jpg">"#;
    let dom = parse_html(html).unwrap();

    let media_ctx = media_ctx_for((800.0, 600.0), 1.0);
    let ctx = ctx_for((800.0, 600.0), 1.0, &media_ctx, "https://example.com/");
    let out = discover_image_prefetch_urls(
      &dom,
      ctx,
      ImagePrefetchLimits {
        max_image_elements: 10,
        max_urls_per_element: 2,
      },
    );

    assert_eq!(out.image_elements, 1);
    assert!(!out.limited);
    assert_eq!(out.urls, vec!["https://example.com/a.jpg".to_string()]);
  }

  #[test]
  fn discovers_crossorigin_attribute_for_img() {
    let html = r#"<img src="a.jpg" crossorigin><img src="b.jpg" crossorigin="use-credentials">"#;
    let dom = parse_html(html).unwrap();

    let media_ctx = media_ctx_for((800.0, 600.0), 1.0);
    let ctx = ctx_for((800.0, 600.0), 1.0, &media_ctx, "https://example.com/");
    let out = discover_image_prefetch_requests(
      &dom,
      ctx,
      ImagePrefetchLimits {
        max_image_elements: 10,
        max_urls_per_element: 1,
      },
    );

    assert_eq!(out.image_elements, 2);
    assert!(!out.limited);
    assert_eq!(out.requests.len(), 2);
    assert_eq!(out.requests[0].url, "https://example.com/a.jpg");
    assert_eq!(out.requests[0].crossorigin, CrossOriginAttribute::Anonymous);
    assert_eq!(out.requests[1].url, "https://example.com/b.jpg");
    assert_eq!(
      out.requests[1].crossorigin,
      CrossOriginAttribute::UseCredentials
    );
  }

  #[test]
  fn falls_back_from_placeholder_src_to_data_src() {
    let html = r#"<img src="about:blank" data-src="a.jpg">"#;
    let dom = parse_html(html).unwrap();

    let media_ctx = media_ctx_for((800.0, 600.0), 1.0);
    let ctx = ctx_for((800.0, 600.0), 1.0, &media_ctx, "https://example.com/");
    let out = discover_image_prefetch_urls(
      &dom,
      ctx,
      ImagePrefetchLimits {
        max_image_elements: 10,
        max_urls_per_element: 2,
      },
    );

    assert_eq!(out.image_elements, 1);
    assert!(!out.limited);
    assert_eq!(out.urls, vec!["https://example.com/a.jpg".to_string()]);
  }

  #[test]
  fn falls_back_from_1x1_gif_data_src_to_data_src() {
    let html = r#"<img src="data:image/gif;base64,R0lGODlhAQABAIAAAAAAAP///ywAAAAAAQABAAACAUwAOw==" data-src="a.jpg">"#;
    let dom = parse_html(html).unwrap();

    let media_ctx = media_ctx_for((800.0, 600.0), 1.0);
    let ctx = ctx_for((800.0, 600.0), 1.0, &media_ctx, "https://example.com/");
    let out = discover_image_prefetch_urls(
      &dom,
      ctx,
      ImagePrefetchLimits {
        max_image_elements: 10,
        max_urls_per_element: 2,
      },
    );

    assert_eq!(out.image_elements, 1);
    assert!(!out.limited);
    assert_eq!(out.urls, vec!["https://example.com/a.jpg".to_string()]);
  }

  #[test]
  fn discovers_img_srcset_from_data_gl_srcset() {
    let html = r#"<img data-gl-srcset="a1.jpg 1x, a2.jpg 2x">"#;
    let dom = parse_html(html).unwrap();

    let media_ctx = media_ctx_for((800.0, 600.0), 2.0);
    let ctx = ctx_for((800.0, 600.0), 2.0, &media_ctx, "https://example.com/");
    let out = discover_image_prefetch_urls(
      &dom,
      ctx,
      ImagePrefetchLimits {
        max_image_elements: 10,
        max_urls_per_element: 1,
      },
    );

    assert_eq!(out.image_elements, 1);
    assert!(!out.limited);
    assert_eq!(out.urls, vec!["https://example.com/a2.jpg".to_string()]);
  }

  #[test]
  fn discovers_img_srcset_from_data_srcset() {
    let html = r#"<img data-srcset="a1.jpg 1x, a2.jpg 2x">"#;
    let dom = parse_html(html).unwrap();

    let media_ctx = media_ctx_for((800.0, 600.0), 2.0);
    let ctx = ctx_for((800.0, 600.0), 2.0, &media_ctx, "https://example.com/");
    let out = discover_image_prefetch_urls(
      &dom,
      ctx,
      ImagePrefetchLimits {
        max_image_elements: 10,
        max_urls_per_element: 1,
      },
    );

    assert_eq!(out.image_elements, 1);
    assert!(!out.limited);
    assert_eq!(out.urls, vec!["https://example.com/a2.jpg".to_string()]);
  }

  #[test]
  fn discovers_picture_source_srcset_from_data_srcset() {
    let html = r#"
      <picture>
        <source data-srcset="a1.jpg 1x, a2.jpg 2x">
        <img src="fallback.jpg">
      </picture>
    "#;
    let dom = parse_html(html).unwrap();

    let media_ctx = media_ctx_for((800.0, 600.0), 2.0);
    let ctx = ctx_for((800.0, 600.0), 2.0, &media_ctx, "https://example.com/");
    let out = discover_image_prefetch_urls(
      &dom,
      ctx,
      ImagePrefetchLimits {
        max_image_elements: 10,
        max_urls_per_element: 2,
      },
    );

    assert_eq!(out.image_elements, 1);
    assert!(!out.limited);
    assert_eq!(
      out.urls,
      vec![
        "https://example.com/a2.jpg".to_string(),
        "https://example.com/fallback.jpg".to_string(),
      ]
    );
  }

  #[test]
  fn discovers_sizes_from_data_sizes() {
    let html = r#"
      <img
        src="fallback.jpg"
        srcset="small.jpg 600w, large.jpg 1200w"
        data-sizes="600px"
      >
    "#;
    let dom = parse_html(html).unwrap();

    let media_ctx = media_ctx_for((800.0, 600.0), 1.0);
    let ctx = ctx_for((800.0, 600.0), 1.0, &media_ctx, "https://example.com/");
    let out = discover_image_prefetch_urls(
      &dom,
      ctx,
      ImagePrefetchLimits {
        max_image_elements: 10,
        max_urls_per_element: 1,
      },
    );

    assert_eq!(out.image_elements, 1);
    assert!(!out.limited);
    assert_eq!(out.urls, vec!["https://example.com/small.jpg".to_string()]);
  }

  #[test]
  fn discovers_video_poster_from_gnt_gl_ps() {
    let html = r#"<video gnt-gl-ps="poster.jpg"></video>"#;
    let dom = parse_html(html).unwrap();

    let media_ctx = media_ctx_for((800.0, 600.0), 1.0);
    let ctx = ctx_for((800.0, 600.0), 1.0, &media_ctx, "https://example.com/");
    let out = discover_image_prefetch_urls(
      &dom,
      ctx,
      ImagePrefetchLimits {
        max_image_elements: 10,
        max_urls_per_element: 2,
      },
    );

    assert_eq!(out.image_elements, 1);
    assert!(!out.limited);
    assert_eq!(out.urls, vec!["https://example.com/poster.jpg".to_string()]);
  }

  #[test]
  fn hedges_width_descriptor_srcset_by_slot_width_guess() {
    let html = r#"
      <img
        src="fallback.jpg"
        srcset="small.jpg 600w, large.jpg 800w"
      >
    "#;
    let dom = parse_html(html).unwrap();

    let media_ctx = media_ctx_for((800.0, 600.0), 1.0);
    let ctx = ctx_for((800.0, 600.0), 1.0, &media_ctx, "https://example.com/");
    let out = discover_image_prefetch_urls(
      &dom,
      ctx,
      ImagePrefetchLimits {
        max_image_elements: 10,
        max_urls_per_element: 2,
      },
    );

    assert_eq!(out.image_elements, 1);
    assert!(!out.limited);
    assert_eq!(
      out.urls,
      vec![
        "https://example.com/large.jpg".to_string(),
        "https://example.com/small.jpg".to_string(),
      ]
    );
  }

  #[test]
  fn unused_declarative_shadow_templates_are_skipped() {
    let html = r#"
      <div id="host">
        <template shadowroot="open"><img src="good.jpg"></template>
        <template shadowroot="open"><img src="bad.jpg"></template>
      </div>
    "#;
    let dom = parse_html(html).unwrap();

    let media_ctx = media_ctx_for((800.0, 600.0), 1.0);
    let ctx = ctx_for((800.0, 600.0), 1.0, &media_ctx, "https://example.com/");
    let out = discover_image_prefetch_urls(
      &dom,
      ctx,
      ImagePrefetchLimits {
        max_image_elements: 10,
        max_urls_per_element: 2,
      },
    );

    assert_eq!(out.image_elements, 1);
    assert!(!out.limited);
    assert_eq!(out.urls, vec!["https://example.com/good.jpg".to_string()]);
  }

  #[test]
  fn ignores_images_inside_unpromoted_shadow_templates() {
    let html = r#"
      <div id="host">
        <template shadowroot="open"><span></span></template>
        <template shadowroot="open">
          <picture>
            <source srcset="bad.jpg 1x, bad@2x.jpg 2x">
            <img src="bad-fallback.jpg">
          </picture>
        </template>
      </div>
      <img src="outside.jpg">
    "#;
    let dom = parse_html(html).unwrap();

    let media_ctx = media_ctx_for((800.0, 600.0), 1.0);
    let ctx = ctx_for((800.0, 600.0), 1.0, &media_ctx, "https://example.com/");
    let out = discover_image_prefetch_urls(
      &dom,
      ctx,
      ImagePrefetchLimits {
        max_image_elements: 10,
        max_urls_per_element: 2,
      },
    );

    assert_eq!(out.image_elements, 1);
    assert!(!out.limited);
    assert_eq!(
      out.urls,
      vec!["https://example.com/outside.jpg".to_string()]
    );
  }
}
