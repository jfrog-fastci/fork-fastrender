//! Image loading and caching
//!
//! This module provides image loading from various sources (HTTP, file, data URLs)
//! with in-memory caching and support for various image formats including SVG.

use crate::api::{RenderDiagnostics, ResourceContext, ResourceKind};
use crate::debug::runtime;
use crate::error::{Error, ImageError, RenderError, RenderStage, Result};
use crate::paint::painter::with_paint_diagnostics;
use crate::paint::pixmap::{new_pixmap, new_pixmap_with_context, MAX_PIXMAP_BYTES};
use crate::render_control::{self, check_root, check_root_periodic};
use crate::resource::CacheArtifactKind;
use crate::resource::CachingFetcher;
use crate::resource::CachingFetcherConfig;
use crate::resource::FetchContextKind;
use crate::resource::FetchCredentialsMode;
use crate::resource::FetchDestination;
use crate::resource::FetchRequest;
use crate::resource::FetchedResource;
#[cfg(feature = "direct_network")]
use crate::resource::HttpFetcher;
use crate::resource::ReferrerPolicy;
use crate::resource::ResourceFetcher;
use crate::resource::{
  ensure_http_success, ensure_image_mime_sane, ensure_stylesheet_mime_sane, origin_from_url,
};
use crate::style::color::Rgba;
use crate::style::types::ImageResolution;
use crate::style::types::OrientationTransform;
use crate::svg::{
  map_svg_aspect_ratio, parse_svg_length_px, parse_svg_view_box,
  svg_intrinsic_dimensions_from_attributes, svg_markup_for_roxmltree, svg_view_box_root_transform,
  SvgPreserveAspectRatio, SvgViewBox,
};
use crate::text::font_db::FontConfig;
use crate::tree::box_tree::CrossOriginAttribute;
use crate::url_normalize::normalize_url_reference_for_resolution;
#[cfg(feature = "avif")]
use avif_decode::Decoder as AvifDecoder;
#[cfg(feature = "avif")]
use avif_decode::Image as AvifImage;
#[cfg(feature = "avif")]
use avif_parse::AvifData;
use exif;
use flate2::read::{GzDecoder, ZlibDecoder};
use image::imageops;
use image::AnimationDecoder;
use image::DynamicImage;
use image::GenericImageView;
use image::ImageDecoder;
use image::ImageFormat;
use image::ImageReader;
use image::RgbaImage;
use lru::LruCache;
use percent_encoding::percent_decode_str;
use roxmltree::Document;
use serde::{Deserialize, Serialize};
use std::borrow::Borrow;
use std::borrow::Cow;
use std::cell::Cell;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::collections::HashSet;
use std::convert::TryFrom;
use std::hash::Hash;
use std::hash::Hasher;
use std::io::{self, BufRead, Cursor, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tiny_skia::{FilterQuality, IntSize, Pixmap};
use url::Url;

const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
const MAX_PNG_ICC_PROFILE_BYTES: usize = 1024 * 1024;
const MAX_WEBP_ICC_PROFILE_BYTES: usize = 1024 * 1024;

fn extract_png_iccp_profile(png_bytes: &[u8]) -> Option<Vec<u8>> {
  if png_bytes.len() < PNG_SIGNATURE.len() || &png_bytes[..PNG_SIGNATURE.len()] != PNG_SIGNATURE {
    return None;
  }

  let mut offset = PNG_SIGNATURE.len();
  while offset + 12 <= png_bytes.len() {
    let length = u32::from_be_bytes(png_bytes.get(offset..offset + 4)?.try_into().ok()?) as usize;
    let chunk_type: [u8; 4] = png_bytes.get(offset + 4..offset + 8)?.try_into().ok()?;
    let data_start = offset + 8;
    let data_end = data_start.checked_add(length)?;
    let next = data_end.checked_add(4)?; // CRC
    if next > png_bytes.len() {
      break;
    }

    if &chunk_type == b"iCCP" {
      let data = png_bytes.get(data_start..data_end)?;
      let nul_pos = data.iter().position(|b| *b == 0)?;
      // Name (null-terminated), then compression method (1 byte), then zlib data.
      let compression_method = *data.get(nul_pos + 1)?;
      if compression_method != 0 {
        return None;
      }
      let compressed = data.get(nul_pos + 2..)?;
      let mut out = Vec::new();
      ZlibDecoder::new(compressed)
        .take((MAX_PNG_ICC_PROFILE_BYTES + 1) as u64)
        .read_to_end(&mut out)
        .ok()?;
      if out.len() > MAX_PNG_ICC_PROFILE_BYTES {
        return None;
      }
      return Some(out);
    }

    if &chunk_type == b"IEND" {
      break;
    }

    offset = next;
  }

  None
}

fn extract_webp_icc_profile(bytes: &[u8]) -> Option<Vec<u8>> {
  if bytes.len() < 12 {
    return None;
  }
  if bytes.get(0..4)? != b"RIFF" || bytes.get(8..12)? != b"WEBP" {
    return None;
  }

  // The RIFF size field counts everything after it (i.e. file size - 8). Use it as a best-effort
  // bound for chunk iteration, but never read beyond the actual input slice.
  let riff_size: [u8; 4] = bytes.get(4..8)?.try_into().ok()?;
  let declared_end = 8usize.checked_add(u32::from_le_bytes(riff_size) as usize)?;
  let container_end = declared_end.min(bytes.len());

  let mut offset = 12usize;
  while offset + 8 <= container_end {
    let tag = bytes.get(offset..offset + 4)?;
    let size_bytes: [u8; 4] = bytes.get(offset + 4..offset + 8)?.try_into().ok()?;
    let size = u32::from_le_bytes(size_bytes) as usize;
    offset = offset.checked_add(8)?;
    let end = offset.checked_add(size)?;
    if end > container_end {
      return None;
    }

    if tag == b"ICCP" {
      if size > MAX_WEBP_ICC_PROFILE_BYTES {
        return None;
      }
      return Some(bytes.get(offset..end)?.to_vec());
    }

    offset = end;
    if size % 2 == 1 && offset < container_end {
      offset = offset.checked_add(1)?;
    }
  }

  None
}

#[derive(Clone)]
enum IccToneCurve {
  Identity,
  Gamma(f64),
  Table(Vec<f64>),
}

impl IccToneCurve {
  fn eval(&self, v: f64) -> f64 {
    let v = if v.is_finite() {
      v.clamp(0.0, 1.0)
    } else {
      0.0
    };
    match self {
      Self::Identity => v,
      Self::Gamma(gamma) if gamma.is_finite() && *gamma > 0.0 => v.powf(*gamma),
      Self::Gamma(_) => v,
      Self::Table(values) => {
        if values.len() < 2 {
          return v;
        }
        let last = (values.len() - 1) as f64;
        let pos = v * last;
        let idx = pos.floor() as usize;
        if idx >= values.len() - 1 {
          return values[values.len() - 1];
        }
        let t = pos - idx as f64;
        values[idx] * (1.0 - t) + values[idx + 1] * t
      }
    }
  }
}

struct IccMatrixProfileToSrgb {
  /// Matrix that maps source *linear* RGB values into linear sRGB values.
  ///
  /// `linear_srgb = matrix * linear_src_rgb`
  matrix: [[f64; 3]; 3],
  /// Lookup tables mapping each encoded u8 channel value to the source profile's linear-light
  /// component, after applying the TRC.
  u8_to_linear: [[f64; 256]; 3],
}

fn mul3(a: &[[f64; 3]; 3], b: &[[f64; 3]; 3]) -> [[f64; 3]; 3] {
  let mut out = [[0.0; 3]; 3];
  for i in 0..3 {
    for j in 0..3 {
      out[i][j] = a[i][0] * b[0][j] + a[i][1] * b[1][j] + a[i][2] * b[2][j];
    }
  }
  out
}

fn apply_icc_profile_to_srgb(rgba: &mut RgbaImage, icc_profile: &[u8]) -> bool {
  const D50_TO_D65: [[f64; 3]; 3] = [
    [0.955_576_615, -0.023_039_344_7, 0.063_163_632_2],
    [-0.028_289_544_2, 1.009_941_617_4, 0.021_007_655],
    [0.012_298_165_7, -0.020_483_025_2, 1.329_909_826_4],
  ];
  // XYZ(D65) -> linear sRGB.
  const XYZ_TO_SRGB: [[f64; 3]; 3] = [
    [3.240_454_836, -1.537_138_850_1, -0.498_531_546_9],
    [-0.969_266_389_9, 1.876_010_928_8, 0.041_556_082_3],
    [0.055_643_419_6, -0.204_025_854_3, 1.057_225_162_5],
  ];

  fn read_u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    bytes
      .get(offset..offset + 4)
      .and_then(|b| <[u8; 4]>::try_from(b).ok())
      .map(u32::from_be_bytes)
  }

  fn read_u16_at(bytes: &[u8], offset: usize) -> Option<u16> {
    bytes
      .get(offset..offset + 2)
      .and_then(|b| <[u8; 2]>::try_from(b).ok())
      .map(u16::from_be_bytes)
  }

  fn read_i32_at(bytes: &[u8], offset: usize) -> Option<i32> {
    bytes
      .get(offset..offset + 4)
      .and_then(|b| <[u8; 4]>::try_from(b).ok())
      .map(i32::from_be_bytes)
  }

  fn parse_xyz_tag(tag: &[u8]) -> Option<[f64; 3]> {
    if tag.len() < 20 || &tag[..4] != b"XYZ " {
      return None;
    }
    let x = read_i32_at(tag, 8)? as f64 / 65_536.0;
    let y = read_i32_at(tag, 12)? as f64 / 65_536.0;
    let z = read_i32_at(tag, 16)? as f64 / 65_536.0;
    Some([x, y, z])
  }

  fn parse_trc_tag(tag: &[u8]) -> Option<IccToneCurve> {
    if tag.len() < 12 {
      return None;
    }
    match &tag[..4] {
      b"curv" => {
        let count = read_u32_at(tag, 8)? as usize;
        match count {
          0 => Some(IccToneCurve::Identity),
          1 => {
            let gamma = read_u16_at(tag, 12)? as f64 / 256.0;
            Some(IccToneCurve::Gamma(gamma))
          }
          n => {
            let table_len = 12usize.checked_add(n.checked_mul(2)?)?;
            if table_len > tag.len() {
              return None;
            }
            let mut values = Vec::with_capacity(n);
            for i in 0..n {
              let raw = read_u16_at(tag, 12 + i * 2)? as f64;
              values.push(raw / 65_535.0);
            }
            Some(IccToneCurve::Table(values))
          }
        }
      }
      _ => None,
    }
  }

  fn srgb_encode(linear: f64) -> f64 {
    let linear = if linear.is_finite() { linear } else { 0.0 };
    let linear = linear.clamp(0.0, 1.0);
    if linear <= 0.003_130_8 {
      12.92 * linear
    } else {
      1.055 * linear.powf(1.0 / 2.4) - 0.055
    }
  }

  fn clamp_and_quantize(v: f64) -> u8 {
    let v = if v.is_finite() {
      v.clamp(0.0, 1.0)
    } else {
      0.0
    };
    let v = (v * 255.0 + 0.5).floor();
    v.clamp(0.0, 255.0) as u8
  }

  let Some(profile) = (|| -> Option<IccMatrixProfileToSrgb> {
    if icc_profile.len() < 132 {
      return None;
    }
    let tag_count = read_u32_at(icc_profile, 128)? as usize;
    let table_len = tag_count.checked_mul(12)?;
    let table_end = 132usize.checked_add(table_len)?;
    if table_end > icc_profile.len() {
      return None;
    }

    let mut rxyz = None;
    let mut gxyz = None;
    let mut bxyz = None;
    let mut rtrc = None;
    let mut gtrc = None;
    let mut btrc = None;

    for i in 0..tag_count {
      let base = 132 + i * 12;
      let sig = icc_profile.get(base..base + 4)?;
      let offset = read_u32_at(icc_profile, base + 4)? as usize;
      let size = read_u32_at(icc_profile, base + 8)? as usize;
      let end = offset.checked_add(size)?;
      if end > icc_profile.len() {
        continue;
      }
      let tag_bytes = icc_profile.get(offset..end)?;
      match sig {
        b"rXYZ" if rxyz.is_none() => rxyz = parse_xyz_tag(tag_bytes),
        b"gXYZ" if gxyz.is_none() => gxyz = parse_xyz_tag(tag_bytes),
        b"bXYZ" if bxyz.is_none() => bxyz = parse_xyz_tag(tag_bytes),
        b"rTRC" if rtrc.is_none() => rtrc = parse_trc_tag(tag_bytes),
        b"gTRC" if gtrc.is_none() => gtrc = parse_trc_tag(tag_bytes),
        b"bTRC" if btrc.is_none() => btrc = parse_trc_tag(tag_bytes),
        _ => {}
      }
    }

    let [rx, ry, rz] = rxyz?;
    let [gx, gy, gz] = gxyz?;
    let [bx, by, bz] = bxyz?;
    let r_curve = rtrc?;
    let g_curve = gtrc?;
    let b_curve = btrc?;

    let m_src = [[rx, gx, bx], [ry, gy, by], [rz, gz, bz]];
    let m_temp = mul3(&D50_TO_D65, &m_src);
    let matrix = mul3(&XYZ_TO_SRGB, &m_temp);

    let mut u8_to_linear = [[0.0; 256]; 3];
    let curves = [r_curve, g_curve, b_curve];
    for (c, curve) in curves.iter().enumerate() {
      for i in 0..256 {
        u8_to_linear[c][i] = curve.eval(i as f64 / 255.0);
      }
    }

    Some(IccMatrixProfileToSrgb {
      matrix,
      u8_to_linear,
    })
  })() else {
    return false;
  };

  for px in rgba.pixels_mut() {
    let src_r = profile.u8_to_linear[0][px[0] as usize];
    let src_g = profile.u8_to_linear[1][px[1] as usize];
    let src_b = profile.u8_to_linear[2][px[2] as usize];

    let lin_r =
      profile.matrix[0][0] * src_r + profile.matrix[0][1] * src_g + profile.matrix[0][2] * src_b;
    let lin_g =
      profile.matrix[1][0] * src_r + profile.matrix[1][1] * src_g + profile.matrix[1][2] * src_b;
    let lin_b =
      profile.matrix[2][0] * src_r + profile.matrix[2][1] * src_g + profile.matrix[2][2] * src_b;

    px[0] = clamp_and_quantize(srgb_encode(lin_r));
    px[1] = clamp_and_quantize(srgb_encode(lin_g));
    px[2] = clamp_and_quantize(srgb_encode(lin_b));
  }

  true
}

fn shared_svg_fontdb() -> Arc<resvg::usvg::fontdb::Database> {
  static SVG_FONT_DB: OnceLock<Arc<resvg::usvg::fontdb::Database>> = OnceLock::new();
  Arc::clone(SVG_FONT_DB.get_or_init(|| {
    // usvg text shaping requires an explicit font database; otherwise `<text>` can be dropped
    // entirely. Populate the fontdb once and share it across SVG parses.
    //
    // Note: resvg/usvg currently depend on `fontdb` v0.21, while FastRender's HTML text engine
    // uses `fontdb` v0.23. The types are incompatible, so we load fonts directly into usvg's
    // fontdb (reusing the same in-repo bundled font *bytes* for deterministic runs).
    //
    // Respect FastRender's "bundled fonts" mode in CI (`CI` or `FASTR_USE_BUNDLED_FONTS=1`) by
    // skipping system font discovery when disabled.
    let config = FontConfig::default();
    let mut db = resvg::usvg::fontdb::Database::new();
    if config.use_system_fonts {
      db.load_system_fonts();
    }
    if config.use_bundled_fonts {
      for data in crate::text::font_db::bundled_font_data() {
        db.load_font_data(data.to_vec());
      }
      if crate::text::font_db::bundled_emoji_fonts_enabled() {
        for data in crate::text::font_db::bundled_emoji_font_data() {
          db.load_font_data(data.to_vec());
        }
      }
      if !config.use_system_fonts {
        // Match FastRender's bundled-font defaults: prefer stable Noto families for generic
        // `serif`/`sans-serif`/`monospace` resolution in hermetic runs.
        db.set_serif_family("Noto Serif".to_string());
        db.set_sans_serif_family("Noto Sans".to_string());
        db.set_monospace_family("Noto Sans Mono".to_string());
        db.set_cursive_family("Noto Sans".to_string());
        db.set_fantasy_family("Noto Sans".to_string());
      }
    }
    // Always load a tiny deterministic fixture font so SVG text renders even in minimal
    // environments (and tests remain hermetic).
    db.load_font_data(include_bytes!("../tests/fixtures/fonts/Cantarell-Test.ttf").to_vec());
    Arc::new(db)
  }))
}

fn usvg_options_for_url(_url: &str) -> resvg::usvg::Options<'_> {
  let mut options = resvg::usvg::Options::default();
  options.fontdb = shared_svg_fontdb();

  // Security: do not give `usvg`/`resvg` a filesystem base directory for resolving relative
  // resources (e.g. `<image href="...">`). In sandboxed renderers all filesystem access must be
  // mediated by FastRender's `ResourceFetcher` and policy enforcement; external SVG resources are
  // instead fetched and inlined during `preprocess_svg_markup` via the `inline_svg_*` pipeline.
  //
  // Keeping this unset prevents downstream libraries from performing direct disk reads when
  // rendering `file://` SVGs.
  options.resources_dir = None;

  options
}

fn fetch_credentials_mode_for_crossorigin(
  crossorigin: CrossOriginAttribute,
) -> FetchCredentialsMode {
  match crossorigin {
    CrossOriginAttribute::None => FetchCredentialsMode::Include,
    CrossOriginAttribute::Anonymous => crate::resource::CorsMode::Anonymous.credentials_mode(),
    CrossOriginAttribute::UseCredentials => {
      crate::resource::CorsMode::UseCredentials.credentials_mode()
    }
  }
}

fn unescape_js_escapes(input: &str) -> Cow<'_, str> {
  if !input.contains('\\') {
    return Cow::Borrowed(input);
  }

  let mut out = String::with_capacity(input.len());
  let bytes = input.as_bytes();
  let mut i = 0;
  while i < bytes.len() {
    if bytes[i] == b'\\' {
      if i + 1 < bytes.len()
        && (bytes[i + 1] == b'"' || bytes[i + 1] == b'\'' || bytes[i + 1] == b'/')
      {
        out.push(bytes[i + 1] as char);
        i += 2;
        continue;
      }

      if i + 5 < bytes.len() && (bytes[i + 1] == b'u' || bytes[i + 1] == b'U') {
        if let Ok(code) = u16::from_str_radix(&input[i + 2..i + 6], 16) {
          if let Some(ch) = char::from_u32(code as u32) {
            out.push(ch);
            i += 6;
            continue;
          }
        }
      }
    }

    out.push(bytes[i] as char);
    i += 1;
  }

  Cow::Owned(out)
}

// HTML/CSS URL-ish values strip leading/trailing ASCII whitespace (TAB/LF/FF/CR/SPACE) but do not
// treat all Unicode whitespace (e.g. NBSP) as ignorable.
fn trim_ascii_whitespace(value: &str) -> &str {
  value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
}

fn trim_ascii_whitespace_start(value: &str) -> &str {
  value.trim_start_matches(|c: char| {
    matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' ')
  })
}

pub(crate) fn url_looks_like_gif(url: &str) -> bool {
  let trimmed = trim_ascii_whitespace(url);
  let lower = trimmed.to_ascii_lowercase();
  if let Some(rest) = lower.strip_prefix("data:") {
    return rest.starts_with("image/gif");
  }
  if let Ok(parsed) = Url::parse(trimmed) {
    return parsed.path().to_ascii_lowercase().ends_with(".gif");
  }
  let without_query = trimmed
    .split(|ch| ch == '?' || ch == '#')
    .next()
    .unwrap_or(trimmed);
  without_query.to_ascii_lowercase().ends_with(".gif")
}

fn css_display_value_is_none(value: &str) -> bool {
  let mut tokens = value.split_ascii_whitespace();
  let Some(first) = tokens.next() else {
    return false;
  };
  if !first.eq_ignore_ascii_case("none") {
    return false;
  }
  for token in tokens {
    // `!important` is parsed outside of the property value in CSS, but inline `style=""` strings
    // often include it and we only need a best-effort check for `display:none`.
    if token.eq_ignore_ascii_case("!important") {
      continue;
    }
    // Common legacy IE hack used in the wild: `display:none \9`. Treat the trailing `\9` token
    // as ignorable so we still recognize that the element is non-displayed in modern parsers.
    if token.eq_ignore_ascii_case("\\9") {
      continue;
    }
    return false;
  }
  true
}

fn svg_node_has_display_none(node: roxmltree::Node<'_, '_>) -> bool {
  if let Some(display) = node.attribute("display") {
    if trim_ascii_whitespace(display).eq_ignore_ascii_case("none") {
      return true;
    }
  }
  let Some(style) = node.attribute("style") else {
    return false;
  };
  for decl in style.split(';') {
    let decl = decl.trim();
    if decl.is_empty() {
      continue;
    }
    let Some((name, value)) = decl.split_once(':') else {
      continue;
    };
    if name.trim().eq_ignore_ascii_case("display") && css_display_value_is_none(value) {
      return true;
    }
  }
  false
}

fn svg_xml_base_chain_for_node<'a, 'input>(node: roxmltree::Node<'a, 'input>) -> Vec<&'a str> {
  let mut chain = Vec::new();
  for ancestor in node.ancestors().filter(|n| n.is_element()) {
    // `roxmltree` exposes namespaced attributes via `{namespace, name}` (without the prefix). For
    // `xml:base` the namespace is the standard XML namespace.
    const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";
    let xml_base = ancestor.attribute("xml:base").or_else(|| {
      ancestor
        .attributes()
        .find(|attr| attr.name() == "base" && attr.namespace() == Some(XML_NS))
        .map(|attr| attr.value())
    });
    if let Some(value) = xml_base {
      let value = trim_ascii_whitespace(value);
      if !value.is_empty() {
        chain.push(value);
      }
    }
  }
  chain.reverse();
  chain
}

fn apply_svg_xml_base_chain(base: Option<&str>, chain: &[&str]) -> Option<String> {
  let mut current = base.map(|base| base.to_string());
  for value in chain {
    let value = trim_ascii_whitespace(value);
    if value.is_empty() {
      continue;
    }
    // If `xml:base` is itself an absolute URL, it resets the base URI.
    if Url::parse(value).is_ok() {
      current = Some(value.to_string());
      continue;
    }
    let Some(cur) = current.as_deref() else {
      return None;
    };
    current = resolve_against_base(cur, value);
  }
  current
}

fn strip_url_fragment(url: &str) -> Cow<'_, str> {
  url
    .split_once('#')
    .map(|(prefix, _)| Cow::Borrowed(prefix))
    .unwrap_or(Cow::Borrowed(url))
}

fn decode_inline_svg_url(url: &str) -> Option<String> {
  let trimmed = trim_ascii_whitespace_start(url);
  if trimmed.is_empty() {
    return None;
  }

  if trimmed.starts_with('<') {
    return Some(unescape_js_escapes(trimmed).into_owned());
  }

  // Inline SVG markup sometimes appears percent-encoded (e.g. `%3Csvg ...`). Treat it the same as
  // raw `<svg...>` strings so the renderer doesn't accidentally resolve it as a network URL.
  if trimmed
    .get(.."%3csvg".len())
    .is_some_and(|prefix| prefix.eq_ignore_ascii_case("%3csvg"))
  {
    if let Ok(decoded) = percent_decode_str(trimmed).decode_utf8() {
      let decoded = decoded.into_owned();
      let decoded_trimmed = trim_ascii_whitespace_start(&decoded);
      if decoded_trimmed.starts_with('<') {
        return Some(unescape_js_escapes(decoded_trimmed).into_owned());
      }
    }
  }

  None
}

fn image_profile_threshold_ms() -> Option<f64> {
  runtime::runtime_toggles().f64("FASTR_IMAGE_PROFILE_MS")
}

fn image_probe_max_bytes() -> usize {
  runtime::runtime_toggles()
    .usize("FASTR_IMAGE_PROBE_MAX_BYTES")
    .unwrap_or(64 * 1024)
    .clamp(1, 16 * 1024 * 1024)
}

const PANIC_REASON_MAX_BYTES: usize = 1024;
const MAX_SVGZ_DECOMPRESSED_BYTES: usize = 16 * 1024 * 1024;

fn truncate_utf8_at_boundary(input: &str, max_bytes: usize) -> &str {
  if input.len() <= max_bytes {
    return input;
  }
  let mut end = max_bytes;
  while end > 0 && !input.is_char_boundary(end) {
    end -= 1;
  }
  &input[..end]
}

fn panic_payload_to_reason(panic: &(dyn std::any::Any + Send)) -> String {
  let message = panic
    .downcast_ref::<&str>()
    .map(|msg| (*msg).to_string())
    .or_else(|| panic.downcast_ref::<String>().cloned())
    .unwrap_or_else(|| "<unknown panic>".to_string());
  truncate_utf8_at_boundary(&message, PANIC_REASON_MAX_BYTES).to_string()
}

/// Image cache diagnostics collection.
#[derive(Debug, Default, Clone)]
pub struct ImageCacheDiagnostics {
  pub requests: usize,
  pub cache_hits: usize,
  pub cache_misses: usize,
  pub decode_ms: f64,
  /// Number of `ResourceFetcher::fetch_partial` calls made by image probes.
  pub probe_partial_requests: usize,
  /// Total bytes returned by partial probe fetches (sum of response body prefixes).
  pub probe_partial_bytes_total: usize,
  /// Number of times an image probe attempted partial fetches but ultimately fell back to a full
  /// `fetch()` due to insufficient bytes / unsupported partial responses.
  pub probe_partial_fallback_full: usize,
  pub raster_pixmap_cache_hits: usize,
  pub raster_pixmap_cache_misses: usize,
  /// Maximum cached bytes observed for the raster pixmap cache during the diagnostic window.
  pub raster_pixmap_cache_bytes: usize,
}

thread_local! {
  static IMAGE_CACHE_DIAGNOSTICS_ACTIVE: Cell<bool> = const { Cell::new(false) };
}
static IMAGE_CACHE_DIAGNOSTICS: Mutex<
  Option<HashMap<std::thread::ThreadId, ImageCacheDiagnostics>>,
> = Mutex::new(None);
static NEXT_IMAGE_CACHE_INSTANCE_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) fn enable_image_cache_diagnostics() {
  IMAGE_CACHE_DIAGNOSTICS_ACTIVE.with(|active| active.set(true));
  let mut guard = IMAGE_CACHE_DIAGNOSTICS
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner());
  let map = guard.get_or_insert_with(HashMap::new);
  map.insert(
    std::thread::current().id(),
    ImageCacheDiagnostics::default(),
  );
}

pub(crate) fn take_image_cache_diagnostics() -> Option<ImageCacheDiagnostics> {
  IMAGE_CACHE_DIAGNOSTICS_ACTIVE.with(|active| active.set(false));
  IMAGE_CACHE_DIAGNOSTICS
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner())
    .as_mut()
    .and_then(|map| map.remove(&std::thread::current().id()))
}

#[inline]
fn with_image_cache_diagnostics<F: FnOnce(&mut ImageCacheDiagnostics)>(f: F) {
  if !IMAGE_CACHE_DIAGNOSTICS_ACTIVE.with(|active| active.get()) {
    return;
  }
  let mut guard = IMAGE_CACHE_DIAGNOSTICS
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner());
  if let Some(map) = guard.as_mut() {
    if let Some(stats) = map.get_mut(&std::thread::current().id()) {
      f(stats);
    }
  }
}

fn record_image_cache_request() {
  with_image_cache_diagnostics(|stats| stats.requests += 1);
}

fn record_image_cache_hit() {
  with_image_cache_diagnostics(|stats| stats.cache_hits += 1);
}

fn record_image_cache_miss() {
  with_image_cache_diagnostics(|stats| stats.cache_misses += 1);
}

fn record_image_decode_ms(duration_ms: f64) {
  if !duration_ms.is_finite() || duration_ms <= 0.0 {
    return;
  }
  with_image_cache_diagnostics(|stats| stats.decode_ms += duration_ms);
}

fn record_probe_partial_fetch(bytes: usize) {
  with_image_cache_diagnostics(|stats| {
    stats.probe_partial_requests += 1;
    stats.probe_partial_bytes_total = stats.probe_partial_bytes_total.saturating_add(bytes);
  });
}

fn record_probe_partial_fallback_full() {
  with_image_cache_diagnostics(|stats| stats.probe_partial_fallback_full += 1);
}

fn record_raster_pixmap_cache_hit() {
  with_image_cache_diagnostics(|stats| stats.raster_pixmap_cache_hits += 1);
}

fn record_raster_pixmap_cache_miss() {
  with_image_cache_diagnostics(|stats| stats.raster_pixmap_cache_misses += 1);
}

fn record_raster_pixmap_cache_bytes(bytes: usize) {
  with_image_cache_diagnostics(|stats| {
    stats.raster_pixmap_cache_bytes = stats.raster_pixmap_cache_bytes.max(bytes);
  });
}

const IMAGE_DECODE_DEADLINE_STRIDE: usize = 8192;

struct DeadlineCursor<'a> {
  inner: Cursor<&'a [u8]>,
  deadline_counter: usize,
}

impl<'a> DeadlineCursor<'a> {
  fn new(bytes: &'a [u8]) -> Self {
    Self {
      inner: Cursor::new(bytes),
      deadline_counter: 0,
    }
  }

  fn check_deadline(&mut self) -> io::Result<()> {
    check_root_periodic(
      &mut self.deadline_counter,
      IMAGE_DECODE_DEADLINE_STRIDE,
      RenderStage::Paint,
    )
    .map_err(|err| io::Error::new(io::ErrorKind::Other, err))
  }
}

impl<'a> Read for DeadlineCursor<'a> {
  fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
    self.check_deadline()?;
    self.inner.read(buf)
  }
}

impl<'a> BufRead for DeadlineCursor<'a> {
  fn fill_buf(&mut self) -> io::Result<&[u8]> {
    self.check_deadline()?;
    self.inner.fill_buf()
  }

  fn consume(&mut self, amt: usize) {
    self.inner.consume(amt);
  }
}

impl<'a> Seek for DeadlineCursor<'a> {
  fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
    self.check_deadline()?;
    self.inner.seek(pos)
  }
}

// ============================================================================
// Embedded ICC profile decoding + color conversion
// ============================================================================

#[derive(Clone)]
enum IccTransferCurve {
  /// `y = x^gamma`
  Gamma(f32),
  /// `curveType` table entries (0..65535 mapped to 0..1), sampled at `i/(n-1)`.
  Table(Vec<f32>),
  /// `parametricCurveType` (ICC v4).
  Parametric {
    kind: u16,
    g: f32,
    a: f32,
    b: f32,
    c: f32,
    d: f32,
    e: f32,
    f: f32,
  },
}

impl IccTransferCurve {
  fn apply(&self, x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    match self {
      IccTransferCurve::Gamma(gamma) => x.powf(*gamma),
      IccTransferCurve::Table(table) => {
        let n = table.len();
        if n == 0 {
          return x;
        }
        if n == 1 {
          return table[0];
        }
        let pos = x * (n - 1) as f32;
        let idx = pos.floor() as usize;
        let frac = pos - idx as f32;
        let a = table[idx];
        let b = table[(idx + 1).min(n - 1)];
        a + (b - a) * frac
      }
      IccTransferCurve::Parametric {
        kind,
        g,
        a,
        b,
        c,
        d,
        e,
        f,
      } => {
        let pow = |base: f32, g: f32| {
          if base <= 0.0 {
            0.0
          } else {
            base.powf(g)
          }
        };
        match *kind {
          // Y = X^g
          0 => pow(x, *g),
          // Y = (aX + b)^g  for X >= -b/a else 0
          1 => {
            let cutoff = if *a == 0.0 { f32::INFINITY } else { -(*b) / *a };
            if x >= cutoff {
              pow(*a * x + *b, *g)
            } else {
              0.0
            }
          }
          // Y = (aX + b)^g + c  for X >= -b/a else c
          2 => {
            let cutoff = if *a == 0.0 { f32::INFINITY } else { -(*b) / *a };
            if x >= cutoff {
              pow(*a * x + *b, *g) + *c
            } else {
              *c
            }
          }
          // Y = (aX + b)^g  for X >= d else cX
          3 => {
            if x >= *d {
              pow(*a * x + *b, *g)
            } else {
              *c * x
            }
          }
          // Y = (aX + b)^g + e  for X >= d else cX + f
          4 => {
            if x >= *d {
              pow(*a * x + *b, *g) + *e
            } else {
              *c * x + *f
            }
          }
          _ => x,
        }
      }
    }
  }
}

#[derive(Clone)]
struct IccToSrgbTransform {
  // Convert linear RGB in the embedded profile to linear sRGB.
  m_profile_to_srgb: [[f32; 3]; 3],
  r_trc_lut: [f32; 256],
  g_trc_lut: [f32; 256],
  b_trc_lut: [f32; 256],
}

impl IccToSrgbTransform {
  fn convert_rgb8(&self, r: u8, g: u8, b: u8) -> (u8, u8, u8) {
    let r_lin = self.r_trc_lut[r as usize];
    let g_lin = self.g_trc_lut[g as usize];
    let b_lin = self.b_trc_lut[b as usize];

    let m = &self.m_profile_to_srgb;
    let sr = m[0][0] * r_lin + m[0][1] * g_lin + m[0][2] * b_lin;
    let sg = m[1][0] * r_lin + m[1][1] * g_lin + m[1][2] * b_lin;
    let sb = m[2][0] * r_lin + m[2][1] * g_lin + m[2][2] * b_lin;

    let lut = srgb_encode_lut();
    let enc = |v: f32| -> u8 {
      let v = v.clamp(0.0, 1.0);
      let idx = (v * 65535.0).round().clamp(0.0, 65535.0) as usize;
      lut[idx]
    };
    (enc(sr), enc(sg), enc(sb))
  }

  fn apply_rgba8_in_place(&self, rgba: &mut [u8]) -> Result<()> {
    let mut stride_counter = 0usize;
    for px in rgba.chunks_exact_mut(4) {
      if stride_counter == 0 {
        check_root(RenderStage::Paint).map_err(Error::Render)?;
      }
      stride_counter += 1;
      if stride_counter >= IMAGE_DECODE_DEADLINE_STRIDE {
        stride_counter = 0;
      }
      let (r, g, b) = self.convert_rgb8(px[0], px[1], px[2]);
      px[0] = r;
      px[1] = g;
      px[2] = b;
    }
    Ok(())
  }
}

fn srgb_encode_lut() -> &'static [u8; 65536] {
  static LUT: OnceLock<[u8; 65536]> = OnceLock::new();
  LUT.get_or_init(|| {
    let mut table = [0u8; 65536];
    for (idx, out) in table.iter_mut().enumerate() {
      let linear = idx as f32 / 65535.0;
      let srgb = if linear <= 0.0031308 {
        12.92 * linear
      } else {
        1.055 * linear.powf(1.0 / 2.4) - 0.055
      };
      *out = (srgb.clamp(0.0, 1.0) * 255.0).round().clamp(0.0, 255.0) as u8;
    }
    table
  })
}

fn icc_s15fixed16_to_f32(value: i32) -> f32 {
  value as f32 / 65536.0
}

fn read_be_u16(bytes: &[u8], offset: usize) -> Option<u16> {
  bytes
    .get(offset..offset + 2)
    .and_then(|s| <[u8; 2]>::try_from(s).ok())
    .map(u16::from_be_bytes)
}

fn read_be_u32(bytes: &[u8], offset: usize) -> Option<u32> {
  bytes
    .get(offset..offset + 4)
    .and_then(|s| <[u8; 4]>::try_from(s).ok())
    .map(u32::from_be_bytes)
}

fn read_be_i32(bytes: &[u8], offset: usize) -> Option<i32> {
  read_be_u32(bytes, offset).map(|v| v as i32)
}

fn icc_tag_slice<'a>(profile: &'a [u8], sig: &[u8; 4]) -> Option<&'a [u8]> {
  let count = read_be_u32(profile, 128)? as usize;
  let table_start = 132usize;
  let entry_size = 12usize;
  let table_end = table_start.checked_add(count.checked_mul(entry_size)?)?;
  if table_end > profile.len() {
    return None;
  }

  for i in 0..count {
    let off = table_start + i * entry_size;
    let entry_sig = profile.get(off..off + 4)?;
    if entry_sig == sig {
      let tag_off = read_be_u32(profile, off + 4)? as usize;
      let tag_size = read_be_u32(profile, off + 8)? as usize;
      let end = tag_off.checked_add(tag_size)?;
      if end > profile.len() {
        return None;
      }
      return profile.get(tag_off..end);
    }
  }
  None
}

fn icc_profile_description(profile: &[u8]) -> Option<String> {
  if profile.len() < 132 {
    return None;
  }

  // Prefer `mluc` when present.
  if let Some(tag) = icc_tag_slice(profile, b"mluc") {
    if tag.len() >= 16 && &tag[0..4] == b"mluc" {
      let count = read_be_u32(tag, 8)? as usize;
      let record_size = read_be_u32(tag, 12)? as usize;
      if record_size < 12 {
        return None;
      }
      let records_start = 16usize;
      let first = records_start.checked_add(record_size)?;
      if count == 0 || first > tag.len() {
        return None;
      }
      let rec = &tag[records_start..first];
      let str_len = read_be_u32(rec, 4)? as usize;
      let str_off = read_be_u32(rec, 8)? as usize;
      let end = str_off.checked_add(str_len)?;
      if end > tag.len() || str_len % 2 != 0 {
        return None;
      }
      let utf16: Vec<u16> = tag[str_off..end]
        .chunks_exact(2)
        .filter_map(|b| <[u8; 2]>::try_from(b).ok())
        .map(u16::from_be_bytes)
        .collect();
      return String::from_utf16(&utf16).ok();
    }
  }

  if let Some(tag) = icc_tag_slice(profile, b"desc") {
    if tag.len() >= 12 && &tag[0..4] == b"desc" {
      let len = read_be_u32(tag, 8)? as usize;
      let start = 12usize;
      let end = start.checked_add(len)?;
      if end > tag.len() || len == 0 {
        return None;
      }
      let mut s = tag[start..end].to_vec();
      if let Some(&0) = s.last() {
        s.pop();
      }
      return String::from_utf8(s).ok();
    }
  }

  None
}

fn icc_parse_xyz_tag(tag: &[u8]) -> Option<[f32; 3]> {
  if tag.len() < 20 || &tag[0..4] != b"XYZ " {
    return None;
  }
  let x = icc_s15fixed16_to_f32(read_be_i32(tag, 8)?);
  let y = icc_s15fixed16_to_f32(read_be_i32(tag, 12)?);
  let z = icc_s15fixed16_to_f32(read_be_i32(tag, 16)?);
  Some([x, y, z])
}

fn icc_parse_trc_tag(tag: &[u8]) -> Option<IccTransferCurve> {
  if tag.len() < 12 {
    return None;
  }
  match &tag[0..4] {
    b"curv" => {
      let count = read_be_u32(tag, 8)? as usize;
      if count == 0 {
        return Some(IccTransferCurve::Gamma(1.0));
      }
      if count == 1 {
        let gamma_u16 = read_be_u16(tag, 12)?;
        let gamma = gamma_u16 as f32 / 256.0;
        return Some(IccTransferCurve::Gamma(gamma));
      }
      let table_bytes = count.checked_mul(2)?;
      let end = 12usize.checked_add(table_bytes)?;
      if end > tag.len() {
        return None;
      }
      let mut table = Vec::with_capacity(count);
      for chunk in tag[12..end].chunks_exact(2) {
        let v = u16::from_be_bytes(chunk.try_into().ok()?);
        table.push(v as f32 / 65535.0);
      }
      Some(IccTransferCurve::Table(table))
    }
    b"para" => {
      if tag.len() < 16 {
        return None;
      }
      let kind = read_be_u16(tag, 8)?;
      // tag[10..12] reserved
      let mut params = [0.0f32; 7];
      let needed: usize = match kind {
        0 => 1,
        1 => 3,
        2 => 4,
        3 => 5,
        4 => 7,
        _ => 0,
      };
      if needed == 0 {
        return None;
      }
      let bytes_needed = 12usize.checked_add(needed.checked_mul(4)?)?;
      if tag.len() < bytes_needed {
        return None;
      }
      for i in 0..needed {
        params[i] = icc_s15fixed16_to_f32(read_be_i32(tag, 12 + i * 4)?);
      }
      let (g, a, b, c, d, e, f) = (
        params[0], params[1], params[2], params[3], params[4], params[5], params[6],
      );
      Some(IccTransferCurve::Parametric {
        kind,
        g,
        a,
        b,
        c,
        d,
        e,
        f,
      })
    }
    _ => None,
  }
}

fn mat3_mul(a: [[f32; 3]; 3], b: [[f32; 3]; 3]) -> [[f32; 3]; 3] {
  let mut out = [[0.0f32; 3]; 3];
  for i in 0..3 {
    for j in 0..3 {
      out[i][j] = a[i][0] * b[0][j] + a[i][1] * b[1][j] + a[i][2] * b[2][j];
    }
  }
  out
}

fn icc_transform_to_srgb(profile: &[u8]) -> Option<IccToSrgbTransform> {
  // Only handle RGB profiles.
  if profile.len() < 40 || &profile[16..20] != b"RGB " {
    return None;
  }

  // Skip conversion for sRGB profiles: the pixel bytes are already in sRGB, and going through a
  // float round-trip can introduce unnecessary quantization differences.
  if icc_profile_description(profile)
    .map(|d| d.to_ascii_lowercase().contains("srgb"))
    .unwrap_or(false)
  {
    return None;
  }

  let r_xyz = icc_tag_slice(profile, b"rXYZ").and_then(icc_parse_xyz_tag)?;
  let g_xyz = icc_tag_slice(profile, b"gXYZ").and_then(icc_parse_xyz_tag)?;
  let b_xyz = icc_tag_slice(profile, b"bXYZ").and_then(icc_parse_xyz_tag)?;

  let r_trc = icc_tag_slice(profile, b"rTRC").and_then(icc_parse_trc_tag)?;
  let g_trc = icc_tag_slice(profile, b"gTRC").and_then(icc_parse_trc_tag)?;
  let b_trc = icc_tag_slice(profile, b"bTRC").and_then(icc_parse_trc_tag)?;

  // Matrix mapping linear profile RGB -> XYZ (PCS, D50).
  let m_profile_to_xyz_d50 = [
    [r_xyz[0], g_xyz[0], b_xyz[0]],
    [r_xyz[1], g_xyz[1], b_xyz[1]],
    [r_xyz[2], g_xyz[2], b_xyz[2]],
  ];

  // Bradford chromatic adaptation matrix: XYZ D50 -> XYZ D65.
  // Values match the standard used by Skia and other browsers.
  let m_d50_to_d65 = [
    [0.9555766, -0.0230393, 0.0631636],
    [-0.0282895, 1.0099416, 0.0210077],
    [0.0122982, -0.0204830, 1.3299098],
  ];

  // XYZ D65 -> linear sRGB.
  let m_xyz_d65_to_srgb = [
    [3.2406, -1.5372, -0.4986],
    [-0.9689, 1.8758, 0.0415],
    [0.0557, -0.2040, 1.0570],
  ];

  let m_profile_to_srgb = mat3_mul(
    m_xyz_d65_to_srgb,
    mat3_mul(m_d50_to_d65, m_profile_to_xyz_d50),
  );

  let build_trc_lut = |curve: &IccTransferCurve| -> [f32; 256] {
    let mut lut = [0.0f32; 256];
    for (idx, slot) in lut.iter_mut().enumerate() {
      let v = idx as f32 / 255.0;
      *slot = curve.apply(v);
    }
    lut
  };

  Some(IccToSrgbTransform {
    m_profile_to_srgb,
    r_trc_lut: build_trc_lut(&r_trc),
    g_trc_lut: build_trc_lut(&g_trc),
    b_trc_lut: build_trc_lut(&b_trc),
  })
}

fn extract_jpeg_icc_profile(bytes: &[u8]) -> Option<Vec<u8>> {
  if bytes.len() < 4 || bytes.get(0..2) != Some(&[0xFF, 0xD8]) {
    return None;
  }

  let mut offset = 2usize;
  let mut segments: Vec<Option<Vec<u8>>> = Vec::new();
  let mut total_segments: Option<u8> = None;

  while offset + 4 <= bytes.len() {
    // Skip padding 0xFF bytes.
    if bytes[offset] != 0xFF {
      break;
    }
    while offset < bytes.len() && bytes[offset] == 0xFF {
      offset += 1;
    }
    if offset >= bytes.len() {
      break;
    }
    let marker = bytes[offset];
    offset += 1;

    // Start of scan / end of image: metadata ends.
    if marker == 0xDA || marker == 0xD9 {
      break;
    }
    // Standalone markers without a length (rare before SOS).
    if marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
      continue;
    }

    let seg_len = read_be_u16(bytes, offset)? as usize;
    offset = offset.checked_add(2)?;
    if seg_len < 2 {
      return None;
    }
    let payload_len = seg_len - 2;
    let payload_end = offset.checked_add(payload_len)?;
    if payload_end > bytes.len() {
      return None;
    }
    let payload = &bytes[offset..payload_end];
    offset = payload_end;

    // APP2 = ICC_PROFILE
    if marker == 0xE2 && payload.len() >= 14 && payload.starts_with(b"ICC_PROFILE\0") {
      let seq = payload[12];
      let count = payload[13];
      if seq == 0 || count == 0 {
        continue;
      }
      if total_segments.is_none() {
        total_segments = Some(count);
        segments.resize(count as usize, None);
      } else if total_segments != Some(count) {
        // Inconsistent ICC segment counts; ignore.
        return None;
      }
      if let Some(slot) = segments.get_mut(seq.saturating_sub(1) as usize) {
        *slot = Some(payload[14..].to_vec());
      }
    }
  }

  let count = total_segments? as usize;
  if count == 0 || segments.len() != count || segments.iter().any(|s| s.is_none()) {
    return None;
  }
  let mut out = Vec::new();
  for seg in segments.into_iter().flatten() {
    out.extend_from_slice(&seg);
  }
  Some(out)
}

#[cfg(feature = "avif")]
enum AvifDecodeError {
  Timeout(RenderError),
  Image(image::ImageError),
}

#[cfg(feature = "avif")]
impl From<RenderError> for AvifDecodeError {
  fn from(err: RenderError) -> Self {
    Self::Timeout(err)
  }
}

#[cfg(feature = "avif")]
impl From<image::ImageError> for AvifDecodeError {
  fn from(err: image::ImageError) -> Self {
    Self::Image(err)
  }
}

#[derive(Clone)]
struct CacheEntry<V> {
  value: V,
  bytes: usize,
}

struct SizedLruCache<K, V> {
  inner: LruCache<K, CacheEntry<V>>,
  max_entries: Option<usize>,
  max_bytes: Option<usize>,
  current_bytes: usize,
}

impl<K: Eq + Hash, V> SizedLruCache<K, V> {
  fn new(max_entries: usize, max_bytes: usize) -> Self {
    Self {
      inner: LruCache::unbounded(),
      max_entries: (max_entries > 0).then_some(max_entries),
      max_bytes: (max_bytes > 0).then_some(max_bytes),
      current_bytes: 0,
    }
  }

  fn len(&self) -> usize {
    self.inner.len()
  }

  fn get_cloned<Q>(&mut self, key: &Q) -> Option<V>
  where
    V: Clone,
    K: Borrow<Q>,
    Q: Hash + Eq + ?Sized,
  {
    self.inner.get(key).map(|entry| entry.value.clone())
  }

  fn take<Q>(&mut self, key: &Q) -> Option<V>
  where
    K: Borrow<Q>,
    Q: Hash + Eq + ?Sized,
  {
    self.inner.pop(key).map(|entry| {
      self.current_bytes = self.current_bytes.saturating_sub(entry.bytes);
      entry.value
    })
  }

  fn clear(&mut self) {
    // `lru` versions vary in whether `clear()` is implemented on `LruCache`, so reset by
    // reinitializing the internal map.
    self.inner = LruCache::unbounded();
    self.current_bytes = 0;
  }

  fn insert(&mut self, key: K, value: V, bytes: usize) {
    if let Some(entry) = self.inner.pop(&key) {
      self.current_bytes = self.current_bytes.saturating_sub(entry.bytes);
    }
    self.inner.put(key, CacheEntry { value, bytes });
    self.current_bytes = self.current_bytes.saturating_add(bytes);
    self.evict_if_needed();
  }

  fn evict_if_needed(&mut self) {
    while self
      .max_entries
      .is_some_and(|limit| self.inner.len() > limit)
      || self
        .max_bytes
        .is_some_and(|limit| self.current_bytes > limit)
    {
      if let Some((_key, entry)) = self.inner.pop_lru() {
        self.current_bytes = self.current_bytes.saturating_sub(entry.bytes);
      } else {
        break;
      }
    }
  }

  fn current_bytes(&self) -> usize {
    self.current_bytes
  }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
struct SvgPixmapKey {
  hash: u64,
  url_hash: u64,
  len: usize,
  width: u32,
  height: u32,
  device_pixel_ratio_bits: u32,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
struct SvgPreprocessKey {
  hash: u64,
  url_hash: u64,
  len: usize,
}

#[inline]
fn f32_to_canonical_bits(value: f32) -> u32 {
  if value == 0.0 {
    0.0f32.to_bits()
  } else {
    value.to_bits()
  }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
struct RasterPixmapKey {
  url_hash: u64,
  len: usize,
  orientation: OrientationTransform,
  decorative: bool,
  target_width: u32,
  target_height: u32,
  quality_bits: u8,
}

fn svg_preprocess_key(svg_content: &str, url: &str) -> SvgPreprocessKey {
  let mut content_hasher = DefaultHasher::new();
  svg_content.hash(&mut content_hasher);
  let mut url_hasher = DefaultHasher::new();
  url.hash(&mut url_hasher);
  SvgPreprocessKey {
    hash: content_hasher.finish(),
    url_hash: url_hasher.finish(),
    len: svg_content.len(),
  }
}

fn raster_pixmap_key(
  url: &str,
  orientation: OrientationTransform,
  decorative: bool,
  target_width: u32,
  target_height: u32,
  quality_bits: u8,
) -> RasterPixmapKey {
  let mut url_hasher = DefaultHasher::new();
  url.hash(&mut url_hasher);
  RasterPixmapKey {
    url_hash: url_hasher.finish(),
    len: url.len(),
    orientation,
    decorative,
    target_width,
    target_height,
    quality_bits,
  }
}

fn raster_pixmap_quality_bits(quality: FilterQuality) -> u8 {
  match quality {
    FilterQuality::Nearest => 0,
    FilterQuality::Bilinear => 1,
    // tiny-skia currently exposes a small fixed set of qualities. Store a conservative default so
    // future additions don't break hashing behaviour.
    _ => 255,
  }
}

fn raster_pixmap_full_key(
  url: &str,
  orientation: OrientationTransform,
  decorative: bool,
) -> RasterPixmapKey {
  raster_pixmap_key(url, orientation, decorative, 0, 0, 0)
}

fn svg_pixmap_key(
  svg_content: &str,
  url: &str,
  device_pixel_ratio: f32,
  width: u32,
  height: u32,
) -> SvgPixmapKey {
  let mut content_hasher = DefaultHasher::new();
  svg_content.hash(&mut content_hasher);
  let mut url_hasher = DefaultHasher::new();
  url.hash(&mut url_hasher);
  SvgPixmapKey {
    hash: content_hasher.finish(),
    url_hash: url_hasher.finish(),
    len: svg_content.len(),
    width,
    height,
    device_pixel_ratio_bits: f32_to_canonical_bits(device_pixel_ratio),
  }
}

fn inline_svg_cache_key(svg_content: &str) -> String {
  let mut hasher = DefaultHasher::new();
  svg_content.hash(&mut hasher);
  format!("inline-svg:{:016x}:{}", hasher.finish(), svg_content.len())
}

fn escape_xml_attr_value(value: &str) -> Cow<'_, str> {
  if !value.contains('&')
    && !value.contains('<')
    && !value.contains('>')
    && !value.contains('"')
    && !value.contains('\'')
  {
    return Cow::Borrowed(value);
  }
  let mut out = String::with_capacity(value.len());
  for ch in value.chars() {
    match ch {
      '&' => out.push_str("&amp;"),
      '<' => out.push_str("&lt;"),
      '>' => out.push_str("&gt;"),
      '"' => out.push_str("&quot;"),
      '\'' => out.push_str("&apos;"),
      other => out.push(other),
    }
  }
  Cow::Owned(out)
}

fn escape_xml_text_value(value: &str) -> Cow<'_, str> {
  if !value.contains('&') && !value.contains('<') {
    return Cow::Borrowed(value);
  }
  let mut out = String::with_capacity(value.len());
  for ch in value.chars() {
    match ch {
      '&' => out.push_str("&amp;"),
      '<' => out.push_str("&lt;"),
      other => out.push(other),
    }
  }
  Cow::Owned(out)
}

fn append_url_fragment_if_missing<'a>(base_url: &'a str, requested_url: &str) -> Cow<'a, str> {
  if base_url.contains('#') {
    return Cow::Borrowed(base_url);
  }
  let Some((_, fragment)) = requested_url.split_once('#') else {
    return Cow::Borrowed(base_url);
  };
  if fragment.is_empty() {
    return Cow::Borrowed(base_url);
  }

  let mut out = String::with_capacity(base_url.len().saturating_add(fragment.len() + 1));
  out.push_str(base_url);
  out.push('#');
  out.push_str(fragment);
  Cow::Owned(out)
}

fn referrer_url_for_svg_importer<'a>(
  importer_url: &'a str,
  ctx: Option<&'a ResourceContext>,
) -> Option<&'a str> {
  if Url::parse(importer_url).is_ok() {
    return Some(importer_url);
  }
  ctx.and_then(|ctx| ctx.document_url.as_deref())
}

#[derive(Debug, Clone)]
struct SvgUseInlineElement {
  tag_name: String,
  range: std::ops::Range<usize>,
  inner_range: Option<std::ops::Range<usize>>,
  view_box: Option<String>,
  preserve_aspect_ratio: Option<String>,
  namespace_attrs: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
struct SvgUseInlineSprite {
  content: String,
  by_id: HashMap<String, SvgUseInlineElement>,
  /// Root element of the sprite document, used when `<use href="icon.svg">` has no fragment.
  root: Option<SvgUseInlineElement>,
  /// Serialized SVG elements keyed by `id`, used to inline `<defs>` dependencies when expanding an
  /// external `<use href="sprite.svg#id">`.
  ///
  /// Fragments are patched to include the sprite's root `xmlns*` declarations so they can be
  /// reparsed as standalone XML documents by `defs_injection_for_svg_fragment`.
  id_defs: HashMap<String, String>,
  /// `xmlns*` attributes copied from the sprite root (plus a default `xmlns` fallback) serialized
  /// as ` name="value"` pairs.
  ///
  /// Used when wrapping non-root fragments in `<svg ...>` for dependency detection.
  xmlns_attrs: String,
  /// Cache of `<defs>...</defs>` injection strings computed per referenced id.
  defs_injection_cache: HashMap<String, Option<String>>,
}

#[derive(Clone)]
struct SvgCachedDataUrl {
  data_url: Arc<str>,
  bytes_len: usize,
  final_url: Option<String>,
}

#[derive(Clone)]
struct SvgCachedSprite {
  sprite: Arc<SvgUseInlineSprite>,
  final_url: Option<String>,
}

#[derive(Clone)]
enum SvgSubresourceCacheValue {
  ImageDataUrl(SvgCachedDataUrl),
  Sprite(SvgCachedSprite),
}

type SvgSubresourceCache = Arc<Mutex<SizedLruCache<String, SvgSubresourceCacheValue>>>;

fn estimate_svg_subresource_cache_entry_bytes(
  key: &str,
  value: &SvgSubresourceCacheValue,
) -> usize {
  let mut bytes = key
    .len()
    .saturating_add(std::mem::size_of::<SvgSubresourceCacheValue>());
  match value {
    SvgSubresourceCacheValue::ImageDataUrl(entry) => {
      bytes = bytes.saturating_add(std::mem::size_of::<SvgCachedDataUrl>());
      bytes = bytes.saturating_add(entry.data_url.len());
      bytes = bytes.saturating_add(entry.final_url.as_ref().map(|s| s.len()).unwrap_or(0));
      bytes = bytes.saturating_add(std::mem::size_of::<Arc<str>>());
    }
    SvgSubresourceCacheValue::Sprite(entry) => {
      bytes = bytes.saturating_add(std::mem::size_of::<SvgCachedSprite>());
      bytes = bytes.saturating_add(entry.final_url.as_ref().map(|s| s.len()).unwrap_or(0));
      bytes = bytes.saturating_add(std::mem::size_of::<Arc<SvgUseInlineSprite>>());

      let sprite = entry.sprite.as_ref();
      bytes = bytes.saturating_add(std::mem::size_of::<SvgUseInlineSprite>());
      bytes = bytes.saturating_add(sprite.content.len());
      bytes = bytes.saturating_add(sprite.xmlns_attrs.len());
      bytes = bytes.saturating_add(std::mem::size_of::<HashMap<String, SvgUseInlineElement>>());
      bytes = sprite.by_id.iter().fold(bytes, |acc, (id, el)| {
        let mut acc = acc;
        acc = acc.saturating_add(id.len());
        acc = acc.saturating_add(el.tag_name.len());
        acc = acc.saturating_add(el.view_box.as_ref().map(|s| s.len()).unwrap_or(0));
        acc = acc.saturating_add(
          el.preserve_aspect_ratio
            .as_ref()
            .map(|s| s.len())
            .unwrap_or(0),
        );
        acc = acc.saturating_add(std::mem::size_of::<SvgUseInlineElement>());
        acc
      });
      if let Some(root) = sprite.root.as_ref() {
        bytes = bytes.saturating_add(root.tag_name.len());
        bytes = bytes.saturating_add(root.view_box.as_ref().map(|s| s.len()).unwrap_or(0));
        bytes = bytes.saturating_add(
          root
            .preserve_aspect_ratio
            .as_ref()
            .map(|s| s.len())
            .unwrap_or(0),
        );
        bytes = bytes.saturating_add(std::mem::size_of::<SvgUseInlineElement>());
      }
      bytes = bytes.saturating_add(std::mem::size_of::<HashMap<String, String>>());
      bytes = sprite.id_defs.iter().fold(bytes, |acc, (id, frag)| {
        acc.saturating_add(id.len()).saturating_add(frag.len())
      });
      bytes = bytes.saturating_add(std::mem::size_of::<HashMap<String, Option<String>>>());
      bytes = sprite
        .defs_injection_cache
        .iter()
        .fold(bytes, |acc, (id, injection)| {
          acc
            .saturating_add(id.len())
            .saturating_add(injection.as_ref().map(|s| s.len()).unwrap_or(0))
        });
    }
  }
  bytes
}

fn fetch_destination_cache_tag(dest: FetchDestination) -> &'static str {
  match dest {
    FetchDestination::Document => "document",
    FetchDestination::DocumentNoUser => "document-no-user",
    FetchDestination::Iframe => "iframe",
    FetchDestination::Style => "style",
    FetchDestination::StyleCors => "style-cors",
    FetchDestination::Script => "script",
    FetchDestination::ScriptCors => "script-cors",
    FetchDestination::Image => "image",
    FetchDestination::ImageCors => "image-cors",
    FetchDestination::Video => "video",
    FetchDestination::VideoCors => "video-cors",
    FetchDestination::Audio => "audio",
    FetchDestination::AudioCors => "audio-cors",
    FetchDestination::Font => "font",
    FetchDestination::Other => "other",
    FetchDestination::Fetch => "fetch",
  }
}

fn fetch_credentials_mode_cache_tag(mode: FetchCredentialsMode) -> &'static str {
  match mode {
    FetchCredentialsMode::Omit => "omit",
    FetchCredentialsMode::SameOrigin => "same-origin",
    FetchCredentialsMode::Include => "include",
  }
}

fn svg_subresource_cache_key(kind: &str, request: &FetchRequest<'_>) -> String {
  // svg_subresource_cache sits above ResourceFetcher and must be partitioned by request context.
  // Otherwise, we could reuse a sprite/data URL fetched under a different referrer/origin/
  // credentials policy.
  //
  // Keep the key bounded by hashing long strings like the referrer URL.
  let resolved_url = strip_url_fragment(request.url);

  let referrer_hash = request.referrer_url.map(|url| {
    let mut hasher = DefaultHasher::new();
    url.hash(&mut hasher);
    (hasher.finish(), url.len())
  });

  let client_origin = request.client_origin.map(|origin| origin.to_string());

  let effective_referrer_policy = match request.referrer_policy {
    ReferrerPolicy::EmptyString => ReferrerPolicy::CHROMIUM_DEFAULT,
    other => other,
  };

  let mut key = String::new();
  key.push_str(kind);
  key.push(':');
  key.push_str(resolved_url.as_ref());

  key.push_str("@@dest=");
  key.push_str(fetch_destination_cache_tag(request.destination));

  key.push_str("@@cred=");
  key.push_str(fetch_credentials_mode_cache_tag(request.credentials_mode));

  key.push_str("@@client_origin=");
  match client_origin.as_deref() {
    Some(origin) => key.push_str(origin),
    None => key.push_str("none"),
  }

  key.push_str("@@referrer=");
  match referrer_hash {
    Some((hash, len)) => {
      key.push_str(&format!("{hash:016x}:{len}"));
    }
    None => key.push_str("none"),
  }

  key.push_str("@@referrer_policy=");
  key.push_str(effective_referrer_policy.as_str());

  key
}

fn svg_xmlns_attributes_for_node(node: roxmltree::Node<'_, '_>) -> Vec<(String, String)> {
  // Namespace declarations in SVG sprites are often on the root element (e.g.
  // `<svg:svg xmlns:svg="http://www.w3.org/2000/svg">`). When we splice only a subtree into the
  // parent document, those declarations can go out of scope and prefixed element/attribute names
  // become invalid/ignored. Collect all `xmlns` declarations that are in scope for the referenced
  // element so we can re-declare them on the injected wrapper.
  const XMLNS_NS: &str = "http://www.w3.org/2000/xmlns/";

  let mut out = Vec::new();
  let mut seen: HashSet<String> = HashSet::new();

  for ancestor in node.ancestors().filter(|n| n.is_element()) {
    for attr in ancestor.attributes() {
      let full_name: Option<Cow<'_, str>> = match attr.name() {
        "xmlns" => Some(Cow::Borrowed("xmlns")),
        name if name.starts_with("xmlns:") => Some(Cow::Borrowed(name)),
        name if attr.namespace() == Some(XMLNS_NS) => {
          // `roxmltree` can expose xmlns declarations via the XMLNS namespace with the prefix
          // stripped (e.g. `xmlns:svg` becomes `{XMLNS_NS}svg`). Preserve the original `xmlns:*`
          // spelling when we later re-emit it as raw markup.
          if name.is_empty() {
            None
          } else {
            Some(Cow::Owned(format!("xmlns:{name}")))
          }
        }
        _ => None,
      };

      let Some(full_name) = full_name else {
        continue;
      };

      if seen.contains(full_name.as_ref()) {
        continue;
      }
      seen.insert(full_name.to_string());
      out.push((full_name.to_string(), attr.value().to_string()));
    }
  }

  out
}

/// Best-effort preprocessor that expands external SVG `<use>` references by fetching the
/// referenced SVG and inlining the referenced element.
///
/// This supports common sprite patterns (`<use href="sprite.svg#id">`) as well as SVG2 external
/// `<use>` without a fragment (`<use href="icon.svg">`), which references the external document's
/// root element.
///
/// This is intentionally narrow: it exists because `usvg`/`resvg` do not fetch HTTP(S) external
/// `<use>` targets by default, which causes common SVG sprite patterns to silently disappear.
fn inline_svg_use_references<'a>(
  svg_content: &'a str,
  svg_url: &str,
  fetcher: &dyn ResourceFetcher,
  ctx: Option<&ResourceContext>,
  subresource_cache: Option<&SvgSubresourceCache>,
) -> Result<Cow<'a, str>> {
  // Avoid parsing unless it looks like we might have `<use>` references.
  // Note: inline SVG content in HTML is sometimes namespaced (e.g. `<svg:use>`). The cheap string
  // check is intentionally conservative to avoid parsing most SVGs while still catching prefixed
  // `<use>` tags.
  if !svg_content.contains("<use") && !svg_content.contains(":use") {
    return Ok(Cow::Borrowed(svg_content));
  }

  const MAX_USE_EXPANSIONS: usize = 128;
  const MAX_INJECTED_BYTES: usize = 512 * 1024;

  check_root(RenderStage::Paint).map_err(Error::Render)?;

  let importer_referrer_url = referrer_url_for_svg_importer(svg_url, ctx);

  let svg_for_parse = svg_markup_for_roxmltree(svg_content);
  let doc = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    roxmltree::Document::parse(svg_for_parse.as_ref())
  })) {
    Ok(Ok(doc)) => doc,
    // Best-effort: if the SVG doesn't parse, don't fail the entire image decode.
    Ok(Err(_)) | Err(_) => return Ok(Cow::Borrowed(svg_content)),
  };

  let mut deadline_counter = 0usize;
  let mut sprite_cache: HashMap<String, SvgUseInlineSprite> = HashMap::new();
  let mut replacements: Vec<(std::ops::Range<usize>, String)> = Vec::new();
  let mut injected_bytes = 0usize;
  let mut expansions = 0usize;

  for node in doc.descendants().filter(|n| n.is_element()) {
    check_root_periodic(
      &mut deadline_counter,
      IMAGE_DECODE_DEADLINE_STRIDE,
      RenderStage::Paint,
    )
    .map_err(Error::Render)?;

    if node.tag_name().name() != "use" {
      continue;
    }

    if svg_node_has_display_none(node) {
      continue;
    }

    let mut href = None;
    for attr in node.attributes() {
      let name = attr.name();
      if name.eq_ignore_ascii_case("href")
        || name
          .rsplit_once(':')
          .is_some_and(|(_, local)| local.eq_ignore_ascii_case("href"))
      {
        href = Some(attr.value());
        break;
      }
    }
    if href.is_none() {
      for attr in node.attributes() {
        if attr.name() == "xlink:href" {
          href = Some(attr.value());
          break;
        }
      }
    }
    let href = href.map(trim_ascii_whitespace).filter(|v| !v.is_empty());

    let Some(href) = href else {
      continue;
    };

    let (href_url_part, fragment) = match href.split_once('#') {
      Some((href_url_part, fragment)) => {
        let href_url_part = trim_ascii_whitespace(href_url_part);
        let fragment = trim_ascii_whitespace(fragment);

        // Internal-only references (`#id`) are handled by usvg; we only patch external sprite uses.
        if href_url_part.is_empty() || href_url_part.starts_with('#') || fragment.is_empty() {
          continue;
        }
        (href_url_part, Some(fragment))
      }
      None => {
        let href_url_part = trim_ascii_whitespace(href);
        if href_url_part.is_empty() || href_url_part.starts_with('#') {
          continue;
        }
        (href_url_part, None)
      }
    };

    // Resolve the URL without the fragment for fetching.
    let xml_base_chain = svg_xml_base_chain_for_node(node);
    let resolve_with_base = |base: Option<&str>| {
      apply_svg_xml_base_chain(base, &xml_base_chain)
        .and_then(|base| resolve_against_base(&base, href_url_part))
    };
    let Some(resolved_base) = resolve_with_base(Some(svg_url))
      .or_else(|| {
        ctx
          .and_then(|ctx| ctx.document_url.as_deref())
          .and_then(|base| resolve_with_base(Some(base)))
      })
      .or_else(|| Url::parse(href_url_part).ok().map(|u| u.to_string()))
    else {
      continue;
    };
    let Ok(mut resolved_url) = Url::parse(&resolved_base) else {
      continue;
    };
    resolved_url.set_fragment(None);
    let resolved_url = resolved_url.to_string();

    if let Some(ctx) = ctx {
      if let Err(err) = ctx.check_allowed(ResourceKind::Image, &resolved_url) {
        return Err(Error::Image(ImageError::LoadFailed {
          url: resolved_url.clone(),
          reason: err.reason,
        }));
      }
    }

    if !sprite_cache.contains_key(&resolved_url) {
      if let Some(shared) = subresource_cache {
        let mut req = FetchRequest::new(&resolved_url, FetchDestination::Image);
        if let Some(ctx) = ctx {
          if let Some(origin) = ctx.policy.document_origin.as_ref() {
            req = req.with_client_origin(origin);
          }
          if let Some(referrer_url) = importer_referrer_url {
            req = req.with_referrer_url(referrer_url);
          }
          req = req.with_referrer_policy(ctx.referrer_policy);
        }
        let cache_key = svg_subresource_cache_key("svg-sprite", &req);
        if let Ok(mut cache) = shared.lock() {
          if let Some(SvgSubresourceCacheValue::Sprite(entry)) = cache.get_cloned(&cache_key) {
            if let Some(ctx) = ctx {
              if let Err(err) = ctx.check_allowed_with_final(
                ResourceKind::Image,
                &resolved_url,
                entry.final_url.as_deref(),
              ) {
                return Err(Error::Image(ImageError::LoadFailed {
                  url: resolved_url.clone(),
                  reason: err.reason,
                }));
              }
            }
            sprite_cache.insert(resolved_url.clone(), (*entry.sprite).clone());
          }
        }
      }
    }

    if !sprite_cache.contains_key(&resolved_url) {
      check_root(RenderStage::Paint).map_err(Error::Render)?;

      let mut req = FetchRequest::new(&resolved_url, FetchDestination::Image);
      if let Some(ctx) = ctx {
        if let Some(origin) = ctx.policy.document_origin.as_ref() {
          req = req.with_client_origin(origin);
        }
        if let Some(referrer_url) = importer_referrer_url {
          req = req.with_referrer_url(referrer_url);
        }
        req = req.with_referrer_policy(ctx.referrer_policy);
      }

      let res = match fetcher.fetch_with_request(req) {
        Ok(res) => res,
        // Best-effort: keep the `<use>` element intact if the sprite fetch fails.
        //
        // Render-control failures (deadline/cancel/budgets) are not ordinary fetch errors and must
        // abort the render.
        Err(err) => {
          if matches!(&err, Error::Render(_)) {
            return Err(err);
          }
          continue;
        }
      };
      if let Some(ctx) = ctx {
        if let Err(err) =
          ctx.check_allowed_with_final(ResourceKind::Image, &resolved_url, res.final_url.as_deref())
        {
          return Err(Error::Image(ImageError::LoadFailed {
            url: resolved_url.clone(),
            reason: err.reason,
          }));
        }
      }
      if let Err(_) = ensure_http_success(&res, &resolved_url)
        .and_then(|()| ensure_image_mime_sane(&res, &resolved_url))
      {
        continue;
      }

      let sprite_final_url = res.final_url.clone();
      let sprite_base_url = sprite_final_url
        .clone()
        .unwrap_or_else(|| resolved_url.clone());

      let mut sprite_text = {
        let bytes = res.bytes;
        if bytes.len() >= 2 && bytes[0] == 0x1F && bytes[1] == 0x8B {
          let mut decoder = GzDecoder::new(bytes.as_slice());
          let mut out = Vec::new();
          let mut buf = [0u8; 8192];
          let mut decompression_deadline_counter = 0usize;
          let mut ok = true;
          loop {
            check_root_periodic(&mut decompression_deadline_counter, 32, RenderStage::Paint)
              .map_err(Error::Render)?;
            let n = match decoder.read(&mut buf) {
              Ok(n) => n,
              Err(_) => {
                // Best-effort: keep the `<use>` intact if sprite decompression fails.
                ok = false;
                break;
              }
            };
            if n == 0 {
              break;
            }
            if out.len().saturating_add(n) > MAX_SVGZ_DECOMPRESSED_BYTES {
              // Best-effort: treat oversized sprites like a parse failure.
              ok = false;
              break;
            }
            out.extend_from_slice(&buf[..n]);
          }
          if !ok {
            continue;
          }
          match String::from_utf8(out) {
            Ok(text) => text,
            Err(_) => continue,
          }
        } else {
          match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(_) => continue,
          }
        }
      };

      // Inline any external raster images referenced by the sprite itself so that when we later
      // embed a single `<symbol>`/`<g>` fragment into the parent document, nested `<image>`
      // references continue to resolve relative to the sprite URL (not the parent document URL).
      sprite_text = inline_svg_image_references(
        &sprite_text,
        &sprite_base_url,
        fetcher,
        ctx,
        subresource_cache,
      )?
      .into_owned();
      sprite_text =
        inline_svg_style_imports(&sprite_text, &sprite_base_url, fetcher, ctx)?.into_owned();

      let sprite_for_parse = svg_markup_for_roxmltree(&sprite_text);
      let sprite_doc = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        roxmltree::Document::parse(sprite_for_parse.as_ref())
      })) {
        Ok(Ok(doc)) => doc,
        Ok(Err(_)) | Err(_) => continue,
      };

      let sprite_root = sprite_doc.root_element();

      let root = Some({
        let tag_name = sprite_root.tag_name().name().to_string();
        let range = sprite_root.range();
        let mut inner_start: Option<usize> = None;
        let mut inner_end: Option<usize> = None;
        for child in sprite_root.children() {
          let r = child.range();
          inner_start = Some(inner_start.map_or(r.start, |s| s.min(r.start)));
          inner_end = Some(inner_end.map_or(r.end, |e| e.max(r.end)));
        }
        let inner_range = match (inner_start, inner_end) {
          (Some(s), Some(e)) if s <= e => Some(s..e),
          _ => None,
        };

        SvgUseInlineElement {
          tag_name,
          range,
          inner_range,
          view_box: sprite_root.attribute("viewBox").map(|v| v.to_string()),
          preserve_aspect_ratio: sprite_root
            .attribute("preserveAspectRatio")
            .map(|v| v.to_string()),
          namespace_attrs: svg_xmlns_attributes_for_node(sprite_root),
        }
      });

      // Collect root namespace declarations so fragments can be reparsed (standalone) for
      // `<defs>` dependency detection.
      let mut xmlns_attrs_list: Vec<(String, String)> = Vec::new();
      let mut xmlns_attrs = String::new();
      let mut had_default_xmlns = false;
      for attr in sprite_root.attributes() {
        let name = attr.name();
        if !name.starts_with("xmlns") {
          continue;
        }
        if name == "xmlns" {
          had_default_xmlns = true;
        }
        let value = attr.value().to_string();
        xmlns_attrs_list.push((name.to_string(), value.clone()));
        xmlns_attrs.push(' ');
        xmlns_attrs.push_str(name);
        xmlns_attrs.push_str("=\"");
        xmlns_attrs.push_str(&escape_xml_attr_value(&value));
        xmlns_attrs.push('"');
      }
      if !had_default_xmlns {
        xmlns_attrs_list.push((
          "xmlns".to_string(),
          "http://www.w3.org/2000/svg".to_string(),
        ));
        xmlns_attrs.push_str(" xmlns=\"http://www.w3.org/2000/svg\"");
      }

      let mut by_id = HashMap::new();
      for sprite_node in sprite_doc.descendants().filter(|n| n.is_element()) {
        check_root_periodic(
          &mut deadline_counter,
          IMAGE_DECODE_DEADLINE_STRIDE,
          RenderStage::Paint,
        )
        .map_err(Error::Render)?;

        let Some(id) = sprite_node
          .attribute("id")
          .map(trim_ascii_whitespace)
          .filter(|id| !id.is_empty())
        else {
          continue;
        };

        // Record the first element for a given id (SVG ids should be unique).
        by_id.entry(id.to_string()).or_insert_with(|| {
          let tag_name = sprite_node.tag_name().name().to_string();
          let range = sprite_node.range();
          let mut inner_start: Option<usize> = None;
          let mut inner_end: Option<usize> = None;
          for child in sprite_node.children() {
            let r = child.range();
            inner_start = Some(inner_start.map_or(r.start, |s| s.min(r.start)));
            inner_end = Some(inner_end.map_or(r.end, |e| e.max(r.end)));
          }
          let inner_range = match (inner_start, inner_end) {
            (Some(s), Some(e)) if s <= e => Some(s..e),
            _ => None,
          };

          SvgUseInlineElement {
            tag_name,
            range,
            inner_range,
            view_box: sprite_node.attribute("viewBox").map(|v| v.to_string()),
            preserve_aspect_ratio: sprite_node
              .attribute("preserveAspectRatio")
              .map(|v| v.to_string()),
            namespace_attrs: svg_xmlns_attributes_for_node(sprite_node),
          }
        });
      }

      fn inject_xmlns_attrs_into_svg_fragment<'a>(
        fragment: &'a str,
        xmlns_attrs: &[(String, String)],
      ) -> Cow<'a, str> {
        if fragment.is_empty() || !fragment.starts_with('<') || xmlns_attrs.is_empty() {
          return Cow::Borrowed(fragment);
        }

        let Some(tag_end) = find_xml_start_tag_end(fragment, 0, fragment.len()) else {
          return Cow::Borrowed(fragment);
        };
        if tag_end > fragment.len() || tag_end == 0 {
          return Cow::Borrowed(fragment);
        }

        // Find the position before `>` (or before `/>` for self-closing tags).
        let mut insert_pos = tag_end.saturating_sub(1);
        if insert_pos == 0 || fragment.as_bytes().get(insert_pos) != Some(&b'>') {
          return Cow::Borrowed(fragment);
        }
        if fragment.as_bytes().get(insert_pos.saturating_sub(1)) == Some(&b'/') {
          insert_pos = insert_pos.saturating_sub(1);
        }

        // Collect existing attribute names in the root start tag so we don't emit duplicates.
        let bytes = fragment.as_bytes();
        let mut i = 0usize;
        if bytes.get(i) == Some(&b'<') {
          i += 1;
        }
        while i < insert_pos && bytes[i].is_ascii_whitespace() {
          i += 1;
        }
        while i < insert_pos
          && !bytes[i].is_ascii_whitespace()
          && bytes[i] != b'>'
          && bytes[i] != b'/'
        {
          i += 1;
        }

        let mut existing: std::collections::HashSet<&str> = std::collections::HashSet::new();
        while i < insert_pos {
          while i < insert_pos && bytes[i].is_ascii_whitespace() {
            i += 1;
          }
          if i >= insert_pos || bytes[i] == b'>' || bytes[i] == b'/' {
            break;
          }

          let name_start = i;
          while i < insert_pos
            && !bytes[i].is_ascii_whitespace()
            && bytes[i] != b'='
            && bytes[i] != b'>'
            && bytes[i] != b'/'
          {
            i += 1;
          }
          let name_end = i;
          if name_end == name_start {
            i = i.saturating_add(1);
            continue;
          }
          if let Some(name) = fragment.get(name_start..name_end) {
            existing.insert(name);
          }

          while i < insert_pos && bytes[i].is_ascii_whitespace() {
            i += 1;
          }
          if i >= insert_pos || bytes[i] != b'=' {
            continue;
          }
          i += 1;
          while i < insert_pos && bytes[i].is_ascii_whitespace() {
            i += 1;
          }
          if i >= insert_pos {
            break;
          }

          if bytes[i] == b'"' || bytes[i] == b'\'' {
            let quote = bytes[i];
            i += 1;
            while i < insert_pos && bytes[i] != quote {
              i += 1;
            }
            if i < insert_pos {
              i += 1;
            }
          } else {
            while i < insert_pos
              && !bytes[i].is_ascii_whitespace()
              && bytes[i] != b'>'
              && bytes[i] != b'/'
            {
              i += 1;
            }
          }
        }

        let mut missing: Vec<(&str, &str)> = Vec::new();
        for pair in xmlns_attrs.iter() {
          let name = pair.0.as_str();
          let value = pair.1.as_str();
          if !existing.contains(name) {
            missing.push((name, value));
          }
        }
        if missing.is_empty() {
          return Cow::Borrowed(fragment);
        }

        let mut out = String::with_capacity(fragment.len().saturating_add(xmlns_attrs.len() * 16));
        out.push_str(fragment.get(..insert_pos).unwrap_or_default());
        for (name, value) in missing {
          out.push(' ');
          out.push_str(name);
          out.push_str("=\"");
          out.push_str(&escape_xml_attr_value(value));
          out.push('"');
        }
        out.push_str(fragment.get(insert_pos..).unwrap_or_default());
        Cow::Owned(out)
      }

      let mut id_defs =
        crate::paint::svg_mask_image::collect_svg_id_defs_from_svg_document(&sprite_text);
      if !id_defs.is_empty() {
        for fragment in id_defs.values_mut() {
          let patched = inject_xmlns_attrs_into_svg_fragment(fragment, &xmlns_attrs_list);
          if let Cow::Owned(owned) = patched {
            *fragment = owned;
          }
        }
      }

      let sprite = SvgUseInlineSprite {
        content: sprite_text,
        by_id,
        root,
        id_defs,
        xmlns_attrs,
        defs_injection_cache: HashMap::new(),
      };
      if let Some(shared) = subresource_cache {
        let mut req = FetchRequest::new(&resolved_url, FetchDestination::Image);
        if let Some(ctx) = ctx {
          if let Some(origin) = ctx.policy.document_origin.as_ref() {
            req = req.with_client_origin(origin);
          }
          if let Some(referrer_url) = importer_referrer_url {
            req = req.with_referrer_url(referrer_url);
          }
          req = req.with_referrer_policy(ctx.referrer_policy);
        }
        let cache_key = svg_subresource_cache_key("svg-sprite", &req);
        let entry = SvgCachedSprite {
          sprite: Arc::new(sprite.clone()),
          final_url: sprite_final_url.clone(),
        };
        let value = SvgSubresourceCacheValue::Sprite(entry);
        let bytes = estimate_svg_subresource_cache_entry_bytes(&cache_key, &value);
        if let Ok(mut cache) = shared.lock() {
          cache.insert(cache_key, value, bytes);
        }
      }
      sprite_cache.insert(resolved_url.clone(), sprite);
    }

    let Some(sprite) = sprite_cache.get_mut(&resolved_url) else {
      continue;
    };

    let element = match fragment {
      Some(fragment) => sprite.by_id.get(fragment),
      None => sprite.root.as_ref(),
    };
    let Some(element) = element else {
      continue;
    };
    let cache_key = fragment.unwrap_or("");

    let mut wrapper_defs_injection: Option<&str> = None;
    let referenced_markup = if element.tag_name == "symbol" {
      let inner = element
        .inner_range
        .as_ref()
        .and_then(|r| sprite.content.get(r.clone()))
        .unwrap_or_default();

      let defs_injection = if sprite.id_defs.is_empty() {
        None
      } else if let Some(cached) = sprite.defs_injection_cache.get(cache_key) {
        cached.as_deref()
      } else {
        let mut wrapped = String::with_capacity(
          "<svg".len() + sprite.xmlns_attrs.len() + ">".len() + inner.len() + "</svg>".len(),
        );
        wrapped.push_str("<svg");
        wrapped.push_str(&sprite.xmlns_attrs);
        wrapped.push('>');
        wrapped.push_str(inner);
        wrapped.push_str("</svg>");

        let injection =
          crate::paint::svg_mask_image::defs_injection_for_svg_fragment(&sprite.id_defs, &wrapped);
        sprite
          .defs_injection_cache
          .insert(cache_key.to_string(), injection);
        sprite
          .defs_injection_cache
          .get(cache_key)
          .and_then(|v| v.as_deref())
      };

      let width = node
        .attribute("width")
        .map(trim_ascii_whitespace)
        .filter(|v| !v.is_empty())
        .unwrap_or("100%");
      let height = node
        .attribute("height")
        .map(trim_ascii_whitespace)
        .filter(|v| !v.is_empty())
        .unwrap_or("100%");

      let mut out = String::new();
      out.push_str("<svg width=\"");
      out.push_str(&escape_xml_attr_value(width));
      out.push_str("\" height=\"");
      out.push_str(&escape_xml_attr_value(height));
      out.push('"');
      if let Some(view_box) = element.view_box.as_deref() {
        out.push_str(" viewBox=\"");
        out.push_str(&escape_xml_attr_value(view_box));
        out.push('"');
      }
      if let Some(par) = element.preserve_aspect_ratio.as_deref() {
        out.push_str(" preserveAspectRatio=\"");
        out.push_str(&escape_xml_attr_value(par));
        out.push('"');
      }
      out.push('>');
      if let Some(defs) = defs_injection {
        out.push_str(defs);
      }
      out.push_str(inner);
      out.push_str("</svg>");
      out
    } else {
      let referenced = match sprite.content.get(element.range.clone()) {
        Some(slice) => slice,
        None => continue,
      };

      if !sprite.id_defs.is_empty() {
        wrapper_defs_injection = if let Some(cached) = sprite.defs_injection_cache.get(cache_key) {
          cached.as_deref()
        } else {
          let mut wrapped = String::with_capacity(
            "<svg".len() + sprite.xmlns_attrs.len() + ">".len() + referenced.len() + "</svg>".len(),
          );
          wrapped.push_str("<svg");
          wrapped.push_str(&sprite.xmlns_attrs);
          wrapped.push('>');
          wrapped.push_str(referenced);
          wrapped.push_str("</svg>");

          let injection = crate::paint::svg_mask_image::defs_injection_for_svg_fragment(
            &sprite.id_defs,
            &wrapped,
          );
          sprite
            .defs_injection_cache
            .insert(cache_key.to_string(), injection);
          sprite
            .defs_injection_cache
            .get(cache_key)
            .and_then(|v| v.as_deref())
        };
      }

      referenced.to_string()
    };

    let x = node
      .attribute("x")
      .and_then(parse_svg_length_px)
      .unwrap_or(0.0);
    let y = node
      .attribute("y")
      .and_then(parse_svg_length_px)
      .unwrap_or(0.0);
    let use_transform = node
      .attribute("transform")
      .map(trim_ascii_whitespace)
      .unwrap_or("");

    let mut transform = String::new();
    if x != 0.0 || y != 0.0 {
      transform.push_str(&format!("translate({x} {y})"));
    }
    if !use_transform.is_empty() {
      if !transform.is_empty() {
        transform.push(' ');
      }
      transform.push_str(use_transform);
    }

    let mut wrapper_attrs = String::new();
    // Ensure the injected wrapper is treated as SVG even when the document uses a prefixed SVG
    // namespace (no default `xmlns` in scope).
    const SVG_NS: &str = "http://www.w3.org/2000/svg";
    let referenced_has_default_svg_ns = element
      .namespace_attrs
      .iter()
      .any(|(name, value)| name == "xmlns" && value == SVG_NS);
    if !referenced_has_default_svg_ns {
      wrapper_attrs.push_str(" xmlns=\"http://www.w3.org/2000/svg\"");
    }
    for (name, value) in element.namespace_attrs.iter() {
      if name == "xmlns" {
        if value != SVG_NS {
          continue;
        }
        if referenced_has_default_svg_ns {
          wrapper_attrs.push_str(" xmlns=\"http://www.w3.org/2000/svg\"");
        }
        continue;
      }
      wrapper_attrs.push(' ');
      wrapper_attrs.push_str(name);
      wrapper_attrs.push_str("=\"");
      wrapper_attrs.push_str(&escape_xml_attr_value(value));
      wrapper_attrs.push('"');
    }
    for attr in node.attributes() {
      // Only copy non-namespaced attributes; otherwise we might lose the prefix (`xml:*`,
      // `xlink:*`, etc) and produce invalid markup.
      if attr.namespace().is_some() {
        continue;
      }

      match attr.name() {
        "href" | "xlink:href" | "x" | "y" | "width" | "height" | "transform" => continue,
        "xmlns" => continue,
        _ => {}
      }

      wrapper_attrs.push(' ');
      wrapper_attrs.push_str(attr.name());
      wrapper_attrs.push_str("=\"");
      wrapper_attrs.push_str(&escape_xml_attr_value(attr.value()));
      wrapper_attrs.push('"');
    }
    if !transform.is_empty() {
      wrapper_attrs.push_str(" transform=\"");
      wrapper_attrs.push_str(&escape_xml_attr_value(&transform));
      wrapper_attrs.push('"');
    }

    let replacement = if let Some(defs) = wrapper_defs_injection {
      let mut replacement = String::with_capacity(
        "<g"
          .len()
          .saturating_add(wrapper_attrs.len())
          .saturating_add(1)
          .saturating_add(defs.len())
          .saturating_add(referenced_markup.len())
          .saturating_add("</g>".len()),
      );
      replacement.push_str("<g");
      replacement.push_str(&wrapper_attrs);
      replacement.push('>');
      replacement.push_str(defs);
      replacement.push_str(&referenced_markup);
      replacement.push_str("</g>");
      replacement
    } else {
      format!("<g{wrapper_attrs}>{referenced_markup}</g>")
    };

    expansions += 1;
    injected_bytes = injected_bytes.saturating_add(replacement.len());
    if expansions > MAX_USE_EXPANSIONS || injected_bytes > MAX_INJECTED_BYTES {
      break;
    }

    replacements.push((node.range(), replacement));
  }

  if replacements.is_empty() {
    return Ok(Cow::Borrowed(svg_content));
  }

  replacements.sort_by_key(|(range, _)| range.start);
  let mut out = String::with_capacity(svg_content.len().saturating_add(injected_bytes));
  let mut cursor = 0usize;
  for (range, replacement) in replacements {
    if range.start < cursor || range.end < range.start || range.end > svg_content.len() {
      // If ranges overlap or are otherwise invalid, fall back to the original markup.
      return Ok(Cow::Borrowed(svg_content));
    }
    out.push_str(&svg_content[cursor..range.start]);
    out.push_str(&replacement);
    cursor = range.end;
  }
  out.push_str(&svg_content[cursor..]);
  Ok(Cow::Owned(out))
}

fn unescape_xml_attr_value(value: &str) -> Cow<'_, str> {
  if !value.contains('&') {
    return Cow::Borrowed(value);
  }
  let mut out = String::with_capacity(value.len());
  let bytes = value.as_bytes();
  let mut i = 0usize;
  while i < bytes.len() {
    if bytes[i] != b'&' {
      out.push(bytes[i] as char);
      i += 1;
      continue;
    }
    let Some(semi_rel) = value[i + 1..].find(';') else {
      out.push('&');
      i += 1;
      continue;
    };
    let semi = i + 1 + semi_rel;
    let entity = &value[i + 1..semi];
    let decoded = match entity {
      "amp" => Some('&'),
      "lt" => Some('<'),
      "gt" => Some('>'),
      "quot" => Some('"'),
      "apos" => Some('\''),
      _ if entity.starts_with("#x") || entity.starts_with("#X") => {
        u32::from_str_radix(&entity[2..], 16)
          .ok()
          .and_then(char::from_u32)
      }
      _ if entity.starts_with('#') => entity[1..].parse::<u32>().ok().and_then(char::from_u32),
      _ => None,
    };
    if let Some(ch) = decoded {
      out.push(ch);
      i = semi + 1;
      continue;
    }
    out.push('&');
    out.push_str(entity);
    out.push(';');
    i = semi + 1;
  }
  Cow::Owned(out)
}

fn find_xml_start_tag_end(svg_content: &str, start: usize, limit: usize) -> Option<usize> {
  let bytes = svg_content.as_bytes();
  let mut quote: Option<u8> = None;
  let mut i = start;
  let limit = limit.min(bytes.len());
  while i < limit {
    let b = bytes[i];
    if let Some(q) = quote {
      if b == q {
        quote = None;
      }
    } else if b == b'"' || b == b'\'' {
      quote = Some(b);
    } else if b == b'>' {
      return Some(i + 1);
    }
    i += 1;
  }
  None
}

fn find_xml_end_tag_start(
  svg_content: &str,
  element_start: usize,
  element_end: usize,
  local_name: &str,
) -> Option<usize> {
  let bytes = svg_content.as_bytes();
  let mut i = element_end.min(bytes.len());
  while i > element_start {
    i -= 1;
    if bytes[i] != b'<' {
      continue;
    }
    if bytes.get(i + 1).copied() != Some(b'/') {
      continue;
    }
    let mut j = i + 2;
    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
      j += 1;
    }
    let name_start = j;
    while j < bytes.len() && !bytes[j].is_ascii_whitespace() && bytes[j] != b'>' {
      j += 1;
    }
    if name_start == j {
      continue;
    }
    let Ok(tag_name) = std::str::from_utf8(&bytes[name_start..j]) else {
      continue;
    };
    let actual_local = tag_name
      .rsplit_once(':')
      .map(|(_, local)| local)
      .unwrap_or(tag_name);
    if actual_local.eq_ignore_ascii_case(local_name) {
      return Some(i);
    }
  }
  None
}

fn svg_data_url_mime_from_content_type(content_type: Option<&str>) -> Option<String> {
  let content_type = content_type?;
  let mime = content_type
    .split(';')
    .next()
    .map(|ct| ct.trim_matches(|c: char| matches!(c, ' ' | '\t')))?;
  if mime.is_empty() {
    return None;
  }
  let lowered = mime.to_ascii_lowercase();
  if lowered == "image/jpg" {
    return Some("image/jpeg".to_string());
  }
  if lowered.starts_with("image/") {
    return Some(lowered);
  }
  None
}

fn sniff_svg_image_data_url_mime(bytes: &[u8]) -> Option<&'static str> {
  if bytes.len() >= 8 && &bytes[..8] == b"\x89PNG\r\n\x1a\n" {
    return Some("image/png");
  }
  if bytes.len() >= 3 && bytes[0] == 0xff && bytes[1] == 0xd8 && bytes[2] == 0xff {
    return Some("image/jpeg");
  }
  if bytes.len() >= 6 && (&bytes[..6] == b"GIF87a" || &bytes[..6] == b"GIF89a") {
    return Some("image/gif");
  }
  if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
    return Some("image/webp");
  }
  let probe_len = bytes.len().min(1024);
  let Ok(prefix) = std::str::from_utf8(&bytes[..probe_len]) else {
    return None;
  };
  if svg_text_looks_like_markup(prefix) {
    return Some("image/svg+xml");
  }
  None
}

fn svg_data_url_mime_for_response(content_type: Option<&str>, bytes: &[u8]) -> String {
  svg_data_url_mime_from_content_type(content_type)
    .or_else(|| sniff_svg_image_data_url_mime(bytes).map(|m| m.to_string()))
    .unwrap_or_else(|| "application/octet-stream".to_string())
}

/// Best-effort preprocessor that inlines external raster image references (`<image>` / `<feImage>`)
/// by fetching the referenced resource and rewriting its `href` into a base64 `data:` URL.
fn inline_svg_image_references<'a>(
  svg_content: &'a str,
  svg_url: &str,
  fetcher: &dyn ResourceFetcher,
  ctx: Option<&ResourceContext>,
  subresource_cache: Option<&SvgSubresourceCache>,
) -> Result<Cow<'a, str>> {
  use base64::Engine;

  // Avoid parsing unless it looks like we might have `<image>` references. Inline SVG content in
  // HTML is sometimes namespaced (e.g. `<svg:image>`), so also scan for `:image`.
  if !svg_content.contains("<image")
    && !svg_content.contains(":image")
    && !svg_content.contains("<feImage")
    && !svg_content.contains(":feImage")
  {
    return Ok(Cow::Borrowed(svg_content));
  }

  const MAX_IMAGE_INLINES: usize = 64;
  const MAX_EMBEDDED_BYTES_TOTAL: usize = 4 * 1024 * 1024;
  const MAX_INJECTED_BYTES: usize = 8 * 1024 * 1024;

  fn maybe_decompress_svgz_for_image_inline(bytes: &[u8]) -> Result<Option<Vec<u8>>> {
    if bytes.len() < 2 || bytes[0] != 0x1F || bytes[1] != 0x8B {
      return Ok(None);
    }

    let mut decoder = GzDecoder::new(bytes);
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    let mut deadline_counter = 0usize;

    loop {
      check_root_periodic(&mut deadline_counter, 32, RenderStage::Paint).map_err(Error::Render)?;
      let n = match decoder.read(&mut buf) {
        Ok(n) => n,
        Err(_) => return Ok(None),
      };
      if n == 0 {
        break;
      }
      if out.len().saturating_add(n) > MAX_SVGZ_DECOMPRESSED_BYTES {
        return Ok(None);
      }
      out.extend_from_slice(&buf[..n]);
    }

    Ok(Some(out))
  }

  check_root(RenderStage::Paint).map_err(Error::Render)?;

  let svg_for_parse = svg_markup_for_roxmltree(svg_content);
  let doc = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    roxmltree::Document::parse(svg_for_parse.as_ref())
  })) {
    Ok(Ok(doc)) => doc,
    Ok(Err(_)) | Err(_) => return Ok(Cow::Borrowed(svg_content)),
  };

  let base_url = Url::parse(svg_url).ok().map(|_| svg_url).or_else(|| {
    ctx
      .and_then(|ctx| ctx.document_url.as_deref())
      .filter(|doc_url| Url::parse(doc_url).is_ok())
  });
  let importer_referrer_url = referrer_url_for_svg_importer(svg_url, ctx);

  let mut deadline_counter = 0usize;
  let mut replacements: Vec<(std::ops::Range<usize>, String)> = Vec::new();
  let mut embedded_bytes_total = 0usize;
  let mut injected_bytes = 0usize;
  let mut inlines = 0usize;
  let mut data_url_cache: HashMap<String, SvgCachedDataUrl> = HashMap::new();

  'node_loop: for node in doc.descendants().filter(|n| n.is_element()) {
    check_root_periodic(
      &mut deadline_counter,
      IMAGE_DECODE_DEADLINE_STRIDE,
      RenderStage::Paint,
    )
    .map_err(Error::Render)?;

    let name = node.tag_name().name();
    let is_image = name.eq_ignore_ascii_case("image");
    let is_fe_image = name.eq_ignore_ascii_case("feimage");
    if !is_image && !is_fe_image {
      continue;
    }

    if svg_node_has_display_none(node) {
      continue;
    }

    // Some real-world SVGs use the nonstandard `src` attribute on `<image>` instead of `href`.
    // Only consider `src` when there is no `href` attribute present on the element (including
    // namespaced `xlink:href`), so we don't override the standard form when both are present.
    let mut element_has_href = false;
    for attr in node.attributes() {
      let raw_name = attr.name();
      let local_name = raw_name
        .rsplit_once(':')
        .map(|(_, local)| local)
        .unwrap_or(raw_name);
      if local_name.eq_ignore_ascii_case("href") {
        element_has_href = true;
        break;
      }
    }

    let node_range = node.range();
    if node_range.end > svg_content.len() || node_range.start >= node_range.end {
      continue;
    }

    let Some(tag_end) = find_xml_start_tag_end(svg_content, node_range.start, node_range.end)
    else {
      continue;
    };
    if tag_end > svg_content.len() || tag_end <= node_range.start {
      continue;
    }

    let xml_base_chain = svg_xml_base_chain_for_node(node);
    let effective_base_url = apply_svg_xml_base_chain(base_url, &xml_base_chain);

    let tag = &svg_content[node_range.start..tag_end];
    let bytes = tag.as_bytes();
    let mut i = 0usize;

    // Skip `<` + element name.
    if bytes.get(i) == Some(&b'<') {
      i += 1;
      while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
      }
      while i < bytes.len()
        && !bytes[i].is_ascii_whitespace()
        && bytes[i] != b'>'
        && bytes[i] != b'/'
      {
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
      if bytes[i] == b'/' {
        i += 1;
        continue;
      }

      let name_start = i;
      while i < bytes.len()
        && !bytes[i].is_ascii_whitespace()
        && bytes[i] != b'='
        && bytes[i] != b'>'
        && bytes[i] != b'/'
      {
        i += 1;
      }
      let name_end = i;
      if name_end == name_start {
        i = i.saturating_add(1);
        continue;
      }
      let attr_name = &tag[name_start..name_end];
      let local_name = attr_name
        .rsplit_once(':')
        .map(|(_, local)| local)
        .unwrap_or(attr_name);

      let is_href_attr = local_name.eq_ignore_ascii_case("href");
      let is_src_attr = is_image
        && !element_has_href
        && !attr_name.contains(':')
        && attr_name.eq_ignore_ascii_case("src");
      let is_candidate = is_href_attr || is_src_attr;
      let name_range = (node_range.start + name_start)..(node_range.start + name_end);
      let name_growth = if is_src_attr {
        "href"
          .len()
          .saturating_sub(name_range.end.saturating_sub(name_range.start))
      } else {
        0
      };

      while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
      }

      if i >= bytes.len() || bytes[i] != b'=' {
        continue;
      }
      i += 1;
      while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
      }

      let value_start = i;
      if i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\'') {
        let quote = bytes[i];
        i += 1;
        let start = i;
        while i < bytes.len() && bytes[i] != quote {
          i += 1;
        }
        let value_end = i;
        if i < bytes.len() {
          i += 1;
        }
        let value_start = start;
        let value_range = (node_range.start + value_start)..(node_range.start + value_end);

        if !is_candidate {
          continue;
        }

        let raw_value = tag.get(value_start..value_end).unwrap_or_default();
        let decoded = unescape_xml_attr_value(raw_value);
        let trimmed = trim_ascii_whitespace(decoded.as_ref());
        if trimmed.is_empty()
          || trimmed.starts_with('#')
          || crate::resource::is_data_url(trimmed)
          || is_about_url(trimmed)
        {
          continue;
        }

        let (href_no_fragment, href_fragment) = trimmed
          .split_once('#')
          .map(|(before, frag)| (before, Some(frag)))
          .unwrap_or((trimmed, None));
        let href_no_fragment = trim_ascii_whitespace(href_no_fragment);
        let href_fragment = href_fragment.filter(|frag| !frag.is_empty());
        if href_no_fragment.is_empty() {
          continue;
        }

        let Some(resolved_base) = effective_base_url
          .as_deref()
          .and_then(|base| resolve_against_base(base, href_no_fragment))
          .or_else(|| Url::parse(href_no_fragment).ok().map(|u| u.to_string()))
        else {
          continue;
        };
        let Ok(mut resolved_url) = Url::parse(&resolved_base) else {
          continue;
        };
        resolved_url.set_fragment(None);
        let scheme = resolved_url.scheme();
        if scheme != "http" && scheme != "https" && scheme != "file" {
          continue;
        }
        let resolved_url = resolved_url.to_string();

        if let Some(ctx) = ctx {
          if let Err(err) = ctx.check_allowed(ResourceKind::Image, &resolved_url) {
            return Err(Error::Image(ImageError::LoadFailed {
              url: resolved_url.clone(),
              reason: err.reason,
            }));
          }
        }

        let original_value_len = value_range.end.saturating_sub(value_range.start);

        let cached = if let Some(cached) = data_url_cache.get(&resolved_url) {
          cached.clone()
        } else {
          if inlines >= MAX_IMAGE_INLINES {
            break 'node_loop;
          }

          let mut req = FetchRequest::new(&resolved_url, FetchDestination::Image);
          if let Some(ctx) = ctx {
            if let Some(origin) = ctx.policy.document_origin.as_ref() {
              req = req.with_client_origin(origin);
            }
            if let Some(referrer_url) = importer_referrer_url {
              req = req.with_referrer_url(referrer_url);
            }
            req = req.with_referrer_policy(ctx.referrer_policy);
          }
          let cache_key = svg_subresource_cache_key("svg-img", &req);

          let mut cached: Option<SvgCachedDataUrl> = None;
          if let Some(shared) = subresource_cache {
            if let Ok(mut cache) = shared.lock() {
              if let Some(SvgSubresourceCacheValue::ImageDataUrl(entry)) =
                cache.get_cloned(&cache_key)
              {
                if let Some(ctx) = ctx {
                  if let Err(err) = ctx.check_allowed_with_final(
                    ResourceKind::Image,
                    &resolved_url,
                    entry.final_url.as_deref(),
                  ) {
                    return Err(Error::Image(ImageError::LoadFailed {
                      url: resolved_url.clone(),
                      reason: err.reason,
                    }));
                  }
                }
                cached = Some(entry);
              }
            }
          }

          let mut fetched_entry: Option<(SvgCachedDataUrl, String)> = None;
          let entry = if let Some(hit) = cached {
            hit
          } else {
            check_root(RenderStage::Paint).map_err(Error::Render)?;

            let res = match fetcher.fetch_with_request(req) {
              Ok(res) => res,
              Err(err) => {
                if matches!(&err, Error::Render(_)) {
                  return Err(err);
                }
                continue;
              }
            };

            if let Some(ctx) = ctx {
              if let Err(err) = ctx.check_allowed_with_final(
                ResourceKind::Image,
                &resolved_url,
                res.final_url.as_deref(),
              ) {
                return Err(Error::Image(ImageError::LoadFailed {
                  url: resolved_url.clone(),
                  reason: err.reason,
                }));
              }
            }

            if let Err(_) = ensure_http_success(&res, &resolved_url)
              .and_then(|()| ensure_image_mime_sane(&res, &resolved_url))
            {
              continue;
            }
            if scheme == "file"
              && crate::resource::strict_mime_checks_enabled()
              && payload_looks_like_markup_but_not_svg(&res.bytes)
            {
              continue;
            }

            let bytes_are_gzipped =
              res.bytes.len() >= 2 && res.bytes[0] == 0x1F && res.bytes[1] == 0x8B;
            let url_is_svgz = url_ends_with_svgz(&resolved_url)
              || res.final_url.as_deref().is_some_and(url_ends_with_svgz);
            let mime_is_svg = res
              .content_type
              .as_deref()
              .map(|m| m.contains("image/svg"))
              .unwrap_or(false);

            let mut bytes_for_data_url: Cow<'_, [u8]> = Cow::Borrowed(&res.bytes);
            let mut mime = svg_data_url_mime_for_response(res.content_type.as_deref(), &res.bytes);

            if bytes_are_gzipped && (url_is_svgz || mime_is_svg) {
              if let Some(decompressed) = maybe_decompress_svgz_for_image_inline(&res.bytes)? {
                if let Ok(text) = std::str::from_utf8(&decompressed) {
                  if svg_text_looks_like_markup(text) {
                    bytes_for_data_url = Cow::Owned(decompressed);
                    mime = "image/svg+xml".to_string();
                  }
                }
              }
            }

            let bytes_len = bytes_for_data_url.len();
            if embedded_bytes_total.saturating_add(bytes_len) > MAX_EMBEDDED_BYTES_TOTAL {
              break 'node_loop;
            }

            let base64_len = u64::try_from(bytes_len)
              .ok()
              .and_then(|n| n.checked_add(2))
              .and_then(|n| n.checked_div(3))
              .and_then(|n| n.checked_mul(4))
              .and_then(|n| usize::try_from(n).ok());
            let Some(base64_len) = base64_len else {
              break 'node_loop;
            };

            let prefix_len = "data:".len() + mime.len() + ";base64,".len();
            let total_len = prefix_len.saturating_add(base64_len);
            let growth = total_len
              .saturating_sub(original_value_len)
              .saturating_add(name_growth);
            if injected_bytes.saturating_add(growth) > MAX_INJECTED_BYTES {
              break 'node_loop;
            }

            let encoded =
              base64::engine::general_purpose::STANDARD.encode(bytes_for_data_url.as_ref());
            let data_url = format!("data:{mime};base64,{encoded}");
            let entry = SvgCachedDataUrl {
              data_url: Arc::from(data_url),
              bytes_len,
              final_url: res.final_url.clone(),
            };
            fetched_entry = Some((entry.clone(), cache_key.clone()));
            entry
          };

          if embedded_bytes_total.saturating_add(entry.bytes_len) > MAX_EMBEDDED_BYTES_TOTAL {
            break 'node_loop;
          }
          let total_len = entry.data_url.len();
          let growth = total_len
            .saturating_sub(original_value_len)
            .saturating_add(name_growth);
          if injected_bytes.saturating_add(growth) > MAX_INJECTED_BYTES {
            break 'node_loop;
          }

          if let Some((entry, cache_key)) = fetched_entry {
            if let Some(shared) = subresource_cache {
              let value = SvgSubresourceCacheValue::ImageDataUrl(entry.clone());
              let bytes = estimate_svg_subresource_cache_entry_bytes(&cache_key, &value);
              if let Ok(mut cache) = shared.lock() {
                cache.insert(cache_key, value, bytes);
              }
            }
          }

          embedded_bytes_total = embedded_bytes_total.saturating_add(entry.bytes_len);
          inlines += 1;
          data_url_cache.insert(resolved_url.clone(), entry.clone());
          entry
        };

        let mut replacement = cached.data_url.to_string();
        if let Some(fragment) = href_fragment {
          replacement.push('#');
          replacement.push_str(fragment);
        }
        let replacement = match escape_xml_attr_value(&replacement) {
          Cow::Borrowed(_) => replacement,
          Cow::Owned(escaped) => escaped,
        };

        let growth = replacement
          .len()
          .saturating_sub(original_value_len)
          .saturating_add(name_growth);
        if injected_bytes.saturating_add(growth) > MAX_INJECTED_BYTES {
          break 'node_loop;
        }
        injected_bytes = injected_bytes.saturating_add(growth);
        if is_src_attr {
          replacements.push((name_range, "href".to_string()));
        }
        replacements.push((value_range, replacement));
        continue;
      }

      let start = value_start;
      while i < bytes.len()
        && !bytes[i].is_ascii_whitespace()
        && bytes[i] != b'>'
        && bytes[i] != b'/'
      {
        i += 1;
      }
      let value_end = i;

      let value_range = (node_range.start + start)..(node_range.start + value_end);
      if !is_candidate {
        continue;
      }
      let raw_value = tag.get(start..value_end).unwrap_or_default();
      let decoded = unescape_xml_attr_value(raw_value);
      let trimmed = trim_ascii_whitespace(decoded.as_ref());
      if trimmed.is_empty()
        || trimmed.starts_with('#')
        || crate::resource::is_data_url(trimmed)
        || is_about_url(trimmed)
      {
        continue;
      }

      let (href_no_fragment, href_fragment) = trimmed
        .split_once('#')
        .map(|(before, frag)| (before, Some(frag)))
        .unwrap_or((trimmed, None));
      let href_no_fragment = trim_ascii_whitespace(href_no_fragment);
      let href_fragment = href_fragment.filter(|frag| !frag.is_empty());
      if href_no_fragment.is_empty() {
        continue;
      }

      let Some(resolved_base) = effective_base_url
        .as_deref()
        .and_then(|base| resolve_against_base(base, href_no_fragment))
        .or_else(|| Url::parse(href_no_fragment).ok().map(|u| u.to_string()))
      else {
        continue;
      };
      let Ok(mut resolved_url) = Url::parse(&resolved_base) else {
        continue;
      };
      resolved_url.set_fragment(None);
      let scheme = resolved_url.scheme();
      if scheme != "http" && scheme != "https" && scheme != "file" {
        continue;
      }
      let resolved_url = resolved_url.to_string();

      if let Some(ctx) = ctx {
        if let Err(err) = ctx.check_allowed(ResourceKind::Image, &resolved_url) {
          return Err(Error::Image(ImageError::LoadFailed {
            url: resolved_url.clone(),
            reason: err.reason,
          }));
        }
      }

      let original_value_len = value_range.end.saturating_sub(value_range.start);
      let cached = if let Some(cached) = data_url_cache.get(&resolved_url) {
        cached.clone()
      } else {
        if inlines >= MAX_IMAGE_INLINES {
          break 'node_loop;
        }

        let mut req = FetchRequest::new(&resolved_url, FetchDestination::Image);
        if let Some(ctx) = ctx {
          if let Some(origin) = ctx.policy.document_origin.as_ref() {
            req = req.with_client_origin(origin);
          }
          if let Some(referrer_url) = importer_referrer_url {
            req = req.with_referrer_url(referrer_url);
          }
          req = req.with_referrer_policy(ctx.referrer_policy);
        }
        let cache_key = svg_subresource_cache_key("svg-img", &req);

        let mut cached: Option<SvgCachedDataUrl> = None;
        if let Some(shared) = subresource_cache {
          if let Ok(mut cache) = shared.lock() {
            if let Some(SvgSubresourceCacheValue::ImageDataUrl(entry)) =
              cache.get_cloned(&cache_key)
            {
              if let Some(ctx) = ctx {
                if let Err(err) = ctx.check_allowed_with_final(
                  ResourceKind::Image,
                  &resolved_url,
                  entry.final_url.as_deref(),
                ) {
                  return Err(Error::Image(ImageError::LoadFailed {
                    url: resolved_url.clone(),
                    reason: err.reason,
                  }));
                }
              }
              cached = Some(entry);
            }
          }
        }

        let mut fetched_entry: Option<(SvgCachedDataUrl, String)> = None;
        let entry = if let Some(hit) = cached {
          hit
        } else {
          check_root(RenderStage::Paint).map_err(Error::Render)?;

          let res = match fetcher.fetch_with_request(req) {
            Ok(res) => res,
            Err(err) => {
              if matches!(&err, Error::Render(_)) {
                return Err(err);
              }
              continue;
            }
          };

          if let Some(ctx) = ctx {
            if let Err(err) = ctx.check_allowed_with_final(
              ResourceKind::Image,
              &resolved_url,
              res.final_url.as_deref(),
            ) {
              return Err(Error::Image(ImageError::LoadFailed {
                url: resolved_url.clone(),
                reason: err.reason,
              }));
            }
          }

          if let Err(_) = ensure_http_success(&res, &resolved_url)
            .and_then(|()| ensure_image_mime_sane(&res, &resolved_url))
          {
            continue;
          }
          if scheme == "file"
            && crate::resource::strict_mime_checks_enabled()
            && payload_looks_like_markup_but_not_svg(&res.bytes)
          {
            continue;
          }

          let bytes_are_gzipped =
            res.bytes.len() >= 2 && res.bytes[0] == 0x1F && res.bytes[1] == 0x8B;
          let url_is_svgz = url_ends_with_svgz(&resolved_url)
            || res.final_url.as_deref().is_some_and(url_ends_with_svgz);
          let mime_is_svg = res
            .content_type
            .as_deref()
            .map(|m| m.contains("image/svg"))
            .unwrap_or(false);

          let mut bytes_for_data_url: Cow<'_, [u8]> = Cow::Borrowed(&res.bytes);
          let mut mime = svg_data_url_mime_for_response(res.content_type.as_deref(), &res.bytes);

          if bytes_are_gzipped && (url_is_svgz || mime_is_svg) {
            if let Some(decompressed) = maybe_decompress_svgz_for_image_inline(&res.bytes)? {
              if let Ok(text) = std::str::from_utf8(&decompressed) {
                if svg_text_looks_like_markup(text) {
                  bytes_for_data_url = Cow::Owned(decompressed);
                  mime = "image/svg+xml".to_string();
                }
              }
            }
          }

          let bytes_len = bytes_for_data_url.len();
          if embedded_bytes_total.saturating_add(bytes_len) > MAX_EMBEDDED_BYTES_TOTAL {
            break 'node_loop;
          }

          let base64_len = u64::try_from(bytes_len)
            .ok()
            .and_then(|n| n.checked_add(2))
            .and_then(|n| n.checked_div(3))
            .and_then(|n| n.checked_mul(4))
            .and_then(|n| usize::try_from(n).ok());
          let Some(base64_len) = base64_len else {
            break 'node_loop;
          };

          let prefix_len = "data:".len() + mime.len() + ";base64,".len();
          let total_len = prefix_len.saturating_add(base64_len);
          let growth = total_len
            .saturating_sub(original_value_len)
            .saturating_add(name_growth);
          if injected_bytes.saturating_add(growth) > MAX_INJECTED_BYTES {
            break 'node_loop;
          }

          let encoded =
            base64::engine::general_purpose::STANDARD.encode(bytes_for_data_url.as_ref());
          let data_url = format!("data:{mime};base64,{encoded}");
          let entry = SvgCachedDataUrl {
            data_url: Arc::from(data_url),
            bytes_len,
            final_url: res.final_url.clone(),
          };
          fetched_entry = Some((entry.clone(), cache_key.clone()));
          entry
        };

        if embedded_bytes_total.saturating_add(entry.bytes_len) > MAX_EMBEDDED_BYTES_TOTAL {
          break 'node_loop;
        }
        let total_len = entry.data_url.len();
        let growth = total_len
          .saturating_sub(original_value_len)
          .saturating_add(name_growth);
        if injected_bytes.saturating_add(growth) > MAX_INJECTED_BYTES {
          break 'node_loop;
        }

        if let Some((entry, cache_key)) = fetched_entry {
          if let Some(shared) = subresource_cache {
            let value = SvgSubresourceCacheValue::ImageDataUrl(entry.clone());
            let bytes = estimate_svg_subresource_cache_entry_bytes(&cache_key, &value);
            if let Ok(mut cache) = shared.lock() {
              cache.insert(cache_key, value, bytes);
            }
          }
        }

        embedded_bytes_total = embedded_bytes_total.saturating_add(entry.bytes_len);
        inlines += 1;
        data_url_cache.insert(resolved_url.clone(), entry.clone());
        entry
      };

      let mut replacement = cached.data_url.to_string();
      if let Some(fragment) = href_fragment {
        replacement.push('#');
        replacement.push_str(fragment);
      }
      let replacement = match escape_xml_attr_value(&replacement) {
        Cow::Borrowed(_) => replacement,
        Cow::Owned(escaped) => escaped,
      };

      let growth = replacement
        .len()
        .saturating_sub(original_value_len)
        .saturating_add(name_growth);
      if injected_bytes.saturating_add(growth) > MAX_INJECTED_BYTES {
        break 'node_loop;
      }
      injected_bytes = injected_bytes.saturating_add(growth);
      if is_src_attr {
        replacements.push((name_range, "href".to_string()));
      }
      replacements.push((value_range, replacement));
    }
  }

  if replacements.is_empty() {
    return Ok(Cow::Borrowed(svg_content));
  }

  replacements.sort_by_key(|(range, _)| range.start);
  let mut out = String::with_capacity(svg_content.len().saturating_add(injected_bytes));
  let mut cursor = 0usize;
  for (range, replacement) in replacements {
    if range.start < cursor || range.end < range.start || range.end > svg_content.len() {
      return Ok(Cow::Borrowed(svg_content));
    }
    out.push_str(&svg_content[cursor..range.start]);
    out.push_str(&replacement);
    cursor = range.end;
  }
  out.push_str(&svg_content[cursor..]);
  Ok(Cow::Owned(out))
}

#[derive(Debug, Clone)]
struct SvgCssImportBudget {
  max_depth: usize,
  max_rules: usize,
  max_bytes: usize,
  used_rules: usize,
  used_bytes: usize,
}

impl SvgCssImportBudget {
  fn new(max_depth: usize, max_rules: usize, max_bytes: usize) -> Self {
    Self {
      max_depth: max_depth.max(1),
      max_rules: max_rules.max(1),
      max_bytes: max_bytes.max(1),
      used_rules: 0,
      used_bytes: 0,
    }
  }

  fn check_next_depth(&self, svg_url: &str, next_depth: usize) -> Result<()> {
    if next_depth > self.max_depth {
      return Err(Error::Image(ImageError::LoadFailed {
        url: svg_url.to_string(),
        reason: format!(
          "SVG stylesheet @import depth exceeded the maximum of {}",
          self.max_depth
        ),
      }));
    }
    Ok(())
  }

  fn spend_rule(&mut self, svg_url: &str) -> Result<()> {
    self.used_rules = self.used_rules.saturating_add(1);
    if self.used_rules > self.max_rules {
      return Err(Error::Image(ImageError::LoadFailed {
        url: svg_url.to_string(),
        reason: format!(
          "SVG stylesheet @import count exceeded the maximum of {} rules",
          self.max_rules
        ),
      }));
    }
    Ok(())
  }

  fn spend_bytes(&mut self, svg_url: &str, bytes: usize) -> Result<()> {
    self.used_bytes = self.used_bytes.saturating_add(bytes);
    if self.used_bytes > self.max_bytes {
      return Err(Error::Image(ImageError::LoadFailed {
        url: svg_url.to_string(),
        reason: format!(
          "SVG imported stylesheets exceeded the maximum of {} bytes",
          self.max_bytes
        ),
      }));
    }
    Ok(())
  }
}

fn css_contains_at_import(css: &str) -> bool {
  let bytes = css.as_bytes();
  let mut i = 0usize;
  while i + 7 <= bytes.len() {
    if bytes[i] == b'@'
      && bytes[i + 1].to_ascii_lowercase() == b'i'
      && bytes[i + 2].to_ascii_lowercase() == b'm'
      && bytes[i + 3].to_ascii_lowercase() == b'p'
      && bytes[i + 4].to_ascii_lowercase() == b'o'
      && bytes[i + 5].to_ascii_lowercase() == b'r'
      && bytes[i + 6].to_ascii_lowercase() == b't'
    {
      return true;
    }
    i += 1;
  }
  false
}

fn push_escaped_url_for_css(out: &mut String, url: &str) {
  if !url
    .as_bytes()
    .iter()
    .any(|b| matches!(*b, b'"' | b'\\' | b'\n' | b'\r' | b'\t'))
  {
    out.push_str(url);
    return;
  }

  for ch in url.chars() {
    match ch {
      '"' => out.push_str("\\\""),
      '\\' => out.push_str("\\\\"),
      '\n' => out.push_str("\\0a "),
      '\r' => out.push_str("\\0d "),
      '\t' => out.push_str("\\09 "),
      _ => out.push(ch),
    }
  }
}

fn css_url_looks_like_absolute(url: &str) -> bool {
  let bytes = url.as_bytes();
  if bytes.is_empty() || !bytes[0].is_ascii_alphabetic() {
    return false;
  }
  let mut idx = 1usize;
  while idx < bytes.len() {
    match bytes[idx] {
      b':' => return true,
      b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'+' | b'-' | b'.' => idx += 1,
      _ => return false,
    }
  }
  false
}

fn should_absolutize_css_url_for_svg_style_import(url: &str) -> bool {
  let trimmed = trim_ascii_whitespace(url);
  if trimmed.is_empty()
    || trimmed.starts_with('#')
    || crate::resource::is_data_url(trimmed)
    || is_about_url(trimmed)
  {
    return false;
  }
  // Avoid rewriting inline SVG markup (the image loader treats `<svg...>` strings as a renderable
  // document; turning them into `https://.../%3Csvg` URLs is almost certainly incorrect).
  if trimmed.starts_with('<')
    || trimmed
      .get(.."%3csvg".len())
      .is_some_and(|prefix| prefix.eq_ignore_ascii_case("%3csvg"))
  {
    return false;
  }
  !css_url_looks_like_absolute(trimmed)
}

fn absolutize_css_urls_for_svg_style_import<'a>(css: &'a str, base_url: &str) -> Cow<'a, str> {
  use cssparser::{Parser, ParserInput, Token};

  fn css_may_contain_resolvable_url_tokens(css: &str) -> bool {
    let bytes = css.as_bytes();
    if bytes.len() < 4 {
      return false;
    }
    let mut i = 0usize;
    while i + 3 < bytes.len() {
      if (bytes[i] == b'u' || bytes[i] == b'U')
        && (bytes[i + 1] == b'r' || bytes[i + 1] == b'R')
        && (bytes[i + 2] == b'l' || bytes[i + 2] == b'L')
        && bytes[i + 3] == b'('
      {
        return true;
      }
      i += 1;
    }
    false
  }

  fn rewrite_urls_in_parser<'i, 't>(
    parser: &mut Parser<'i, 't>,
    base_url: &str,
    capacity_hint: usize,
    depth: usize,
  ) -> Cow<'i, str> {
    const MAX_DEPTH: usize = 32;
    if depth > MAX_DEPTH {
      return Cow::Borrowed(parser.slice_from(parser.position()));
    }

    let start_pos = parser.position();
    let mut out: Option<String> = None;
    let mut last_emitted = start_pos;

    // `Parser::is_exhausted()` ignores trailing whitespace/comments, but this routine must preserve
    // them verbatim when rewriting nested blocks. Drive the loop solely via
    // `next_including_whitespace_and_comments()` so `parser.position()` advances to the true end
    // of the input slice.
    loop {
      let token_start = parser.position();
      let token = match parser.next_including_whitespace_and_comments() {
        Ok(t) => t,
        Err(_) => break,
      };

      match token {
        Token::UnquotedUrl(url_value) => {
          let url_value = url_value.as_ref();
          if !should_absolutize_css_url_for_svg_style_import(url_value) {
            continue;
          }
          let Some(resolved) = resolve_against_base(base_url, url_value) else {
            continue;
          };

          let token_text = parser.slice_from(token_start);
          let chunk = parser.slice_from(last_emitted);
          let prefix_len = chunk.len().saturating_sub(token_text.len());
          let out = out.get_or_insert_with(|| String::with_capacity(capacity_hint));
          out.push_str(&chunk[..prefix_len]);

          out.push_str("url(\"");
          push_escaped_url_for_css(out, resolved.as_str());
          out.push_str("\")");

          last_emitted = parser.position();
        }
        Token::Function(ref name) if name.eq_ignore_ascii_case("url") => {
          let parse_result = parser.parse_nested_block(|nested| {
            let mut arg: Option<cssparser::CowRcStr<'i>> = None;
            while !nested.is_exhausted() {
              match nested.next_including_whitespace_and_comments() {
                Ok(Token::WhiteSpace(_)) | Ok(Token::Comment(_)) => {}
                Ok(Token::QuotedString(s)) | Ok(Token::UnquotedUrl(s)) | Ok(Token::Ident(s)) => {
                  arg = Some(s.clone());
                  break;
                }
                Ok(Token::BadUrl(_)) => {
                  arg = None;
                  break;
                }
                Ok(_) => {}
                Err(_) => break,
              }
            }
            Ok::<_, cssparser::ParseError<'i, ()>>(arg)
          });

          let Ok(Some(url_arg)) = parse_result else {
            continue;
          };
          let url_arg = url_arg.as_ref();
          if !should_absolutize_css_url_for_svg_style_import(url_arg) {
            continue;
          }
          let Some(resolved) = resolve_against_base(base_url, url_arg) else {
            continue;
          };

          let block_text = parser.slice_from(token_start);
          let chunk = parser.slice_from(last_emitted);
          let prefix_len = chunk.len().saturating_sub(block_text.len());
          let out = out.get_or_insert_with(|| String::with_capacity(capacity_hint));
          out.push_str(&chunk[..prefix_len]);

          out.push_str("url(\"");
          push_escaped_url_for_css(out, resolved.as_str());
          out.push_str("\")");

          last_emitted = parser.position();
        }
        Token::Function(_)
        | Token::ParenthesisBlock
        | Token::SquareBracketBlock
        | Token::CurlyBracketBlock => {
          let open_len = parser.slice_from(token_start).len();
          let parse_result = parser.parse_nested_block(|nested| {
            let rewritten = rewrite_urls_in_parser(nested, base_url, 0, depth + 1);
            let changed = matches!(rewritten, Cow::Owned(_));
            Ok::<_, cssparser::ParseError<'i, ()>>((rewritten, changed))
          });

          let Ok((inner_rewritten, changed)) = parse_result else {
            continue;
          };
          if !changed {
            continue;
          }

          let block_text = parser.slice_from(token_start);
          const CLOSING_LEN: usize = 1;
          if block_text.len() < open_len + CLOSING_LEN {
            continue;
          }

          let chunk = parser.slice_from(last_emitted);
          let prefix_len = chunk.len().saturating_sub(block_text.len());
          let out = out.get_or_insert_with(|| String::with_capacity(capacity_hint));
          out.push_str(&chunk[..prefix_len]);

          let close_part = &block_text[block_text.len() - CLOSING_LEN..];
          out.push_str(&block_text[..open_len]);
          out.push_str(inner_rewritten.as_ref());
          out.push_str(close_part);

          last_emitted = parser.position();
        }
        _ => {}
      }
    }

    let Some(mut out) = out else {
      return Cow::Borrowed(parser.slice_from(start_pos));
    };
    out.push_str(parser.slice_from(last_emitted));
    Cow::Owned(out)
  }

  if css.is_empty() {
    return Cow::Borrowed(css);
  }
  if !css_may_contain_resolvable_url_tokens(css) {
    return Cow::Borrowed(css);
  }

  let mut input = ParserInput::new(css);
  let mut parser = Parser::new(&mut input);
  rewrite_urls_in_parser(&mut parser, base_url, css.len(), 0)
}

fn resolve_svg_stylesheet_import_url(
  base_url: Option<&str>,
  ctx: Option<&ResourceContext>,
  href: &str,
) -> Option<String> {
  let href = trim_ascii_whitespace(href);
  if href.is_empty()
    || href.starts_with('#')
    || crate::resource::is_data_url(href)
    || is_about_url(href)
  {
    return None;
  }

  base_url
    .and_then(|base| resolve_against_base(base, href))
    .or_else(|| {
      ctx
        .and_then(|ctx| ctx.document_url.as_deref())
        .and_then(|doc_url| resolve_against_base(doc_url, href))
    })
    .or_else(|| Url::parse(href).ok().map(|u| u.to_string()))
}

fn fetch_svg_stylesheet_import(
  requested_url: &str,
  importer_url: Option<&str>,
  fetcher: &dyn ResourceFetcher,
  ctx: Option<&ResourceContext>,
) -> Result<Option<(String, String, usize)>> {
  check_root(RenderStage::Paint).map_err(Error::Render)?;

  if let Some(ctx) = ctx {
    if let Err(err) = ctx.check_allowed(ResourceKind::Stylesheet, requested_url) {
      return Err(Error::Image(ImageError::LoadFailed {
        url: requested_url.to_string(),
        reason: err.reason,
      }));
    }
  }

  let mut req = FetchRequest::new(requested_url, FetchDestination::Style);
  if let Some(ctx) = ctx {
    if let Some(origin) = ctx.policy.document_origin.as_ref() {
      req = req.with_client_origin(origin);
    }
    req = req.with_referrer_policy(ctx.referrer_policy);
  }

  let referrer_url = importer_url
    .and_then(|importer_url| referrer_url_for_svg_importer(importer_url, ctx))
    .or_else(|| ctx.and_then(|ctx| ctx.document_url.as_deref()));
  if let Some(referrer_url) = referrer_url {
    req = req.with_referrer_url(referrer_url);
  }

  let res = match fetcher.fetch_with_request(req) {
    Ok(res) => res,
    Err(err) => {
      if matches!(&err, Error::Render(_)) {
        return Err(err);
      }
      return Ok(None);
    }
  };

  if let Some(ctx) = ctx {
    if let Err(err) = ctx.check_allowed_with_final(
      ResourceKind::Stylesheet,
      requested_url,
      res.final_url.as_deref(),
    ) {
      return Err(Error::Image(ImageError::LoadFailed {
        url: requested_url.to_string(),
        reason: err.reason,
      }));
    }
  }

  if ensure_http_success(&res, requested_url)
    .and_then(|()| ensure_stylesheet_mime_sane(&res, requested_url))
    .is_err()
  {
    return Ok(None);
  }

  let bytes_len = res.bytes.len();
  let final_url = res
    .final_url
    .clone()
    .unwrap_or_else(|| requested_url.to_string());
  let final_url = strip_url_fragment(&final_url).into_owned();
  let css_text = String::from_utf8_lossy(&res.bytes).into_owned();
  Ok(Some((final_url, css_text, bytes_len)))
}

fn inline_css_imports_with_budget<'a>(
  css: &'a str,
  base_url: Option<&str>,
  importer_url: Option<&str>,
  fetcher: &dyn ResourceFetcher,
  ctx: Option<&ResourceContext>,
  budget: &mut SvgCssImportBudget,
  stack: &mut Vec<String>,
  depth: usize,
  svg_url: &str,
) -> Result<Cow<'a, str>> {
  use cssparser::{Parser, ParserInput, Token};

  if !css_contains_at_import(css) {
    return Ok(Cow::Borrowed(css));
  }

  // Bound CSS token scanning. This is separate from the imported-bytes budget, and prevents
  // extremely large inline `<style>` blocks from causing unbounded work.
  const MAX_SCANNED_CSS_BYTES: usize = 512 * 1024;
  if css.len() > MAX_SCANNED_CSS_BYTES {
    return Err(Error::Image(ImageError::LoadFailed {
      url: svg_url.to_string(),
      reason: format!(
        "SVG embedded stylesheet exceeded the maximum of {MAX_SCANNED_CSS_BYTES} bytes"
      ),
    }));
  }

  let mut deadline_counter = 0usize;

  let mut input = ParserInput::new(css);
  let mut parser = Parser::new(&mut input);

  let start_pos = parser.position();
  let mut out: Option<String> = None;
  let mut last_emitted = start_pos;

  loop {
    check_root_periodic(&mut deadline_counter, 256, RenderStage::Paint).map_err(Error::Render)?;

    let token_start = parser.position();
    let token = match parser.next_including_whitespace_and_comments() {
      Ok(t) => t,
      Err(_) => break,
    };

    match token {
      Token::AtKeyword(ref name) if name.eq_ignore_ascii_case("import") => {
        let import_start = token_start;

        // Parse the import URL (quoted string or url(...)).
        let mut url_token: Option<String> = None;
        let mut media_start: Option<cssparser::SourcePosition> = None;

        loop {
          let t = match parser.next_including_whitespace_and_comments() {
            Ok(t) => t,
            Err(_) => break,
          };
          match t {
            Token::WhiteSpace(_) | Token::Comment(_) => continue,
            Token::QuotedString(s) => {
              url_token = Some(s.as_ref().to_string());
              media_start = Some(parser.position());
              break;
            }
            Token::UnquotedUrl(s) => {
              url_token = Some(s.as_ref().to_string());
              media_start = Some(parser.position());
              break;
            }
            Token::Function(ref func) if func.eq_ignore_ascii_case("url") => {
              let parsed = parser.parse_nested_block(|nested| {
                let mut url: Option<cssparser::CowRcStr<'a>> = None;
                while !nested.is_exhausted() {
                  match nested.next_including_whitespace_and_comments() {
                    Ok(Token::WhiteSpace(_)) | Ok(Token::Comment(_)) => {}
                    Ok(Token::QuotedString(s))
                    | Ok(Token::UnquotedUrl(s))
                    | Ok(Token::Ident(s)) => {
                      url = Some(s.clone());
                      break;
                    }
                    Ok(Token::BadUrl(_)) => break,
                    Ok(Token::Function(_))
                    | Ok(Token::ParenthesisBlock)
                    | Ok(Token::SquareBracketBlock)
                    | Ok(Token::CurlyBracketBlock) => {
                      let _ =
                        nested.parse_nested_block(|_| Ok::<_, cssparser::ParseError<'a, ()>>(()));
                    }
                    Ok(_) => {}
                    Err(_) => break,
                  }
                }
                Ok::<_, cssparser::ParseError<'a, ()>>(url)
              });
              if let Ok(Some(url)) = parsed {
                url_token = Some(url.as_ref().to_string());
                media_start = Some(parser.position());
              }
              break;
            }
            _ => break,
          }
        }

        // Consume to the end of the at-rule so the main loop continues at the next token.
        let mut semicolon_start: Option<cssparser::SourcePosition> = None;
        loop {
          let t_start = parser.position();
          let t = match parser.next_including_whitespace_and_comments() {
            Ok(t) => t,
            Err(_) => break,
          };
          match t {
            Token::Semicolon => {
              semicolon_start = Some(t_start);
              break;
            }
            Token::Function(_)
            | Token::ParenthesisBlock
            | Token::SquareBracketBlock
            | Token::CurlyBracketBlock => {
              let _ = parser.parse_nested_block(|_| Ok::<_, cssparser::ParseError<'a, ()>>(()));
            }
            _ => {}
          }
        }

        let Some(url_token) = url_token else {
          // If we can't parse the URL, treat as a no-op import and preserve the original text.
          continue;
        };

        let should_rewrite_url = should_absolutize_css_url_for_svg_style_import(&url_token);

        let Some(requested_url) = resolve_svg_stylesheet_import_url(base_url, ctx, &url_token)
        else {
          continue;
        };
        let requested_url = strip_url_fragment(&requested_url).into_owned();

        budget.spend_rule(svg_url)?;
        budget.check_next_depth(svg_url, depth.saturating_add(1))?;

        let import_tail = media_start.map(|media_start| parser.slice_from(media_start));
        let mut emit_import_rewrite = |resolved_url: &str| {
          let mut replacement = String::new();
          replacement.push_str("@import url(\"");
          push_escaped_url_for_css(&mut replacement, resolved_url);
          replacement.push_str("\")");
          if let Some(tail) = import_tail {
            replacement.push_str(tail);
          } else {
            replacement.push(';');
          }

          let import_text = parser.slice_from(import_start);
          let chunk = parser.slice_from(last_emitted);
          let prefix_len = chunk.len().saturating_sub(import_text.len());
          let out = out.get_or_insert_with(|| {
            String::with_capacity(css.len().saturating_add(replacement.len()))
          });
          out.push_str(&chunk[..prefix_len]);
          out.push_str(&replacement);
          last_emitted = parser.position();
        };

        // Break cycles by skipping inlining when the URL is already on the stack.
        if stack.iter().any(|u| u == &requested_url) {
          if should_rewrite_url {
            emit_import_rewrite(&requested_url);
          }
          continue;
        }

        let fetched = fetch_svg_stylesheet_import(&requested_url, importer_url, fetcher, ctx)?;
        let Some((final_url, fetched_css, fetched_bytes)) = fetched else {
          if should_rewrite_url {
            emit_import_rewrite(&requested_url);
          }
          continue;
        };

        if stack.iter().any(|u| u == &final_url) {
          if should_rewrite_url {
            emit_import_rewrite(&requested_url);
          }
          continue;
        }

        budget.spend_bytes(svg_url, fetched_bytes)?;

        stack.push(final_url.clone());
        let mut nested = inline_css_imports_with_budget(
          &fetched_css,
          Some(&final_url),
          Some(&final_url),
          fetcher,
          ctx,
          budget,
          stack,
          depth + 1,
          svg_url,
        )?
        .into_owned();
        stack.pop();

        if let Cow::Owned(rewritten) = absolutize_css_urls_for_svg_style_import(&nested, &final_url)
        {
          nested = rewritten;
        }

        let media = media_start.map(|media_start| {
          if let Some(semi_start) = semicolon_start {
            let full = parser.slice_from(media_start);
            let tail = parser.slice_from(semi_start);
            let len = full.len().saturating_sub(tail.len());
            trim_ascii_whitespace(&full[..len]).to_string()
          } else {
            trim_ascii_whitespace(parser.slice_from(media_start)).to_string()
          }
        });
        let media = media
          .as_deref()
          .map(trim_ascii_whitespace)
          .filter(|m| !m.is_empty())
          .map(|m| m.to_string());

        let mut replacement = String::new();
        if let Some(media) = media {
          replacement.push_str("@media ");
          replacement.push_str(&media);
          replacement.push_str(" {\n");
          replacement.push_str(&nested);
          replacement.push_str("\n}\n");
        } else {
          replacement.push_str(&nested);
          replacement.push('\n');
        }

        let import_text = parser.slice_from(import_start);
        let chunk = parser.slice_from(last_emitted);
        let prefix_len = chunk.len().saturating_sub(import_text.len());
        let out = out.get_or_insert_with(|| {
          String::with_capacity(css.len().saturating_add(replacement.len()))
        });
        out.push_str(&chunk[..prefix_len]);
        out.push_str(&replacement);
        last_emitted = parser.position();
      }
      Token::Function(_)
      | Token::ParenthesisBlock
      | Token::SquareBracketBlock
      | Token::CurlyBracketBlock => {
        // Skip nested blocks: SVG `@import` is only meaningful at the top level.
        let _ = parser.parse_nested_block(|_| Ok::<_, cssparser::ParseError<'a, ()>>(()));
      }
      _ => {}
    }
  }

  let Some(mut out) = out else {
    return Ok(Cow::Borrowed(css));
  };
  out.push_str(parser.slice_from(last_emitted));
  Ok(Cow::Owned(out))
}

/// Best-effort preprocessor that expands `@import` rules inside SVG `<style>` elements by fetching
/// and inlining the referenced stylesheets before handing the SVG off to `usvg`.
///
/// When inlining, relative `url(...)` tokens inside imported stylesheets are rewritten to be
/// absolute so they continue to resolve relative to the imported stylesheet's URL (not the parent
/// SVG's URL).
fn inline_svg_style_imports<'a>(
  svg_content: &'a str,
  svg_url: &str,
  fetcher: &dyn ResourceFetcher,
  ctx: Option<&ResourceContext>,
) -> Result<Cow<'a, str>> {
  // Avoid parsing unless it looks like we might have `<style>` blocks with `@import`.
  if (!svg_content.contains("<style") && !svg_content.contains(":style"))
    || !css_contains_at_import(svg_content)
  {
    return Ok(Cow::Borrowed(svg_content));
  }

  const MAX_IMPORT_DEPTH: usize = 16;
  const MAX_IMPORT_RULES: usize = 64;
  const MAX_IMPORTED_CSS_BYTES: usize = 1024 * 1024;

  check_root(RenderStage::Paint).map_err(Error::Render)?;

  let svg_for_parse = svg_markup_for_roxmltree(svg_content);
  let doc = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    roxmltree::Document::parse(svg_for_parse.as_ref())
  })) {
    Ok(Ok(doc)) => doc,
    Ok(Err(_)) | Err(_) => return Ok(Cow::Borrowed(svg_content)),
  };

  let base_url = Url::parse(svg_url).ok().map(|_| svg_url).or_else(|| {
    ctx
      .and_then(|ctx| ctx.document_url.as_deref())
      .filter(|doc_url| Url::parse(doc_url).is_ok())
  });
  let importer_url = if base_url == Some(svg_url) {
    Some(svg_url)
  } else {
    ctx.and_then(|ctx| ctx.document_url.as_deref())
  };

  let mut budget =
    SvgCssImportBudget::new(MAX_IMPORT_DEPTH, MAX_IMPORT_RULES, MAX_IMPORTED_CSS_BYTES);
  let mut stack: Vec<String> = Vec::new();
  let mut deadline_counter = 0usize;
  let mut replacements: Vec<(std::ops::Range<usize>, String)> = Vec::new();

  for node in doc.descendants().filter(|n| n.is_element()) {
    check_root_periodic(&mut deadline_counter, 256, RenderStage::Paint).map_err(Error::Render)?;

    if node.tag_name().name() != "style" {
      continue;
    }

    let mut css_text = String::new();
    for child in node.children() {
      if child.is_text() {
        if let Some(t) = child.text() {
          css_text.push_str(t);
        }
      }
    }

    if !css_contains_at_import(&css_text) {
      continue;
    }

    let xml_base_chain = svg_xml_base_chain_for_node(node);
    let effective_base = apply_svg_xml_base_chain(base_url, &xml_base_chain);
    let effective_base = effective_base.as_deref().or(base_url);

    let expanded = inline_css_imports_with_budget(
      &css_text,
      effective_base,
      importer_url,
      fetcher,
      ctx,
      &mut budget,
      &mut stack,
      0,
      svg_url,
    )?;
    if matches!(expanded, Cow::Borrowed(_)) {
      continue;
    }
    let escaped = escape_xml_attr_value(expanded.as_ref()).into_owned();

    let node_range = node.range();
    if node_range.end > svg_content.len() || node_range.start >= node_range.end {
      continue;
    }
    let Some(start_tag_end) = find_xml_start_tag_end(svg_content, node_range.start, node_range.end)
    else {
      continue;
    };
    if start_tag_end >= 2 && svg_content.as_bytes().get(start_tag_end - 2) == Some(&b'/') {
      // Self-closing `<style/>` has no content to patch.
      continue;
    }
    let Some(slice) = svg_content.get(node_range.clone()) else {
      continue;
    };
    // Locate the closing tag (`</...>`). Do not use `rfind('<')` because `<style>` content can
    // contain literal `<` bytes when authored via CDATA sections, and we must not treat those as
    // markup boundaries when patching the original SVG string.
    let Some(end_tag_rel) = slice.rfind("</") else {
      continue;
    };
    let end_tag_start = node_range.start.saturating_add(end_tag_rel);
    if end_tag_start < start_tag_end || end_tag_start > svg_content.len() {
      continue;
    }
    replacements.push((start_tag_end..end_tag_start, escaped));
  }

  if replacements.is_empty() {
    return Ok(Cow::Borrowed(svg_content));
  }

  replacements.sort_by_key(|(range, _)| range.start);
  let mut out = String::with_capacity(svg_content.len());
  let mut cursor = 0usize;
  for (range, replacement) in replacements {
    if range.start < cursor || range.end < range.start || range.end > svg_content.len() {
      return Ok(Cow::Borrowed(svg_content));
    }
    out.push_str(&svg_content[cursor..range.start]);
    out.push_str(&replacement);
    cursor = range.end;
  }
  out.push_str(&svg_content[cursor..]);
  Ok(Cow::Owned(out))
}

#[derive(Debug, Clone)]
struct SvgExternalUrlFragmentOccurrence {
  /// Range of bytes inside the `url(...)` token that should be replaced. This range indexes into
  /// the full SVG string.
  ///
  /// Specifically, this is the substring between the `(` and `)` characters.
  arg_range: std::ops::Range<usize>,
  doc_url: String,
  id: String,
}

/// Best-effort preprocessor that expands external SVG fragment `url(<svg-url>#<id>)` references
/// (commonly used for patterns/gradients/filters/masks) by fetching the referenced SVG document,
/// extracting the referenced element, and injecting it into the current document as a local
/// fragment (`url(#id)`).
///
/// External SVG fragments are injected into a new `<defs>` block immediately after the root `<svg>`
/// start tag.
///
/// This is intentionally narrow and bounded: it only patches absolute/relative URL fragments, and
/// skips malformed inputs rather than failing the entire SVG decode.
fn inline_svg_external_url_fragment_references<'a>(
  svg_content: &'a str,
  svg_url: &str,
  fetcher: &dyn ResourceFetcher,
  ctx: Option<&ResourceContext>,
  subresource_cache: Option<&SvgSubresourceCache>,
) -> Result<Cow<'a, str>> {
  fn contains_ascii_case_insensitive(haystack: &str, needle: &[u8]) -> bool {
    let bytes = haystack.as_bytes();
    if bytes.len() < needle.len() {
      return false;
    }
    bytes.windows(needle.len()).any(|window| {
      window
        .iter()
        .zip(needle)
        .all(|(a, b)| a.to_ascii_lowercase() == *b)
    })
  }

  // Avoid scanning unless we plausibly have url() tokens.
  if !contains_ascii_case_insensitive(svg_content, b"url(") {
    return Ok(Cow::Borrowed(svg_content));
  }

  const MAX_URL_FRAGMENTS: usize = 64;
  const MAX_INJECTED_DEFS_BYTES: usize = 512 * 1024;

  check_root(RenderStage::Paint).map_err(Error::Render)?;

  let base_url = Url::parse(svg_url).ok().map(|_| svg_url).or_else(|| {
    ctx
      .and_then(|ctx| ctx.document_url.as_deref())
      .filter(|doc_url| Url::parse(doc_url).is_ok())
  });
  let importer_referrer_url = referrer_url_for_svg_importer(svg_url, ctx);

  // Scan the raw SVG text for `url(...)` tokens and record external fragments.
  let bytes = svg_content.as_bytes();
  let mut occurrences: Vec<SvgExternalUrlFragmentOccurrence> = Vec::new();
  let mut deadline_counter = 0usize;
  let mut i = 0usize;
  while i + 4 <= bytes.len() {
    check_root_periodic(
      &mut deadline_counter,
      IMAGE_DECODE_DEADLINE_STRIDE,
      RenderStage::Paint,
    )
    .map_err(Error::Render)?;

    if (bytes[i] == b'u' || bytes[i] == b'U')
      && (bytes[i + 1] == b'r' || bytes[i + 1] == b'R')
      && (bytes[i + 2] == b'l' || bytes[i + 2] == b'L')
      && bytes[i + 3] == b'('
      && (i == 0 || !bytes[i.saturating_sub(1)].is_ascii_alphanumeric())
    {
      let mut quote: Option<u8> = None;
      let mut close = None;
      let mut j = i + 4;
      while j < bytes.len() {
        let b = bytes[j];
        if let Some(q) = quote {
          if b == q {
            quote = None;
          }
        } else if b == b'"' || b == b'\'' {
          quote = Some(b);
        } else if b == b')' {
          close = Some(j);
          break;
        }
        j += 1;
      }
      let Some(close_idx) = close else {
        break;
      };

      let inner = svg_content.get(i + 4..close_idx).unwrap_or_default();
      let mut value = trim_ascii_whitespace(inner);
      // Strip simple quotes when the entire argument is quoted.
      if value.len() >= 2 {
        let first = value.as_bytes()[0];
        let last = value.as_bytes()[value.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
          value = trim_ascii_whitespace(&value[1..value.len() - 1]);
        }
      }
      let value = trim_ascii_whitespace(value);
      if value.is_empty()
        || value.starts_with('#')
        || crate::resource::is_data_url(value)
        || is_about_url(value)
      {
        i = close_idx + 1;
        continue;
      }

      let Some((doc_part, frag)) = value.split_once('#') else {
        i = close_idx + 1;
        continue;
      };
      let doc_part = trim_ascii_whitespace(doc_part);
      let frag = trim_ascii_whitespace(frag);
      if doc_part.is_empty() || frag.is_empty() || doc_part.starts_with('#') {
        i = close_idx + 1;
        continue;
      }

      let resolved = base_url
        .as_deref()
        .and_then(|base| resolve_against_base(base, doc_part))
        .or_else(|| {
          ctx
            .and_then(|ctx| ctx.document_url.as_deref())
            .and_then(|base| resolve_against_base(base, doc_part))
        })
        .or_else(|| Url::parse(doc_part).ok().map(|u| u.to_string()));
      let Some(resolved_base) = resolved else {
        i = close_idx + 1;
        continue;
      };

      let Ok(mut parsed) = Url::parse(&resolved_base) else {
        i = close_idx + 1;
        continue;
      };
      parsed.set_fragment(None);
      let scheme = parsed.scheme();
      if scheme != "http" && scheme != "https" && scheme != "file" {
        i = close_idx + 1;
        continue;
      }

      occurrences.push(SvgExternalUrlFragmentOccurrence {
        arg_range: (i + 4)..close_idx,
        doc_url: parsed.to_string(),
        id: frag.to_string(),
      });
      if occurrences.len() >= MAX_URL_FRAGMENTS {
        break;
      }

      i = close_idx + 1;
      continue;
    }

    i += 1;
  }

  if occurrences.is_empty() {
    return Ok(Cow::Borrowed(svg_content));
  }

  // Collect ids already defined in the host document so we don't inject duplicates.
  let mut existing_ids: HashSet<String> = HashSet::new();
  let svg_for_parse = svg_markup_for_roxmltree(svg_content);
  if let Ok(Ok(doc)) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    roxmltree::Document::parse(svg_for_parse.as_ref())
  })) {
    for node in doc.descendants().filter(|n| n.is_element()) {
      if let Some(id) = node
        .attribute("id")
        .map(trim_ascii_whitespace)
        .filter(|id| !id.is_empty())
      {
        existing_ids.insert(id.to_string());
      }
    }
  }

  // Group by external document URL so we only fetch each SVG once.
  let mut ids_by_doc: HashMap<String, HashSet<String>> = HashMap::new();
  for occ in &occurrences {
    ids_by_doc
      .entry(occ.doc_url.clone())
      .or_default()
      .insert(occ.id.clone());
  }

  let mut injected_keys: HashSet<(String, String)> = HashSet::new();
  let mut injected_defs = String::new();

  // Iterate documents in a stable order so preprocessing doesn't introduce nondeterministic output
  // (and thus unstable SVG pixmap cache keys).
  let mut doc_urls: Vec<String> = ids_by_doc.keys().cloned().collect();
  doc_urls.sort();

  for doc_url in doc_urls {
    let ids = ids_by_doc.remove(&doc_url).unwrap_or_default();
    let mut ids: Vec<String> = ids.into_iter().collect();
    ids.sort();

    // Skip ids that are already defined in the host document.
    ids.retain(|id| !existing_ids.contains(id));
    if ids.is_empty() {
      continue;
    }
    let wanted_ids: HashSet<&str> = ids.iter().map(|s| s.as_str()).collect();

    if let Some(ctx) = ctx {
      if let Err(err) = ctx.check_allowed(ResourceKind::Image, &doc_url) {
        return Err(Error::Image(ImageError::LoadFailed {
          url: doc_url.clone(),
          reason: err.reason,
        }));
      }
    }

    check_root(RenderStage::Paint).map_err(Error::Render)?;

    let mut req = FetchRequest::new(&doc_url, FetchDestination::Image);
    if let Some(ctx) = ctx {
      if let Some(origin) = ctx.policy.document_origin.as_ref() {
        req = req.with_client_origin(origin);
      }
      if let Some(referrer_url) = importer_referrer_url {
        req = req.with_referrer_url(referrer_url);
      }
      req = req.with_referrer_policy(ctx.referrer_policy);
    }

    let res = match fetcher.fetch_with_request(req) {
      Ok(res) => res,
      Err(err) => {
        // Render-control failures must abort the render; other fetch failures are best-effort.
        if matches!(&err, Error::Render(_)) {
          return Err(err);
        }
        continue;
      }
    };

    if let Some(ctx) = ctx {
      if let Err(err) =
        ctx.check_allowed_with_final(ResourceKind::Image, &doc_url, res.final_url.as_deref())
      {
        return Err(Error::Image(ImageError::LoadFailed {
          url: doc_url.clone(),
          reason: err.reason,
        }));
      }
    }
    if ensure_http_success(&res, &doc_url)
      .and_then(|()| ensure_image_mime_sane(&res, &doc_url))
      .is_err()
    {
      continue;
    }

    let doc_base_url = res.final_url.clone().unwrap_or_else(|| doc_url.clone());

    let mut doc_text = {
      let bytes = res.bytes;
      if bytes.len() >= 2 && bytes[0] == 0x1F && bytes[1] == 0x8B {
        let mut decoder = GzDecoder::new(bytes.as_slice());
        let mut out = Vec::new();
        let mut buf = [0u8; 8192];
        let mut decompression_deadline_counter = 0usize;
        let mut ok = true;
        loop {
          check_root_periodic(&mut decompression_deadline_counter, 32, RenderStage::Paint)
            .map_err(Error::Render)?;
          let n = match decoder.read(&mut buf) {
            Ok(n) => n,
            Err(_) => {
              ok = false;
              break;
            }
          };
          if n == 0 {
            break;
          }
          if out.len().saturating_add(n) > MAX_SVGZ_DECOMPRESSED_BYTES {
            ok = false;
            break;
          }
          out.extend_from_slice(&buf[..n]);
        }
        if !ok {
          continue;
        }
        match String::from_utf8(out) {
          Ok(text) => text,
          Err(_) => continue,
        }
      } else {
        match String::from_utf8(bytes) {
          Ok(text) => text,
          Err(_) => continue,
        }
      }
    };

    // Preprocess the external SVG so any nested external resources resolve relative to the external
    // document (not the host SVG).
    doc_text =
      inline_svg_use_references(&doc_text, &doc_base_url, fetcher, ctx, subresource_cache)?
        .into_owned();
    doc_text =
      inline_svg_image_references(&doc_text, &doc_base_url, fetcher, ctx, subresource_cache)?
        .into_owned();

    let doc_for_parse = svg_markup_for_roxmltree(&doc_text);
    let doc = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      roxmltree::Document::parse(doc_for_parse.as_ref())
    })) {
      Ok(Ok(doc)) => doc,
      Ok(Err(_)) | Err(_) => continue,
    };

    // Index elements by id for extraction.
    let mut by_id: HashMap<String, std::ops::Range<usize>> = HashMap::new();
    for node in doc.descendants().filter(|n| n.is_element()) {
      let Some(id) = node
        .attribute("id")
        .map(trim_ascii_whitespace)
        .filter(|id| !id.is_empty())
      else {
        continue;
      };
      if !wanted_ids.contains(id) {
        continue;
      }
      by_id.entry(id.to_string()).or_insert_with(|| node.range());
    }

    for id in ids {
      let Some(range) = by_id.get(&id).cloned() else {
        continue;
      };
      let Some(fragment) = doc_text.get(range) else {
        continue;
      };

      // Keep injection bounded.
      if injected_defs.len().saturating_add(fragment.len()) > MAX_INJECTED_DEFS_BYTES {
        break;
      }

      injected_keys.insert((doc_url.clone(), id.clone()));
      existing_ids.insert(id);
      injected_defs.push_str(fragment);
    }
  }

  if injected_defs.is_empty() {
    return Ok(Cow::Borrowed(svg_content));
  }

  // Rewrite url() references for successfully-injected fragments.
  let mut replacements: Vec<(std::ops::Range<usize>, String)> = Vec::new();
  for occ in &occurrences {
    if injected_keys.contains(&(occ.doc_url.clone(), occ.id.clone())) {
      replacements.push((occ.arg_range.clone(), format!("#{}", occ.id)));
    }
  }
  if replacements.is_empty() {
    return Ok(Cow::Borrowed(svg_content));
  }

  replacements.sort_by_key(|(range, _)| range.start);
  let mut rewritten = String::with_capacity(svg_content.len());
  let mut cursor = 0usize;
  for (range, replacement) in replacements {
    if range.start < cursor || range.end < range.start || range.end > svg_content.len() {
      return Ok(Cow::Borrowed(svg_content));
    }
    rewritten.push_str(&svg_content[cursor..range.start]);
    rewritten.push_str(&replacement);
    cursor = range.end;
  }
  rewritten.push_str(&svg_content[cursor..]);

  // Inject collected defs into the root SVG element.
  fn find_svg_root_start_tag_bounds(svg: &str) -> Option<(usize, usize)> {
    const NEEDLE: &[u8] = b"<svg";
    let bytes = svg.as_bytes();
    if bytes.len() < NEEDLE.len() {
      return None;
    }
    let mut start = None;
    for idx in 0..=bytes.len() - NEEDLE.len() {
      if bytes[idx..idx + NEEDLE.len()].eq_ignore_ascii_case(NEEDLE) {
        // Ensure we're not matching `<svgFoo>`.
        let boundary = bytes.get(idx + NEEDLE.len()).copied().unwrap_or(b'>');
        if !(boundary.is_ascii_whitespace() || matches!(boundary, b'>' | b'/' | b':')) {
          continue;
        }
        start = Some(idx);
        break;
      }
    }
    let start = start?;

    let mut quote: Option<u8> = None;
    let mut idx = start + NEEDLE.len();
    while idx < bytes.len() {
      let b = bytes[idx];
      if let Some(q) = quote {
        if b == q {
          quote = None;
        }
      } else if b == b'\'' || b == b'"' {
        quote = Some(b);
      } else if b == b'>' {
        return Some((start, idx + 1));
      }
      idx += 1;
    }
    None
  }

  fn contains_xlink_prefix(value: &str) -> bool {
    const NEEDLE: &[u8] = b"xlink:";
    value
      .as_bytes()
      .windows(NEEDLE.len())
      .any(|window| window.eq_ignore_ascii_case(NEEDLE))
  }

  fn start_tag_has_xmlns_xlink(start_tag: &str) -> bool {
    const NEEDLE: &[u8] = b"xmlns:xlink";
    start_tag
      .as_bytes()
      .windows(NEEDLE.len())
      .any(|window| window.eq_ignore_ascii_case(NEEDLE))
  }

  let (start_tag_start, start_tag_end) = match find_svg_root_start_tag_bounds(&rewritten) {
    Some(bounds) => bounds,
    None => return Ok(Cow::Borrowed(svg_content)),
  };
  let start_tag = match rewritten.get(start_tag_start..start_tag_end) {
    Some(tag) => tag,
    None => return Ok(Cow::Borrowed(svg_content)),
  };

  // Do not attempt to inject into a self-closing root `<svg/>`.
  if start_tag_end >= 2 && rewritten.as_bytes()[start_tag_end.saturating_sub(2)] == b'/' {
    return Ok(Cow::Borrowed(svg_content));
  }

  let needs_xlink = contains_xlink_prefix(&injected_defs);
  let add_xlink = needs_xlink && !start_tag_has_xmlns_xlink(start_tag);
  let extra_root_attr = if add_xlink {
    " xmlns:xlink=\"http://www.w3.org/1999/xlink\""
  } else {
    ""
  };

  let mut out = String::with_capacity(
    rewritten
      .len()
      .saturating_add(extra_root_attr.len())
      .saturating_add("<defs></defs>".len() + injected_defs.len()),
  );

  if extra_root_attr.is_empty() {
    out.push_str(&rewritten[..start_tag_end]);
  } else {
    // Insert before `>` (or before `/>`) of the root start tag.
    let mut insert_at = start_tag_end - 1;
    if insert_at > start_tag_start && rewritten.as_bytes()[insert_at - 1] == b'/' {
      insert_at -= 1;
    }
    out.push_str(&rewritten[..insert_at]);
    out.push_str(extra_root_attr);
    out.push_str(&rewritten[insert_at..start_tag_end]);
  }

  out.push_str("<defs>");
  out.push_str(&injected_defs);
  out.push_str("</defs>");
  out.push_str(&rewritten[start_tag_end..]);

  Ok(Cow::Owned(out))
}

fn apply_svg_url_fragment<'a>(svg_content: &'a str, requested_url: &str) -> Cow<'a, str> {
  let Some((_, fragment)) = requested_url.split_once('#') else {
    return Cow::Borrowed(svg_content);
  };
  let fragment = trim_ascii_whitespace(fragment);
  if fragment.is_empty() {
    return Cow::Borrowed(svg_content);
  }

  let svg_for_parse = svg_markup_for_roxmltree(svg_content);
  let doc = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    roxmltree::Document::parse(svg_for_parse.as_ref())
  })) {
    Ok(Ok(doc)) => doc,
    Ok(Err(_)) | Err(_) => return Cow::Borrowed(svg_content),
  };

  let root = doc.root_element();
  if !root.tag_name().name().eq_ignore_ascii_case("svg") {
    return Cow::Borrowed(svg_content);
  }

  // Only inject when the referenced element exists; otherwise we'd unnecessarily introduce a
  // `<use>` element into non-sprite SVGs (which can disable simple fast-path rendering).
  let Some(target) = doc.descendants().find(|node| {
    node.is_element()
      && *node != root
      && node
        .attribute("id")
        .is_some_and(|id| trim_ascii_whitespace(id) == fragment)
  }) else {
    return Cow::Borrowed(svg_content);
  };

  if target.tag_name().name().eq_ignore_ascii_case("symbol") {
    // Symbols are not directly rendered. Wrap their contents in a new <svg>, copying the root
    // viewport size and the symbol's viewBox/preserveAspectRatio when present.
    let symbol = target;
    let mut inner_start: Option<usize> = None;
    let mut inner_end: Option<usize> = None;
    for child in symbol.children() {
      let r = child.range();
      inner_start = Some(inner_start.map_or(r.start, |s| s.min(r.start)));
      inner_end = Some(inner_end.map_or(r.end, |e| e.max(r.end)));
    }
    let inner_range = match (inner_start, inner_end) {
      (Some(s), Some(e)) if s <= e => Some(s..e),
      _ => None,
    };
    let inner = inner_range
      .as_ref()
      .and_then(|r| svg_content.get(r.clone()))
      .unwrap_or_default();

    let root_width = root
      .attribute("width")
      .map(trim_ascii_whitespace)
      .filter(|v| !v.is_empty());
    let root_height = root
      .attribute("height")
      .map(trim_ascii_whitespace)
      .filter(|v| !v.is_empty());

    let view_box = symbol
      .attribute("viewBox")
      .map(trim_ascii_whitespace)
      .filter(|v| !v.is_empty())
      .or_else(|| {
        root
          .attribute("viewBox")
          .map(trim_ascii_whitespace)
          .filter(|v| !v.is_empty())
      });
    let preserve_aspect_ratio = symbol
      .attribute("preserveAspectRatio")
      .map(trim_ascii_whitespace)
      .filter(|v| !v.is_empty())
      .or_else(|| {
        root
          .attribute("preserveAspectRatio")
          .map(trim_ascii_whitespace)
          .filter(|v| !v.is_empty())
      });

    let mut out = String::new();
    out.push_str("<svg");
    // Ensure the output still parses as SVG even when the original document had unusual namespace
    // placement.
    let mut had_xmlns = false;
    for attr in root.attributes() {
      let name = attr.name();
      if !name.starts_with("xmlns") {
        continue;
      }
      if name == "xmlns" {
        had_xmlns = true;
      }
      out.push(' ');
      out.push_str(name);
      out.push_str("=\"");
      out.push_str(&escape_xml_attr_value(attr.value()));
      out.push('"');
    }
    if !had_xmlns {
      out.push_str(" xmlns=\"http://www.w3.org/2000/svg\"");
    }

    if let Some(width) = root_width {
      out.push_str(" width=\"");
      out.push_str(&escape_xml_attr_value(width));
      out.push('"');
    }
    if let Some(height) = root_height {
      out.push_str(" height=\"");
      out.push_str(&escape_xml_attr_value(height));
      out.push('"');
    }
    if let Some(view_box) = view_box {
      out.push_str(" viewBox=\"");
      out.push_str(&escape_xml_attr_value(view_box));
      out.push('"');
    }
    if let Some(par) = preserve_aspect_ratio {
      out.push_str(" preserveAspectRatio=\"");
      out.push_str(&escape_xml_attr_value(par));
      out.push('"');
    }
    out.push('>');

    // Keep root <defs>/<style> blocks so symbol content can reference gradients/filters and reuse
    // shared CSS.
    for child in root.children().filter(|n| n.is_element()) {
      let name = child.tag_name().name();
      let keep = name.eq_ignore_ascii_case("defs") || name.eq_ignore_ascii_case("style");
      if !keep {
        continue;
      }
      if let Some(slice) = svg_content.get(child.range()) {
        out.push_str(slice);
      }
    }

    out.push_str(inner);
    out.push_str("</svg>");
    return Cow::Owned(out);
  }

  let root_range = root.range();
  let Some(root_slice) = svg_content.get(root_range.clone()) else {
    return Cow::Borrowed(svg_content);
  };
  let Some(close_pos) = root_slice.rfind("</") else {
    return Cow::Borrowed(svg_content);
  };
  let insert_pos = root_range.start.saturating_add(close_pos);
  if insert_pos > svg_content.len() {
    return Cow::Borrowed(svg_content);
  }

  let escaped_id = escape_xml_attr_value(fragment);
  let mut out = String::with_capacity(svg_content.len().saturating_add(escaped_id.len() + 16));
  out.push_str(&svg_content[..insert_pos]);
  // Ensure the injected element is in the SVG namespace even when the document uses prefixed SVG
  // elements (no default `xmlns` in scope).
  out.push_str("<use xmlns=\"http://www.w3.org/2000/svg\" href=\"#");
  out.push_str(escaped_id.as_ref());
  out.push_str("\"/>");
  out.push_str(&svg_content[insert_pos..]);
  Cow::Owned(out)
}

fn svg_with_resolved_root_viewport_size<'a>(
  svg_content: &'a str,
  render_width: u32,
  render_height: u32,
) -> Cow<'a, str> {
  // When an SVG is used as an image (e.g. `background-image`), the embedding context provides a
  // concrete viewport size. If the outermost `<svg>` omits `width`/`height`, SVG defaults them to
  // `100%`. That means any percentage lengths (including nested `<image width="100%">`) should be
  // resolved against the concrete viewport size.
  //
  // `usvg`/`resvg` parse percentage lengths during tree construction. When no explicit viewport is
  // present, percentage lengths can resolve to zero, producing fully transparent output. Fix this
  // by injecting a definite `width`/`height` (in px) when rasterizing to an explicit size.
  if render_width == 0 || render_height == 0 {
    return Cow::Borrowed(svg_content);
  }

  let svg_for_parse = svg_markup_for_roxmltree(svg_content);
  let doc = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    roxmltree::Document::parse(svg_for_parse.as_ref())
  })) {
    Ok(Ok(doc)) => doc,
    Ok(Err(_)) | Err(_) => return Cow::Borrowed(svg_content),
  };

  let root = doc.root_element();
  if !root.tag_name().name().eq_ignore_ascii_case("svg") {
    return Cow::Borrowed(svg_content);
  }

  fn parse_percent(value: &str) -> Option<f32> {
    let trimmed = trim_ascii_whitespace(value);
    let number = trimmed.strip_suffix('%')?;
    trim_ascii_whitespace(number).parse::<f32>().ok()
  }

  fn format_px(value: f32) -> String {
    if !value.is_finite() {
      return "0".to_string();
    }
    let rounded = value.round();
    if (value - rounded).abs() < 0.000_1 {
      return format!("{}", rounded as i64);
    }
    value.to_string()
  }

  let width_attr_raw = root.attribute("width");
  let height_attr_raw = root.attribute("height");
  let width_trimmed = width_attr_raw.map(trim_ascii_whitespace).unwrap_or("");
  let height_trimmed = height_attr_raw.map(trim_ascii_whitespace).unwrap_or("");

  let width_needs_resolution =
    width_attr_raw.is_none() || width_trimmed.is_empty() || width_trimmed.ends_with('%');
  let height_needs_resolution =
    height_attr_raw.is_none() || height_trimmed.is_empty() || height_trimmed.ends_with('%');

  if !width_needs_resolution && !height_needs_resolution {
    return Cow::Borrowed(svg_content);
  }

  let width_resolved = if width_needs_resolution {
    let px = parse_percent(width_trimmed)
      .map(|pct| render_width as f32 * (pct / 100.0))
      .unwrap_or(render_width as f32);
    Some(format_px(px))
  } else {
    None
  };

  let height_resolved = if height_needs_resolution {
    let px = parse_percent(height_trimmed)
      .map(|pct| render_height as f32 * (pct / 100.0))
      .unwrap_or(render_height as f32);
    Some(format_px(px))
  } else {
    None
  };

  let root_range = root.range();
  if root_range.end > svg_content.len() || root_range.start >= root_range.end {
    return Cow::Borrowed(svg_content);
  }

  let Some(tag_end) = find_xml_start_tag_end(svg_content, root_range.start, root_range.end) else {
    return Cow::Borrowed(svg_content);
  };
  if tag_end > svg_content.len() || tag_end <= root_range.start {
    return Cow::Borrowed(svg_content);
  }

  let bytes_all = svg_content.as_bytes();
  let tag_close = tag_end.saturating_sub(1);
  let mut insert_pos = tag_close;
  let mut j = tag_close;
  while j > root_range.start && bytes_all[j.saturating_sub(1)].is_ascii_whitespace() {
    j = j.saturating_sub(1);
  }
  if j > root_range.start && bytes_all[j.saturating_sub(1)] == b'/' {
    insert_pos = j.saturating_sub(1);
  }

  let tag = &svg_content[root_range.start..tag_end];
  let bytes = tag.as_bytes();
  let mut i = 0usize;

  // Skip `<` + element name.
  if bytes.get(i) == Some(&b'<') {
    i += 1;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
      i += 1;
    }
    while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'>' && bytes[i] != b'/'
    {
      i += 1;
    }
  }

  let mut width_range: Option<std::ops::Range<usize>> = None;
  let mut height_range: Option<std::ops::Range<usize>> = None;

  while i < bytes.len() {
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
      i += 1;
    }
    if i >= bytes.len() || bytes[i] == b'>' {
      break;
    }
    if bytes[i] == b'/' {
      i += 1;
      continue;
    }

    let name_start = i;
    while i < bytes.len()
      && !bytes[i].is_ascii_whitespace()
      && bytes[i] != b'='
      && bytes[i] != b'>'
      && bytes[i] != b'/'
    {
      i += 1;
    }
    let name_end = i;
    if name_end == name_start {
      i = i.saturating_add(1);
      continue;
    }

    let attr_name = &tag[name_start..name_end];
    let local_name = attr_name
      .rsplit_once(':')
      .map(|(_, local)| local)
      .unwrap_or(attr_name);

    let is_width = local_name.eq_ignore_ascii_case("width");
    let is_height = local_name.eq_ignore_ascii_case("height");

    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
      i += 1;
    }
    if i >= bytes.len() || bytes[i] != b'=' {
      continue;
    }
    i += 1;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
      i += 1;
    }
    if i >= bytes.len() {
      continue;
    }

    let value_start = i;
    if bytes[i] == b'"' || bytes[i] == b'\'' {
      let quote = bytes[i];
      i += 1;
      let start = i;
      while i < bytes.len() && bytes[i] != quote {
        i += 1;
      }
      let value_end = i;
      if i < bytes.len() {
        i += 1;
      }

      let range = (root_range.start + start)..(root_range.start + value_end);
      if is_width {
        width_range = Some(range);
      } else if is_height {
        height_range = Some(range);
      }
      continue;
    }

    while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'>' && bytes[i] != b'/'
    {
      i += 1;
    }
    let value_end = i;
    let range = (root_range.start + value_start)..(root_range.start + value_end);
    if is_width {
      width_range = Some(range);
    } else if is_height {
      height_range = Some(range);
    }
  }

  let mut replacements: Vec<(std::ops::Range<usize>, String)> = Vec::new();
  let mut injected_bytes = 0usize;

  if let Some(width) = width_resolved {
    if width_attr_raw.is_some() {
      let Some(range) = width_range else {
        return Cow::Borrowed(svg_content);
      };
      injected_bytes = injected_bytes.saturating_add(
        width
          .len()
          .saturating_sub(range.end.saturating_sub(range.start)),
      );
      replacements.push((range, width));
    } else {
      let snippet = format!(" width=\"{width}\"");
      injected_bytes = injected_bytes.saturating_add(snippet.len());
      replacements.push((insert_pos..insert_pos, snippet));
    }
  }

  if let Some(height) = height_resolved {
    if height_attr_raw.is_some() {
      let Some(range) = height_range else {
        return Cow::Borrowed(svg_content);
      };
      injected_bytes = injected_bytes.saturating_add(
        height
          .len()
          .saturating_sub(range.end.saturating_sub(range.start)),
      );
      replacements.push((range, height));
    } else {
      let snippet = format!(" height=\"{height}\"");
      injected_bytes = injected_bytes.saturating_add(snippet.len());
      replacements.push((insert_pos..insert_pos, snippet));
    }
  }

  if replacements.is_empty() {
    return Cow::Borrowed(svg_content);
  }

  replacements.sort_by_key(|(range, _)| range.start);
  let mut out = String::with_capacity(svg_content.len().saturating_add(injected_bytes));
  let mut cursor = 0usize;
  for (range, replacement) in replacements {
    if range.start < cursor || range.end < range.start || range.end > svg_content.len() {
      return Cow::Borrowed(svg_content);
    }
    out.push_str(&svg_content[cursor..range.start]);
    out.push_str(&replacement);
    cursor = range.end;
  }
  out.push_str(&svg_content[cursor..]);
  Cow::Owned(out)
}

fn svg_parse_fill_color(value: &str) -> Option<Rgba> {
  let trimmed = trim_ascii_whitespace(value);
  if trimmed.is_empty() {
    return None;
  }
  if trimmed.eq_ignore_ascii_case("none") {
    return Some(Rgba::new(0, 0, 0, 0.0));
  }
  if trimmed.eq_ignore_ascii_case("currentColor") {
    return None;
  }

  // SVG paint attributes use CSS color syntax; do not apply HTML "legacy color value" parsing
  // here because values like `url(#gradient)` must not be coerced into arbitrary colors.
  fn parse_hex_color_strict(value: &str) -> Option<Rgba> {
    let value = value.strip_prefix('#').unwrap_or(value);
    if !value.chars().all(|c| c.is_ascii_hexdigit()) {
      return None;
    }
    match value.len() {
      6 => {
        let r = u8::from_str_radix(&value[0..2], 16).ok()?;
        let g = u8::from_str_radix(&value[2..4], 16).ok()?;
        let b = u8::from_str_radix(&value[4..6], 16).ok()?;
        Some(Rgba::rgb(r, g, b))
      }
      3 => {
        let r = u8::from_str_radix(&value[0..1], 16).ok()?;
        let g = u8::from_str_radix(&value[1..2], 16).ok()?;
        let b = u8::from_str_radix(&value[2..3], 16).ok()?;
        Some(Rgba::rgb(r * 17, g * 17, b * 17))
      }
      _ => None,
    }
  }

  if let Some(hex) = parse_hex_color_strict(trimmed) {
    return Some(hex);
  }
  trimmed.parse::<csscolorparser::Color>().ok().map(|c| {
    Rgba::new(
      (c.r * 255.0).round() as u8,
      (c.g * 255.0).round() as u8,
      (c.b * 255.0).round() as u8,
      c.a as f32,
    )
  })
}

fn multiply_alpha(mut color: Rgba, alpha: f32) -> Rgba {
  if !alpha.is_finite() {
    return color;
  }
  color.a = (color.a * alpha).clamp(0.0, 1.0);
  color
}

fn try_render_simple_svg_pixmap(
  svg_content: &str,
  render_width: u32,
  render_height: u32,
) -> std::result::Result<Option<Pixmap>, RenderError> {
  use tiny_skia::{FillRule, LineCap, LineJoin, Paint, Stroke, StrokeDash};

  if render_width == 0 || render_height == 0 {
    return Ok(None);
  }

  fn inherited_attr<'a>(node: roxmltree::Node<'a, 'a>, name: &str) -> Option<&'a str> {
    for ancestor in node.ancestors().filter(|n| n.is_element()) {
      if let Some(value) = ancestor.attribute(name) {
        let trimmed = trim_ascii_whitespace(value);
        if trimmed.eq_ignore_ascii_case("inherit") {
          continue;
        }
        return Some(trimmed);
      }
    }
    None
  }

  fn parse_svg_dash_array(value: &str) -> Option<Vec<f32>> {
    let trimmed = trim_ascii_whitespace(value);
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("none") {
      return Some(Vec::new());
    }

    let mut values = Vec::new();
    for part in trimmed
      .split(|c: char| {
        c == ',' || matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' ')
      })
      .filter(|s| !s.is_empty())
    {
      let v = parse_svg_length_px(part)?;
      if !v.is_finite() || v < 0.0 {
        return None;
      }
      values.push(v);
    }
    Some(values)
  }

  let mut deadline_counter = 0usize;
  check_root_periodic(
    &mut deadline_counter,
    IMAGE_DECODE_DEADLINE_STRIDE,
    RenderStage::Paint,
  )?;

  let svg_for_parse = svg_markup_for_roxmltree(svg_content);
  let doc = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    Document::parse(svg_for_parse.as_ref())
  })) {
    Ok(Ok(doc)) => doc,
    Ok(Err(_)) | Err(_) => return Ok(None),
  };
  let root = doc.root_element();
  let has_view_box_attr = root.attribute("viewBox").is_some();
  if !root.tag_name().name().eq_ignore_ascii_case("svg") {
    return Ok(None);
  }

  for node in root.descendants().filter(|n| n.is_element()) {
    check_root_periodic(
      &mut deadline_counter,
      IMAGE_DECODE_DEADLINE_STRIDE,
      RenderStage::Paint,
    )?;
    let name = node.tag_name().name();
    let allowed = matches!(name, "svg" | "g" | "path" | "title" | "desc" | "metadata");
    if !allowed {
      return Ok(None);
    }
    if node.attribute("transform").is_some()
      || node.attribute("filter").is_some()
      || node.attribute("mask").is_some()
      || node.attribute("clip-path").is_some()
      || node.attribute("style").is_some()
    {
      return Ok(None);
    }
    if name.eq_ignore_ascii_case("path") {
      if node.attribute("d").is_none() {
        return Ok(None);
      }
    } else if node.attribute("opacity").is_some() {
      // `opacity` on groups/root requires separate compositing that this fast-path does not
      // implement (and is uncommon for icon SVGs). Only allow `opacity` on leaf paths.
      return Ok(None);
    }
  }

  let view_box = root
    .attribute("viewBox")
    .and_then(parse_svg_view_box)
    .or_else(|| {
      let w = root.attribute("width").and_then(parse_svg_length_px)?;
      let h = root.attribute("height").and_then(parse_svg_length_px)?;
      Some(SvgViewBox {
        min_x: 0.0,
        min_y: 0.0,
        width: w,
        height: h,
      })
    })
    .unwrap_or(SvgViewBox {
      min_x: 0.0,
      min_y: 0.0,
      width: render_width as f32,
      height: render_height as f32,
    });
  if !(view_box.width.is_finite()
    && view_box.height.is_finite()
    && view_box.width > 0.0
    && view_box.height > 0.0)
  {
    return Ok(None);
  }

  let mut preserve = SvgPreserveAspectRatio::parse(root.attribute("preserveAspectRatio"));
  // Without a viewBox, the viewport and user coordinate systems are the same, so the
  // viewBox-to-viewport preserveAspectRatio mapping must be ignored (equivalent to `none`).
  if !has_view_box_attr {
    preserve.none = true;
  }
  let transform = map_svg_aspect_ratio(
    view_box,
    preserve,
    render_width as f32,
    render_height as f32,
  );

  let Some(mut pixmap) = new_pixmap(render_width, render_height) else {
    return Ok(None);
  };
  for node in root.descendants().filter(|n| n.is_element()) {
    check_root_periodic(
      &mut deadline_counter,
      IMAGE_DECODE_DEADLINE_STRIDE,
      RenderStage::Paint,
    )?;
    if !node.tag_name().name().eq_ignore_ascii_case("path") {
      continue;
    }
    let Some(d) = node.attribute("d") else {
      return Ok(None);
    };
    let path = match crate::svg_path::build_tiny_skia_path_from_svg_path_data(
      d,
      &mut deadline_counter,
      IMAGE_DECODE_DEADLINE_STRIDE,
      RenderStage::Paint,
    )? {
      Some(path) => path,
      None => return Ok(None),
    };

    let mut opacity = 1.0f32;
    if let Some(opacity_raw) = node.attribute("opacity") {
      if let Ok(alpha) = trim_ascii_whitespace(opacity_raw).parse::<f32>() {
        opacity = alpha;
      }
    }

    let fill_rule = match node.attribute("fill-rule").map(trim_ascii_whitespace) {
      None => FillRule::Winding,
      Some(v) if v.eq_ignore_ascii_case("nonzero") => FillRule::Winding,
      Some(v) if v.eq_ignore_ascii_case("evenodd") => FillRule::EvenOdd,
      Some(_) => return Ok(None),
    };

    let fill = inherited_attr(node, "fill");
    let mut fill_color = match fill {
      Some(v) => match svg_parse_fill_color(v) {
        Some(color) => color,
        None => return Ok(None),
      },
      None => Rgba::new(0, 0, 0, 1.0),
    };
    fill_color = multiply_alpha(fill_color, opacity);
    if let Some(opacity_raw) = inherited_attr(node, "fill-opacity") {
      if let Ok(alpha) = opacity_raw.parse::<f32>() {
        fill_color = multiply_alpha(fill_color, alpha);
      }
    }
    if fill_color.a > 0.0 {
      let mut paint = Paint::default();
      paint.set_color_rgba8(
        fill_color.r,
        fill_color.g,
        fill_color.b,
        fill_color.alpha_u8(),
      );
      paint.anti_alias = true;
      pixmap.fill_path(&path, &paint, fill_rule, transform, None);
    }

    let stroke = inherited_attr(node, "stroke");
    let mut stroke_color = match stroke {
      Some(v) => match svg_parse_fill_color(v) {
        Some(color) => color,
        None => return Ok(None),
      },
      None => Rgba::new(0, 0, 0, 0.0),
    };
    stroke_color = multiply_alpha(stroke_color, opacity);
    if let Some(opacity_raw) = inherited_attr(node, "stroke-opacity") {
      if let Ok(alpha) = opacity_raw.parse::<f32>() {
        stroke_color = multiply_alpha(stroke_color, alpha);
      }
    }
    if stroke_color.a > 0.0 {
      let stroke_width = inherited_attr(node, "stroke-width")
        .and_then(parse_svg_length_px)
        .unwrap_or(1.0);
      if stroke_width > 0.0 && stroke_width.is_finite() {
        let line_cap = match inherited_attr(node, "stroke-linecap") {
          None => LineCap::Butt,
          Some(v) if v.eq_ignore_ascii_case("butt") => LineCap::Butt,
          Some(v) if v.eq_ignore_ascii_case("round") => LineCap::Round,
          Some(v) if v.eq_ignore_ascii_case("square") => LineCap::Square,
          Some(_) => return Ok(None),
        };
        let line_join = match inherited_attr(node, "stroke-linejoin") {
          None => LineJoin::Miter,
          Some(v) if v.eq_ignore_ascii_case("miter") => LineJoin::Miter,
          Some(v) if v.eq_ignore_ascii_case("round") => LineJoin::Round,
          Some(v) if v.eq_ignore_ascii_case("bevel") => LineJoin::Bevel,
          Some(_) => return Ok(None),
        };
        let miter_limit = match inherited_attr(node, "stroke-miterlimit") {
          None => 4.0,
          Some(v) => match v.parse::<f32>() {
            Ok(val) if val.is_finite() && val > 0.0 => val,
            _ => return Ok(None),
          },
        };

        let mut dash = None;
        if let Some(raw) = inherited_attr(node, "stroke-dasharray") {
          let mut values = match parse_svg_dash_array(raw) {
            Some(values) => values,
            None => return Ok(None),
          };
          if !values.is_empty() {
            if values.iter().all(|v| *v == 0.0) {
              values.clear();
            }
          }
          if !values.is_empty() {
            if values.len() % 2 == 1 {
              let extra = values.clone();
              values.extend(extra);
            }
            let mut offset = 0.0;
            if let Some(raw) = inherited_attr(node, "stroke-dashoffset") {
              if let Some(v) = parse_svg_length_px(raw) {
                offset = v;
              }
            }
            dash = StrokeDash::new(values, offset);
          }
        }

        let mut stroke = Stroke::default();
        stroke.width = stroke_width;
        stroke.line_cap = line_cap;
        stroke.line_join = line_join;
        stroke.miter_limit = miter_limit;
        stroke.dash = dash;

        let mut paint = Paint::default();
        paint.set_color_rgba8(
          stroke_color.r,
          stroke_color.g,
          stroke_color.b,
          stroke_color.alpha_u8(),
        );
        paint.anti_alias = true;
        pixmap.stroke_path(&path, &paint, &stroke, transform, None);
      }
    }
  }

  Ok(Some(pixmap))
}

// ============================================================================
// CachedImage
// ============================================================================

/// Decoded image plus orientation metadata.
pub struct CachedImage {
  pub image: Arc<DynamicImage>,
  pub orientation: Option<OrientationTransform>,
  /// Resolution in image pixels per CSS px (dppx) when provided by metadata.
  pub resolution: Option<f32>,
  /// True when the source image is animated (e.g. multi-frame GIF).
  ///
  /// Note: `CachedImage` stores a single decoded frame for formats like GIF, but callers may still
  /// need to know whether the underlying resource is animated so they can schedule repaints/ticks.
  pub is_animated: bool,
  /// True when the decoded source image contains alpha information.
  ///
  /// Note: Some bitmap decoders normalize to RGBA (adding an opaque alpha channel) even when the
  /// original image format did not include alpha. This flag preserves whether the source image
  /// actually provided alpha so features like `mask-mode: match-source` can distinguish
  /// alpha-masks vs luminance-masks.
  pub has_alpha: bool,
  /// Whether this image originated from a vector source (SVG).
  pub is_vector: bool,
  /// Intrinsic aspect ratio when known. SVGs that opt out of aspect-ratio preservation keep this
  /// as `None` and set `aspect_ratio_none` to true.
  pub intrinsic_ratio: Option<f32>,
  /// True when the resource explicitly disables aspect-ratio preservation (e.g., SVG
  /// `preserveAspectRatio="none"`).
  pub aspect_ratio_none: bool,
  /// Raw SVG markup when the image originated from a vector source.
  pub svg_content: Option<Arc<str>>,
  /// For SVG images, whether the root element specified an absolute width or height, giving the
  /// image an intrinsic size in CSS px.
  ///
  /// SVG images that only provide a `viewBox` have an intrinsic *ratio* but no intrinsic *size*.
  /// Browsers treat them differently depending on the context:
  /// - replaced elements (e.g. `<img>`) fall back to a default object size (300×150) constrained by
  ///   the intrinsic ratio
  /// - CSS images (`background-image`, `mask-image`) treat the natural size as missing (so
  ///   `*-size: auto` behaves like `contain` when only an intrinsic ratio is present)
  ///
  /// FastRender stores a synthesized raster size for vector sources so they can be decoded into a
  /// `DynamicImage`, but this flag allows callers to distinguish "real" intrinsic sizes from that
  /// fallback when implementing CSS image sizing algorithms.
  pub svg_has_intrinsic_size: bool,
}

impl CachedImage {
  pub fn dimensions(&self) -> (u32, u32) {
    self.image.dimensions()
  }

  pub fn width(&self) -> u32 {
    self.image.width()
  }

  pub fn height(&self) -> u32 {
    self.image.height()
  }

  pub fn oriented_dimensions(&self, transform: OrientationTransform) -> (u32, u32) {
    let (w, h) = self.dimensions();
    transform.oriented_dimensions(w, h)
  }

  /// Computes CSS pixel dimensions after applying orientation and the provided image-resolution.
  pub fn css_dimensions(
    &self,
    transform: OrientationTransform,
    resolution: &ImageResolution,
    device_pixel_ratio: f32,
    override_resolution: Option<f32>,
  ) -> Option<(f32, f32)> {
    let (w, h) = self.oriented_dimensions(transform);
    if w == 0 || h == 0 {
      return None;
    }
    if self.is_vector {
      return Some((w as f32, h as f32));
    }
    let used = resolution.used_resolution(override_resolution, self.resolution, device_pixel_ratio);
    if used <= 0.0 || !used.is_finite() {
      return None;
    }
    Some((w as f32 / used, h as f32 / used))
  }

  /// Natural image size for CSS images (backgrounds/masks).
  ///
  /// Unlike `css_dimensions`, this returns `None` for SVGs that do not specify an intrinsic size
  /// (e.g. viewBox-only SVGs). Those images still have an intrinsic ratio, so callers should pair
  /// this with [`Self::intrinsic_ratio`] when resolving `*-size: auto`.
  pub fn css_natural_dimensions(
    &self,
    transform: OrientationTransform,
    resolution: &ImageResolution,
    device_pixel_ratio: f32,
    override_resolution: Option<f32>,
  ) -> (Option<f32>, Option<f32>) {
    if self.is_vector && !self.svg_has_intrinsic_size {
      return (None, None);
    }
    match self.css_dimensions(
      transform,
      resolution,
      device_pixel_ratio,
      override_resolution,
    ) {
      Some((w, h)) => (Some(w), Some(h)),
      None => (None, None),
    }
  }

  /// Intrinsic aspect ratio, adjusted for EXIF orientation when present.
  pub fn intrinsic_ratio(&self, transform: OrientationTransform) -> Option<f32> {
    if self.aspect_ratio_none {
      return None;
    }

    let mut ratio = self.intrinsic_ratio;
    if ratio.is_none() {
      // The stored pixel dimensions are in the *unoriented* image space. Apply EXIF/CSS rotation
      // by swapping the ratio when the transform rotates by 90/270 degrees.
      let (w, h) = self.dimensions();
      if h > 0 {
        ratio = Some(w as f32 / h as f32);
      }
    }

    if let Some(r) = ratio {
      if transform.quarter_turns % 2 == 1 {
        return Some(1.0 / r);
      }
    }

    ratio
  }

  pub fn to_oriented_rgba(&self, transform: OrientationTransform) -> RgbaImage {
    let mut rgba = self.image.to_rgba8();

    match transform.quarter_turns % 4 {
      0 => {}
      1 => rgba = imageops::rotate90(&rgba),
      2 => rgba = imageops::rotate180(&rgba),
      3 => rgba = imageops::rotate270(&rgba),
      _ => {}
    }

    if transform.flip_x {
      rgba = imageops::flip_horizontal(&rgba);
    }

    rgba
  }
}

#[derive(Debug, Clone)]
pub struct CachedImageMetadata {
  pub width: u32,
  pub height: u32,
  pub orientation: Option<OrientationTransform>,
  pub resolution: Option<f32>,
  pub is_vector: bool,
  /// True when the source image is animated (e.g. multi-frame GIF).
  ///
  /// This is populated by [`ImageCache::probe`] so upstream systems can schedule periodic ticks
  /// without fully decoding the image.
  pub is_animated: bool,
  pub intrinsic_ratio: Option<f32>,
  pub aspect_ratio_none: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct DiskCachedOrientationTransformV1 {
  quarter_turns: u8,
  flip_x: bool,
}

impl From<OrientationTransform> for DiskCachedOrientationTransformV1 {
  fn from(value: OrientationTransform) -> Self {
    Self {
      quarter_turns: value.quarter_turns,
      flip_x: value.flip_x,
    }
  }
}

impl From<DiskCachedOrientationTransformV1> for OrientationTransform {
  fn from(value: DiskCachedOrientationTransformV1) -> Self {
    Self {
      quarter_turns: value.quarter_turns,
      flip_x: value.flip_x,
    }
  }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DiskCachedImageProbeMetadataV1 {
  width: u32,
  height: u32,
  orientation: Option<DiskCachedOrientationTransformV1>,
  resolution: Option<f32>,
  is_vector: bool,
  intrinsic_ratio: Option<f32>,
  aspect_ratio_none: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DiskCachedImageProbeMetadataV2 {
  width: u32,
  height: u32,
  orientation: Option<DiskCachedOrientationTransformV1>,
  resolution: Option<f32>,
  is_vector: bool,
  is_animated: bool,
  intrinsic_ratio: Option<f32>,
  aspect_ratio_none: bool,
}

impl From<&CachedImageMetadata> for DiskCachedImageProbeMetadataV2 {
  fn from(meta: &CachedImageMetadata) -> Self {
    Self {
      width: meta.width,
      height: meta.height,
      orientation: meta.orientation.map(Into::into),
      resolution: meta.resolution,
      is_vector: meta.is_vector,
      is_animated: meta.is_animated,
      intrinsic_ratio: meta.intrinsic_ratio,
      aspect_ratio_none: meta.aspect_ratio_none,
    }
  }
}

impl From<DiskCachedImageProbeMetadataV2> for CachedImageMetadata {
  fn from(meta: DiskCachedImageProbeMetadataV2) -> Self {
    Self {
      width: meta.width,
      height: meta.height,
      orientation: meta.orientation.map(Into::into),
      resolution: meta.resolution,
      is_vector: meta.is_vector,
      is_animated: meta.is_animated,
      intrinsic_ratio: meta.intrinsic_ratio,
      aspect_ratio_none: meta.aspect_ratio_none,
    }
  }
}

fn encode_probe_metadata_for_disk(meta: &CachedImageMetadata) -> Option<Vec<u8>> {
  serde_json::to_vec(&DiskCachedImageProbeMetadataV2::from(meta)).ok()
}

fn decode_probe_metadata_from_disk(bytes: &[u8]) -> Option<CachedImageMetadata> {
  serde_json::from_slice::<DiskCachedImageProbeMetadataV2>(bytes)
    .ok()
    .map(Into::into)
    .or_else(|| {
      // Backward compatibility for older on-disk probe metadata entries.
      serde_json::from_slice::<DiskCachedImageProbeMetadataV1>(bytes)
        .ok()
        .map(|meta| CachedImageMetadata {
          width: meta.width,
          height: meta.height,
          orientation: meta.orientation.map(Into::into),
          resolution: meta.resolution,
          is_vector: meta.is_vector,
          is_animated: false,
          intrinsic_ratio: meta.intrinsic_ratio,
          aspect_ratio_none: meta.aspect_ratio_none,
        })
    })
}

impl CachedImageMetadata {
  pub fn dimensions(&self) -> (u32, u32) {
    (self.width, self.height)
  }

  pub fn oriented_dimensions(&self, transform: OrientationTransform) -> (u32, u32) {
    transform.oriented_dimensions(self.width, self.height)
  }

  pub fn css_dimensions(
    &self,
    transform: OrientationTransform,
    resolution: &ImageResolution,
    device_pixel_ratio: f32,
    override_resolution: Option<f32>,
  ) -> Option<(f32, f32)> {
    let (w, h) = self.oriented_dimensions(transform);
    if w == 0 || h == 0 {
      return None;
    }
    if self.is_vector {
      return Some((w as f32, h as f32));
    }
    let used = resolution.used_resolution(override_resolution, self.resolution, device_pixel_ratio);
    if used <= 0.0 || !used.is_finite() {
      return None;
    }
    Some((w as f32 / used, h as f32 / used))
  }

  pub fn intrinsic_ratio(&self, transform: OrientationTransform) -> Option<f32> {
    if self.aspect_ratio_none {
      return None;
    }

    let mut ratio = self.intrinsic_ratio;
    if ratio.is_none() {
      let (w, h) = self.dimensions();
      if h > 0 {
        ratio = Some(w as f32 / h as f32);
      }
    }

    if let Some(r) = ratio {
      if transform.quarter_turns % 2 == 1 {
        return Some(1.0 / r);
      }
    }

    ratio
  }
}

impl From<&CachedImage> for CachedImageMetadata {
  fn from(image: &CachedImage) -> Self {
    let (width, height) = image.dimensions();
    Self {
      width,
      height,
      orientation: image.orientation,
      resolution: image.resolution,
      is_vector: image.is_vector,
      is_animated: image.is_animated,
      intrinsic_ratio: image.intrinsic_ratio,
      aspect_ratio_none: image.aspect_ratio_none,
    }
  }
}

fn is_about_url(url: &str) -> bool {
  let trimmed = trim_ascii_whitespace_start(url);
  trimmed
    .get(..6)
    .is_some_and(|prefix| prefix.eq_ignore_ascii_case("about:"))
}

fn url_ends_with_svgz(url: &str) -> bool {
  let trimmed = trim_ascii_whitespace(url);
  if trimmed.is_empty() {
    return false;
  }
  let base = trimmed
    .split(|c: char| c == '?' || c == '#')
    .next()
    .unwrap_or(trimmed);
  base
    .get(base.len().saturating_sub(5)..)
    .is_some_and(|suffix| suffix.eq_ignore_ascii_case(".svgz"))
}

fn svg_text_looks_like_markup(text: &str) -> bool {
  let without_bom = text.strip_prefix('\u{feff}').unwrap_or(text);
  let trimmed = trim_ascii_whitespace_start(without_bom);
  trimmed
    .get(..4)
    .is_some_and(|prefix| prefix.eq_ignore_ascii_case("<svg"))
    || trimmed
      .get(..5)
      .is_some_and(|prefix| prefix.eq_ignore_ascii_case("<?xml"))
}

fn about_url_placeholder_image() -> Arc<CachedImage> {
  static PLACEHOLDER: OnceLock<Arc<CachedImage>> = OnceLock::new();
  Arc::clone(PLACEHOLDER.get_or_init(|| {
    // A 1×1 fully transparent RGBA buffer so layout/paint can proceed deterministically for
    // non-fetchable `about:` URLs (e.g. `about:blank`).
    let img = RgbaImage::new(1, 1);
    Arc::new(CachedImage {
      image: Arc::new(DynamicImage::ImageRgba8(img)),
      orientation: None,
      resolution: None,
      is_animated: false,
      has_alpha: true,
      is_vector: false,
      intrinsic_ratio: None,
      aspect_ratio_none: false,
      svg_content: None,
      svg_has_intrinsic_size: true,
    })
  }))
}

fn about_url_placeholder_metadata() -> Arc<CachedImageMetadata> {
  static PLACEHOLDER_META: OnceLock<Arc<CachedImageMetadata>> = OnceLock::new();
  Arc::clone(
    PLACEHOLDER_META
      .get_or_init(|| Arc::new(CachedImageMetadata::from(&*about_url_placeholder_image()))),
  )
}

fn about_url_placeholder_pixmap() -> Result<Arc<tiny_skia::Pixmap>> {
  static PLACEHOLDER_PIXMAP: OnceLock<std::result::Result<Arc<tiny_skia::Pixmap>, RenderError>> =
    OnceLock::new();
  match PLACEHOLDER_PIXMAP.get_or_init(|| {
    // `new_pixmap_with_context` returns a zeroed RGBA buffer, which is already a premultiplied
    // fully-transparent pixel for tiny-skia.
    new_pixmap_with_context(1, 1, "about URL placeholder pixmap").map(Arc::new)
  }) {
    Ok(pixmap) => Ok(Arc::clone(pixmap)),
    Err(err) => Err(Error::Render(err.clone())),
  }
}

fn status_is_http_success(status: Option<u16>) -> bool {
  matches!(status, Some(200..=299))
}

fn is_empty_body_error_for_image(error: &Error) -> bool {
  matches!(
    error,
    Error::Resource(res)
      if status_is_http_success(res.status) && res.message.contains("empty HTTP response body")
  )
}

fn payload_looks_like_markup_but_not_svg(bytes: &[u8]) -> bool {
  let sample = &bytes[..bytes.len().min(256)];
  let mut i = 0;
  if sample.starts_with(b"\xef\xbb\xbf") {
    i = 3;
  }
  while i < sample.len() && sample[i].is_ascii_whitespace() {
    i += 1;
  }
  let rest = &sample[i..];
  if rest.is_empty() || rest[0] != b'<' {
    return false;
  }

  // Accept common SVG/XML prologs so that SVG documents aren't mistaken for HTML.
  if rest.len() >= 4
    && rest[0] == b'<'
    && rest[1].to_ascii_lowercase() == b's'
    && rest[2].to_ascii_lowercase() == b'v'
    && rest[3].to_ascii_lowercase() == b'g'
  {
    return false;
  }
  if rest.len() >= 5
    && rest[0] == b'<'
    && rest[1] == b'?'
    && rest[2].to_ascii_lowercase() == b'x'
    && rest[3].to_ascii_lowercase() == b'm'
    && rest[4].to_ascii_lowercase() == b'l'
  {
    return false;
  }
  if rest.len() >= 10
    && rest[0] == b'<'
    && rest[1] == b'!'
    && rest[2].to_ascii_lowercase() == b'd'
    && rest[3].to_ascii_lowercase() == b'o'
    && rest[4].to_ascii_lowercase() == b'c'
    && rest[5].to_ascii_lowercase() == b't'
    && rest[6].to_ascii_lowercase() == b'y'
    && rest[7].to_ascii_lowercase() == b'p'
    && rest[8].to_ascii_lowercase() == b'e'
  {
    let mut j = 9;
    while j < rest.len() && rest[j].is_ascii_whitespace() {
      j += 1;
    }
    if rest.len().saturating_sub(j) >= 3
      && rest[j].to_ascii_lowercase() == b's'
      && rest[j + 1].to_ascii_lowercase() == b'v'
      && rest[j + 2].to_ascii_lowercase() == b'g'
    {
      return false;
    }
  }

  true
}

fn should_substitute_markup_payload_for_image(
  requested_url: &str,
  final_url: Option<&str>,
  status: Option<u16>,
  bytes: &[u8],
) -> bool {
  if !status_is_http_success(status) || !payload_looks_like_markup_but_not_svg(bytes) {
    return false;
  }
  let final_url = final_url.unwrap_or(requested_url);
  // Avoid masking "HTML returned for a real image URL" cases (common bot-mitigation behavior).
  // For URLs that look like real image assets, prefer surfacing a `ResourceError` via
  // `ensure_image_mime_sane` over silently substituting a placeholder.
  if crate::resource::url_looks_like_image_asset(requested_url)
    || crate::resource::url_looks_like_image_asset(final_url)
  {
    return false;
  }

  true
}

// ============================================================================
// ImageCache
// ============================================================================

/// Configuration for [`ImageCache`].
#[derive(Debug, Clone, Copy)]
pub struct ImageCacheConfig {
  /// Maximum number of decoded pixels (width * height). `0` disables the limit.
  pub max_decoded_pixels: u64,
  /// Maximum allowed width or height for a decoded image. `0` disables the limit.
  pub max_decoded_dimension: u32,
  /// Maximum number of decoded images kept in memory (`0` disables eviction by count).
  pub max_cached_images: usize,
  /// Maximum estimated bytes of decoded images kept in memory (`0` disables eviction by size).
  pub max_cached_image_bytes: usize,
  /// Maximum number of rasterized SVG pixmaps kept in memory (`0` disables eviction by count).
  pub max_cached_svg_pixmaps: usize,
  /// Maximum estimated bytes of cached SVG pixmaps (`0` disables eviction by size).
  pub max_cached_svg_bytes: usize,
  /// Maximum number of cached SVG preprocessing results (`0` disables eviction by count).
  pub max_cached_svg_preprocess_items: usize,
  /// Maximum estimated bytes of cached SVG preprocessing results (`0` disables eviction by size).
  pub max_cached_svg_preprocess_bytes: usize,
  /// Maximum number of cached SVG subresources (inlined data URLs, external sprites, ...) (`0`
  /// disables eviction by count).
  pub max_cached_svg_subresource_items: usize,
  /// Maximum estimated bytes of cached SVG subresources (`0` disables eviction by size).
  pub max_cached_svg_subresource_bytes: usize,
  /// Maximum number of cached premultiplied raster pixmaps (`0` disables eviction by count).
  pub max_cached_raster_pixmaps: usize,
  /// Maximum estimated bytes of cached raster pixmaps (`0` disables eviction by size).
  pub max_cached_raster_bytes: usize,
  /// Maximum number of cached image probe metadata entries (`0` disables eviction by count).
  pub max_cached_metadata_items: usize,
  /// Maximum estimated bytes of cached image probe metadata (`0` disables eviction by size).
  pub max_cached_metadata_bytes: usize,
  /// Maximum number of raw image resources cached between `probe()` and `load()` (`0` disables
  /// eviction by count).
  pub max_raw_cached_items: usize,
  /// Maximum estimated bytes of raw image resources cached between `probe()` and `load()` (`0`
  /// disables eviction by size).
  pub max_raw_cached_bytes: usize,
}

impl Default for ImageCacheConfig {
  fn default() -> Self {
    const DEFAULT_MAX_METADATA_CACHE_ITEMS: usize = 2_000;
    const DEFAULT_MAX_METADATA_CACHE_BYTES: usize = 16 * 1024 * 1024;
    const DEFAULT_MAX_RAW_CACHE_ITEMS: usize = 64;
    const DEFAULT_MAX_RAW_CACHE_BYTES: usize = 64 * 1024 * 1024;
    const DEFAULT_MAX_RASTER_PIXMAP_CACHE_ITEMS: usize = 256;
    const DEFAULT_MAX_RASTER_PIXMAP_CACHE_BYTES: usize = 128 * 1024 * 1024;
    const DEFAULT_MAX_SVG_PREPROCESS_CACHE_ITEMS: usize = 64;
    const DEFAULT_MAX_SVG_PREPROCESS_CACHE_BYTES: usize = 32 * 1024 * 1024;
    const DEFAULT_MAX_SVG_SUBRESOURCE_CACHE_ITEMS: usize = 64;
    const DEFAULT_MAX_SVG_SUBRESOURCE_CACHE_BYTES: usize = 64 * 1024 * 1024;

    let toggles = runtime::runtime_toggles();
    let max_cached_metadata_items = toggles
      .usize("FASTR_IMAGE_META_CACHE_ITEMS")
      .unwrap_or(DEFAULT_MAX_METADATA_CACHE_ITEMS);
    let max_cached_metadata_bytes = toggles
      .usize("FASTR_IMAGE_META_CACHE_BYTES")
      .unwrap_or(DEFAULT_MAX_METADATA_CACHE_BYTES);
    let max_raw_cached_items = toggles
      .usize("FASTR_IMAGE_RAW_CACHE_ITEMS")
      .unwrap_or(DEFAULT_MAX_RAW_CACHE_ITEMS);
    let max_raw_cached_bytes = toggles
      .usize("FASTR_IMAGE_RAW_CACHE_BYTES")
      .unwrap_or(DEFAULT_MAX_RAW_CACHE_BYTES);

    let max_cached_raster_pixmaps = toggles
      .usize("FASTR_IMAGE_RASTER_PIXMAP_CACHE_ITEMS")
      .unwrap_or(DEFAULT_MAX_RASTER_PIXMAP_CACHE_ITEMS);
    let max_cached_raster_bytes = toggles
      .usize("FASTR_IMAGE_RASTER_PIXMAP_CACHE_BYTES")
      .unwrap_or(DEFAULT_MAX_RASTER_PIXMAP_CACHE_BYTES);

    let max_cached_svg_preprocess_items = toggles
      .usize("FASTR_IMAGE_SVG_PREPROCESS_CACHE_ITEMS")
      .unwrap_or(DEFAULT_MAX_SVG_PREPROCESS_CACHE_ITEMS);
    let max_cached_svg_preprocess_bytes = toggles
      .usize("FASTR_IMAGE_SVG_PREPROCESS_CACHE_BYTES")
      .unwrap_or(DEFAULT_MAX_SVG_PREPROCESS_CACHE_BYTES);
    let max_cached_svg_subresource_items = toggles
      .usize("FASTR_IMAGE_SVG_SUBRESOURCE_CACHE_ITEMS")
      .unwrap_or(DEFAULT_MAX_SVG_SUBRESOURCE_CACHE_ITEMS);
    let max_cached_svg_subresource_bytes = toggles
      .usize("FASTR_IMAGE_SVG_SUBRESOURCE_CACHE_BYTES")
      .unwrap_or(DEFAULT_MAX_SVG_SUBRESOURCE_CACHE_BYTES);

    Self {
      max_decoded_pixels: 100_000_000,
      max_decoded_dimension: 32768,
      max_cached_images: 256,
      max_cached_image_bytes: 256 * 1024 * 1024,
      max_cached_svg_pixmaps: 128,
      max_cached_svg_bytes: 128 * 1024 * 1024,
      max_cached_svg_preprocess_items,
      max_cached_svg_preprocess_bytes,
      max_cached_svg_subresource_items,
      max_cached_svg_subresource_bytes,
      max_cached_raster_pixmaps,
      max_cached_raster_bytes,
      max_cached_metadata_items,
      max_cached_metadata_bytes,
      max_raw_cached_items,
      max_raw_cached_bytes,
    }
  }
}

impl ImageCacheConfig {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn with_max_decoded_pixels(mut self, max: u64) -> Self {
    self.max_decoded_pixels = max;
    self
  }

  pub fn with_max_decoded_dimension(mut self, max: u32) -> Self {
    self.max_decoded_dimension = max;
    self
  }

  pub fn with_max_cached_images(mut self, max: usize) -> Self {
    self.max_cached_images = max;
    self
  }

  pub fn with_max_cached_image_bytes(mut self, max: usize) -> Self {
    self.max_cached_image_bytes = max;
    self
  }

  pub fn with_max_cached_svg_pixmaps(mut self, max: usize) -> Self {
    self.max_cached_svg_pixmaps = max;
    self
  }

  pub fn with_max_cached_svg_bytes(mut self, max: usize) -> Self {
    self.max_cached_svg_bytes = max;
    self
  }

  pub fn with_max_cached_svg_preprocess_items(mut self, max: usize) -> Self {
    self.max_cached_svg_preprocess_items = max;
    self
  }

  pub fn with_max_cached_svg_preprocess_bytes(mut self, max: usize) -> Self {
    self.max_cached_svg_preprocess_bytes = max;
    self
  }

  pub fn with_max_cached_svg_subresource_items(mut self, max: usize) -> Self {
    self.max_cached_svg_subresource_items = max;
    self
  }

  pub fn with_max_cached_svg_subresource_bytes(mut self, max: usize) -> Self {
    self.max_cached_svg_subresource_bytes = max;
    self
  }

  pub fn with_max_cached_raster_pixmaps(mut self, max: usize) -> Self {
    self.max_cached_raster_pixmaps = max;
    self
  }

  pub fn with_max_cached_raster_bytes(mut self, max: usize) -> Self {
    self.max_cached_raster_bytes = max;
    self
  }

  pub fn with_max_cached_metadata_items(mut self, max: usize) -> Self {
    self.max_cached_metadata_items = max;
    self
  }

  pub fn with_max_cached_metadata_bytes(mut self, max: usize) -> Self {
    self.max_cached_metadata_bytes = max;
    self
  }

  pub fn with_max_raw_cached_items(mut self, max: usize) -> Self {
    self.max_raw_cached_items = max;
    self
  }

  pub fn with_max_raw_cached_bytes(mut self, max: usize) -> Self {
    self.max_raw_cached_bytes = max;
    self
  }
}

#[derive(Clone)]
enum SharedImageResult {
  Success(Arc<CachedImage>),
  Error(Error),
}

impl SharedImageResult {
  fn as_result(&self) -> Result<Arc<CachedImage>> {
    match self {
      Self::Success(img) => Ok(Arc::clone(img)),
      Self::Error(err) => Err(err.clone()),
    }
  }
}

struct DecodeInFlight {
  result: Mutex<Option<SharedImageResult>>,
  cv: Condvar,
}

impl DecodeInFlight {
  fn new() -> Self {
    Self {
      result: Mutex::new(None),
      cv: Condvar::new(),
    }
  }

  fn set(&self, result: SharedImageResult) {
    let mut slot = self
      .result
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    *slot = Some(result);
    self.cv.notify_all();
  }

  fn wait(&self, _url: &str) -> Result<Arc<CachedImage>> {
    let mut guard = self
      .result
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    let deadline = render_control::root_deadline().filter(|d| d.is_enabled());
    while guard.is_none() {
      if let Some(deadline) = deadline.as_ref() {
        deadline.check(RenderStage::Paint).map_err(Error::Render)?;
        let wait_for = if deadline.timeout_limit().is_some() {
          match deadline.remaining_timeout() {
            Some(remaining) if !remaining.is_zero() => remaining.min(Duration::from_millis(10)),
            _ => {
              return Err(Error::Render(RenderError::Timeout {
                stage: RenderStage::Paint,
                elapsed: deadline.elapsed(),
              }));
            }
          }
        } else {
          Duration::from_millis(10)
        };
        guard = self
          .cv
          .wait_timeout(guard, wait_for)
          .unwrap_or_else(|poisoned| poisoned.into_inner())
          .0;
      } else {
        guard = self
          .cv
          .wait(guard)
          .unwrap_or_else(|poisoned| poisoned.into_inner());
      }
    }
    guard.as_ref().unwrap().as_result()
  }
}

#[derive(Clone)]
enum SharedMetaResult {
  Success(Arc<CachedImageMetadata>),
  Error(Error),
}

impl SharedMetaResult {
  fn as_result(&self) -> Result<Arc<CachedImageMetadata>> {
    match self {
      Self::Success(meta) => Ok(Arc::clone(meta)),
      Self::Error(err) => Err(err.clone()),
    }
  }
}

struct ProbeInFlight {
  result: Mutex<Option<SharedMetaResult>>,
  cv: Condvar,
}

impl ProbeInFlight {
  fn new() -> Self {
    Self {
      result: Mutex::new(None),
      cv: Condvar::new(),
    }
  }

  fn set(&self, result: SharedMetaResult) {
    let mut slot = self
      .result
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    *slot = Some(result);
    self.cv.notify_all();
  }

  fn wait(&self, _url: &str) -> Result<Arc<CachedImageMetadata>> {
    let mut guard = self
      .result
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    let deadline = render_control::root_deadline().filter(|d| d.is_enabled());
    while guard.is_none() {
      if let Some(deadline) = deadline.as_ref() {
        deadline.check(RenderStage::Paint).map_err(Error::Render)?;
        let wait_for = if deadline.timeout_limit().is_some() {
          match deadline.remaining_timeout() {
            Some(remaining) if !remaining.is_zero() => remaining.min(Duration::from_millis(10)),
            _ => {
              return Err(Error::Render(RenderError::Timeout {
                stage: RenderStage::Paint,
                elapsed: deadline.elapsed(),
              }));
            }
          }
        } else {
          Duration::from_millis(10)
        };
        guard = self
          .cv
          .wait_timeout(guard, wait_for)
          .unwrap_or_else(|poisoned| poisoned.into_inner())
          .0;
      } else {
        guard = self
          .cv
          .wait(guard)
          .unwrap_or_else(|poisoned| poisoned.into_inner());
      }
    }
    guard.as_ref().unwrap().as_result()
  }
}

#[derive(Debug, Clone)]
struct GifTiming {
  delays_ms: Vec<u32>,
  total_ms: u64,
  loop_count: Option<u16>,
}

impl GifTiming {
  fn parse(bytes: &[u8]) -> Option<Self> {
    const MAX_GIF_TIMING_FRAMES: usize = 4096;

    if bytes.len() < 13 {
      return None;
    }
    let header = bytes.get(0..6)?;
    if header != b"GIF87a" && header != b"GIF89a" {
      return None;
    }

    // Logical Screen Descriptor starts at byte 6.
    let packed = *bytes.get(10)?;
    let mut offset = 13usize;

    // Skip global color table if present.
    if packed & 0x80 != 0 {
      let table_bits = (packed & 0x07) as usize;
      let entries = 1usize.checked_shl((table_bits + 1) as u32)?;
      let table_bytes = 3usize.checked_mul(entries)?;
      offset = offset.checked_add(table_bytes)?;
      if offset > bytes.len() {
        return None;
      }
    }

    let mut delays_ms = Vec::new();
    let mut total_ms: u64 = 0;
    let mut loop_count: Option<u16> = None;
    let mut pending_delay: Option<u16> = None;

    while offset < bytes.len() {
      match *bytes.get(offset)? {
        0x3B => break, // trailer
        0x21 => {
          // Extension introducer.
          let label = *bytes.get(offset + 1)?;
          offset = offset.checked_add(2)?;
          match label {
            0xF9 => {
              // Graphics Control Extension.
              let block_size = *bytes.get(offset)? as usize;
              offset = offset.checked_add(1)?;
              let end = offset.checked_add(block_size)?;
              if end > bytes.len() {
                return None;
              }
              if block_size >= 4 {
                let delay_bytes: [u8; 2] = bytes.get(offset + 1..offset + 3)?.try_into().ok()?;
                pending_delay = Some(u16::from_le_bytes(delay_bytes));
              }
              offset = end;
              if *bytes.get(offset)? != 0x00 {
                return None;
              }
              offset = offset.checked_add(1)?;
            }
            0xFF => {
              // Application Extension. Track Netscape loop count when present.
              let block_size = *bytes.get(offset)? as usize;
              offset = offset.checked_add(1)?;
              let end = offset.checked_add(block_size)?;
              if end > bytes.len() {
                return None;
              }
              let ident = bytes.get(offset..end)?;
              offset = end;

              loop {
                let size = *bytes.get(offset)? as usize;
                offset = offset.checked_add(1)?;
                if size == 0 {
                  break;
                }
                let payload_end = offset.checked_add(size)?;
                if payload_end > bytes.len() {
                  return None;
                }
                if loop_count.is_none()
                  && (ident == b"NETSCAPE2.0" || ident == b"ANIMEXTS1.0")
                  && size >= 3
                  && bytes.get(offset) == Some(&0x01)
                {
                  let loop_bytes: [u8; 2] = bytes.get(offset + 1..offset + 3)?.try_into().ok()?;
                  loop_count = Some(u16::from_le_bytes(loop_bytes));
                }
                offset = payload_end;
              }
            }
            _ => {
              // Skip data sub-blocks.
              loop {
                let size = *bytes.get(offset)? as usize;
                offset = offset.checked_add(1)?;
                if size == 0 {
                  break;
                }
                offset = offset.checked_add(size)?;
                if offset > bytes.len() {
                  return None;
                }
              }
            }
          }
        }
        0x2C => {
          // Image descriptor.
          let desc_end = offset.checked_add(10)?;
          if desc_end > bytes.len() {
            return None;
          }
          let packed = *bytes.get(offset + 9)?;
          offset = desc_end;
          if packed & 0x80 != 0 {
            let table_bits = (packed & 0x07) as usize;
            let entries = 1usize.checked_shl((table_bits + 1) as u32)?;
            let table_bytes = 3usize.checked_mul(entries)?;
            offset = offset.checked_add(table_bytes)?;
            if offset > bytes.len() {
              return None;
            }
          }

          // LZW minimum code size.
          offset = offset.checked_add(1)?;
          if offset > bytes.len() {
            return None;
          }

          // Image data sub-blocks.
          loop {
            let size = *bytes.get(offset)? as usize;
            offset = offset.checked_add(1)?;
            if size == 0 {
              break;
            }
            offset = offset.checked_add(size)?;
            if offset > bytes.len() {
              return None;
            }
          }

          if delays_ms.len() >= MAX_GIF_TIMING_FRAMES {
            return None;
          }

          let delay_cs = pending_delay.take().unwrap_or(0).max(1);
          let delay_ms_u64 = u64::from(delay_cs) * 10;
          total_ms = total_ms.saturating_add(delay_ms_u64);
          delays_ms.push(delay_ms_u64.min(u64::from(u32::MAX)) as u32);
        }
        _ => return None,
      }
    }

    if delays_ms.is_empty() {
      return None;
    }

    Some(Self {
      delays_ms,
      total_ms,
      loop_count,
    })
  }

  fn frame_index_for_time_ms(&self, time_ms: f32) -> usize {
    if self.delays_ms.len() <= 1 || self.total_ms == 0 {
      return 0;
    }

    let mut t_ms = if time_ms.is_finite() {
      time_ms.max(0.0) as f64
    } else {
      0.0
    };
    let total_ms_f64 = self.total_ms as f64;

    match self.loop_count {
      Some(0) => {
        // 0 = loop forever (Netscape extension semantics).
        t_ms = t_ms % total_ms_f64;
      }
      Some(count) => {
        // A positive loop count indicates how many times the animation should play in total.
        let play_ms = total_ms_f64 * (count as f64);
        if t_ms >= play_ms {
          t_ms = total_ms_f64 - f64::EPSILON;
        } else {
          t_ms = t_ms % total_ms_f64;
        }
      }
      None => {
        // No loop extension → play once.
        if t_ms >= total_ms_f64 {
          t_ms = total_ms_f64 - f64::EPSILON;
        }
      }
    }

    let mut remaining = t_ms;
    let mut idx = self.delays_ms.len().saturating_sub(1);
    for (i, delay) in self.delays_ms.iter().enumerate() {
      let delay = *delay as f64;
      if remaining < delay {
        idx = i;
        break;
      }
      remaining -= delay;
    }
    idx
  }
}

#[derive(Debug, Clone)]
enum GifTimingCacheValue {
  Timing(Arc<GifTiming>),
  NotGif,
}

/// Cache for loaded images
///
/// `ImageCache` provides in-memory caching of decoded images, with support for
/// loading from URLs, files, and data URLs. It uses a [`ResourceFetcher`] for
/// the actual byte fetching, allowing custom fetching strategies (caching,
/// mocking, etc.) to be injected.
///
/// # Example
///
/// ```rust,no_run
/// # use fastrender::image_loader::ImageCache;
/// # use std::sync::Arc;
/// # fn main() -> fastrender::Result<()> {
/// # #[cfg(feature = "direct_network")]
/// # use fastrender::resource::HttpFetcher;
///
/// # #[cfg(feature = "direct_network")]
/// # {
/// let fetcher = Arc::new(HttpFetcher::new());
/// let cache = ImageCache::with_fetcher(fetcher);
/// let image = cache.load("https://example.com/image.png")?;
/// # let _ = image;
/// # }
/// # Ok(())
/// # }
/// ```
pub struct ImageCache {
  instance_id: u64,
  /// Monotonically increasing generation counter for paint-affecting cache changes.
  ///
  /// This is used by scroll-blit / incremental paint paths to conservatively detect when pixels
  /// from a previous frame may have become stale due to an image being decoded and inserted into
  /// the cache between frames.
  epoch: Arc<AtomicU64>,
  /// In-memory cache of decoded images (keyed by resolved URL)
  cache: Arc<Mutex<SizedLruCache<String, Arc<CachedImage>>>>,
  /// In-flight decodes keyed by resolved URL to de-duplicate concurrent loads.
  in_flight: Arc<Mutex<HashMap<String, Arc<DecodeInFlight>>>>,
  /// In-memory cache of probed metadata (keyed by resolved URL).
  meta_cache: Arc<Mutex<SizedLruCache<String, Arc<CachedImageMetadata>>>>,
  /// Raw resources captured during metadata probes to avoid duplicate fetches between layout and paint.
  raw_cache: Arc<Mutex<SizedLruCache<String, Arc<FetchedResource>>>>,
  /// Parsed GIF timing metadata keyed by the cache key without animation suffixes.
  gif_timing_cache: Arc<Mutex<SizedLruCache<String, GifTimingCacheValue>>>,
  /// In-flight probes keyed by resolved URL to de-duplicate concurrent metadata loads.
  meta_in_flight: Arc<Mutex<HashMap<String, Arc<ProbeInFlight>>>>,
  /// In-memory cache of preprocessed SVG markup (external `<use>`, `<image>`, ...).
  svg_preprocess_cache: Arc<Mutex<SizedLruCache<SvgPreprocessKey, Arc<str>>>>,
  /// In-memory cache of SVG subresources (external sprites and resolved `data:` URLs).
  svg_subresource_cache: SvgSubresourceCache,
  /// In-memory cache of rendered inline SVG pixmaps keyed by (hash, size).
  svg_pixmap_cache: Arc<Mutex<SizedLruCache<SvgPixmapKey, Arc<tiny_skia::Pixmap>>>>,
  /// In-memory cache of premultiplied raster pixmaps keyed by URL + orientation.
  raster_pixmap_cache: Arc<Mutex<SizedLruCache<RasterPixmapKey, Arc<tiny_skia::Pixmap>>>>,
  /// Base URL for resolving relative image sources
  base_url: Option<String>,
  /// Resource fetcher for loading bytes from URLs
  fetcher: Arc<dyn ResourceFetcher>,
  /// Decode limits.
  config: ImageCacheConfig,
  /// Sampling timestamp (ms since load) for animated image formats (e.g. GIF).
  ///
  /// This is used to select a specific animation frame for deterministic fixture renders.
  animation_time_ms: Option<f32>,
  /// Optional diagnostics sink for recording fetch failures.
  diagnostics: Option<Arc<Mutex<RenderDiagnostics>>>,
  /// Optional resource context (policy + diagnostics).
  resource_context: Option<ResourceContext>,
}

struct DecodeInFlightOwnerGuard<'a> {
  cache: &'a ImageCache,
  url: &'a str,
  flight: Arc<DecodeInFlight>,
  finished: bool,
}

impl<'a> DecodeInFlightOwnerGuard<'a> {
  fn new(cache: &'a ImageCache, url: &'a str, flight: Arc<DecodeInFlight>) -> Self {
    Self {
      cache,
      url,
      flight,
      finished: false,
    }
  }

  fn finish(&mut self, result: SharedImageResult) {
    if self.finished {
      return;
    }
    self.finished = true;
    self.cache.finish_inflight(self.url, &self.flight, result);
  }
}

impl Drop for DecodeInFlightOwnerGuard<'_> {
  fn drop(&mut self) {
    if self.finished {
      return;
    }

    self.finished = true;
    let err = Error::Image(ImageError::LoadFailed {
      url: self.url.to_string(),
      reason: "in-flight image decode owner dropped without resolving".to_string(),
    });
    self.cache.record_image_error(self.url, &err);
    self
      .cache
      .finish_inflight(self.url, &self.flight, SharedImageResult::Error(err));
  }
}

struct ProbeInFlightOwnerGuard<'a> {
  cache: &'a ImageCache,
  url: &'a str,
  flight: Arc<ProbeInFlight>,
  finished: bool,
}

impl<'a> ProbeInFlightOwnerGuard<'a> {
  fn new(cache: &'a ImageCache, url: &'a str, flight: Arc<ProbeInFlight>) -> Self {
    Self {
      cache,
      url,
      flight,
      finished: false,
    }
  }

  fn finish(&mut self, result: SharedMetaResult) {
    if self.finished {
      return;
    }
    self.finished = true;
    self
      .cache
      .finish_meta_inflight(self.url, &self.flight, result);
  }
}

impl Drop for ProbeInFlightOwnerGuard<'_> {
  fn drop(&mut self) {
    if self.finished {
      return;
    }

    self.finished = true;
    let err = Error::Image(ImageError::LoadFailed {
      url: self.url.to_string(),
      reason: "in-flight image probe owner dropped without resolving".to_string(),
    });
    self.cache.record_image_error(self.url, &err);
    self
      .cache
      .finish_meta_inflight(self.url, &self.flight, SharedMetaResult::Error(err));
  }
}

enum SvgPreprocessedMarkup<'a> {
  Borrowed(&'a str),
  Shared(Arc<str>),
}

impl AsRef<str> for SvgPreprocessedMarkup<'_> {
  fn as_ref(&self) -> &str {
    match self {
      Self::Borrowed(s) => s,
      Self::Shared(s) => s.as_ref(),
    }
  }
}

#[cfg(not(feature = "direct_network"))]
#[derive(Debug, Default)]
struct SandboxedImageCacheFetcher;

#[cfg(not(feature = "direct_network"))]
impl ResourceFetcher for SandboxedImageCacheFetcher {
  fn fetch(&self, url: &str) -> Result<FetchedResource> {
    if url
      .get(..5)
      .map(|prefix| prefix.eq_ignore_ascii_case("data:"))
      .unwrap_or(false)
    {
      crate::resource::data_url::decode_data_url(url)
    } else {
      Err(Error::Resource(crate::error::ResourceError::new(
        url,
        "ImageCache requires an injected ResourceFetcher in sandboxed builds",
      )))
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GifAnimationProbe {
  Determined(bool),
  /// The provided byte slice ended before the GIF trailer was reached (likely a truncated probe).
  NeedMoreData,
  /// The payload did not match the GIF block structure.
  Invalid,
}

impl ImageCache {
  fn content_type_looks_like_image(content_type: Option<&str>) -> bool {
    let Some(content_type) = content_type else {
      return false;
    };
    let mime = content_type
      .split(';')
      .next()
      .unwrap_or(content_type)
      .trim_matches(|c: char| matches!(c, ' ' | '\t'));
    mime
      .as_bytes()
      .get(..6)
      .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"image/"))
  }

  fn decoded_bitmap_is_single_transparent_pixel(img: &DynamicImage, has_alpha: bool) -> bool {
    if !has_alpha {
      return false;
    }
    let (w, h) = img.dimensions();
    if w != 1 || h != 1 {
      return false;
    }
    // Treat any fully-transparent 1×1 bitmap as a placeholder pixel. To keep this lightweight we
    // only convert to RGBA when the dimensions are already known to be tiny.
    img.to_rgba8().get_pixel(0, 0).0[3] == 0
  }

  fn should_map_decoded_image_to_placeholder(
    resource: &FetchedResource,
    img: &DynamicImage,
    has_alpha: bool,
    is_vector: bool,
  ) -> bool {
    if is_vector {
      return false;
    }
    // Only apply this heuristic for non-HTTP(S) loads (e.g. offline fixtures loaded from file://).
    // HTTP responses have a real content-type/status and are already guarded by stricter checks.
    if resource.status.is_some() {
      return false;
    }
    // When the fetcher did not classify the payload as an image (common for offline fixtures whose
    // asset filenames preserve a non-image URL extension), treat a single fully-transparent pixel
    // as a "missing image" sentinel so replaced element painting can render UA fallback UI.
    if Self::content_type_looks_like_image(resource.content_type.as_deref()) {
      return false;
    }
    Self::decoded_bitmap_is_single_transparent_pixel(img, has_alpha)
  }

  #[cfg(feature = "direct_network")]
  fn default_fetcher() -> Arc<dyn ResourceFetcher> {
    Arc::new(CachingFetcher::with_config(
      HttpFetcher::new(),
      CachingFetcherConfig::default(),
    ))
  }

  #[cfg(not(feature = "direct_network"))]
  fn default_fetcher() -> Arc<dyn ResourceFetcher> {
    Arc::new(SandboxedImageCacheFetcher::default())
  }

  /// Create a new ImageCache.
  ///
  /// When built with the `direct_network` feature (default), this uses the built-in `HttpFetcher`
  /// wrapped in a `CachingFetcher`.
  ///
  /// When `direct_network` is disabled (sandboxed renderer builds), this returns an `ImageCache`
  /// that rejects `http://`, `https://`, and `file://` fetches until a real [`ResourceFetcher`] is
  /// injected via [`ImageCache::with_fetcher`] or [`ImageCache::set_fetcher`].
  pub fn new() -> Self {
    Self::with_config(ImageCacheConfig::default())
  }

  /// Create a new ImageCache with a custom fetcher
  pub fn with_fetcher(fetcher: Arc<dyn ResourceFetcher>) -> Self {
    Self::with_fetcher_and_config(fetcher, ImageCacheConfig::default())
  }

  /// Create a new ImageCache with custom limits.
  ///
  /// See [`ImageCache::new`] for `direct_network` feature behavior.
  pub fn with_config(config: ImageCacheConfig) -> Self {
    Self::with_base_url_fetcher_and_config(None, Self::default_fetcher(), config)
  }

  /// Create a new ImageCache with a custom fetcher and limits.
  pub fn with_fetcher_and_config(
    fetcher: Arc<dyn ResourceFetcher>,
    config: ImageCacheConfig,
  ) -> Self {
    Self::with_base_url_fetcher_and_config(None, fetcher, config)
  }

  /// Create a new ImageCache with a base URL.
  ///
  /// See [`ImageCache::new`] for `direct_network` feature behavior.
  pub fn with_base_url(base_url: String) -> Self {
    Self::with_base_url_and_config(base_url, ImageCacheConfig::default())
  }

  /// Create a new ImageCache with a base URL and custom limits.
  ///
  /// See [`ImageCache::new`] for `direct_network` feature behavior.
  pub fn with_base_url_and_config(base_url: String, config: ImageCacheConfig) -> Self {
    Self::with_base_url_fetcher_and_config(Some(base_url), Self::default_fetcher(), config)
  }

  /// Create a new ImageCache with both a base URL and a custom fetcher
  pub fn with_base_url_and_fetcher(base_url: String, fetcher: Arc<dyn ResourceFetcher>) -> Self {
    Self::with_base_url_fetcher_and_config(Some(base_url), fetcher, ImageCacheConfig::default())
  }

  /// Create a new ImageCache with a base URL, custom fetcher, and limits.
  pub fn with_base_url_and_fetcher_and_config(
    base_url: String,
    fetcher: Arc<dyn ResourceFetcher>,
    config: ImageCacheConfig,
  ) -> Self {
    Self::with_base_url_fetcher_and_config(Some(base_url), fetcher, config)
  }

  fn with_base_url_fetcher_and_config(
    base_url: Option<String>,
    fetcher: Arc<dyn ResourceFetcher>,
    config: ImageCacheConfig,
  ) -> Self {
    Self {
      instance_id: NEXT_IMAGE_CACHE_INSTANCE_ID.fetch_add(1, Ordering::Relaxed),
      epoch: Arc::new(AtomicU64::new(0)),
      cache: Arc::new(Mutex::new(SizedLruCache::new(
        config.max_cached_images,
        config.max_cached_image_bytes,
      ))),
      in_flight: Arc::new(Mutex::new(HashMap::new())),
      meta_cache: Arc::new(Mutex::new(SizedLruCache::new(
        config.max_cached_metadata_items,
        config.max_cached_metadata_bytes,
      ))),
      raw_cache: Arc::new(Mutex::new(SizedLruCache::new(
        config.max_raw_cached_items,
        config.max_raw_cached_bytes,
      ))),
      gif_timing_cache: Arc::new(Mutex::new(SizedLruCache::new(
        config.max_cached_metadata_items,
        config.max_cached_metadata_bytes,
      ))),
      meta_in_flight: Arc::new(Mutex::new(HashMap::new())),
      svg_preprocess_cache: Arc::new(Mutex::new(SizedLruCache::new(
        config.max_cached_svg_preprocess_items,
        config.max_cached_svg_preprocess_bytes,
      ))),
      svg_subresource_cache: Arc::new(Mutex::new(SizedLruCache::new(
        config.max_cached_svg_subresource_items,
        config.max_cached_svg_subresource_bytes,
      ))),
      svg_pixmap_cache: Arc::new(Mutex::new(SizedLruCache::new(
        config.max_cached_svg_pixmaps,
        config.max_cached_svg_bytes,
      ))),
      raster_pixmap_cache: Arc::new(Mutex::new(SizedLruCache::new(
        config.max_cached_raster_pixmaps,
        config.max_cached_raster_bytes,
      ))),
      base_url,
      fetcher,
      config,
      animation_time_ms: None,
      diagnostics: None,
      resource_context: None,
    }
  }

  /// Sets or replaces the base URL used to resolve relative image sources.
  pub fn set_base_url(&mut self, base_url: impl Into<String>) {
    self.base_url = Some(base_url.into());
  }

  /// Clears any previously configured base URL.
  pub fn clear_base_url(&mut self) {
    self.base_url = None;
  }

  /// Returns the configured base URL for resolving relative paths.
  pub fn base_url(&self) -> Option<String> {
    self.base_url.clone()
  }

  /// Set the active resource context for policy and diagnostics.
  pub fn set_resource_context(&mut self, context: Option<ResourceContext>) {
    self.resource_context = context;
    // SVG preprocessing can fetch and inline subresources, so avoid reusing entries across context
    // changes (origin/referrer/policy).
    if let Ok(mut cache) = self.svg_preprocess_cache.lock() {
      cache.clear();
    }
    if let Ok(mut cache) = self.svg_subresource_cache.lock() {
      cache.clear();
    }
  }

  /// Retrieve the current resource context.
  pub fn resource_context(&self) -> Option<ResourceContext> {
    self.resource_context.clone()
  }

  /// Sets the resource fetcher
  pub fn set_fetcher(&mut self, fetcher: Arc<dyn ResourceFetcher>) {
    self.fetcher = fetcher;
  }

  /// Returns a reference to the current fetcher
  pub fn fetcher(&self) -> &Arc<dyn ResourceFetcher> {
    &self.fetcher
  }

  /// Sets the animation timestamp used for sampling animated image formats (e.g. GIF).
  ///
  /// Values are interpreted as milliseconds since load; negative and non-finite values are clamped
  /// to 0.
  pub fn set_animation_time_ms(&mut self, time_ms: Option<f32>) {
    self.animation_time_ms = time_ms.map(|ms| if ms.is_finite() { ms.max(0.0) } else { 0.0 });
  }

  pub fn animation_time_ms(&self) -> Option<f32> {
    self.animation_time_ms
  }

  pub(crate) fn instance_id(&self) -> u64 {
    self.instance_id
  }

  /// Returns the current image-cache epoch/generation.
  ///
  /// The epoch is incremented whenever a decoded **non-placeholder** image is inserted into the
  /// cache (including replacing a previously cached placeholder), since that can change future
  /// paint output even if layout does not change.
  pub fn epoch(&self) -> u64 {
    self.epoch.load(Ordering::Relaxed)
  }

  pub(crate) fn is_placeholder_image(&self, image: &Arc<CachedImage>) -> bool {
    Arc::ptr_eq(image, &about_url_placeholder_image())
  }

  pub(crate) fn is_placeholder_metadata(&self, meta: &Arc<CachedImageMetadata>) -> bool {
    Arc::ptr_eq(meta, &about_url_placeholder_metadata())
  }

  /// Attach a diagnostics sink for recording fetch failures.
  pub fn set_diagnostics_sink(&mut self, diagnostics: Option<Arc<Mutex<RenderDiagnostics>>>) {
    self.diagnostics = diagnostics;
  }

  /// Resolve a potentially relative URL to an absolute URL
  pub fn resolve_url(&self, url: &str) -> String {
    let url = trim_ascii_whitespace(url);
    if url.is_empty() {
      return String::new();
    }

    // Absolute or data URLs can be returned directly.
    if crate::resource::is_data_url(url) {
      // Data URLs often appear inside CSS/JS strings with backslash-escaped quotes
      // (e.g. `data:image/svg+xml,<svg xmlns=\\\"...\\\">`). Unescape those so the SVG/XML parser
      // sees valid markup.
      if url.contains('\\') {
        return unescape_js_escapes(url).into_owned();
      }
      return url.to_string();
    }
    if let Ok(parsed) = url::Url::parse(url) {
      return parsed.to_string();
    }

    // Resolve against the configured base URL when present.
    if let Some(base) = &self.base_url {
      if let Some(resolved) = resolve_against_base(base, url) {
        return resolved;
      }
    }

    // No usable base; return the reference unchanged.
    url.to_string()
  }

  fn cache_key_for_crossorigin(
    &self,
    resolved_url: &str,
    crossorigin: CrossOriginAttribute,
    referrer_policy: Option<ReferrerPolicy>,
  ) -> String {
    // ImageCache keys must be partitioned by any request metadata that can affect the fetch
    // profile. Otherwise, we'd risk satisfying a request with cached results that were fetched
    // under a different referrer policy / referrer URL / CORS mode.
    //
    // Note: The key intentionally includes the *effective* referrer policy (after resolving the
    // empty-string state to the Chromium default) and a hashed representation of the document URL
    // used as the request referrer.
    let crossorigin_key = match crossorigin {
      CrossOriginAttribute::None => "none",
      CrossOriginAttribute::Anonymous => "anonymous",
      CrossOriginAttribute::UseCredentials => "use-credentials",
    };

    let referrer_url = self
      .resource_context
      .as_ref()
      .and_then(|ctx| ctx.document_url.as_deref());
    let referrer_hash = referrer_url.map(|url| {
      let mut hasher = DefaultHasher::new();
      url.hash(&mut hasher);
      (hasher.finish(), url.len())
    });

    let doc_referrer_policy = self
      .resource_context
      .as_ref()
      .map(|ctx| ctx.referrer_policy)
      .unwrap_or_default();
    let request_referrer_policy = referrer_policy.unwrap_or(doc_referrer_policy);
    let effective_referrer_policy = match request_referrer_policy {
      ReferrerPolicy::EmptyString => ReferrerPolicy::CHROMIUM_DEFAULT,
      other => other,
    };

    let mut key = resolved_url.to_string();
    key.push_str("@@crossorigin=");
    key.push_str(crossorigin_key);

    key.push_str("@@referrer=");
    match referrer_hash {
      Some((hash, len)) => {
        key.push_str(&format!("{hash:016x}:{len}"));
      }
      None => key.push_str("none"),
    }

    key.push_str("@@referrer_policy=");
    key.push_str(effective_referrer_policy.as_str());

    if crossorigin != CrossOriginAttribute::None {
      let document_origin = self
        .resource_context
        .as_ref()
        .and_then(|ctx| ctx.policy.document_origin.as_ref())
        .map(|origin| origin.to_string())
        .unwrap_or_else(|| "<unknown>".to_string());
      key.push_str("@@doc_origin=");
      key.push_str(&document_origin);
    }

    if let Some(time_ms) = self.animation_time_ms {
      if url_looks_like_gif(resolved_url) {
        match self.get_cached_gif_timing(&key) {
          Some(GifTimingCacheValue::Timing(timing)) => {
            if timing.delays_ms.len() > 1 {
              let frame_idx = timing.frame_index_for_time_ms(time_ms);
              key.push_str("@@gif_frame=");
              key.push_str(&frame_idx.to_string());
            }
          }
          Some(GifTimingCacheValue::NotGif) => {}
          None => {
            key.push_str("@@animation_time_ms=");
            key.push_str(&format!("{:08x}", f32_to_canonical_bits(time_ms)));
          }
        }
      }
    }

    key
  }

  fn get_cached_gif_timing(&self, cache_key_without_animation: &str) -> Option<GifTimingCacheValue> {
    self
      .gif_timing_cache
      .lock()
      .ok()
      .and_then(|mut cache| cache.get_cloned(cache_key_without_animation))
  }

  fn put_cached_gif_timing(&self, cache_key_without_animation: &str, value: GifTimingCacheValue) {
    if let Ok(mut cache) = self.gif_timing_cache.lock() {
      let key = cache_key_without_animation.to_string();
      let bytes = Self::estimate_gif_timing_cache_entry_bytes(&key, &value);
      cache.insert(key, value, bytes);
    }
  }

  fn estimate_gif_timing_cache_entry_bytes(key: &str, value: &GifTimingCacheValue) -> usize {
    let mut bytes = key.len().saturating_add(std::mem::size_of_val(value));
    match value {
      GifTimingCacheValue::Timing(timing) => {
        bytes = bytes.saturating_add(std::mem::size_of::<GifTiming>());
        bytes = bytes.saturating_add(
          timing
            .delays_ms
            .len()
            .saturating_mul(std::mem::size_of::<u32>()),
        );
      }
      GifTimingCacheValue::NotGif => {}
    }
    bytes
  }

  fn cache_key_without_animation_suffix(cache_key: &str) -> &str {
    if let Some((base, frame)) = cache_key.rsplit_once("@@gif_frame=") {
      if !frame.is_empty() && frame.as_bytes().iter().all(|b| b.is_ascii_digit()) {
        return base;
      }
    }
    if let Some((base, bits)) = cache_key.rsplit_once("@@animation_time_ms=") {
      if bits.len() == 8 && bits.as_bytes().iter().all(|b| b.is_ascii_hexdigit()) {
        return base;
      }
    }
    cache_key
  }

  fn canonical_cache_key_for_bytes(&self, cache_key: &str, resolved_url: &str, bytes: &[u8]) -> String {
    let cache_key_without_animation = Self::cache_key_without_animation_suffix(cache_key);
    let Some(time_ms) = self.animation_time_ms else {
      return cache_key_without_animation.to_string();
    };
    if !url_looks_like_gif(resolved_url) {
      return cache_key_without_animation.to_string();
    }

    match self.get_cached_gif_timing(cache_key_without_animation) {
      Some(GifTimingCacheValue::Timing(timing)) => {
        if timing.delays_ms.len() <= 1 {
          return cache_key_without_animation.to_string();
        }
        let frame_idx = timing.frame_index_for_time_ms(time_ms);
        let mut key = cache_key_without_animation.to_string();
        key.push_str("@@gif_frame=");
        key.push_str(&frame_idx.to_string());
        key
      }
      Some(GifTimingCacheValue::NotGif) => cache_key_without_animation.to_string(),
      None => match GifTiming::parse(bytes) {
        Some(timing) => {
          if timing.delays_ms.len() <= 1 {
            self.put_cached_gif_timing(cache_key_without_animation, GifTimingCacheValue::NotGif);
            return cache_key_without_animation.to_string();
          }
          let timing = Arc::new(timing);
          let frame_idx = timing.frame_index_for_time_ms(time_ms);
          self.put_cached_gif_timing(
            cache_key_without_animation,
            GifTimingCacheValue::Timing(Arc::clone(&timing)),
          );
          let mut key = cache_key_without_animation.to_string();
          key.push_str("@@gif_frame=");
          key.push_str(&frame_idx.to_string());
          key
        }
        None => {
          self.put_cached_gif_timing(cache_key_without_animation, GifTimingCacheValue::NotGif);
          cache_key_without_animation.to_string()
        }
      },
    }
  }

  fn canonical_cache_key_for_placeholder(&self, cache_key: &str, resolved_url: &str) -> String {
    let cache_key_without_animation = Self::cache_key_without_animation_suffix(cache_key);
    if self.animation_time_ms.is_some() && url_looks_like_gif(resolved_url) {
      self.put_cached_gif_timing(cache_key_without_animation, GifTimingCacheValue::NotGif);
    }
    cache_key_without_animation.to_string()
  }

  /// Load an image from a URL or file path
  ///
  /// The URL is first resolved against the base URL if one is configured.
  /// Results are cached in memory, so subsequent loads of the same URL
  /// return the cached image.
  pub fn load(&self, url: &str) -> Result<Arc<CachedImage>> {
    self.load_with_crossorigin(url, CrossOriginAttribute::None)
  }

  /// Load an image, using the provided `<img crossorigin>` state to control the fetch mode.
  ///
  /// When `crossorigin` is not [`CrossOriginAttribute::None`], the resource is fetched in CORS
  /// mode (headers: `Origin`, `Sec-Fetch-Mode: cors`) and, when the `FASTR_FETCH_ENFORCE_CORS`
  /// runtime toggle is enabled, the response's `Access-Control-Allow-Origin`/`Credentials` headers
  /// are validated.
  pub fn load_with_crossorigin(
    &self,
    url: &str,
    crossorigin: CrossOriginAttribute,
  ) -> Result<Arc<CachedImage>> {
    self.load_with_crossorigin_and_referrer_policy(url, crossorigin, None)
  }

  pub fn load_with_crossorigin_and_referrer_policy(
    &self,
    url: &str,
    crossorigin: CrossOriginAttribute,
    referrer_policy: Option<ReferrerPolicy>,
  ) -> Result<Arc<CachedImage>> {
    let trimmed = trim_ascii_whitespace(url);
    if trimmed.is_empty() {
      return Ok(about_url_placeholder_image());
    }
    if let Some(svg) = decode_inline_svg_url(trimmed) {
      return self.render_svg(svg.as_str());
    }
    if is_about_url(trimmed) {
      return Ok(about_url_placeholder_image());
    }

    // Resolve the URL first.
    let resolved_url = self.resolve_url(trimmed);
    self.enforce_image_policy(&resolved_url)?;

    let destination = match crossorigin {
      CrossOriginAttribute::None => FetchDestination::Image,
      _ => FetchDestination::ImageCors,
    };
    let cache_key = self.cache_key_for_crossorigin(&resolved_url, crossorigin, referrer_policy);

    // Check cache first.
    record_image_cache_request();
    if let Some(img) = self.get_cached(&cache_key) {
      record_image_cache_hit();
      return Ok(img);
    }
    let (flight, is_owner) = self.join_inflight(&cache_key);
    if !is_owner {
      record_image_cache_hit();
      return flight.wait(&cache_key);
    }

    let mut inflight_guard = DecodeInFlightOwnerGuard::new(self, &cache_key, flight);

    if let Some(resource) = self.take_raw_cached_resource(&cache_key) {
      record_image_cache_hit();
      let result =
        self.decode_resource_into_cache(&cache_key, &resolved_url, &resource, crossorigin);
      let shared = match &result {
        Ok(img) => SharedImageResult::Success(Arc::clone(img)),
        Err(err) => {
          self.record_image_error(&resolved_url, err);
          SharedImageResult::Error(err.clone())
        }
      };
      inflight_guard.finish(shared);
      return result;
    }

    record_image_cache_miss();
    let result = self.fetch_and_decode(
      &cache_key,
      &resolved_url,
      destination,
      crossorigin,
      referrer_policy,
    );
    let shared = match &result {
      Ok(img) => SharedImageResult::Success(Arc::clone(img)),
      Err(err) => SharedImageResult::Error(err.clone()),
    };
    inflight_guard.finish(shared);

    result
  }

  /// Load a decoded raster image and convert it to a premultiplied [`tiny_skia::Pixmap`],
  /// caching the result for reuse in subsequent paint calls.
  ///
  /// Returns `Ok(None)` when the resource is a vector image (SVG) or the conversion fails.
  pub fn load_raster_pixmap(
    &self,
    url: &str,
    orientation: OrientationTransform,
    decorative: bool,
  ) -> Result<Option<Arc<tiny_skia::Pixmap>>> {
    let resolved_url = self.resolve_url(url);
    if is_about_url(&resolved_url) {
      return Ok(Some(about_url_placeholder_pixmap()?));
    }
    self.enforce_image_policy(&resolved_url)?;

    let cache_key = self.cache_key_for_crossorigin(&resolved_url, CrossOriginAttribute::None, None);
    let key = raster_pixmap_full_key(&cache_key, orientation, decorative);
    if let Ok(mut cache) = self.raster_pixmap_cache.lock() {
      if let Some(cached) = cache.get_cloned(&key) {
        record_raster_pixmap_cache_hit();
        record_raster_pixmap_cache_bytes(cache.current_bytes());
        with_paint_diagnostics(|diag| {
          diag.image_pixmap_cache_hits = diag.image_pixmap_cache_hits.saturating_add(1);
        });
        return Ok(Some(cached));
      }
    }
    record_raster_pixmap_cache_miss();
    with_paint_diagnostics(|diag| {
      diag.image_pixmap_cache_misses = diag.image_pixmap_cache_misses.saturating_add(1);
    });

    let image = self.load(&resolved_url)?;
    if image.is_vector {
      return Ok(None);
    }

    let (width, height) = image.oriented_dimensions(orientation);
    if width == 0 || height == 0 {
      return Ok(None);
    }

    let Some(bytes_u64) = u64::from(width)
      .checked_mul(u64::from(height))
      .and_then(|px| px.checked_mul(4))
    else {
      return Ok(None);
    };
    if bytes_u64 > MAX_PIXMAP_BYTES {
      return Ok(None);
    }
    let bytes = match usize::try_from(bytes_u64) {
      Ok(bytes) => bytes,
      Err(_) => return Ok(None),
    };
    render_control::reserve_allocation_with(bytes_u64, || {
      format!(
        "image raster pixmap {}x{} url={}",
        width, height, resolved_url
      )
    })
    .map_err(Error::Render)?;

    let rgba = image.to_oriented_rgba(orientation);
    let (rgba_w, rgba_h) = rgba.dimensions();
    if rgba_w != width || rgba_h != height {
      return Ok(None);
    }
    let mut data = rgba.into_raw();

    // tiny-skia expects premultiplied RGBA.
    for pixel in data.chunks_exact_mut(4) {
      let alpha = pixel[3] as f32 / 255.0;
      pixel[0] = (pixel[0] as f32 * alpha).round() as u8;
      pixel[1] = (pixel[1] as f32 * alpha).round() as u8;
      pixel[2] = (pixel[2] as f32 * alpha).round() as u8;
    }

    let Some(size) = IntSize::from_wh(width, height) else {
      return Ok(None);
    };
    let Some(pixmap) = Pixmap::from_vec(data, size) else {
      return Ok(None);
    };
    let pixmap = Arc::new(pixmap);

    if self.config.max_cached_raster_bytes > 0 && bytes > self.config.max_cached_raster_bytes {
      return Ok(Some(pixmap));
    }

    if let Ok(mut cache) = self.raster_pixmap_cache.lock() {
      cache.insert(key, Arc::clone(&pixmap), bytes);
      record_raster_pixmap_cache_bytes(cache.current_bytes());
    }

    Ok(Some(pixmap))
  }

  pub fn load_raster_pixmap_with_crossorigin(
    &self,
    url: &str,
    crossorigin: CrossOriginAttribute,
    orientation: OrientationTransform,
    decorative: bool,
  ) -> Result<Option<Arc<tiny_skia::Pixmap>>> {
    if crossorigin == CrossOriginAttribute::None {
      return self.load_raster_pixmap(url, orientation, decorative);
    }

    let resolved_url = self.resolve_url(url);
    if is_about_url(&resolved_url) {
      return Ok(Some(about_url_placeholder_pixmap()?));
    }
    self.enforce_image_policy(&resolved_url)?;
    let cache_key = self.cache_key_for_crossorigin(&resolved_url, crossorigin, None);

    let key = raster_pixmap_full_key(&cache_key, orientation, decorative);
    if let Ok(mut cache) = self.raster_pixmap_cache.lock() {
      if let Some(cached) = cache.get_cloned(&key) {
        record_raster_pixmap_cache_hit();
        record_raster_pixmap_cache_bytes(cache.current_bytes());
        with_paint_diagnostics(|diag| {
          diag.image_pixmap_cache_hits = diag.image_pixmap_cache_hits.saturating_add(1);
        });
        return Ok(Some(cached));
      }
    }
    record_raster_pixmap_cache_miss();
    with_paint_diagnostics(|diag| {
      diag.image_pixmap_cache_misses = diag.image_pixmap_cache_misses.saturating_add(1);
    });

    let image = self.load_with_crossorigin(&resolved_url, crossorigin)?;
    if image.is_vector {
      return Ok(None);
    }

    let (width, height) = image.oriented_dimensions(orientation);
    if width == 0 || height == 0 {
      return Ok(None);
    }

    let Some(bytes_u64) = u64::from(width)
      .checked_mul(u64::from(height))
      .and_then(|px| px.checked_mul(4))
    else {
      return Ok(None);
    };
    if bytes_u64 > MAX_PIXMAP_BYTES {
      return Ok(None);
    }
    let bytes = match usize::try_from(bytes_u64) {
      Ok(bytes) => bytes,
      Err(_) => return Ok(None),
    };
    render_control::reserve_allocation_with(bytes_u64, || {
      format!(
        "image raster pixmap {}x{} url={}",
        width, height, resolved_url
      )
    })
    .map_err(Error::Render)?;

    let rgba = image.to_oriented_rgba(orientation);
    let (rgba_w, rgba_h) = rgba.dimensions();
    if rgba_w != width || rgba_h != height {
      return Ok(None);
    }
    let mut data = rgba.into_raw();

    // tiny-skia expects premultiplied RGBA.
    for pixel in data.chunks_exact_mut(4) {
      let alpha = pixel[3] as f32 / 255.0;
      pixel[0] = (pixel[0] as f32 * alpha).round() as u8;
      pixel[1] = (pixel[1] as f32 * alpha).round() as u8;
      pixel[2] = (pixel[2] as f32 * alpha).round() as u8;
    }

    let Some(size) = IntSize::from_wh(width, height) else {
      return Ok(None);
    };
    let Some(pixmap) = Pixmap::from_vec(data, size) else {
      return Ok(None);
    };
    let pixmap = Arc::new(pixmap);

    if self.config.max_cached_raster_bytes > 0 && bytes > self.config.max_cached_raster_bytes {
      return Ok(Some(pixmap));
    }

    if let Ok(mut cache) = self.raster_pixmap_cache.lock() {
      cache.insert(key, Arc::clone(&pixmap), bytes);
      record_raster_pixmap_cache_bytes(cache.current_bytes());
    }

    Ok(Some(pixmap))
  }

  /// Load a decoded raster image and convert it to a premultiplied [`tiny_skia::Pixmap`], but
  /// first resample it toward the requested target size.
  ///
  /// This is intended for paint call sites that render the image substantially smaller than its
  /// intrinsic dimensions: resampling before premultiplication keeps work proportional to the
  /// destination pixel count.
  ///
  /// Callers should pass target sizes in *device pixels* (after applying device pixel ratio) and
  /// only use this path when downscaling is desired (i.e., target <= intrinsic). If the requested
  /// target would require upscaling, this function falls back to [`Self::load_raster_pixmap`].
  pub fn load_raster_pixmap_at_size(
    &self,
    url: &str,
    orientation: OrientationTransform,
    decorative: bool,
    target_width: u32,
    target_height: u32,
    quality: FilterQuality,
  ) -> Result<Option<Arc<tiny_skia::Pixmap>>> {
    if target_width == 0 || target_height == 0 {
      return Ok(None);
    }
    let resolved_url = self.resolve_url(url);
    if is_about_url(&resolved_url) {
      return Ok(Some(about_url_placeholder_pixmap()?));
    }
    self.enforce_image_policy(&resolved_url)?;

    let cache_key = self.cache_key_for_crossorigin(&resolved_url, CrossOriginAttribute::None, None);
    let key = raster_pixmap_key(
      &cache_key,
      orientation,
      decorative,
      target_width,
      target_height,
      raster_pixmap_quality_bits(quality),
    );
    if let Ok(mut cache) = self.raster_pixmap_cache.lock() {
      if let Some(cached) = cache.get_cloned(&key) {
        record_raster_pixmap_cache_hit();
        record_raster_pixmap_cache_bytes(cache.current_bytes());
        with_paint_diagnostics(|diag| {
          diag.image_pixmap_cache_hits = diag.image_pixmap_cache_hits.saturating_add(1);
        });
        return Ok(Some(cached));
      }
    }
    record_raster_pixmap_cache_miss();
    with_paint_diagnostics(|diag| {
      diag.image_pixmap_cache_misses = diag.image_pixmap_cache_misses.saturating_add(1);
    });

    let image = self.load(&resolved_url)?;
    if image.is_vector {
      return Ok(None);
    }

    let (src_w, src_h) = image.oriented_dimensions(orientation);
    if src_w == 0 || src_h == 0 {
      return Ok(None);
    }
    if target_width >= src_w || target_height >= src_h {
      // Callers should only use this path when downscaling. If any axis would require upscaling,
      // fall back to the full-resolution pixmap cache so results stay stable (important for
      // pixelated/crisp-edges semantics and to avoid needless resampling work).
      return self.load_raster_pixmap(&resolved_url, orientation, decorative);
    }

    let Some(bytes_u64) = u64::from(target_width)
      .checked_mul(u64::from(target_height))
      .and_then(|px| px.checked_mul(4))
    else {
      return Ok(None);
    };
    if bytes_u64 > MAX_PIXMAP_BYTES {
      return Ok(None);
    }
    let bytes = match usize::try_from(bytes_u64) {
      Ok(bytes) => bytes,
      Err(_) => return Ok(None),
    };
    render_control::reserve_allocation_with(bytes_u64, || {
      format!(
        "image raster pixmap {}x{} url={} resampled=true",
        target_width, target_height, resolved_url
      )
    })
    .map_err(Error::Render)?;

    let filter = match quality {
      FilterQuality::Nearest => image::imageops::FilterType::Nearest,
      FilterQuality::Bilinear => image::imageops::FilterType::Triangle,
      _ => image::imageops::FilterType::Triangle,
    };

    // We resize in the decoded image's native orientation and then apply the requested
    // orientation transform on the smaller output buffer.
    let (pre_w, pre_h) = if orientation.swaps_axes() {
      (target_height, target_width)
    } else {
      (target_width, target_height)
    };
    let resized = image.image.resize_exact(pre_w, pre_h, filter);
    let mut rgba = resized.to_rgba8();

    match orientation.quarter_turns % 4 {
      0 => {}
      1 => rgba = imageops::rotate90(&rgba),
      2 => rgba = imageops::rotate180(&rgba),
      3 => rgba = imageops::rotate270(&rgba),
      _ => {}
    }
    if orientation.flip_x {
      rgba = imageops::flip_horizontal(&rgba);
    }

    let (rgba_w, rgba_h) = rgba.dimensions();
    if rgba_w != target_width || rgba_h != target_height {
      return Ok(None);
    }
    let mut data = rgba.into_raw();

    // tiny-skia expects premultiplied RGBA.
    for pixel in data.chunks_exact_mut(4) {
      let alpha = pixel[3] as f32 / 255.0;
      pixel[0] = (pixel[0] as f32 * alpha).round() as u8;
      pixel[1] = (pixel[1] as f32 * alpha).round() as u8;
      pixel[2] = (pixel[2] as f32 * alpha).round() as u8;
    }

    let Some(size) = IntSize::from_wh(target_width, target_height) else {
      return Ok(None);
    };
    let Some(pixmap) = Pixmap::from_vec(data, size) else {
      return Ok(None);
    };
    let pixmap = Arc::new(pixmap);

    if self.config.max_cached_raster_bytes > 0 && bytes > self.config.max_cached_raster_bytes {
      return Ok(Some(pixmap));
    }

    if let Ok(mut cache) = self.raster_pixmap_cache.lock() {
      cache.insert(key, Arc::clone(&pixmap), bytes);
      record_raster_pixmap_cache_bytes(cache.current_bytes());
    }

    Ok(Some(pixmap))
  }

  pub fn load_raster_pixmap_at_size_with_crossorigin(
    &self,
    url: &str,
    crossorigin: CrossOriginAttribute,
    orientation: OrientationTransform,
    decorative: bool,
    target_width: u32,
    target_height: u32,
    quality: FilterQuality,
  ) -> Result<Option<Arc<tiny_skia::Pixmap>>> {
    if crossorigin == CrossOriginAttribute::None {
      return self.load_raster_pixmap_at_size(
        url,
        orientation,
        decorative,
        target_width,
        target_height,
        quality,
      );
    }

    if target_width == 0 || target_height == 0 {
      return Ok(None);
    }
    let resolved_url = self.resolve_url(url);
    if is_about_url(&resolved_url) {
      return Ok(Some(about_url_placeholder_pixmap()?));
    }
    self.enforce_image_policy(&resolved_url)?;

    let cache_key = self.cache_key_for_crossorigin(&resolved_url, crossorigin, None);
    let key = raster_pixmap_key(
      &cache_key,
      orientation,
      decorative,
      target_width,
      target_height,
      raster_pixmap_quality_bits(quality),
    );
    if let Ok(mut cache) = self.raster_pixmap_cache.lock() {
      if let Some(cached) = cache.get_cloned(&key) {
        record_raster_pixmap_cache_hit();
        record_raster_pixmap_cache_bytes(cache.current_bytes());
        with_paint_diagnostics(|diag| {
          diag.image_pixmap_cache_hits = diag.image_pixmap_cache_hits.saturating_add(1);
        });
        return Ok(Some(cached));
      }
    }
    record_raster_pixmap_cache_miss();
    with_paint_diagnostics(|diag| {
      diag.image_pixmap_cache_misses = diag.image_pixmap_cache_misses.saturating_add(1);
    });

    let image = self.load_with_crossorigin(&resolved_url, crossorigin)?;
    if image.is_vector {
      return Ok(None);
    }

    let (src_w, src_h) = image.oriented_dimensions(orientation);
    if src_w == 0 || src_h == 0 {
      return Ok(None);
    }
    if target_width >= src_w || target_height >= src_h {
      // Callers should only use this path when downscaling. If any axis would require upscaling,
      // fall back to the full-resolution pixmap cache so results stay stable (important for
      // pixelated/crisp-edges semantics and to avoid needless resampling work).
      return self.load_raster_pixmap_with_crossorigin(
        &resolved_url,
        crossorigin,
        orientation,
        decorative,
      );
    }

    let Some(bytes_u64) = u64::from(target_width)
      .checked_mul(u64::from(target_height))
      .and_then(|px| px.checked_mul(4))
    else {
      return Ok(None);
    };
    if bytes_u64 > MAX_PIXMAP_BYTES {
      return Ok(None);
    }
    let bytes = match usize::try_from(bytes_u64) {
      Ok(bytes) => bytes,
      Err(_) => return Ok(None),
    };
    render_control::reserve_allocation_with(bytes_u64, || {
      format!(
        "image raster pixmap {}x{} url={} resampled=true",
        target_width, target_height, resolved_url
      )
    })
    .map_err(Error::Render)?;

    let filter = match quality {
      FilterQuality::Nearest => image::imageops::FilterType::Nearest,
      FilterQuality::Bilinear => image::imageops::FilterType::Triangle,
      _ => image::imageops::FilterType::Triangle,
    };

    // We resize in the decoded image's native orientation and then apply the requested
    // orientation transform on the smaller output buffer.
    let (pre_w, pre_h) = if orientation.swaps_axes() {
      (target_height, target_width)
    } else {
      (target_width, target_height)
    };
    let resized = image.image.resize_exact(pre_w, pre_h, filter);
    let mut rgba = resized.to_rgba8();

    match orientation.quarter_turns % 4 {
      0 => {}
      1 => rgba = imageops::rotate90(&rgba),
      2 => rgba = imageops::rotate180(&rgba),
      3 => rgba = imageops::rotate270(&rgba),
      _ => {}
    }
    if orientation.flip_x {
      rgba = imageops::flip_horizontal(&rgba);
    }

    let (rgba_w, rgba_h) = rgba.dimensions();
    if rgba_w != target_width || rgba_h != target_height {
      return Ok(None);
    }
    let mut data = rgba.into_raw();

    // tiny-skia expects premultiplied RGBA.
    for pixel in data.chunks_exact_mut(4) {
      let alpha = pixel[3] as f32 / 255.0;
      pixel[0] = (pixel[0] as f32 * alpha).round() as u8;
      pixel[1] = (pixel[1] as f32 * alpha).round() as u8;
      pixel[2] = (pixel[2] as f32 * alpha).round() as u8;
    }

    let Some(size) = IntSize::from_wh(target_width, target_height) else {
      return Ok(None);
    };
    let Some(pixmap) = Pixmap::from_vec(data, size) else {
      return Ok(None);
    };
    let pixmap = Arc::new(pixmap);

    if self.config.max_cached_raster_bytes > 0 && bytes > self.config.max_cached_raster_bytes {
      return Ok(Some(pixmap));
    }

    if let Ok(mut cache) = self.raster_pixmap_cache.lock() {
      cache.insert(key, Arc::clone(&pixmap), bytes);
      record_raster_pixmap_cache_bytes(cache.current_bytes());
    }

    Ok(Some(pixmap))
  }

  /// Probe image metadata (dimensions, EXIF orientation/resolution, SVG intrinsic ratio)
  /// without fully decoding the image.
  pub fn probe(&self, url: &str) -> Result<Arc<CachedImageMetadata>> {
    self.probe_with_crossorigin(url, CrossOriginAttribute::None)
  }

  /// Probe image metadata, using the provided `<img crossorigin>` state to control the fetch mode.
  ///
  /// When `crossorigin` is not [`CrossOriginAttribute::None`], the probe issues a CORS-mode fetch
  /// (`Sec-Fetch-Mode: cors`, `Origin`) and, when `FASTR_FETCH_ENFORCE_CORS` is enabled, validates
  /// the response's `Access-Control-Allow-Origin`/`Credentials` headers.
  pub fn probe_with_crossorigin(
    &self,
    url: &str,
    crossorigin: CrossOriginAttribute,
  ) -> Result<Arc<CachedImageMetadata>> {
    self.probe_with_crossorigin_and_referrer_policy(url, crossorigin, None)
  }

  pub fn probe_with_crossorigin_and_referrer_policy(
    &self,
    url: &str,
    crossorigin: CrossOriginAttribute,
    referrer_policy: Option<ReferrerPolicy>,
  ) -> Result<Arc<CachedImageMetadata>> {
    let trimmed = trim_ascii_whitespace(url);
    if trimmed.is_empty() {
      return Ok(about_url_placeholder_metadata());
    }
    if let Some(svg) = decode_inline_svg_url(trimmed) {
      let cache_key = inline_svg_cache_key(trim_ascii_whitespace_start(&svg));
      record_image_cache_request();

      if let Some(img) = self.get_cached(&cache_key) {
        record_image_cache_hit();
        return Ok(Arc::new(CachedImageMetadata::from(&*img)));
      }

      if let Some(meta) = self.get_cached_meta(&cache_key) {
        record_image_cache_hit();
        return Ok(meta);
      }

      let (flight, is_owner) = self.join_meta_inflight(&cache_key);
      if !is_owner {
        record_image_cache_hit();
        return flight.wait(&cache_key);
      }

      let mut inflight_guard = ProbeInFlightOwnerGuard::new(self, &cache_key, flight);
      record_image_cache_miss();
      let result = self
        .probe_svg_content(svg.as_str(), "inline-svg")
        .map(Arc::new);
      let shared = match &result {
        Ok(meta) => {
          if let Ok(mut cache) = self.meta_cache.lock() {
            let key = cache_key.clone();
            let bytes = Self::estimate_meta_cache_entry_bytes(&key, meta.as_ref());
            cache.insert(key, Arc::clone(meta), bytes);
          }
          SharedMetaResult::Success(Arc::clone(meta))
        }
        Err(err) => SharedMetaResult::Error(err.clone()),
      };
      inflight_guard.finish(shared);
      return result;
    }
    if is_about_url(trimmed) {
      return Ok(about_url_placeholder_metadata());
    }

    let resolved_url = self.resolve_url(trimmed);
    self.probe_resolved_with_crossorigin_and_referrer_policy(
      &resolved_url,
      crossorigin,
      referrer_policy,
    )
  }

  pub fn probe_resolved(&self, resolved_url: &str) -> Result<Arc<CachedImageMetadata>> {
    self.probe_resolved_with_crossorigin(resolved_url, CrossOriginAttribute::None)
  }

  pub fn probe_resolved_with_crossorigin(
    &self,
    resolved_url: &str,
    crossorigin: CrossOriginAttribute,
  ) -> Result<Arc<CachedImageMetadata>> {
    self.probe_resolved_with_crossorigin_and_referrer_policy(resolved_url, crossorigin, None)
  }

  pub fn probe_resolved_with_crossorigin_and_referrer_policy(
    &self,
    resolved_url: &str,
    crossorigin: CrossOriginAttribute,
    referrer_policy: Option<ReferrerPolicy>,
  ) -> Result<Arc<CachedImageMetadata>> {
    let resolved_url = trim_ascii_whitespace(resolved_url);
    let trimmed = trim_ascii_whitespace_start(resolved_url);
    if trimmed.starts_with('<') {
      return self.probe_with_crossorigin(trimmed, crossorigin);
    }
    let cache_key = self.cache_key_for_crossorigin(resolved_url, crossorigin, referrer_policy);
    let kind = match crossorigin {
      CrossOriginAttribute::None => FetchContextKind::Image,
      _ => FetchContextKind::ImageCors,
    };
    self.probe_resolved_url(&cache_key, resolved_url, kind, crossorigin, referrer_policy)
  }

  fn probe_resolved_url(
    &self,
    cache_key: &str,
    resolved_url: &str,
    kind: FetchContextKind,
    crossorigin: CrossOriginAttribute,
    referrer_policy: Option<ReferrerPolicy>,
  ) -> Result<Arc<CachedImageMetadata>> {
    if resolved_url.is_empty() {
      return Err(Error::Image(ImageError::LoadFailed {
        url: resolved_url.to_string(),
        reason: "image probe URL is empty".to_string(),
      }));
    }
    if is_about_url(resolved_url) {
      return Ok(about_url_placeholder_metadata());
    }

    self.enforce_image_policy(resolved_url)?;
    record_image_cache_request();
    if let Some(img) = self.get_cached(cache_key) {
      record_image_cache_hit();
      if self.is_placeholder_image(&img) {
        return Ok(self.cache_placeholder_metadata(cache_key));
      }
      return Ok(Arc::new(CachedImageMetadata::from(&*img)));
    }

    if let Some(meta) = self.get_cached_meta(cache_key) {
      record_image_cache_hit();
      return Ok(meta);
    }

    let (flight, is_owner) = self.join_meta_inflight(cache_key);
    if !is_owner {
      record_image_cache_hit();
      return flight.wait(cache_key);
    }

    let mut inflight_guard = ProbeInFlightOwnerGuard::new(self, cache_key, flight);

    let destination = match kind {
      FetchContextKind::ImageCors => FetchDestination::ImageCors,
      _ => FetchDestination::Image,
    };
    let credentials_mode = fetch_credentials_mode_for_crossorigin(crossorigin);
    let referrer_url = self
      .resource_context
      .as_ref()
      .and_then(|ctx| ctx.document_url.as_deref());
    let doc_referrer_policy = self
      .resource_context
      .as_ref()
      .map(|ctx| ctx.referrer_policy)
      .unwrap_or_default();
    let request_referrer_policy = referrer_policy.unwrap_or(doc_referrer_policy);
    let origin_fallback = referrer_url.and_then(origin_from_url);
    let client_origin = self
      .resource_context
      .as_ref()
      .and_then(|ctx| ctx.policy.document_origin.as_ref())
      .or(origin_fallback.as_ref());
    let fetch_url_no_fragment = strip_url_fragment(resolved_url);
    let mut request = FetchRequest::new(fetch_url_no_fragment.as_ref(), destination)
      .with_credentials_mode(credentials_mode);
    if let Some(origin) = client_origin {
      request = request.with_client_origin(origin);
    }
    if let Some(referrer_url) = referrer_url {
      request = request.with_referrer_url(referrer_url);
    }
    request = request.with_referrer_policy(request_referrer_policy);

    if let Some(cached) = self
      .fetcher
      .read_cache_artifact_with_request(request, CacheArtifactKind::ImageProbeMetadata)
    {
      if let Some(ctx) = &self.resource_context {
        let policy_url = cached
          .final_url
          .as_deref()
          .unwrap_or(fetch_url_no_fragment.as_ref());
        if let Err(err) = ctx.check_allowed(ResourceKind::Image, policy_url) {
          let blocked = Error::Image(ImageError::LoadFailed {
            url: resolved_url.to_string(),
            reason: err.reason,
          });
          self.record_image_error(resolved_url, &blocked);
          inflight_guard.finish(SharedMetaResult::Error(blocked.clone()));
          return Err(blocked);
        }
      }
      if let Err(err) = self.enforce_image_cors(resolved_url, &cached, crossorigin) {
        self.record_image_error(resolved_url, &err);
        inflight_guard.finish(SharedMetaResult::Error(err.clone()));
        return Err(err);
      }

      if let Some(decoded) = decode_probe_metadata_from_disk(&cached.bytes) {
        let meta = Arc::new(decoded);
        if let Ok(mut cache) = self.meta_cache.lock() {
          let key = cache_key.to_string();
          let bytes = Self::estimate_meta_cache_entry_bytes(&key, meta.as_ref());
          cache.insert(key, Arc::clone(&meta), bytes);
        }
        record_image_cache_hit();
        inflight_guard.finish(SharedMetaResult::Success(Arc::clone(&meta)));
        return Ok(meta);
      }

      // Corrupt or incompatible cache entry; evict so we don't repeatedly reparse it.
      let artifact_url = cached
        .final_url
        .as_deref()
        .unwrap_or(fetch_url_no_fragment.as_ref());
      let mut remove_request =
        FetchRequest::new(artifact_url, destination).with_credentials_mode(credentials_mode);
      if let Some(origin) = client_origin {
        remove_request = remove_request.with_client_origin(origin);
      }
      if let Some(referrer_url) = referrer_url {
        remove_request = remove_request.with_referrer_url(referrer_url);
      }
      remove_request = remove_request.with_referrer_policy(request_referrer_policy);
      self
        .fetcher
        .remove_cache_artifact_with_request(remove_request, CacheArtifactKind::ImageProbeMetadata);
    }

    record_image_cache_miss();
    let result = self.fetch_and_probe(
      cache_key,
      resolved_url,
      destination,
      crossorigin,
      referrer_policy,
    );
    let shared = match &result {
      Ok(meta) => SharedMetaResult::Success(Arc::clone(meta)),
      Err(err) => SharedMetaResult::Error(err.clone()),
    };
    inflight_guard.finish(shared);

    result
  }

  fn get_cached(&self, resolved_url: &str) -> Option<Arc<CachedImage>> {
    self
      .cache
      .lock()
      .ok()
      .and_then(|mut cache| cache.get_cloned(resolved_url))
  }

  fn get_cached_meta(&self, resolved_url: &str) -> Option<Arc<CachedImageMetadata>> {
    self
      .meta_cache
      .lock()
      .ok()
      .and_then(|mut cache| cache.get_cloned(resolved_url))
  }

  fn take_raw_cached_resource(&self, resolved_url: &str) -> Option<Arc<FetchedResource>> {
    self
      .raw_cache
      .lock()
      .ok()
      .and_then(|mut cache| cache.take(resolved_url))
  }

  fn cache_placeholder_image(&self, resolved_url: &str) -> Arc<CachedImage> {
    let image = about_url_placeholder_image();
    self.insert_cached_image(resolved_url, Arc::clone(&image));
    let meta = about_url_placeholder_metadata();
    if let Ok(mut cache) = self.meta_cache.lock() {
      let key = resolved_url.to_string();
      let bytes = Self::estimate_meta_cache_entry_bytes(&key, meta.as_ref());
      cache.insert(key, Arc::clone(&meta), bytes);
    }
    image
  }

  fn cache_placeholder_metadata(&self, resolved_url: &str) -> Arc<CachedImageMetadata> {
    let meta = about_url_placeholder_metadata();
    if let Ok(mut cache) = self.meta_cache.lock() {
      let key = resolved_url.to_string();
      let bytes = Self::estimate_meta_cache_entry_bytes(&key, meta.as_ref());
      cache.insert(key, Arc::clone(&meta), bytes);
    }
    meta
  }

  fn estimate_meta_cache_entry_bytes(key: &str, meta: &CachedImageMetadata) -> usize {
    key
      .len()
      .saturating_add(std::mem::size_of_val(meta))
      .saturating_add(std::mem::size_of::<Arc<CachedImageMetadata>>())
  }

  fn estimate_raw_cache_entry_bytes(key: &str, resource: &FetchedResource) -> usize {
    let mut bytes = key.len().saturating_add(resource.bytes.len());
    bytes = bytes.saturating_add(resource.content_type.as_ref().map(|s| s.len()).unwrap_or(0));
    bytes = bytes.saturating_add(
      resource
        .content_encoding
        .as_ref()
        .map(|s| s.len())
        .unwrap_or(0),
    );
    bytes = bytes.saturating_add(resource.etag.as_ref().map(|s| s.len()).unwrap_or(0));
    bytes = bytes.saturating_add(
      resource
        .last_modified
        .as_ref()
        .map(|s| s.len())
        .unwrap_or(0),
    );
    bytes = bytes.saturating_add(
      resource
        .access_control_allow_origin
        .as_ref()
        .map(|s| s.len())
        .unwrap_or(0),
    );
    bytes = bytes.saturating_add(
      resource
        .timing_allow_origin
        .as_ref()
        .map(|s| s.len())
        .unwrap_or(0),
    );
    bytes = bytes.saturating_add(resource.vary.as_ref().map(|s| s.len()).unwrap_or(0));
    bytes = bytes.saturating_add(resource.final_url.as_ref().map(|s| s.len()).unwrap_or(0));
    if let Some(headers) = resource.response_headers.as_ref() {
      bytes = bytes.saturating_add(headers.len().saturating_mul(2));
      bytes = headers.iter().fold(bytes, |acc, (k, v)| {
        acc.saturating_add(k.len().saturating_add(v.len()))
      });
    }
    bytes
  }

  fn insert_cached_image(&self, resolved_url: &str, image: Arc<CachedImage>) {
    let mut bytes = Self::estimate_image_bytes(&image.image);
    if let Some(svg) = &image.svg_content {
      bytes = bytes.saturating_add(svg.len());
    }
    if let Ok(mut cache) = self.cache.lock() {
      // Bump the epoch when this insertion can change paint output. This is intentionally
      // conservative: any transition from "missing/placeholder" → real image must bump so scroll
      // blit paths don't reuse stale pixels from an older frame.
      if !self.is_placeholder_image(&image) {
        let should_bump = match cache.get_cloned(resolved_url) {
          None => true,
          Some(prev) => self.is_placeholder_image(&prev) || !Arc::ptr_eq(&prev, &image),
        };
        if should_bump {
          self.epoch.fetch_add(1, Ordering::Relaxed);
        }
      }
      cache.insert(resolved_url.to_string(), image, bytes);
    }
  }

  fn insert_svg_pixmap(&self, key: SvgPixmapKey, pixmap: Arc<tiny_skia::Pixmap>) {
    let bytes = pixmap.data().len();
    if let Ok(mut cache) = self.svg_pixmap_cache.lock() {
      cache.insert(key, pixmap, bytes);
    }
  }

  fn estimate_image_bytes(image: &DynamicImage) -> usize {
    let (width, height) = image.dimensions();
    let pixels = usize::try_from(width)
      .unwrap_or(0)
      .saturating_mul(usize::try_from(height).unwrap_or(0));
    let bpp = usize::from(image.color().bytes_per_pixel()).max(1);
    pixels.saturating_mul(bpp)
  }

  fn record_image_error(&self, url: &str, error: &Error) {
    if let Some(diag) = &self.diagnostics {
      if let Ok(mut guard) = diag.lock() {
        guard.record_error(ResourceKind::Image, url, error);
      }
    }
  }

  fn record_invalid_image(&self, url: &str) {
    const INVALID_IMAGE_LIMIT: usize = 64;
    let Some(diag) = &self.diagnostics else {
      return;
    };
    let Ok(mut guard) = diag.lock() else {
      return;
    };
    if guard.invalid_images.len() >= INVALID_IMAGE_LIMIT {
      return;
    }
    if guard.invalid_images.iter().any(|u| u == url) {
      return;
    }
    guard.invalid_images.push(url.to_string());
  }

  fn enforce_image_policy(&self, url: &str) -> Result<()> {
    if let Some(ctx) = &self.resource_context {
      if let Err(err) = ctx.check_allowed(ResourceKind::Image, url) {
        let blocked = Error::Image(ImageError::LoadFailed {
          url: url.to_string(),
          reason: err.reason,
        });
        if ctx.diagnostics.is_none() {
          self.record_image_error(url, &blocked);
        }
        return Err(blocked);
      }
    }

    Ok(())
  }

  fn enforce_image_cors(
    &self,
    requested_url: &str,
    resource: &FetchedResource,
    crossorigin: CrossOriginAttribute,
  ) -> Result<()> {
    if crossorigin == CrossOriginAttribute::None {
      return Ok(());
    }
    if !crate::resource::cors_enforcement_enabled() {
      return Ok(());
    }

    let document_origin = self
      .resource_context
      .as_ref()
      .and_then(|ctx| ctx.policy.document_origin.as_ref());
    let Some(document_origin) = document_origin else {
      // Without a known document origin, avoid over-blocking.
      return Ok(());
    };
    let credentials_mode = fetch_credentials_mode_for_crossorigin(crossorigin);
    if let Err(reason) = crate::resource::validate_cors_allow_origin(
      resource,
      requested_url,
      Some(document_origin),
      credentials_mode,
    ) {
      return Err(Error::Image(ImageError::LoadFailed {
        url: requested_url.to_string(),
        reason,
      }));
    }
    Ok(())
  }

  /// Enforce the active resource policy for subresources referenced within an SVG document.
  fn enforce_svg_resource_policy(&self, svg_content: &str, svg_url: &str) -> Result<()> {
    let Some(ctx) = &self.resource_context else {
      return Ok(());
    };

    // Determine the base URL for resolving `xml:base` chains. Inline SVGs are rendered with the
    // synthetic URL `"inline-svg"`, so we must fall back to the embedding document URL when
    // available; otherwise `xml:base` values like `//cdn.example/...` would incorrectly resolve
    // against the dummy base and could bypass policy enforcement on cached pixmaps.
    let document_base_url = Url::parse(svg_url).ok().map(|_| svg_url).or_else(|| {
      ctx
        .document_url
        .as_deref()
        .filter(|doc_url| Url::parse(doc_url).is_ok())
    });

    // Avoid paying the cost of an XML parse when the SVG clearly cannot trigger any external
    // subresource fetches. Large SVG exports (Illustrator/Figma/etc.) can contain hundreds of
    // thousands of nodes and attributes (filters, clip paths, patterns, ...). Parsing those
    // documents with `roxmltree` in debug builds is extremely expensive and can dominate pageset
    // fixture runtime even though most of them only reference internal fragments (`url(#id)`) or
    // inline data URLs.
    //
    // `enforce_svg_resource_policy` is purely about *network* access. If every `href`/`src`/`url()`
    // reference is either:
    // - an internal fragment (`#...`), or
    // - a `data:` URL, or
    // - an `about:` URL,
    // then the resource policy has nothing to enforce and we can return early.
    fn svg_may_reference_external_resources(svg_content: &str) -> bool {
      fn is_safe_ref(value: &str) -> bool {
        let value = trim_ascii_whitespace(value);
        value.is_empty()
          || value.starts_with('#')
          || crate::resource::is_data_url(value)
          || is_about_url(value)
      }

      fn is_attr_boundary_before(b: u8) -> bool {
        b.is_ascii_whitespace() || matches!(b, b':' | b'<' | b'/' | b'?')
      }

      let bytes = svg_content.as_bytes();

      // Any `@import` could reference an external stylesheet (even if it later resolves to same
      // origin). Keep the conservative slow path.
      let mut i = 0usize;
      while i + 7 <= bytes.len() {
        if bytes[i] == b'@'
          && bytes[i + 1].to_ascii_lowercase() == b'i'
          && bytes[i + 2].to_ascii_lowercase() == b'm'
          && bytes[i + 3].to_ascii_lowercase() == b'p'
          && bytes[i + 4].to_ascii_lowercase() == b'o'
          && bytes[i + 5].to_ascii_lowercase() == b'r'
          && bytes[i + 6].to_ascii_lowercase() == b't'
        {
          return true;
        }
        i += 1;
      }

      // Scan for `url(...)` references. We only care about the first non-whitespace token inside
      // the parentheses; if it isn't a fragment/data/about URL, it may cause a fetch.
      let mut i = 0usize;
      while i + 4 <= bytes.len() {
        if bytes[i].to_ascii_lowercase() == b'u'
          && bytes[i + 1].to_ascii_lowercase() == b'r'
          && bytes[i + 2].to_ascii_lowercase() == b'l'
          && bytes[i + 3] == b'('
          && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric())
        {
          let mut j = i + 4;
          while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
          }
          if j >= bytes.len() {
            return true;
          }

          let quote = match bytes[j] {
            b'"' | b'\'' => {
              let q = bytes[j];
              j += 1;
              Some(q)
            }
            _ => None,
          };
          let start = j;
          let end = if let Some(q) = quote {
            while j < bytes.len() && bytes[j] != q {
              j += 1;
            }
            j
          } else {
            while j < bytes.len()
              && !bytes[j].is_ascii_whitespace()
              && bytes[j] != b')'
              && bytes[j] != b';'
            {
              j += 1;
            }
            j
          };

          if let Ok(value) = std::str::from_utf8(&bytes[start..end]) {
            if !is_safe_ref(value) {
              return true;
            }
          } else {
            return true;
          }
        }
        i += 1;
      }

      // Scan for `href="..."` and `src="..."` style attributes. We don't attempt to validate the
      // element name here; false positives only trigger the slower XML parse, which is fine.
      for target in [&b"href"[..], &b"src"[..]] {
        let mut i = 0usize;
        while i + target.len() <= bytes.len() {
          if bytes[i].to_ascii_lowercase() == target[0]
            && bytes
              .get(i..i + target.len())
              .is_some_and(|slice| slice.eq_ignore_ascii_case(target))
            && (i == 0 || is_attr_boundary_before(bytes[i - 1]))
          {
            let after = bytes.get(i + target.len()).copied().unwrap_or(b' ');
            if !(after.is_ascii_whitespace() || after == b'=') {
              i += 1;
              continue;
            }

            let mut j = i + target.len();
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
              j += 1;
            }
            if j >= bytes.len() || bytes[j] != b'=' {
              i += 1;
              continue;
            }
            j += 1; // '='
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
              j += 1;
            }
            if j >= bytes.len() {
              return true;
            }

            let quote = match bytes[j] {
              b'"' | b'\'' => {
                let q = bytes[j];
                j += 1;
                Some(q)
              }
              _ => None,
            };
            let start = j;
            let end = if let Some(q) = quote {
              while j < bytes.len() && bytes[j] != q {
                j += 1;
              }
              j
            } else {
              while j < bytes.len()
                && !bytes[j].is_ascii_whitespace()
                && !matches!(bytes[j], b'>' | b'/')
              {
                j += 1;
              }
              j
            };

            if let Ok(value) = std::str::from_utf8(&bytes[start..end]) {
              if !is_safe_ref(value) {
                return true;
              }
            } else {
              return true;
            }
          }
          i += 1;
        }
      }

      false
    }

    if !svg_may_reference_external_resources(svg_content) {
      return Ok(());
    }

    // SVGs can reference external resources via multiple vectors:
    // - `href` / `xlink:href` attributes on elements like <image>, <use>, <feImage>, ...
    // - inline CSS: `style="... url(...) ..."`
    // - <style> blocks: `url(...)` and `@import`
    //
    // Keep scanning bounded so adversarial SVGs can't cause unbounded work or memory usage.
    const MAX_URL_REFERENCES: usize = 128;
    const MAX_CSS_BYTES_SCANNED: usize = 512 * 1024;

    let mut url_refs_seen = 0usize;
    let mut css_budget_remaining = MAX_CSS_BYTES_SCANNED;

    let mut check_url = |raw: &str, base: &str, kind: ResourceKind| -> Result<()> {
      let href = trim_ascii_whitespace(raw);
      if href.is_empty()
        || href.starts_with('#')
        || crate::resource::is_data_url(href)
        || is_about_url(href)
      {
        return Ok(());
      }

      url_refs_seen = url_refs_seen.saturating_add(1);
      if url_refs_seen > MAX_URL_REFERENCES {
        return Err(Error::Image(ImageError::LoadFailed {
          url: svg_url.to_string(),
          reason: format!(
            "SVG subresource scan exceeded the maximum of {MAX_URL_REFERENCES} URL references"
          ),
        }));
      }

      let resolved = resolve_against_base(base, href)
        .or_else(|| {
          ctx
            .document_url
            .as_deref()
            .and_then(|doc_url| resolve_against_base(doc_url, href))
        })
        .unwrap_or_else(|| href.to_string());
      if let Err(err) = ctx.check_allowed(kind, &resolved) {
        return Err(Error::Image(ImageError::LoadFailed {
          url: resolved,
          reason: format!("Blocked SVG subresource by policy: {}", err.reason),
        }));
      }
      Ok(())
    };

    fn contains_ascii_case_insensitive_url_open_paren(value: &str) -> bool {
      let bytes = value.as_bytes();
      if bytes.len() < 4 {
        return false;
      }
      let mut i = 0usize;
      while i + 3 < bytes.len() {
        let b0 = bytes[i];
        if b0 != b'u' && b0 != b'U' {
          i += 1;
          continue;
        }
        let b1 = bytes[i + 1];
        if b1 != b'r' && b1 != b'R' {
          i += 1;
          continue;
        }
        let b2 = bytes[i + 2];
        if b2 != b'l' && b2 != b'L' {
          i += 1;
          continue;
        }
        if bytes[i + 3] == b'(' {
          return true;
        }
        i += 1;
      }
      false
    }

    fn scan_css_urls<F: FnMut(&str, ResourceKind) -> Result<()>>(
      css: &str,
      include_imports: bool,
      budget_remaining: &mut usize,
      svg_url: &str,
      record: &mut F,
    ) -> Result<()> {
      use cssparser::{Parser, ParserInput, Token};

      if css.is_empty() {
        return Ok(());
      }

      // Ensure CSS scanning remains bounded. Subtract based on the bytes actually scanned so this
      // cap works across multiple style attributes/blocks.
      let css_len = css.len();
      if css_len > *budget_remaining {
        return Err(Error::Image(ImageError::LoadFailed {
          url: svg_url.to_string(),
          reason: format!(
            "SVG embedded CSS exceeded the scan budget of {MAX_CSS_BYTES_SCANNED} bytes"
          ),
        }));
      }
      *budget_remaining -= css_len;

      let mut input = ParserInput::new(css);
      let mut parser = Parser::new(&mut input);

      fn scan_parser<'i, 't, F: FnMut(&str, ResourceKind) -> Result<()>>(
        parser: &mut cssparser::Parser<'i, 't>,
        include_imports: bool,
        svg_url: &str,
        record: &mut F,
        depth: usize,
        in_font_face: bool,
      ) -> Result<()> {
        // Avoid pathological recursion on deeply nested blocks.
        const MAX_DEPTH: usize = 32;
        if depth > MAX_DEPTH {
          return Err(Error::Image(ImageError::LoadFailed {
            url: svg_url.to_string(),
            reason: "SVG embedded CSS exceeded the maximum nested parse depth".to_string(),
          }));
        }

        // Track whether we've seen a `@font-face` at-rule token and are now looking for its `{...}`
        // block. Once we enter the block, all `url(...)` tokens inside are treated as font loads
        // so CSP `font-src` is enforced for SVG-embedded CSS.
        let mut pending_font_face_block = false;

        while let Ok(token) = parser.next_including_whitespace_and_comments() {
          match token {
            Token::UnquotedUrl(url) => {
              let kind = if in_font_face {
                ResourceKind::Font
              } else {
                ResourceKind::Image
              };
              record(url.as_ref(), kind)?;
            }
            Token::Function(ref name) if name.eq_ignore_ascii_case("url") => {
              let mut nested_error: Option<Error> = None;
              let parsed = parser.parse_nested_block(|nested| {
                let mut url: Option<cssparser::CowRcStr<'i>> = None;

                while !nested.is_exhausted() {
                  match nested.next_including_whitespace_and_comments() {
                    Ok(Token::WhiteSpace(_)) | Ok(Token::Comment(_)) => {}
                    Ok(Token::QuotedString(s))
                    | Ok(Token::UnquotedUrl(s))
                    | Ok(Token::Ident(s)) => {
                      url = Some(s.clone());
                      break;
                    }
                    Ok(Token::BadUrl(_)) => {
                      url = None;
                      break;
                    }
                    Ok(Token::Function(_))
                    | Ok(Token::ParenthesisBlock)
                    | Ok(Token::SquareBracketBlock)
                    | Ok(Token::CurlyBracketBlock) => {
                      // Ignore nested blocks when parsing the url() argument; only first token
                      // matters for our best-effort scan.
                      let _ =
                        nested.parse_nested_block(|_| Ok::<_, cssparser::ParseError<'i, ()>>(()));
                    }
                    Ok(_) => {}
                    Err(_) => break,
                  }
                }

                Ok::<_, cssparser::ParseError<'i, ()>>(url)
              });

              if let Some(err) = nested_error.take() {
                return Err(err);
              }

              if let Ok(Some(url)) = parsed {
                let kind = if in_font_face {
                  ResourceKind::Font
                } else {
                  ResourceKind::Image
                };
                record(url.as_ref(), kind)?;
              }
            }
            Token::AtKeyword(ref name) if name.eq_ignore_ascii_case("font-face") => {
              pending_font_face_block = true;
            }
            Token::AtKeyword(ref name)
              if include_imports && name.eq_ignore_ascii_case("import") =>
            {
              // `@import` accepts either a quoted string or a `url(...)` token.
              loop {
                let token = match parser.next_including_whitespace_and_comments() {
                  Ok(t) => t,
                  Err(_) => break,
                };
                match token {
                  Token::WhiteSpace(_) | Token::Comment(_) => continue,
                  Token::QuotedString(s) => {
                    record(s.as_ref(), ResourceKind::Stylesheet)?;
                    break;
                  }
                  Token::UnquotedUrl(s) => {
                    record(s.as_ref(), ResourceKind::Stylesheet)?;
                    break;
                  }
                  Token::Function(ref func) if func.eq_ignore_ascii_case("url") => {
                    let mut url: Option<cssparser::CowRcStr<'i>> = None;
                    let _ = parser.parse_nested_block(|nested| {
                      while !nested.is_exhausted() {
                        match nested.next_including_whitespace_and_comments() {
                          Ok(Token::WhiteSpace(_)) | Ok(Token::Comment(_)) => {}
                          Ok(Token::QuotedString(s))
                          | Ok(Token::UnquotedUrl(s))
                          | Ok(Token::Ident(s)) => {
                            url = Some(s.clone());
                            break;
                          }
                          Ok(Token::BadUrl(_)) => break,
                          Ok(_) => {}
                          Err(_) => break,
                        }
                      }
                      Ok::<_, cssparser::ParseError<'i, ()>>(())
                    });
                    if let Some(url) = url {
                      record(url.as_ref(), ResourceKind::Stylesheet)?;
                    }
                    break;
                  }
                  _ => break,
                }
              }
            }
            Token::Semicolon => {
              // At-rules without blocks terminate at the next semicolon. Clear any pending
              // `@font-face` state so we don't mis-classify the next `{...}` block.
              pending_font_face_block = false;
            }
            Token::CurlyBracketBlock => {
              let nested_in_font_face = in_font_face || pending_font_face_block;
              pending_font_face_block = false;

              let mut nested_error: Option<Error> = None;
              let _ = parser.parse_nested_block(|nested| {
                if let Err(err) = scan_parser(
                  nested,
                  include_imports,
                  svg_url,
                  record,
                  depth + 1,
                  nested_in_font_face,
                ) {
                  nested_error = Some(err);
                  return Err(nested.new_custom_error::<(), ()>(()));
                }
                Ok::<_, cssparser::ParseError<'i, ()>>(())
              });
              if let Some(err) = nested_error {
                return Err(err);
              }
            }
            Token::Function(_) | Token::ParenthesisBlock | Token::SquareBracketBlock => {
              let mut nested_error: Option<Error> = None;
              let _ = parser.parse_nested_block(|nested| {
                if let Err(err) = scan_parser(
                  nested,
                  include_imports,
                  svg_url,
                  record,
                  depth + 1,
                  in_font_face,
                ) {
                  nested_error = Some(err);
                  return Err(nested.new_custom_error::<(), ()>(()));
                }
                Ok::<_, cssparser::ParseError<'i, ()>>(())
              });
              if let Some(err) = nested_error {
                return Err(err);
              }
            }
            _ => {}
          }
        }

        Ok(())
      }

      scan_parser(&mut parser, include_imports, svg_url, record, 0, false)
    }

    let svg_for_parse = svg_markup_for_roxmltree(svg_content);
    let doc = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      roxmltree::Document::parse(svg_for_parse.as_ref())
    })) {
      Ok(Ok(doc)) => doc,
      Ok(Err(_)) | Err(_) => return Ok(()),
    };

    for node in doc.descendants() {
      if node.is_element() {
        let xml_base_chain = svg_xml_base_chain_for_node(node);
        let base = apply_svg_xml_base_chain(document_base_url, &xml_base_chain)
          .unwrap_or_else(|| document_base_url.unwrap_or(svg_url).to_string());

        for attr in node.attributes() {
          let local_name = attr
            .name()
            .rsplit_once(':')
            .map(|(_, name)| name)
            .unwrap_or(attr.name());
          let value = attr.value();

          let tag_name = node.tag_name().name();
          // SVG uses `href` in a variety of places (e.g. `<a href="...">` hyperlinks). We only
          // enforce the image subresource policy for elements that can actually trigger resource
          // fetches in our pipeline.
          let is_image_href = local_name == "href"
            && (tag_name.eq_ignore_ascii_case("image")
              || tag_name.eq_ignore_ascii_case("use")
              || tag_name.eq_ignore_ascii_case("feimage"));
          let is_image_src = local_name == "src" && tag_name.eq_ignore_ascii_case("image");
          if is_image_href || is_image_src {
            if svg_node_has_display_none(node) {
              continue;
            }
            check_url(value, &base, ResourceKind::Image)?;
            continue;
          }

          if local_name == "style" {
            scan_css_urls(
              value,
              false,
              &mut css_budget_remaining,
              svg_url,
              &mut |url, kind| check_url(url, &base, kind),
            )?;
            continue;
          }

          // Only scan other attributes when they plausibly contain `url(...)` references. Some SVG
          // attributes (notably <path d="...">) can be extremely large but are not CSS, and
          // scanning them unconditionally can exhaust our embedded-CSS scan budget.
          if !contains_ascii_case_insensitive_url_open_paren(value) {
            continue;
          }

          scan_css_urls(
            value,
            false,
            &mut css_budget_remaining,
            svg_url,
            &mut |url, kind| check_url(url, &base, kind),
          )?;
        }

        if node.tag_name().name() == "style" {
          // roxmltree normalizes CDATA sections into text nodes, so scanning text nodes covers both.
          for child in node.children() {
            if child.is_text() {
              if let Some(text) = child.text() {
                scan_css_urls(
                  text,
                  true,
                  &mut css_budget_remaining,
                  svg_url,
                  &mut |url, kind| check_url(url, &base, kind),
                )?;
              }
            }
          }
        }
      }
    }

    Ok(())
  }

  fn fetch_and_decode(
    &self,
    cache_key: &str,
    resolved_url: &str,
    destination: FetchDestination,
    crossorigin: CrossOriginAttribute,
    referrer_policy: Option<ReferrerPolicy>,
  ) -> Result<Arc<CachedImage>> {
    let threshold_ms = image_profile_threshold_ms();
    let profile_enabled = threshold_ms.is_some();
    let total_start = profile_enabled.then(Instant::now);
    let fetch_start = profile_enabled.then(Instant::now);

    let referrer_url = self
      .resource_context
      .as_ref()
      .and_then(|ctx| ctx.document_url.as_deref());
    let doc_referrer_policy = self
      .resource_context
      .as_ref()
      .map(|ctx| ctx.referrer_policy)
      .unwrap_or_default();
    let request_referrer_policy = referrer_policy.unwrap_or(doc_referrer_policy);
    let credentials_mode = fetch_credentials_mode_for_crossorigin(crossorigin);
    let origin_fallback = referrer_url.and_then(origin_from_url);
    let client_origin = self
      .resource_context
      .as_ref()
      .and_then(|ctx| ctx.policy.document_origin.as_ref())
      .or(origin_fallback.as_ref());
    let fetch_url_no_fragment = strip_url_fragment(resolved_url);
    let mut request = FetchRequest::new(fetch_url_no_fragment.as_ref(), destination)
      .with_credentials_mode(credentials_mode);
    if let Some(origin) = client_origin {
      request = request.with_client_origin(origin);
    }
    if let Some(referrer_url) = referrer_url {
      request = request.with_referrer_url(referrer_url);
    }
    request = request.with_referrer_policy(request_referrer_policy);
    let resource = match self.fetcher.fetch_with_request(request) {
      Ok(res) => res,
      Err(err) => {
        if is_empty_body_error_for_image(&err) {
          let placeholder_key = self.canonical_cache_key_for_placeholder(cache_key, resolved_url);
          return Ok(self.cache_placeholder_image(&placeholder_key));
        }
        self.record_image_error(resolved_url, &err);
        return Err(err);
      }
    };
    // Offline fixtures (and some bot-mitigation paths) may substitute missing/invalid image bytes
    // with a deterministic 1×1 transparent PNG. Treat those resources as the shared `about:`
    // placeholder image so painters can detect and reject them (e.g. to render UA broken-image
    // UI for `<img>`).
    if crate::resource::content_type_is_offline_placeholder_png(resource.content_type.as_deref())
      || resource.bytes.as_slice() == crate::resource::offline_placeholder_png_bytes()
    {
      let placeholder_key = self.canonical_cache_key_for_placeholder(cache_key, resolved_url);
      return Ok(self.cache_placeholder_image(&placeholder_key));
    }
    if let Some(ctx) = &self.resource_context {
      let policy_url = resource.final_url.as_deref().unwrap_or(resolved_url);
      if let Err(err) = ctx.check_allowed(ResourceKind::Image, policy_url) {
        let blocked = Error::Image(ImageError::LoadFailed {
          url: resolved_url.to_string(),
          reason: err.reason,
        });
        self.record_image_error(resolved_url, &blocked);
        return Err(blocked);
      }
    }
    if resource.bytes.is_empty() {
      // Treat empty bodies the same as `about:` URL placeholders: callers (notably the painters)
      // rely on `ImageCache::is_placeholder_image` to detect missing images and render UA fallback
      // UI for `<img>` elements.
      let placeholder_key = self.canonical_cache_key_for_placeholder(cache_key, resolved_url);
      return Ok(self.cache_placeholder_image(&placeholder_key));
    }
    if should_substitute_markup_payload_for_image(
      resolved_url,
      resource.final_url.as_deref(),
      resource.status,
      &resource.bytes,
    ) {
      self.record_invalid_image(resolved_url);
      let placeholder_key = self.canonical_cache_key_for_placeholder(cache_key, resolved_url);
      return Ok(self.cache_placeholder_image(&placeholder_key));
    }
    if let Err(err) = ensure_http_success(&resource, resolved_url)
      .and_then(|()| ensure_image_mime_sane(&resource, resolved_url))
    {
      self.record_image_error(resolved_url, &err);
      return Err(err);
    }
    if let Err(err) = self.enforce_image_cors(resolved_url, &resource, crossorigin) {
      self.record_image_error(resolved_url, &err);
      return Err(err);
    }
    let fetch_ms = fetch_start.map(|s| s.elapsed().as_secs_f64() * 1000.0);
    let decode_timer = Instant::now();
    let decode_start = profile_enabled.then_some(decode_timer);
    let (
      img,
      has_alpha,
      orientation,
      resolution,
      is_vector,
      intrinsic_ratio,
      aspect_ratio_none,
      svg_content,
      svg_has_intrinsic_size,
    ) = match {
      // Image decoding can be triggered deep inside nested deadline budgets (e.g. display-list
      // builder time slices). Those scoped budgets are meant to bound renderer algorithm choice
      // (e.g. fall back to legacy paint) but should not cause subresource decoding to be dropped
      // when the overall render deadline still has time remaining.
      let deadline = render_control::root_deadline();
      render_control::with_deadline(deadline.as_ref(), || {
        self.decode_resource(&resource, resolved_url)
      })
    } {
      Ok(decoded) => decoded,
      Err(err) => {
        self.record_image_error(resolved_url, &err);
        return Err(err);
      }
    };
    if Self::should_map_decoded_image_to_placeholder(&resource, &img, has_alpha, is_vector) {
      let placeholder_key = self.canonical_cache_key_for_placeholder(cache_key, resolved_url);
      return Ok(self.cache_placeholder_image(&placeholder_key));
    }
    let decode_ms_value = decode_timer.elapsed().as_secs_f64() * 1000.0;
    let decode_ms = decode_start.map(|_| decode_ms_value);
    record_image_decode_ms(decode_ms_value);

    let is_animated = if !is_vector
      && (resource.bytes.starts_with(b"GIF87a") || resource.bytes.starts_with(b"GIF89a"))
    {
      matches!(
        Self::gif_is_animated(&resource.bytes),
        GifAnimationProbe::Determined(true)
      )
    } else {
      false
    };

    let img_arc = Arc::new(CachedImage {
      image: Arc::new(img),
      orientation,
      resolution,
      is_animated,
      has_alpha,
      is_vector,
      intrinsic_ratio,
      aspect_ratio_none,
      svg_content,
      svg_has_intrinsic_size,
    });

    let canonical_key = self.canonical_cache_key_for_bytes(cache_key, resolved_url, &resource.bytes);
    self.insert_cached_image(&canonical_key, Arc::clone(&img_arc));

    if let (Some(threshold_ms), Some(total_start)) = (threshold_ms, total_start) {
      let total_ms = total_start.elapsed().as_secs_f64() * 1000.0;
      if total_ms >= threshold_ms {
        let content_type = resource
          .content_type
          .as_deref()
          .unwrap_or("<unknown>")
          .split(';')
          .next()
          .unwrap_or("<unknown>");
        eprintln!(
          "image_profile kind=decode total_ms={total_ms:.2} fetch_ms={:.2} decode_ms={:.2} bytes={} dims={}x{} vector={} url={}",
          fetch_ms.unwrap_or(0.0),
          decode_ms.unwrap_or(0.0),
          resource.bytes.len(),
          img_arc.image.width(),
          img_arc.image.height(),
          img_arc.is_vector,
          resolved_url
        );
        eprintln!(" image_profile content_type={content_type}");
      }
    }

    Ok(img_arc)
  }

  fn decode_resource_into_cache(
    &self,
    cache_key: &str,
    resolved_url: &str,
    resource: &FetchedResource,
    crossorigin: CrossOriginAttribute,
  ) -> Result<Arc<CachedImage>> {
    // Offline fixtures substitute missing images with a deterministic 1×1 transparent PNG so layout
    // and paint can proceed without hard failures. Treat that payload as the same placeholder image
    // used for `about:` URLs so callers can detect it (e.g. painters may want to render a "broken
    // image" UI for `<img>` while keeping CSS background images transparent).
    if resource.bytes.as_slice() == crate::resource::offline_placeholder_png_bytes() {
      let placeholder_key = self.canonical_cache_key_for_placeholder(cache_key, resolved_url);
      return Ok(self.cache_placeholder_image(&placeholder_key));
    }
    if resource.bytes.is_empty() {
      let placeholder_key = self.canonical_cache_key_for_placeholder(cache_key, resolved_url);
      return Ok(self.cache_placeholder_image(&placeholder_key));
    }
    // `ResourceFetcher` implementations (notably the offline fixture tooling) may substitute missing
    // image responses with a deterministic 1×1 transparent PNG. This is a "missing image" sentinel
    // and must behave like our internal `about:` placeholder so callers can reliably detect it
    // (e.g. replaced `<img>` fallback UI / broken-image icon).
    if crate::resource::content_type_is_offline_placeholder_png(resource.content_type.as_deref())
      || resource.bytes.as_slice() == crate::resource::offline_placeholder_png_bytes()
    {
      let placeholder_key = self.canonical_cache_key_for_placeholder(cache_key, resolved_url);
      return Ok(self.cache_placeholder_image(&placeholder_key));
    }
    if should_substitute_markup_payload_for_image(
      resolved_url,
      resource.final_url.as_deref(),
      resource.status,
      &resource.bytes,
    ) {
      self.record_invalid_image(resolved_url);
      let placeholder_key = self.canonical_cache_key_for_placeholder(cache_key, resolved_url);
      return Ok(self.cache_placeholder_image(&placeholder_key));
    }
    if let Err(err) = self.enforce_image_cors(resolved_url, resource, crossorigin) {
      self.record_image_error(resolved_url, &err);
      return Err(err);
    }
    let threshold_ms = image_profile_threshold_ms();
    let profile_enabled = threshold_ms.is_some();
    let total_start = profile_enabled.then(Instant::now);
    let decode_timer = Instant::now();
    let decode_start = profile_enabled.then_some(decode_timer);
    let (
      img,
      has_alpha,
      orientation,
      resolution,
      is_vector,
      intrinsic_ratio,
      aspect_ratio_none,
      svg_content,
      svg_has_intrinsic_size,
    ) = {
      // See `fetch_and_decode` for why we temporarily switch to the root deadline for decoding.
      let deadline = render_control::root_deadline();
      render_control::with_deadline(deadline.as_ref(), || {
        self.decode_resource(resource, resolved_url)
      })
    }?;
    if Self::should_map_decoded_image_to_placeholder(resource, &img, has_alpha, is_vector) {
      let placeholder_key = self.canonical_cache_key_for_placeholder(cache_key, resolved_url);
      return Ok(self.cache_placeholder_image(&placeholder_key));
    }
    let decode_ms_value = decode_timer.elapsed().as_secs_f64() * 1000.0;
    let decode_ms = decode_start.map(|_| decode_ms_value);
    record_image_decode_ms(decode_ms_value);

    let is_animated = if !is_vector
      && (resource.bytes.starts_with(b"GIF87a") || resource.bytes.starts_with(b"GIF89a"))
    {
      matches!(
        Self::gif_is_animated(&resource.bytes),
        GifAnimationProbe::Determined(true)
      )
    } else {
      false
    };

    let img_arc = Arc::new(CachedImage {
      image: Arc::new(img),
      orientation,
      resolution,
      is_animated,
      has_alpha,
      is_vector,
      intrinsic_ratio,
      aspect_ratio_none,
      svg_content,
      svg_has_intrinsic_size,
    });

    let canonical_key = self.canonical_cache_key_for_bytes(cache_key, resolved_url, &resource.bytes);
    self.insert_cached_image(&canonical_key, Arc::clone(&img_arc));

    if let (Some(threshold_ms), Some(total_start)) = (threshold_ms, total_start) {
      let total_ms = total_start.elapsed().as_secs_f64() * 1000.0;
      if total_ms >= threshold_ms {
        let content_type = resource
          .content_type
          .as_deref()
          .unwrap_or("<unknown>")
          .split(';')
          .next()
          .unwrap_or("<unknown>");
        eprintln!(
          "image_profile kind=decode total_ms={total_ms:.2} fetch_ms=0.00 decode_ms={:.2} bytes={} dims={}x{} vector={} url={}",
          decode_ms.unwrap_or(0.0),
          resource.bytes.len(),
          img_arc.image.width(),
          img_arc.image.height(),
          img_arc.is_vector,
          resolved_url
        );
        eprintln!(" image_profile content_type={content_type}");
      }
    }

    Ok(img_arc)
  }

  fn fetch_and_probe(
    &self,
    cache_key: &str,
    resolved_url: &str,
    destination: FetchDestination,
    crossorigin: CrossOriginAttribute,
    referrer_policy: Option<ReferrerPolicy>,
  ) -> Result<Arc<CachedImageMetadata>> {
    let threshold_ms = image_profile_threshold_ms();
    let profile_enabled = threshold_ms.is_some();
    let total_start = profile_enabled.then(Instant::now);
    let fetch_start = profile_enabled.then(Instant::now);
    let referrer_url = self
      .resource_context
      .as_ref()
      .and_then(|ctx| ctx.document_url.as_deref());
    let doc_referrer_policy = self
      .resource_context
      .as_ref()
      .map(|ctx| ctx.referrer_policy)
      .unwrap_or_default();
    let request_referrer_policy = referrer_policy.unwrap_or(doc_referrer_policy);
    let credentials_mode = fetch_credentials_mode_for_crossorigin(crossorigin);
    let origin_fallback = referrer_url.and_then(origin_from_url);
    let client_origin = self
      .resource_context
      .as_ref()
      .and_then(|ctx| ctx.policy.document_origin.as_ref())
      .or(origin_fallback.as_ref());
    let fetch_url_no_fragment = strip_url_fragment(resolved_url);
    let mut request = FetchRequest::new(fetch_url_no_fragment.as_ref(), destination)
      .with_credentials_mode(credentials_mode);
    if let Some(origin) = client_origin {
      request = request.with_client_origin(origin);
    }
    if let Some(referrer_url) = referrer_url {
      request = request.with_referrer_url(referrer_url);
    }
    request = request.with_referrer_policy(request_referrer_policy);

    let check_resource_allowed = |resource: &FetchedResource| -> Result<()> {
      if let Some(ctx) = &self.resource_context {
        let policy_url = resource.final_url.as_deref().unwrap_or(resolved_url);
        if let Err(err) = ctx.check_allowed(ResourceKind::Image, policy_url) {
          return Err(Error::Image(ImageError::LoadFailed {
            url: resolved_url.to_string(),
            reason: err.reason,
          }));
        }
      }
      ensure_http_success(resource, resolved_url)?;
      self.enforce_image_cors(resolved_url, resource, crossorigin)
    };

    let probe_limit = image_probe_max_bytes();
    let retry_limit = probe_limit
      .saturating_mul(8)
      .max(512 * 1024)
      .clamp(1, 64 * 1024 * 1024);

    const RAW_RESOURCE_CACHE_LIMIT_BYTES: usize = 5 * 1024 * 1024;

    for (idx, limit) in [probe_limit, retry_limit].into_iter().enumerate() {
      let resource = match self
        .fetcher
        .fetch_partial_with_request(request.clone(), limit)
      {
        Ok(res) => res,
        Err(err) => {
          if is_empty_body_error_for_image(&err) {
            return Ok(self.cache_placeholder_metadata(cache_key));
          }
          let _ = err;
          break;
        }
      };
      // Some servers reject `Range` requests for images (or bot-mitigation paths) with status codes
      // like 405/416 even though a full GET without a `Range` header would succeed. In that case,
      // skip reporting a fetch error from the probe and fall back to a full fetch.
      if matches!(resource.status, Some(405 | 416)) {
        record_probe_partial_fetch(resource.bytes.len());
        break;
      }
      record_probe_partial_fetch(resource.bytes.len());
      let resource = Arc::new(resource);

      if let Err(err) = check_resource_allowed(resource.as_ref()) {
        self.record_image_error(resolved_url, &err);
        return Err(err);
      }
      if resource.bytes.is_empty()
        || crate::resource::content_type_is_offline_placeholder_png(
          resource.content_type.as_deref(),
        )
        || resource.bytes.as_slice() == crate::resource::offline_placeholder_png_bytes()
      {
        // Preserve the raw probe response when it was small enough to fit in this probe prefix so
        // a later `load()` can reuse it without issuing another HTTP request.
        if !resource.bytes.is_empty()
          && resource.bytes.len() < limit
          && resource.bytes.len() <= RAW_RESOURCE_CACHE_LIMIT_BYTES
        {
          if let Ok(mut cache) = self.raw_cache.lock() {
            let key = cache_key.to_string();
            let bytes = Self::estimate_raw_cache_entry_bytes(&key, resource.as_ref());
            cache.insert(key, Arc::clone(&resource), bytes);
          }
        }
        return Ok(self.cache_placeholder_metadata(cache_key));
      }
      if should_substitute_markup_payload_for_image(
        resolved_url,
        resource.final_url.as_deref(),
        resource.status,
        &resource.bytes,
      ) {
        self.record_invalid_image(resolved_url);
        return Ok(self.cache_placeholder_metadata(cache_key));
      }
      if let Err(err) = ensure_image_mime_sane(resource.as_ref(), resolved_url) {
        self.record_image_error(resolved_url, &err);
        return Err(err);
      }

      let fetch_ms = fetch_start.map(|s| s.elapsed().as_secs_f64() * 1000.0);
      let attempt_probe_start = profile_enabled.then(Instant::now);
      match self.probe_resource(&resource, resolved_url) {
        Ok(meta) => {
          let probe_ms = attempt_probe_start.map(|s| s.elapsed().as_secs_f64() * 1000.0);
          let meta = Arc::new(meta);

          if let Ok(mut cache) = self.meta_cache.lock() {
            let key = cache_key.to_string();
            let bytes = Self::estimate_meta_cache_entry_bytes(&key, meta.as_ref());
            cache.insert(key, Arc::clone(&meta), bytes);
          }
          if let Some(serialized) = encode_probe_metadata_for_disk(&meta) {
            self.fetcher.write_cache_artifact_with_request(
              request,
              CacheArtifactKind::ImageProbeMetadata,
              &serialized,
              Some(resource.as_ref()),
            );
          }

          // When the image is small enough to fit in the probe prefix, keep the bytes so a later
          // decode can reuse them without issuing another HTTP request.
          if resource.bytes.len() < limit && resource.bytes.len() <= RAW_RESOURCE_CACHE_LIMIT_BYTES
          {
            if let Ok(mut cache) = self.raw_cache.lock() {
              let key = cache_key.to_string();
              let bytes = Self::estimate_raw_cache_entry_bytes(&key, resource.as_ref());
              cache.insert(key, Arc::clone(&resource), bytes);
            }
          }

          if let (Some(threshold_ms), Some(total_start)) = (threshold_ms, total_start) {
            let total_ms = total_start.elapsed().as_secs_f64() * 1000.0;
            if total_ms >= threshold_ms {
              let content_type = resource
                .content_type
                .as_deref()
                .unwrap_or("<unknown>")
                .split(';')
                .next()
                .unwrap_or("<unknown>");
              eprintln!(
                "image_profile kind=probe total_ms={total_ms:.2} fetch_ms={:.2} probe_ms={:.2} bytes={} dims={}x{} vector={} url={}",
                fetch_ms.unwrap_or(0.0),
                probe_ms.unwrap_or(0.0),
                resource.bytes.len(),
                meta.width,
                meta.height,
                meta.is_vector,
                resolved_url
              );
              eprintln!(" image_profile content_type={content_type}");
            }
          }

          return Ok(meta);
        }
        Err(err) => {
          // Only retry/fallback when it looks like the prefix may have been truncated.
          if resource.bytes.len() < limit {
            self.record_image_error(resolved_url, &err);
            return Err(err);
          }
          let _ = err;
          // Retry once with a larger prefix, then fall back to a full fetch.
          if idx == 0 {
            continue;
          }
          break;
        }
      }
    }

    if probe_limit > 0 {
      record_probe_partial_fallback_full();
    }

    let resource = match self.fetcher.fetch_with_request(request.clone()) {
      Ok(res) => res,
      Err(err) => {
        if is_empty_body_error_for_image(&err) {
          return Ok(self.cache_placeholder_metadata(cache_key));
        }
        self.record_image_error(resolved_url, &err);
        return Err(err);
      }
    };
    let resource = Arc::new(resource);
    if let Err(err) = check_resource_allowed(resource.as_ref()) {
      self.record_image_error(resolved_url, &err);
      return Err(err);
    }
    if resource.bytes.is_empty()
      || crate::resource::content_type_is_offline_placeholder_png(resource.content_type.as_deref())
      || resource.bytes.as_slice() == crate::resource::offline_placeholder_png_bytes()
    {
      if !resource.bytes.is_empty() && resource.bytes.len() <= RAW_RESOURCE_CACHE_LIMIT_BYTES {
        if let Ok(mut cache) = self.raw_cache.lock() {
          let key = cache_key.to_string();
          let bytes = Self::estimate_raw_cache_entry_bytes(&key, resource.as_ref());
          cache.insert(key, Arc::clone(&resource), bytes);
        }
      }
      return Ok(self.cache_placeholder_metadata(cache_key));
    }
    if should_substitute_markup_payload_for_image(
      resolved_url,
      resource.final_url.as_deref(),
      resource.status,
      &resource.bytes,
    ) {
      self.record_invalid_image(resolved_url);
      return Ok(self.cache_placeholder_metadata(cache_key));
    }
    if let Err(err) = ensure_image_mime_sane(resource.as_ref(), resolved_url) {
      self.record_image_error(resolved_url, &err);
      return Err(err);
    }
    let fetch_ms = fetch_start.map(|s| s.elapsed().as_secs_f64() * 1000.0);
    let probe_start = profile_enabled.then(Instant::now);
    let meta = match self.probe_resource(&resource, resolved_url) {
      Ok(meta) => meta,
      Err(err) => {
        self.record_image_error(resolved_url, &err);
        return Err(err);
      }
    };
    let probe_ms = probe_start.map(|s| s.elapsed().as_secs_f64() * 1000.0);
    let meta = Arc::new(meta);

    if let Ok(mut cache) = self.meta_cache.lock() {
      let key = cache_key.to_string();
      let bytes = Self::estimate_meta_cache_entry_bytes(&key, meta.as_ref());
      cache.insert(key, Arc::clone(&meta), bytes);
    }
    if let Some(serialized) = encode_probe_metadata_for_disk(&meta) {
      self.fetcher.write_cache_artifact_with_request(
        request,
        CacheArtifactKind::ImageProbeMetadata,
        &serialized,
        Some(resource.as_ref()),
      );
    }

    if resource.bytes.len() <= RAW_RESOURCE_CACHE_LIMIT_BYTES {
      if let Ok(mut cache) = self.raw_cache.lock() {
        let key = cache_key.to_string();
        let bytes = Self::estimate_raw_cache_entry_bytes(&key, resource.as_ref());
        cache.insert(key, Arc::clone(&resource), bytes);
      }
    }

    if let (Some(threshold_ms), Some(total_start)) = (threshold_ms, total_start) {
      let total_ms = total_start.elapsed().as_secs_f64() * 1000.0;
      if total_ms >= threshold_ms {
        let content_type = resource
          .content_type
          .as_deref()
          .unwrap_or("<unknown>")
          .split(';')
          .next()
          .unwrap_or("<unknown>");
        eprintln!(
          "image_profile kind=probe total_ms={total_ms:.2} fetch_ms={:.2} probe_ms={:.2} bytes={} dims={}x{} vector={} url={}",
          fetch_ms.unwrap_or(0.0),
          probe_ms.unwrap_or(0.0),
          resource.bytes.len(),
          meta.width,
          meta.height,
          meta.is_vector,
          resolved_url
        );
        eprintln!(" image_profile content_type={content_type}");
      }
    }

    Ok(meta)
  }

  fn join_inflight(&self, resolved_url: &str) -> (Arc<DecodeInFlight>, bool) {
    let mut map = match self.in_flight.lock() {
      Ok(map) => map,
      Err(poisoned) => {
        let mut map = poisoned.into_inner();
        map.clear();
        map
      }
    };
    if let Some(existing) = map.get(resolved_url) {
      return (Arc::clone(existing), false);
    }

    let flight = Arc::new(DecodeInFlight::new());
    map.insert(resolved_url.to_string(), Arc::clone(&flight));
    (flight, true)
  }

  fn join_meta_inflight(&self, resolved_url: &str) -> (Arc<ProbeInFlight>, bool) {
    let mut map = match self.meta_in_flight.lock() {
      Ok(map) => map,
      Err(poisoned) => {
        let mut map = poisoned.into_inner();
        map.clear();
        map
      }
    };
    if let Some(existing) = map.get(resolved_url) {
      return (Arc::clone(existing), false);
    }

    let flight = Arc::new(ProbeInFlight::new());
    map.insert(resolved_url.to_string(), Arc::clone(&flight));
    (flight, true)
  }

  fn finish_inflight(
    &self,
    resolved_url: &str,
    flight: &Arc<DecodeInFlight>,
    result: SharedImageResult,
  ) {
    flight.set(result);
    let mut map = self
      .in_flight
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    map.remove(resolved_url);
  }

  fn finish_meta_inflight(
    &self,
    resolved_url: &str,
    flight: &Arc<ProbeInFlight>,
    result: SharedMetaResult,
  ) {
    flight.set(result);
    let mut map = self
      .meta_in_flight
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    map.remove(resolved_url);
  }

  fn preprocess_svg_markup<'a>(
    &self,
    svg_content: &'a str,
    svg_url: &str,
  ) -> Result<SvgPreprocessedMarkup<'a>> {
    let key = svg_preprocess_key(svg_content, svg_url);
    if let Ok(mut cache) = self.svg_preprocess_cache.lock() {
      if let Some(cached) = cache.get_cloned(&key) {
        return Ok(SvgPreprocessedMarkup::Shared(cached));
      }
    }

    let svg_url_no_fragment = strip_url_fragment(svg_url);

    let svg_external_fragments_inlined = inline_svg_external_url_fragment_references(
      svg_content,
      svg_url_no_fragment.as_ref(),
      self.fetcher.as_ref(),
      self.resource_context.as_ref(),
      Some(&self.svg_subresource_cache),
    )?;
    let svg_use_inlined = inline_svg_use_references(
      svg_external_fragments_inlined.as_ref(),
      svg_url_no_fragment.as_ref(),
      self.fetcher.as_ref(),
      self.resource_context.as_ref(),
      Some(&self.svg_subresource_cache),
    )?;
    let svg_images_inlined = inline_svg_image_references(
      svg_use_inlined.as_ref(),
      svg_url_no_fragment.as_ref(),
      self.fetcher.as_ref(),
      self.resource_context.as_ref(),
      Some(&self.svg_subresource_cache),
    )?;
    let svg_fragment_applied = apply_svg_url_fragment(svg_images_inlined.as_ref(), svg_url);
    let svg_imports_inlined = inline_svg_style_imports(
      svg_fragment_applied.as_ref(),
      svg_url_no_fragment.as_ref(),
      self.fetcher.as_ref(),
      self.resource_context.as_ref(),
    )?;

    let modified = matches!(svg_external_fragments_inlined, Cow::Owned(_))
      || matches!(svg_use_inlined, Cow::Owned(_))
      || matches!(svg_images_inlined, Cow::Owned(_))
      || matches!(svg_fragment_applied, Cow::Owned(_))
      || matches!(svg_imports_inlined, Cow::Owned(_));
    if !modified {
      return Ok(SvgPreprocessedMarkup::Borrowed(svg_content));
    }

    let preprocessed = match svg_imports_inlined {
      Cow::Owned(s) => s,
      Cow::Borrowed(_) => match svg_fragment_applied {
        Cow::Owned(s) => s,
        Cow::Borrowed(_) => match svg_images_inlined {
          Cow::Owned(s) => s,
          Cow::Borrowed(_) => match svg_use_inlined {
            Cow::Owned(s) => s,
            Cow::Borrowed(_) => match svg_external_fragments_inlined {
              Cow::Owned(s) => s,
              Cow::Borrowed(_) => {
                return Ok(SvgPreprocessedMarkup::Borrowed(svg_content));
              }
            },
          },
        },
      },
    };

    let preprocessed = Arc::<str>::from(preprocessed);
    let bytes = std::mem::size_of::<SvgPreprocessKey>()
      .saturating_add(std::mem::size_of::<Arc<str>>())
      .saturating_add(preprocessed.len());
    if let Ok(mut cache) = self.svg_preprocess_cache.lock() {
      cache.insert(key, Arc::clone(&preprocessed), bytes);
    }
    Ok(SvgPreprocessedMarkup::Shared(preprocessed))
  }

  /// Render raw SVG content to an image, caching by content hash.
  pub fn render_svg(&self, svg_content: &str) -> Result<Arc<CachedImage>> {
    record_image_cache_request();
    let cache_key = inline_svg_cache_key(svg_content);
    if let Some(image) = self.get_cached(&cache_key) {
      self.enforce_svg_resource_policy(svg_content, "inline-svg")?;
      record_image_cache_hit();
      return Ok(image);
    }
    record_image_cache_miss();
    let decode_timer = Instant::now();
    let (img, intrinsic_ratio, aspect_ratio_none, svg_has_intrinsic_size) =
      self.render_svg_to_image_with_url(svg_content, "inline-svg")?;
    let svg_content = Arc::<str>::from(svg_content);
    record_image_decode_ms(decode_timer.elapsed().as_secs_f64() * 1000.0);
    let cached = Arc::new(CachedImage {
      image: Arc::new(img),
      orientation: None,
      resolution: None,
      is_animated: false,
      has_alpha: true,
      is_vector: true,
      intrinsic_ratio,
      aspect_ratio_none,
      svg_content: Some(svg_content),
      svg_has_intrinsic_size,
    });
    self.insert_cached_image(&cache_key, Arc::clone(&cached));
    Ok(cached)
  }

  pub fn render_svg_pixmap_at_size(
    &self,
    svg_content: &str,
    render_width: u32,
    render_height: u32,
    url: &str,
    device_pixel_ratio: f32,
  ) -> Result<Arc<tiny_skia::Pixmap>> {
    self.enforce_svg_resource_policy(svg_content, url)?;
    self.enforce_decode_limits(render_width, render_height, url)?;
    check_root(RenderStage::Paint).map_err(Error::Render)?;

    let key = svg_pixmap_key(
      svg_content,
      url,
      device_pixel_ratio,
      render_width,
      render_height,
    );
    record_image_cache_request();
    if let Ok(mut cache) = self.svg_pixmap_cache.lock() {
      if let Some(cached) = cache.get_cloned(&key) {
        record_image_cache_hit();
        return Ok(cached);
      }
    }

    record_image_cache_miss();
    self.render_svg_pixmap_at_size_uncached(svg_content, key, render_width, render_height, url)
  }

  pub fn render_svg_pixmap_at_size_with_injected_style(
    &self,
    svg_content: &str,
    insert_pos: usize,
    style_element: &str,
    render_width: u32,
    render_height: u32,
    url: &str,
    device_pixel_ratio: f32,
  ) -> Result<Arc<tiny_skia::Pixmap>> {
    self.enforce_decode_limits(render_width, render_height, url)?;
    check_root(RenderStage::Paint).map_err(Error::Render)?;

    let Some(prefix) = svg_content.get(..insert_pos) else {
      return self.render_svg_pixmap_at_size(
        svg_content,
        render_width,
        render_height,
        url,
        device_pixel_ratio,
      );
    };
    let Some(suffix) = svg_content.get(insert_pos..) else {
      return self.render_svg_pixmap_at_size(
        svg_content,
        render_width,
        render_height,
        url,
        device_pixel_ratio,
      );
    };

    let mut combined = String::with_capacity(prefix.len() + style_element.len() + suffix.len());
    combined.push_str(prefix);
    combined.push_str(style_element);
    combined.push_str(suffix);

    // Policy enforcement must consider the *final* SVG markup. The injected `<style>` element can
    // contain `@import`/`url(...)` references whose resolution depends on the SVG's `xml:base`
    // chain, so scanning the fragments separately can under/over-enforce and allow cached pixmaps
    // to bypass policy.
    self.enforce_svg_resource_policy(&combined, url)?;

    let key = svg_pixmap_key(
      &combined,
      url,
      device_pixel_ratio,
      render_width,
      render_height,
    );

    record_image_cache_request();
    if let Ok(mut cache) = self.svg_pixmap_cache.lock() {
      if let Some(cached) = cache.get_cloned(&key) {
        record_image_cache_hit();
        return Ok(cached);
      }
    }

    record_image_cache_miss();
    self.render_svg_pixmap_at_size_uncached(&combined, key, render_width, render_height, url)
  }

  fn render_svg_pixmap_at_size_uncached(
    &self,
    svg_content: &str,
    key: SvgPixmapKey,
    render_width: u32,
    render_height: u32,
    url: &str,
  ) -> Result<Arc<tiny_skia::Pixmap>> {
    use resvg::usvg;

    let render_timer = Instant::now();

    let svg_preprocessed = self.preprocess_svg_markup(svg_content, url)?;
    let svg_viewport_resolved =
      svg_with_resolved_root_viewport_size(svg_preprocessed.as_ref(), render_width, render_height);
    let svg_content = svg_viewport_resolved.as_ref();
    if let Some(pixmap) = try_render_simple_svg_pixmap(svg_content, render_width, render_height)? {
      let pixmap = Arc::new(pixmap);
      record_image_decode_ms(render_timer.elapsed().as_secs_f64() * 1000.0);
      self.insert_svg_pixmap(key, Arc::clone(&pixmap));
      return Ok(pixmap);
    }

    let options = usvg_options_for_url(url);
    // `usvg` uses `roxmltree` under the hood. `roxmltree` deliberately rejects `<!DOCTYPE ...>`,
    // but many real-world SVGs include a doctype (e.g. Adobe Illustrator output, including the
    // IANA logo). Strip/blank out doctypes so vector images don't silently disappear.
    let svg_for_parse = svg_markup_for_roxmltree(svg_content);
    let tree = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      usvg::Tree::from_str(svg_for_parse.as_ref(), &options)
    })) {
      Ok(Ok(tree)) => tree,
      Ok(Err(e)) => {
        return Err(Error::Image(ImageError::DecodeFailed {
          url: url.to_string(),
          reason: format!("Failed to parse SVG: {}", e),
        }));
      }
      Err(panic) => {
        return Err(Error::Image(ImageError::DecodeFailed {
          url: url.to_string(),
          reason: format!("SVG parse panicked: {}", panic_payload_to_reason(&*panic)),
        }));
      }
    };

    let size = tree.size();
    let source_width = size.width();
    let source_height = size.height();
    if source_width <= 0.0 || source_height <= 0.0 {
      return Err(Error::Render(RenderError::CanvasCreationFailed {
        width: source_width as u32,
        height: source_height as u32,
      }));
    }

    let Some(mut pixmap) = new_pixmap(render_width, render_height) else {
      return Err(Error::Render(RenderError::CanvasCreationFailed {
        width: render_width,
        height: render_height,
      }));
    };

    let transform = match svg_view_box_root_transform(
      svg_content,
      source_width,
      source_height,
      render_width as f32,
      render_height as f32,
    ) {
      Some(transform) => transform,
      None => {
        let scale_x = render_width as f32 / source_width;
        let scale_y = render_height as f32 / source_height;
        tiny_skia::Transform::from_scale(scale_x, scale_y)
      }
    };
    check_root(RenderStage::Paint).map_err(Error::Render)?;
    if let Err(panic) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      resvg::render(&tree, transform, &mut pixmap.as_mut());
    })) {
      return Err(Error::Image(ImageError::DecodeFailed {
        url: url.to_string(),
        reason: format!("SVG render panicked: {}", panic_payload_to_reason(&*panic)),
      }));
    }
    check_root(RenderStage::Paint).map_err(Error::Render)?;

    let pixmap = Arc::new(pixmap);
    record_image_decode_ms(render_timer.elapsed().as_secs_f64() * 1000.0);
    self.insert_svg_pixmap(key, Arc::clone(&pixmap));

    Ok(pixmap)
  }

  /// Probe intrinsic SVG metadata (dimensions/aspect ratio) from raw markup without rasterizing.
  pub fn probe_svg_content(
    &self,
    svg_content: &str,
    url_hint: &str,
  ) -> Result<CachedImageMetadata> {
    self.enforce_svg_resource_policy(svg_content, url_hint)?;

    let svg_with_fragment = apply_svg_url_fragment(svg_content, url_hint);
    let svg_content = svg_with_fragment.as_ref();

    let (meta_width, meta_height, meta_ratio, aspect_ratio_none) =
      svg_intrinsic_metadata(svg_content, 16.0, 16.0).unwrap_or((None, None, None, false));

    let ratio = meta_ratio.filter(|r| *r > 0.0);
    let (target_width, target_height) =
      svg_intrinsic_target_dimensions(meta_width, meta_height, ratio);

    let width = target_width.max(1.0).round() as u32;
    let height = target_height.max(1.0).round() as u32;
    self.enforce_decode_limits(width, height, url_hint)?;

    let intrinsic_ratio = if aspect_ratio_none {
      None
    } else {
      ratio.or_else(|| {
        if height > 0 {
          Some(width as f32 / height as f32)
        } else {
          None
        }
      })
    };

    Ok(CachedImageMetadata {
      width,
      height,
      orientation: None,
      resolution: None,
      is_vector: true,
      is_animated: false,
      intrinsic_ratio,
      aspect_ratio_none,
    })
  }

  fn maybe_decompress_svgz(&self, bytes: &[u8], url: &str) -> Result<Option<Vec<u8>>> {
    if bytes.len() < 2 || bytes[0] != 0x1F || bytes[1] != 0x8B {
      return Ok(None);
    }

    let mut decoder = GzDecoder::new(bytes);
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    let mut deadline_counter = 0usize;

    loop {
      check_root_periodic(&mut deadline_counter, 32, RenderStage::Paint).map_err(Error::Render)?;
      let n = decoder.read(&mut buf).map_err(|e| {
        Error::Image(ImageError::DecodeFailed {
          url: url.to_string(),
          reason: format!("SVGZ decompression failed: {e}"),
        })
      })?;
      if n == 0 {
        break;
      }
      if out.len().saturating_add(n) > MAX_SVGZ_DECOMPRESSED_BYTES {
        return Err(Error::Image(ImageError::DecodeFailed {
          url: url.to_string(),
          reason: format!("SVGZ decompressed payload exceeded {MAX_SVGZ_DECOMPRESSED_BYTES} bytes"),
        }));
      }
      out.extend_from_slice(&buf[..n]);
    }

    Ok(Some(out))
  }

  /// Decode a fetched resource into an image
  fn decode_resource(
    &self,
    resource: &FetchedResource,
    url: &str,
  ) -> Result<(
    DynamicImage,
    bool,
    Option<OrientationTransform>,
    Option<f32>,
    bool,
    Option<f32>,
    bool,
    Option<Arc<str>>,
    bool,
  )> {
    let bytes = &resource.bytes;
    let content_type = resource.content_type.as_deref();
    check_root(RenderStage::Paint).map_err(Error::Render)?;
    if bytes.is_empty() {
      let img = RgbaImage::new(1, 1);
      return Ok((
        DynamicImage::ImageRgba8(img),
        true,
        None,
        None,
        false,
        None,
        false,
        None,
        true,
      ));
    }

    let url_hint =
      append_url_fragment_if_missing(resource.final_url.as_deref().unwrap_or(url), url);
    let url_hint_str = url_hint.as_ref();

    // Check if this is SVG (plain UTF-8 payload, or gzip-compressed `.svgz`).
    let mime_is_svg = content_type
      .map(|m| m.contains("image/svg"))
      .unwrap_or(false);
    let url_is_svgz = url_ends_with_svgz(url)
      || resource
        .final_url
        .as_deref()
        .is_some_and(url_ends_with_svgz);

    if let Ok(content) = std::str::from_utf8(bytes) {
      if mime_is_svg || svg_text_looks_like_markup(content) {
        let svg_content: Arc<str> = Arc::from(content);
        let (img, ratio, aspect_none, svg_has_intrinsic_size) =
          self.render_svg_to_image_with_url(&svg_content, url_hint_str)?;
        return Ok((
          img,
          true,
          None,
          None,
          true,
          ratio,
          aspect_none,
          Some(svg_content),
          svg_has_intrinsic_size,
        ));
      }
    } else if url_is_svgz || mime_is_svg {
      if let Some(decompressed) = self.maybe_decompress_svgz(bytes, url)? {
        if let Ok(content) = std::str::from_utf8(&decompressed) {
          if mime_is_svg || svg_text_looks_like_markup(content) {
            let svg_content: Arc<str> = Arc::from(content);
            let (img, ratio, aspect_none, svg_has_intrinsic_size) =
              self.render_svg_to_image_with_url(&svg_content, url_hint_str)?;
            return Ok((
              img,
              true,
              None,
              None,
              true,
              ratio,
              aspect_none,
              Some(svg_content),
              svg_has_intrinsic_size,
            ));
          }

          // Decompressed to UTF-8 but doesn't look like SVG markup; treat as a (possibly mislabelled)
          // bitmap payload.
          let (orientation, resolution) = Self::exif_metadata(&decompressed);
          return self
            .decode_bitmap(&decompressed, content_type, url)
            .map(|(img, has_alpha)| {
              (
                img,
                has_alpha,
                orientation,
                resolution,
                false,
                None,
                false,
                None,
                true,
              )
            });
        }

        // Not valid UTF-8 after decompression; treat as a (possibly mislabelled) bitmap.
        let (orientation, resolution) = Self::exif_metadata(&decompressed);
        return self
          .decode_bitmap(&decompressed, content_type, url)
          .map(|(img, has_alpha)| {
            (
              img,
              has_alpha,
              orientation,
              resolution,
              false,
              None,
              false,
              None,
              true,
            )
          });
      }
    }

    // Regular image - extract EXIF metadata and decode.
    let (orientation, resolution) = Self::exif_metadata(bytes);
    self
      .decode_bitmap(bytes, content_type, url)
      .map(|(img, has_alpha)| {
        (
          img,
          has_alpha,
          orientation,
          resolution,
          false,
          None,
          false,
          None,
          true,
        )
      })
  }

  fn probe_resource(&self, resource: &FetchedResource, url: &str) -> Result<CachedImageMetadata> {
    let bytes = &resource.bytes;
    let content_type = resource.content_type.as_deref();
    if bytes.is_empty() {
      return Ok((*about_url_placeholder_metadata()).clone());
    }

    let url_hint =
      append_url_fragment_if_missing(resource.final_url.as_deref().unwrap_or(url), url);
    let url_hint_str = url_hint.as_ref();

    // SVG (including gzip-compressed `.svgz` responses).
    let mime_is_svg = content_type
      .map(|m| m.contains("image/svg"))
      .unwrap_or(false);
    let url_is_svgz = url_ends_with_svgz(url)
      || resource
        .final_url
        .as_deref()
        .is_some_and(url_ends_with_svgz);

    if let Ok(content) = std::str::from_utf8(bytes) {
      if mime_is_svg || svg_text_looks_like_markup(content) {
        return self.probe_svg_content(content, url_hint_str);
      }
    } else if url_is_svgz || mime_is_svg {
      if let Some(decompressed) = self.maybe_decompress_svgz(bytes, url)? {
        if let Ok(content) = std::str::from_utf8(&decompressed) {
          if mime_is_svg || svg_text_looks_like_markup(content) {
            return self.probe_svg_content(content, url_hint_str);
          }
        }

        // Not UTF-8 (or not SVG markup) after decompression; treat as a bitmap probe on the
        // decompressed bytes.
        let bytes = decompressed;
        let (orientation, resolution) = Self::exif_metadata(&bytes);
        let format_from_content_type = Self::format_from_content_type(content_type);
        let (sniffed_format, sniff_panic) = Self::sniff_image_format(&bytes);
        let (dims, dims_panic) =
          self.predecoded_dimensions(&bytes, format_from_content_type, sniffed_format);
        let (width, height) = dims.ok_or_else(|| {
          let reason = dims_panic
            .or(sniff_panic)
            .map(|panic| format!("Image probe panicked: {panic}"))
            .unwrap_or_else(|| "Unable to determine image dimensions".to_string());
          Error::Image(ImageError::DecodeFailed {
            url: url.to_string(),
            reason,
          })
        })?;
        self.enforce_decode_limits(width, height, url)?;

        let is_animated = if matches!(format_from_content_type, Some(ImageFormat::Gif))
          || matches!(sniffed_format, Some(ImageFormat::Gif))
        {
          match Self::gif_is_animated(&bytes) {
            GifAnimationProbe::Determined(animated) => animated,
            GifAnimationProbe::NeedMoreData => {
              return Err(Error::Image(ImageError::DecodeFailed {
                url: url.to_string(),
                reason: "Unable to determine GIF animation status from probe bytes".to_string(),
              }));
            }
            GifAnimationProbe::Invalid => false,
          }
        } else {
          false
        };

        return Ok(CachedImageMetadata {
          width,
          height,
          orientation,
          resolution,
          is_vector: false,
          is_animated,
          intrinsic_ratio: None,
          aspect_ratio_none: false,
        });
      }
    }

    let (orientation, resolution) = Self::exif_metadata(bytes);
    let format_from_content_type = Self::format_from_content_type(content_type);
    let (sniffed_format, sniff_panic) = Self::sniff_image_format(bytes);
    let (dims, dims_panic) =
      self.predecoded_dimensions(bytes, format_from_content_type, sniffed_format);
    let (width, height) = dims.ok_or_else(|| {
      let reason = dims_panic
        .or(sniff_panic)
        .map(|panic| format!("Image probe panicked: {panic}"))
        .unwrap_or_else(|| "Unable to determine image dimensions".to_string());
      Error::Image(ImageError::DecodeFailed {
        url: url.to_string(),
        reason,
      })
    })?;
    self.enforce_decode_limits(width, height, url)?;

    let is_animated = if matches!(format_from_content_type, Some(ImageFormat::Gif))
      || matches!(sniffed_format, Some(ImageFormat::Gif))
    {
      match Self::gif_is_animated(bytes) {
        GifAnimationProbe::Determined(animated) => animated,
        GifAnimationProbe::NeedMoreData => {
          return Err(Error::Image(ImageError::DecodeFailed {
            url: url.to_string(),
            reason: "Unable to determine GIF animation status from probe bytes".to_string(),
          }));
        }
        GifAnimationProbe::Invalid => false,
      }
    } else {
      false
    };

    Ok(CachedImageMetadata {
      width,
      height,
      orientation,
      resolution,
      is_vector: false,
      is_animated,
      intrinsic_ratio: None,
      aspect_ratio_none: false,
    })
  }

  fn decode_bitmap(
    &self,
    bytes: &[u8],
    content_type: Option<&str>,
    url: &str,
  ) -> Result<(DynamicImage, bool)> {
    check_root(RenderStage::Paint).map_err(Error::Render)?;
    let format_from_content_type = Self::format_from_content_type(content_type);
    let (sniffed_format, sniff_panic) = Self::sniff_image_format(bytes);
    let icc_transform = extract_jpeg_icc_profile(bytes).and_then(|icc| icc_transform_to_srgb(&icc));
    let (pre_dims, dims_panic) =
      self.predecoded_dimensions(bytes, format_from_content_type, sniffed_format);
    let panic_error = dims_panic.or(sniff_panic).map(|panic| {
      Error::Image(ImageError::DecodeFailed {
        url: url.to_string(),
        reason: format!("Image decode panicked: {panic}"),
      })
    });
    if let Some((width, height)) = pre_dims {
      self.enforce_decode_limits(width, height, url)?;
    }
    let mut last_error: Option<Error> = None;

    #[cfg(feature = "avif")]
    {
      if matches!(format_from_content_type, Some(ImageFormat::Avif))
        || matches!(sniffed_format, Some(ImageFormat::Avif))
      {
        match Self::decode_avif(bytes) {
          Ok(img) => {
            let has_alpha = img.color().has_alpha();
            let img = self.finish_bitmap_decode(img, url)?;
            return Ok((img, has_alpha));
          }
          Err(AvifDecodeError::Timeout(err)) => return Err(Error::Render(err)),
          Err(AvifDecodeError::Image(err)) => last_error = Some(self.decode_error(url, err)),
        }
      }
    }

    if let Some(format) = format_from_content_type {
      if format != ImageFormat::Avif {
        check_root(RenderStage::Paint).map_err(Error::Render)?;
        match self.decode_with_format(bytes, format, url) {
          Ok((img, has_alpha)) => {
            let img = if let Some(transform) = icc_transform.as_ref() {
              let mut rgba = img.to_rgba8();
              transform.apply_rgba8_in_place(rgba.as_mut())?;
              DynamicImage::ImageRgba8(rgba)
            } else {
              img
            };
            let img = self.finish_bitmap_decode(img, url)?;
            return Ok((img, has_alpha));
          }
          Err(err) => {
            if let Error::Render(_) = err {
              return Err(err);
            }
            last_error = Some(err);
          }
        }
      }
    }

    if let Some(format) = sniffed_format {
      if Some(format) != format_from_content_type && format != ImageFormat::Avif {
        check_root(RenderStage::Paint).map_err(Error::Render)?;
        match self.decode_with_format(bytes, format, url) {
          Ok((img, has_alpha)) => {
            let img = if let Some(transform) = icc_transform.as_ref() {
              let mut rgba = img.to_rgba8();
              transform.apply_rgba8_in_place(rgba.as_mut())?;
              DynamicImage::ImageRgba8(rgba)
            } else {
              img
            };
            let img = self.finish_bitmap_decode(img, url)?;
            return Ok((img, has_alpha));
          }
          Err(err) => {
            if let Error::Render(_) = err {
              return Err(err);
            }
            last_error = Some(err);
          }
        }
      }
    }

    check_root(RenderStage::Paint).map_err(Error::Render)?;
    match self.decode_with_guess(bytes, url) {
      Ok(img) => {
        let has_alpha_hint = format_from_content_type
          .or(sniffed_format)
          .and_then(|format| Self::source_has_alpha(bytes, format));
        let has_alpha = has_alpha_hint.unwrap_or_else(|| img.color().has_alpha());
        let img = if let Some(transform) = icc_transform.as_ref() {
          let mut rgba = img.to_rgba8();
          transform.apply_rgba8_in_place(rgba.as_mut())?;
          DynamicImage::ImageRgba8(rgba)
        } else {
          img
        };
        let img = self.finish_bitmap_decode(img, url)?;
        Ok((img, has_alpha))
      }
      Err(err) => Err(match err {
        Error::Render(_) => err,
        _ => panic_error.or(last_error).unwrap_or(err),
      }),
    }
  }

  fn finish_bitmap_decode(&self, img: DynamicImage, url: &str) -> Result<DynamicImage> {
    self.enforce_decode_limits(img.width(), img.height(), url)?;
    if let Some(bytes) = u64::from(img.width())
      .checked_mul(u64::from(img.height()))
      .and_then(|px| px.checked_mul(4))
    {
      render_control::reserve_allocation_with(bytes, || {
        format!(
          "image decode pixel buffer {}x{} url={}",
          img.width(),
          img.height(),
          url
        )
      })
      .map_err(Error::Render)?;
    }
    Ok(img)
  }

  fn format_from_content_type(content_type: Option<&str>) -> Option<ImageFormat> {
    let mime = content_type?.split(';').next().map(|ct| {
      ct.trim_matches(|c: char| matches!(c, ' ' | '\t'))
        .to_ascii_lowercase()
    })?;
    ImageFormat::from_mime_type(mime)
  }

  fn looks_like_avif(bytes: &[u8]) -> bool {
    // AVIF is an ISO-BMFF container. We only need a lightweight sniff (ftyp box contains the brand)
    // rather than invoking a full parser (which can panic in debug builds for malformed/trailing
    // data).
    if bytes.len() < 12 {
      return false;
    }

    let size = u32::from_be_bytes(bytes[0..4].try_into().unwrap_or([0; 4]));
    if &bytes[4..8] != b"ftyp" {
      return false;
    }

    let (header_len, box_size) = match size {
      0 => (8usize, bytes.len()),
      1 => {
        if bytes.len() < 16 {
          return false;
        }
        let ext = u64::from_be_bytes(bytes[8..16].try_into().unwrap_or([0; 8]));
        let Ok(ext) = usize::try_from(ext) else {
          return false;
        };
        (16usize, ext)
      }
      n => (8usize, n as usize),
    };

    if box_size < header_len + 8 {
      return false;
    }

    let avail = bytes.len().min(box_size);
    if avail < header_len + 8 {
      return false;
    }

    let payload = &bytes[header_len..avail];
    let major_brand = &payload[0..4];
    if major_brand == b"avif" || major_brand == b"avis" {
      return true;
    }

    // Skip minor_version (4 bytes) and scan compatible brands.
    if payload.len() <= 8 {
      return false;
    }
    payload[8..]
      .chunks_exact(4)
      .any(|brand| brand == b"avif" || brand == b"avis")
  }

  fn sniff_image_format_fast(bytes: &[u8]) -> Option<ImageFormat> {
    if bytes.len() >= 8 && &bytes[..8] == b"\x89PNG\r\n\x1a\n" {
      return Some(ImageFormat::Png);
    }
    if bytes.len() >= 3 && bytes[0] == 0xff && bytes[1] == 0xd8 && bytes[2] == 0xff {
      return Some(ImageFormat::Jpeg);
    }
    if bytes.len() >= 6 && (&bytes[..6] == b"GIF87a" || &bytes[..6] == b"GIF89a") {
      return Some(ImageFormat::Gif);
    }
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
      return Some(ImageFormat::WebP);
    }
    if Self::looks_like_avif(bytes) {
      return Some(ImageFormat::Avif);
    }
    None
  }

  fn sniff_image_format(bytes: &[u8]) -> (Option<ImageFormat>, Option<String>) {
    if let Some(format) = Self::sniff_image_format_fast(bytes) {
      return (Some(format), None);
    }
    // `image::guess_format` may invoke codec sniffers that use debug assertions (notably for AVIF).
    // Guard against panics so metadata probing can't crash the renderer in debug builds. Preserve
    // the panic message so callers can surface it in decode/probe errors.
    match std::panic::catch_unwind(|| image::guess_format(bytes).ok()) {
      Ok(format) => (format, None),
      Err(panic) => (
        None,
        Some(format!(
          "image::guess_format panicked: {}",
          panic_payload_to_reason(&*panic)
        )),
      ),
    }
  }

  /// Attempt to determine whether the *source* image contains alpha information without relying on
  /// the decoded pixel buffer's color type.
  ///
  /// Some decoders normalize output to RGBA even when the source container did not include
  /// transparency. Features like `mask-mode: match-source` need to know whether the source provided
  /// alpha so they can choose alpha vs luminance masking.
  fn source_has_alpha(bytes: &[u8], format: ImageFormat) -> Option<bool> {
    match format {
      ImageFormat::Gif => Self::gif_has_transparency(bytes),
      ImageFormat::WebP => Self::webp_has_alpha(bytes),
      _ => None,
    }
  }

  fn gif_is_animated(bytes: &[u8]) -> GifAnimationProbe {
    // Minimal GIF parser that counts image descriptor blocks (frames) without decoding pixel data.
    // Stops as soon as a second frame is observed.
    //
    // Format reference: https://www.w3.org/Graphics/GIF/spec-gif89a.txt
    if bytes.len() < 13 {
      return GifAnimationProbe::Invalid;
    }
    let Some(header) = bytes.get(0..6) else {
      return GifAnimationProbe::Invalid;
    };
    if header != b"GIF87a" && header != b"GIF89a" {
      return GifAnimationProbe::Invalid;
    }

    // Logical Screen Descriptor starts at byte 6.
    let packed = match bytes.get(10) {
      Some(v) => *v,
      None => return GifAnimationProbe::NeedMoreData,
    };
    let mut offset = 13usize;

    // Skip global color table if present.
    if packed & 0x80 != 0 {
      let table_bits = (packed & 0x07) as usize;
      let Some(entries) = 1usize.checked_shl((table_bits + 1) as u32) else {
        return GifAnimationProbe::Invalid;
      };
      let Some(table_bytes) = 3usize.checked_mul(entries) else {
        return GifAnimationProbe::Invalid;
      };
      let Some(next) = offset.checked_add(table_bytes) else {
        return GifAnimationProbe::Invalid;
      };
      if next > bytes.len() {
        return GifAnimationProbe::NeedMoreData;
      }
      offset = next;
    }

    let mut frame_count = 0usize;

    while offset < bytes.len() {
      match bytes[offset] {
        0x3B => {
          // GIF trailer.
          return GifAnimationProbe::Determined(frame_count > 1);
        }
        0x21 => {
          // Extension introducer.
          let Some(label) = bytes.get(offset + 1).copied() else {
            return GifAnimationProbe::NeedMoreData;
          };
          offset = match offset.checked_add(2) {
            Some(v) => v,
            None => return GifAnimationProbe::Invalid,
          };

          if label == 0xF9 {
            // Graphics Control Extension. Fixed-length block followed by a terminator.
            let Some(block_size) = bytes.get(offset).copied().map(|b| b as usize) else {
              return GifAnimationProbe::NeedMoreData;
            };
            offset = match offset.checked_add(1) {
              Some(v) => v,
              None => return GifAnimationProbe::Invalid,
            };
            let Some(end) = offset.checked_add(block_size) else {
              return GifAnimationProbe::Invalid;
            };
            if end > bytes.len() {
              return GifAnimationProbe::NeedMoreData;
            }
            offset = end;
            // Block terminator byte.
            match bytes.get(offset) {
              Some(0x00) => offset += 1,
              Some(_) => return GifAnimationProbe::Invalid,
              None => return GifAnimationProbe::NeedMoreData,
            }
          } else {
            // Skip extension data sub-blocks.
            loop {
              let Some(size) = bytes.get(offset).copied().map(|b| b as usize) else {
                return GifAnimationProbe::NeedMoreData;
              };
              offset = match offset.checked_add(1) {
                Some(v) => v,
                None => return GifAnimationProbe::Invalid,
              };
              if size == 0 {
                break;
              }
              let Some(next) = offset.checked_add(size) else {
                return GifAnimationProbe::Invalid;
              };
              if next > bytes.len() {
                return GifAnimationProbe::NeedMoreData;
              }
              offset = next;
            }
          }
        }
        0x2C => {
          // Image descriptor (frame).
          frame_count = frame_count.saturating_add(1);
          if frame_count > 1 {
            return GifAnimationProbe::Determined(true);
          }

          let Some(desc_end) = offset.checked_add(10) else {
            return GifAnimationProbe::Invalid;
          };
          if desc_end > bytes.len() {
            return GifAnimationProbe::NeedMoreData;
          }
          let packed = bytes.get(offset + 9).copied().unwrap_or(0);
          offset = desc_end;

          // Skip local color table if present.
          if packed & 0x80 != 0 {
            let table_bits = (packed & 0x07) as usize;
            let Some(entries) = 1usize.checked_shl((table_bits + 1) as u32) else {
              return GifAnimationProbe::Invalid;
            };
            let Some(table_bytes) = 3usize.checked_mul(entries) else {
              return GifAnimationProbe::Invalid;
            };
            let Some(next) = offset.checked_add(table_bytes) else {
              return GifAnimationProbe::Invalid;
            };
            if next > bytes.len() {
              return GifAnimationProbe::NeedMoreData;
            }
            offset = next;
          }

          // LZW minimum code size.
          offset = match offset.checked_add(1) {
            Some(v) => v,
            None => return GifAnimationProbe::Invalid,
          };
          if offset > bytes.len() {
            return GifAnimationProbe::NeedMoreData;
          }

          // Image data sub-blocks (length-prefixed).
          loop {
            let Some(size) = bytes.get(offset).copied().map(|b| b as usize) else {
              return GifAnimationProbe::NeedMoreData;
            };
            offset = match offset.checked_add(1) {
              Some(v) => v,
              None => return GifAnimationProbe::Invalid,
            };
            if size == 0 {
              break;
            }
            let Some(next) = offset.checked_add(size) else {
              return GifAnimationProbe::Invalid;
            };
            if next > bytes.len() {
              return GifAnimationProbe::NeedMoreData;
            }
            offset = next;
          }
        }
        _ => return GifAnimationProbe::Invalid,
      }
    }

    GifAnimationProbe::NeedMoreData
  }

  fn gif_has_transparency(bytes: &[u8]) -> Option<bool> {
    // Minimal GIF parser that scans extension blocks for a Graphics Control Extension with the
    // transparency flag set (packed field bit 0).
    //
    // Format reference: https://www.w3.org/Graphics/GIF/spec-gif89a.txt
    if bytes.len() < 13 {
      return None;
    }
    let header = bytes.get(0..6)?;
    if header != b"GIF87a" && header != b"GIF89a" {
      return None;
    }

    // Logical Screen Descriptor starts at byte 6.
    let packed = *bytes.get(10)?;
    let mut offset = 13usize;

    // Skip global color table if present.
    if packed & 0x80 != 0 {
      let table_bits = (packed & 0x07) as usize;
      let entries = 1usize.checked_shl((table_bits + 1) as u32)?;
      let table_bytes = 3usize.checked_mul(entries)?;
      offset = offset.checked_add(table_bytes)?;
      if offset > bytes.len() {
        return None;
      }
    }

    while offset < bytes.len() {
      match *bytes.get(offset)? {
        0x3B => return Some(false), // trailer
        0x21 => {
          // Extension introducer.
          let label = *bytes.get(offset + 1)?;
          offset = offset.checked_add(2)?;
          if label == 0xF9 {
            // Graphics Control Extension.
            let block_size = *bytes.get(offset)? as usize;
            offset = offset.checked_add(1)?;
            let end = offset.checked_add(block_size)?;
            if end > bytes.len() {
              return None;
            }
            let packed = *bytes.get(offset).unwrap_or(&0);
            if packed & 0x01 != 0 {
              return Some(true);
            }
            offset = end;
            if *bytes.get(offset)? != 0x00 {
              return None;
            }
            offset = offset.checked_add(1)?;
          } else {
            // Skip data sub-blocks.
            loop {
              let size = *bytes.get(offset)? as usize;
              offset = offset.checked_add(1)?;
              if size == 0 {
                break;
              }
              offset = offset.checked_add(size)?;
              if offset > bytes.len() {
                return None;
              }
            }
          }
        }
        0x2C => {
          // Image descriptor.
          let desc_end = offset.checked_add(10)?;
          if desc_end > bytes.len() {
            return None;
          }
          let packed = *bytes.get(offset + 9)?;
          offset = desc_end;
          if packed & 0x80 != 0 {
            let table_bits = (packed & 0x07) as usize;
            let entries = 1usize.checked_shl((table_bits + 1) as u32)?;
            let table_bytes = 3usize.checked_mul(entries)?;
            offset = offset.checked_add(table_bytes)?;
            if offset > bytes.len() {
              return None;
            }
          }

          // LZW minimum code size.
          offset = offset.checked_add(1)?;
          if offset > bytes.len() {
            return None;
          }

          // Image data sub-blocks.
          loop {
            let size = *bytes.get(offset)? as usize;
            offset = offset.checked_add(1)?;
            if size == 0 {
              break;
            }
            offset = offset.checked_add(size)?;
            if offset > bytes.len() {
              return None;
            }
          }
        }
        _ => return None,
      }
    }

    Some(false)
  }

  fn decode_gif_at_time(
    &self,
    bytes: &[u8],
    url: &str,
    time_ms: f32,
  ) -> Result<(DynamicImage, bool)> {
    let timing = GifTiming::parse(bytes).ok_or_else(|| {
      Error::Image(ImageError::DecodeFailed {
        url: url.to_string(),
        reason: "Unable to parse GIF animation metadata".to_string(),
      })
    })?;
    let selected_frame = timing.frame_index_for_time_ms(time_ms);

    let decode_frame = || -> std::result::Result<DynamicImage, image::ImageError> {
      let decoder = image::codecs::gif::GifDecoder::new(DeadlineCursor::new(bytes))?;
      let mut last_frame: Option<image::Frame> = None;
      for (idx, frame) in decoder.into_frames().enumerate() {
        let frame = frame?;
        if idx == selected_frame {
          return Ok(DynamicImage::ImageRgba8(frame.into_buffer()));
        }
        last_frame = Some(frame);
      }
      if let Some(frame) = last_frame {
        return Ok(DynamicImage::ImageRgba8(frame.into_buffer()));
      }
      Err(image::ImageError::Decoding(
        image::error::DecodingError::new(
          image::error::ImageFormatHint::Exact(ImageFormat::Gif),
          "GIF contained no frames",
        ),
      ))
    };

    let img = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(decode_frame)) {
      Ok(Ok(img)) => img,
      Ok(Err(err)) => return Err(self.decode_error(url, err)),
      Err(panic) => {
        return Err(Error::Image(ImageError::DecodeFailed {
          url: url.to_string(),
          reason: format!(
            "image decode (gif frames) panicked: {}",
            panic_payload_to_reason(&*panic)
          ),
        }));
      }
    };

    let has_alpha =
      Self::source_has_alpha(bytes, ImageFormat::Gif).unwrap_or_else(|| img.color().has_alpha());
    Ok((img, has_alpha))
  }

  fn webp_has_alpha(bytes: &[u8]) -> Option<bool> {
    // Parse the RIFF container for alpha signalling:
    // - VP8X feature flags (`ALPHA` bit)
    // - ALPH chunk
    // - VP8L lossless header alpha bit
    if bytes.len() < 12 {
      return None;
    }
    if bytes.get(0..4)? != b"RIFF" || bytes.get(8..12)? != b"WEBP" {
      return None;
    }

    let mut offset = 12usize;
    let mut vp8x_alpha: Option<bool> = None;
    let mut vp8l_alpha: Option<bool> = None;
    let mut saw_vp8 = false;

    while offset + 8 <= bytes.len() {
      let tag = bytes.get(offset..offset + 4)?;
      let size_bytes: [u8; 4] = bytes.get(offset + 4..offset + 8)?.try_into().ok()?;
      let size = u32::from_le_bytes(size_bytes) as usize;
      offset = offset.checked_add(8)?;
      let end = offset.checked_add(size)?;
      if end > bytes.len() {
        return None;
      }

      let payload = bytes.get(offset..end)?;
      match tag {
        b"VP8X" => {
          if let Some(flags) = payload.first() {
            vp8x_alpha = Some(flags & 0x10 != 0);
          }
        }
        b"ALPH" => return Some(true),
        b"VP8L" => {
          if payload.len() < 5 || payload[0] != 0x2F {
            return None;
          }
          let header: [u8; 4] = payload.get(1..5)?.try_into().ok()?;
          let bits = u32::from_le_bytes(header);
          vp8l_alpha = Some(bits & (1 << 28) != 0);
        }
        b"VP8 " => {
          saw_vp8 = true;
        }
        _ => {}
      }

      offset = end;
      if size % 2 == 1 && offset < bytes.len() {
        offset = offset.checked_add(1)?;
      }
    }

    if vp8x_alpha == Some(true) || vp8l_alpha == Some(true) {
      return Some(true);
    }
    if vp8x_alpha == Some(false) {
      return Some(false);
    }
    if vp8l_alpha == Some(false) {
      return Some(false);
    }
    if saw_vp8 {
      return Some(false);
    }

    None
  }

  fn decode_with_format(
    &self,
    bytes: &[u8],
    format: ImageFormat,
    url: &str,
  ) -> Result<(DynamicImage, bool)> {
    if format == ImageFormat::Gif {
      if let Some(time_ms) = self.animation_time_ms {
        match self.decode_gif_at_time(bytes, url, time_ms) {
          Ok(result) => return Ok(result),
          Err(err) => {
            if matches!(err, Error::Render(_)) {
              // Render deadlines must abort immediately.
              return Err(err);
            }
            // Fall back to the first frame decode below. This keeps GIF decode behavior robust when
            // timing metadata parsing fails, while still allowing time-based selection when
            // supported.
          }
        };
      }
    }
    if format == ImageFormat::Jpeg {
      // The `image` crate's JPEG backend (zune-jpeg) can produce pixel values that differ
      // substantially from Chrome/libjpeg, which shows up as large fixture diffs even when the
      // rendered geometry is correct. Decode JPEGs with `jpeg-decoder` instead to match
      // browser output more closely.
      if let Ok(img) = self.decode_jpeg(bytes, url) {
        return Ok((img, false));
      }
      // Fall back to `image` if the JPEG payload uses an unsupported colorspace.
    }
    if format == ImageFormat::Png {
      // Decode PNG via the `png` crate so output buffers are allocated fallibly (avoiding process
      // aborts on OOM inside `image`'s decoders). Keep render deadlines responsive by reading
      // through `DeadlineCursor`.
      fn map_png_io_error(url: &str, io_err: &io::Error) -> Error {
        if let Some(render_err) = io_err
          .get_ref()
          .and_then(|source| source.downcast_ref::<RenderError>())
        {
          return Error::Render(render_err.clone());
        }

        Error::Image(ImageError::DecodeFailed {
          url: url.to_string(),
          reason: io_err.to_string(),
        })
      }

      fn map_png_error(url: &str, err: &png::DecodingError) -> Error {
        match err {
          png::DecodingError::IoError(io_err) => map_png_io_error(url, io_err),
          _ => Error::Image(ImageError::DecodeFailed {
            url: url.to_string(),
            reason: err.to_string(),
          }),
        }
      }

      let mut decoder = png::Decoder::new(DeadlineCursor::new(bytes));
      decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
      let mut reader = decoder.read_info().map_err(|e| map_png_error(url, &e))?;
      let info = reader.info();
      let width = info.width;
      let height = info.height;
      if width == 0 || height == 0 {
        return Err(Error::Image(ImageError::DecodeFailed {
          url: url.to_string(),
          reason: format!("PNG image size is zero ({width}x{height})"),
        }));
      }

      let rgba_bytes = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|px| px.checked_mul(4))
        .ok_or_else(|| {
          Error::Image(ImageError::DecodeFailed {
            url: url.to_string(),
            reason: format!("PNG dimensions overflow ({width}x{height})"),
          })
        })?;
      if rgba_bytes > MAX_PIXMAP_BYTES {
        return Err(Error::Image(ImageError::DecodeFailed {
          url: url.to_string(),
          reason: format!(
            "PNG decoded image {width}x{height} is {rgba_bytes} bytes (limit {MAX_PIXMAP_BYTES})"
          ),
        }));
      }
      let rgba_len = usize::try_from(rgba_bytes).map_err(|_| {
        Error::Image(ImageError::DecodeFailed {
          url: url.to_string(),
          reason: format!("PNG decoded byte size does not fit in usize ({width}x{height})"),
        })
      })?;

      let out_size = reader.output_buffer_size().ok_or_else(|| {
        Error::Image(ImageError::DecodeFailed {
          url: url.to_string(),
          reason: "PNG output buffer size not available".to_string(),
        })
      })?;
      let out_size_u64 = u64::try_from(out_size).map_err(|_| {
        Error::Image(ImageError::DecodeFailed {
          url: url.to_string(),
          reason: "PNG output buffer size does not fit in u64".to_string(),
        })
      })?;
      if out_size_u64 > MAX_PIXMAP_BYTES {
        return Err(Error::Image(ImageError::DecodeFailed {
          url: url.to_string(),
          reason: format!("PNG output buffer is {out_size_u64} bytes (limit {MAX_PIXMAP_BYTES})"),
        }));
      }
      let out_size = usize::try_from(out_size_u64).map_err(|_| {
        Error::Image(ImageError::DecodeFailed {
          url: url.to_string(),
          reason: "PNG output buffer size does not fit in usize".to_string(),
        })
      })?;

      let mut buf = Vec::new();
      buf.try_reserve_exact(out_size).map_err(|err| {
        Error::Image(ImageError::DecodeFailed {
          url: url.to_string(),
          reason: format!("PNG output buffer allocation failed for {out_size} bytes: {err}"),
        })
      })?;
      buf.resize(out_size, 0);

      let frame = reader
        .next_frame(&mut buf)
        .map_err(|e| map_png_error(url, &e))?;
      buf.truncate(frame.buffer_size());

      let has_alpha = matches!(
        frame.color_type,
        png::ColorType::Rgba | png::ColorType::GrayscaleAlpha
      );
      let mut rgba = match (frame.color_type, frame.bit_depth) {
        (png::ColorType::Rgba, png::BitDepth::Eight) => {
          if buf.len() != rgba_len {
            return Err(Error::Image(ImageError::DecodeFailed {
              url: url.to_string(),
              reason: format!(
                "PNG RGBA output length mismatch (expected {rgba_len} bytes, got {})",
                buf.len()
              ),
            }));
          }
          RgbaImage::from_raw(width, height, buf).ok_or_else(|| {
            Error::Image(ImageError::DecodeFailed {
              url: url.to_string(),
              reason: "PNG RGBA buffer was invalid".to_string(),
            })
          })?
        }
        (png::ColorType::Rgb, png::BitDepth::Eight) => {
          let rgb_len = u64::from(width)
            .checked_mul(u64::from(height))
            .and_then(|px| px.checked_mul(3))
            .ok_or_else(|| {
              Error::Image(ImageError::DecodeFailed {
                url: url.to_string(),
                reason: "PNG RGB byte size overflow".to_string(),
              })
            })?;
          let rgb_len = usize::try_from(rgb_len).map_err(|_| {
            Error::Image(ImageError::DecodeFailed {
              url: url.to_string(),
              reason: "PNG RGB byte size does not fit in usize".to_string(),
            })
          })?;
          if buf.len() != rgb_len {
            return Err(Error::Image(ImageError::DecodeFailed {
              url: url.to_string(),
              reason: format!(
                "PNG RGB output length mismatch (expected {rgb_len} bytes, got {})",
                buf.len()
              ),
            }));
          }

          let mut out = Vec::new();
          out.try_reserve_exact(rgba_len).map_err(|err| {
            Error::Image(ImageError::DecodeFailed {
              url: url.to_string(),
              reason: format!("PNG RGBA buffer allocation failed for {rgba_len} bytes: {err}"),
            })
          })?;
          out.resize(rgba_len, 0);
          for (in_px, out_px) in buf.chunks_exact(3).zip(out.chunks_exact_mut(4)) {
            out_px[0] = in_px[0];
            out_px[1] = in_px[1];
            out_px[2] = in_px[2];
            out_px[3] = 255;
          }
          RgbaImage::from_raw(width, height, out).ok_or_else(|| {
            Error::Image(ImageError::DecodeFailed {
              url: url.to_string(),
              reason: "PNG RGB->RGBA buffer was invalid".to_string(),
            })
          })?
        }
        (png::ColorType::Grayscale, png::BitDepth::Eight) => {
          let gray_len = u64::from(width)
            .checked_mul(u64::from(height))
            .ok_or_else(|| {
              Error::Image(ImageError::DecodeFailed {
                url: url.to_string(),
                reason: "PNG grayscale byte size overflow".to_string(),
              })
            })?;
          let gray_len = usize::try_from(gray_len).map_err(|_| {
            Error::Image(ImageError::DecodeFailed {
              url: url.to_string(),
              reason: "PNG grayscale byte size does not fit in usize".to_string(),
            })
          })?;
          if buf.len() != gray_len {
            return Err(Error::Image(ImageError::DecodeFailed {
              url: url.to_string(),
              reason: format!(
                "PNG grayscale output length mismatch (expected {gray_len} bytes, got {})",
                buf.len()
              ),
            }));
          }

          let mut out = Vec::new();
          out.try_reserve_exact(rgba_len).map_err(|err| {
            Error::Image(ImageError::DecodeFailed {
              url: url.to_string(),
              reason: format!("PNG RGBA buffer allocation failed for {rgba_len} bytes: {err}"),
            })
          })?;
          out.resize(rgba_len, 0);
          for (gray, out_px) in buf.iter().zip(out.chunks_exact_mut(4)) {
            out_px[0] = *gray;
            out_px[1] = *gray;
            out_px[2] = *gray;
            out_px[3] = 255;
          }
          RgbaImage::from_raw(width, height, out).ok_or_else(|| {
            Error::Image(ImageError::DecodeFailed {
              url: url.to_string(),
              reason: "PNG grayscale->RGBA buffer was invalid".to_string(),
            })
          })?
        }
        (png::ColorType::GrayscaleAlpha, png::BitDepth::Eight) => {
          let ga_len = u64::from(width)
            .checked_mul(u64::from(height))
            .and_then(|px| px.checked_mul(2))
            .ok_or_else(|| {
              Error::Image(ImageError::DecodeFailed {
                url: url.to_string(),
                reason: "PNG grayscale-alpha byte size overflow".to_string(),
              })
            })?;
          let ga_len = usize::try_from(ga_len).map_err(|_| {
            Error::Image(ImageError::DecodeFailed {
              url: url.to_string(),
              reason: "PNG grayscale-alpha byte size does not fit in usize".to_string(),
            })
          })?;
          if buf.len() != ga_len {
            return Err(Error::Image(ImageError::DecodeFailed {
              url: url.to_string(),
              reason: format!(
                "PNG grayscale-alpha output length mismatch (expected {ga_len} bytes, got {})",
                buf.len()
              ),
            }));
          }

          let mut out = Vec::new();
          out.try_reserve_exact(rgba_len).map_err(|err| {
            Error::Image(ImageError::DecodeFailed {
              url: url.to_string(),
              reason: format!("PNG RGBA buffer allocation failed for {rgba_len} bytes: {err}"),
            })
          })?;
          out.resize(rgba_len, 0);
          for (in_px, out_px) in buf.chunks_exact(2).zip(out.chunks_exact_mut(4)) {
            let gray = in_px[0];
            out_px[0] = gray;
            out_px[1] = gray;
            out_px[2] = gray;
            out_px[3] = in_px[1];
          }
          RgbaImage::from_raw(width, height, out).ok_or_else(|| {
            Error::Image(ImageError::DecodeFailed {
              url: url.to_string(),
              reason: "PNG grayscale-alpha->RGBA buffer was invalid".to_string(),
            })
          })?
        }
        _ => {
          return Err(Error::Image(ImageError::DecodeFailed {
            url: url.to_string(),
            reason: format!(
              "Unsupported PNG color type {:?} ({:?})",
              frame.color_type, frame.bit_depth
            ),
          }));
        }
      };

      if let Some(profile) = extract_png_iccp_profile(bytes) {
        apply_icc_profile_to_srgb(&mut rgba, &profile);
      }
      return Ok((DynamicImage::ImageRgba8(rgba), has_alpha));
    }
    if format == ImageFormat::WebP {
      // The `image` crate's WebP backend can produce subtly different pixels than Chrome/libwebp
      // (and has regressed on real-world assets with corrupted alpha blocks). Decode via libwebp
      // for closer browser parity.
      let img = self.decode_webp(bytes, url)?;
      let has_alpha =
        Self::source_has_alpha(bytes, format).unwrap_or_else(|| img.color().has_alpha());
      return Ok((img, has_alpha));
    }

    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      ImageReader::with_format(DeadlineCursor::new(bytes), format).decode()
    })) {
      Ok(Ok(img)) => {
        let has_alpha =
          Self::source_has_alpha(bytes, format).unwrap_or_else(|| img.color().has_alpha());
        Ok((img, has_alpha))
      }
      Ok(Err(err)) => Err(self.decode_error(url, err)),
      Err(panic) => Err(Error::Image(ImageError::DecodeFailed {
        url: url.to_string(),
        reason: format!(
          "image decode panicked: {}",
          panic_payload_to_reason(&*panic)
        ),
      })),
    }
  }

  fn decode_webp(&self, bytes: &[u8], url: &str) -> Result<DynamicImage> {
    use std::ffi::c_int;

    check_root(RenderStage::Paint).map_err(Error::Render)?;

    let icc_transform = extract_webp_icc_profile(bytes).and_then(|icc| icc_transform_to_srgb(&icc));

    let mut width: c_int = 0;
    let mut height: c_int = 0;
    unsafe {
      if libwebp_sys::WebPGetInfo(bytes.as_ptr(), bytes.len(), &mut width, &mut height) == 0 {
        return Err(Error::Image(ImageError::DecodeFailed {
          url: url.to_string(),
          reason: "libwebp WebPGetInfo failed".to_string(),
        }));
      }
    }

    let width_i32 = width;
    let height_i32 = height;
    let width = u32::try_from(width_i32).map_err(|_| {
      Error::Image(ImageError::DecodeFailed {
        url: url.to_string(),
        reason: format!("WebP width out of range ({width_i32})"),
      })
    })?;
    let height = u32::try_from(height_i32).map_err(|_| {
      Error::Image(ImageError::DecodeFailed {
        url: url.to_string(),
        reason: format!("WebP height out of range ({height_i32})"),
      })
    })?;
    if width == 0 || height == 0 {
      return Err(Error::Image(ImageError::DecodeFailed {
        url: url.to_string(),
        reason: format!("WebP image size is zero ({width}x{height})"),
      }));
    }

    self.enforce_decode_limits(width, height, url)?;

    let stride_bytes = u64::from(width).checked_mul(4).ok_or_else(|| {
      Error::Image(ImageError::DecodeFailed {
        url: url.to_string(),
        reason: format!("WebP row byte size overflow (width={width})"),
      })
    })?;
    let stride = i32::try_from(stride_bytes).map_err(|_| {
      Error::Image(ImageError::DecodeFailed {
        url: url.to_string(),
        reason: format!("WebP stride out of range ({stride_bytes})"),
      })
    })?;

    let buf_size_bytes = stride_bytes.checked_mul(u64::from(height)).ok_or_else(|| {
      Error::Image(ImageError::DecodeFailed {
        url: url.to_string(),
        reason: format!("WebP decoded byte size overflow ({width}x{height})"),
      })
    })?;
    let buf_len = usize::try_from(buf_size_bytes).map_err(|_| {
      Error::Image(ImageError::DecodeFailed {
        url: url.to_string(),
        reason: format!("WebP decoded byte size does not fit in usize ({width}x{height})"),
      })
    })?;

    let mut buf = Vec::new();
    buf.try_reserve_exact(buf_len).map_err(|err| {
      Error::Image(ImageError::DecodeFailed {
        url: url.to_string(),
        reason: format!("WebP RGBA buffer allocation failed for {buf_size_bytes} bytes: {err}"),
      })
    })?;
    buf.resize(buf_len, 0);

    check_root(RenderStage::Paint).map_err(Error::Render)?;
    unsafe {
      let out = libwebp_sys::WebPDecodeRGBAInto(
        bytes.as_ptr(),
        bytes.len(),
        buf.as_mut_ptr(),
        buf.len(),
        stride,
      );
      if out.is_null() {
        return Err(Error::Image(ImageError::DecodeFailed {
          url: url.to_string(),
          reason: "libwebp WebPDecodeRGBAInto failed".to_string(),
        }));
      }
    }

    if let Some(transform) = icc_transform.as_ref() {
      transform.apply_rgba8_in_place(&mut buf)?;
    }

    let rgba = RgbaImage::from_raw(width, height, buf).ok_or_else(|| {
      Error::Image(ImageError::DecodeFailed {
        url: url.to_string(),
        reason: "WebP RGBA buffer was invalid".to_string(),
      })
    })?;
    Ok(DynamicImage::ImageRgba8(rgba))
  }

  fn decode_jpeg(&self, bytes: &[u8], url: &str) -> Result<DynamicImage> {
    use jpeg_decoder::{Decoder, PixelFormat};

    // Decode through `DeadlineCursor` so long-running JPEGs still respect render deadlines.
    let mut decoder = Decoder::new(DeadlineCursor::new(bytes));
    let pixels = decoder.decode().map_err(|err| {
      Error::Image(ImageError::DecodeFailed {
        url: url.to_string(),
        reason: err.to_string(),
      })
    })?;
    let Some(info) = decoder.info() else {
      return Err(Error::Image(ImageError::DecodeFailed {
        url: url.to_string(),
        reason: "JPEG decoder did not expose image info".to_string(),
      }));
    };
    let (width, height) = (u32::from(info.width), u32::from(info.height));
    if width == 0 || height == 0 {
      return Err(Error::Image(ImageError::DecodeFailed {
        url: url.to_string(),
        reason: format!("JPEG image size is zero ({width}x{height})"),
      }));
    }
    self.enforce_decode_limits(width, height, url)?;

    match info.pixel_format {
      PixelFormat::RGB24 => {
        let expected = u64::from(width)
          .checked_mul(u64::from(height))
          .and_then(|px| px.checked_mul(3))
          .ok_or_else(|| {
            Error::Image(ImageError::DecodeFailed {
              url: url.to_string(),
              reason: "JPEG RGB byte size overflow".to_string(),
            })
          })? as usize;
        if pixels.len() != expected {
          return Err(Error::Image(ImageError::DecodeFailed {
            url: url.to_string(),
            reason: format!(
              "JPEG RGB output length mismatch (expected {expected} bytes, got {})",
              pixels.len()
            ),
          }));
        }

        let rgba_len = u64::from(width)
          .checked_mul(u64::from(height))
          .and_then(|px| px.checked_mul(4))
          .ok_or_else(|| {
            Error::Image(ImageError::DecodeFailed {
              url: url.to_string(),
              reason: "JPEG RGBA byte size overflow".to_string(),
            })
          })?;
        if rgba_len > MAX_PIXMAP_BYTES {
          return Err(Error::Image(ImageError::DecodeFailed {
            url: url.to_string(),
            reason: format!(
              "JPEG decoded image {width}x{height} is {rgba_len} bytes (limit {MAX_PIXMAP_BYTES})"
            ),
          }));
        }
        let rgba_len = usize::try_from(rgba_len).map_err(|_| {
          Error::Image(ImageError::DecodeFailed {
            url: url.to_string(),
            reason: "JPEG RGBA byte size does not fit in usize".to_string(),
          })
        })?;

        let mut out = Vec::new();
        out.try_reserve_exact(rgba_len).map_err(|err| {
          Error::Image(ImageError::DecodeFailed {
            url: url.to_string(),
            reason: format!("JPEG RGBA buffer allocation failed for {rgba_len} bytes: {err}"),
          })
        })?;
        out.resize(rgba_len, 0);
        for (in_px, out_px) in pixels.chunks_exact(3).zip(out.chunks_exact_mut(4)) {
          out_px[0] = in_px[0];
          out_px[1] = in_px[1];
          out_px[2] = in_px[2];
          out_px[3] = 255;
        }
        let rgba = RgbaImage::from_raw(width, height, out).ok_or_else(|| {
          Error::Image(ImageError::DecodeFailed {
            url: url.to_string(),
            reason: "JPEG RGB->RGBA buffer was invalid".to_string(),
          })
        })?;
        Ok(DynamicImage::ImageRgba8(rgba))
      }
      PixelFormat::L8 => {
        let expected = u64::from(width)
          .checked_mul(u64::from(height))
          .ok_or_else(|| {
            Error::Image(ImageError::DecodeFailed {
              url: url.to_string(),
              reason: "JPEG luma byte size overflow".to_string(),
            })
          })? as usize;
        if pixels.len() != expected {
          return Err(Error::Image(ImageError::DecodeFailed {
            url: url.to_string(),
            reason: format!(
              "JPEG luma output length mismatch (expected {expected} bytes, got {})",
              pixels.len()
            ),
          }));
        }

        let rgba_len = u64::from(width)
          .checked_mul(u64::from(height))
          .and_then(|px| px.checked_mul(4))
          .ok_or_else(|| {
            Error::Image(ImageError::DecodeFailed {
              url: url.to_string(),
              reason: "JPEG RGBA byte size overflow".to_string(),
            })
          })?;
        if rgba_len > MAX_PIXMAP_BYTES {
          return Err(Error::Image(ImageError::DecodeFailed {
            url: url.to_string(),
            reason: format!(
              "JPEG decoded image {width}x{height} is {rgba_len} bytes (limit {MAX_PIXMAP_BYTES})"
            ),
          }));
        }
        let rgba_len = usize::try_from(rgba_len).map_err(|_| {
          Error::Image(ImageError::DecodeFailed {
            url: url.to_string(),
            reason: "JPEG RGBA byte size does not fit in usize".to_string(),
          })
        })?;

        let mut out = Vec::new();
        out.try_reserve_exact(rgba_len).map_err(|err| {
          Error::Image(ImageError::DecodeFailed {
            url: url.to_string(),
            reason: format!("JPEG RGBA buffer allocation failed for {rgba_len} bytes: {err}"),
          })
        })?;
        out.resize(rgba_len, 0);
        for (gray, out_px) in pixels.iter().zip(out.chunks_exact_mut(4)) {
          out_px[0] = *gray;
          out_px[1] = *gray;
          out_px[2] = *gray;
          out_px[3] = 255;
        }
        let rgba = RgbaImage::from_raw(width, height, out).ok_or_else(|| {
          Error::Image(ImageError::DecodeFailed {
            url: url.to_string(),
            reason: "JPEG grayscale->RGBA buffer was invalid".to_string(),
          })
        })?;
        Ok(DynamicImage::ImageRgba8(rgba))
      }
      // CMYK/YCCK JPEGs are rare on the web. Fall back to the `image` crate for those so we keep
      // existing support without having to reimplement the colorspace conversion here.
      _ => Err(Error::Image(ImageError::DecodeFailed {
        url: url.to_string(),
        reason: format!("Unsupported JPEG pixel format {:?}", info.pixel_format),
      })),
    }
  }

  fn decode_with_guess(&self, bytes: &[u8], url: &str) -> Result<DynamicImage> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      ImageReader::new(DeadlineCursor::new(bytes))
        .with_guessed_format()?
        .decode()
    })) {
      Ok(Ok(img)) => Ok(img),
      Ok(Err(err)) => Err(self.decode_error(url, err)),
      Err(panic) => Err(Error::Image(ImageError::DecodeFailed {
        url: url.to_string(),
        reason: format!(
          "image decode (guessed format) panicked: {}",
          panic_payload_to_reason(&*panic)
        ),
      })),
    }
  }

  fn decode_error(&self, url: &str, err: image::ImageError) -> Error {
    if let image::ImageError::IoError(io_err) = &err {
      if let Some(render_err) = io_err
        .get_ref()
        .and_then(|source| source.downcast_ref::<RenderError>())
      {
        return Error::Render(render_err.clone());
      }
    }
    Error::Image(ImageError::DecodeFailed {
      url: url.to_string(),
      reason: err.to_string(),
    })
  }

  fn predecoded_dimensions(
    &self,
    bytes: &[u8],
    format_from_content_type: Option<ImageFormat>,
    sniffed_format: Option<ImageFormat>,
  ) -> (Option<(u32, u32)>, Option<String>) {
    let mut panic_reason = None;

    if let Some(format) = format_from_content_type {
      let (dims, panic) = Self::dimensions_for_format(bytes, format);
      if dims.is_some() {
        return (dims, None);
      }
      if panic_reason.is_none() {
        panic_reason = panic;
      }
    }

    if let Some(format) = sniffed_format {
      let (dims, panic) = Self::dimensions_for_format(bytes, format);
      if dims.is_some() {
        return (dims, None);
      }
      if panic_reason.is_none() {
        panic_reason = panic;
      }
    }

    (None, panic_reason)
  }

  #[cfg(feature = "avif")]
  /// Returns a prefix of `bytes` that excludes any trailing data that does not form a valid top
  /// level ISO-BMFF box.
  ///
  /// Some AVIF payloads (including pageset content) include non-box trailer bytes that trip
  /// debug-only assertions inside `avif_parse`. Trimming to the last complete box keeps intrinsic
  /// probing robust without impacting real decoders (which generally ignore such trailers).
  fn trim_isobmff_trailing_bytes(bytes: &[u8]) -> &[u8] {
    let len = bytes.len();
    let mut offset = 0usize;

    while offset
      .checked_add(8)
      .is_some_and(|next_header| next_header <= len)
    {
      let Some(size_end) = offset.checked_add(4).filter(|end| *end <= len) else {
        break;
      };
      let Some(size_bytes) = bytes.get(offset..size_end) else {
        break;
      };
      let Ok(size_bytes) = <[u8; 4]>::try_from(size_bytes) else {
        break;
      };
      let size = u32::from_be_bytes(size_bytes);

      let box_size = match size {
        // Box extends to end of file.
        0 => len - offset,
        // Extended size stored in the next 8 bytes.
        1 => {
          let Some(ext_end) = offset.checked_add(16).filter(|end| *end <= len) else {
            break;
          };
          let Some(ext_start) = offset.checked_add(8) else {
            break;
          };
          let Some(ext_slice) = bytes.get(ext_start..ext_end) else {
            break;
          };
          let Ok(ext_bytes) = <[u8; 8]>::try_from(ext_slice) else {
            break;
          };
          let ext = u64::from_be_bytes(ext_bytes);
          if ext < 16 {
            break;
          }
          let Ok(ext) = usize::try_from(ext) else {
            break;
          };
          ext
        }
        // Regular 32-bit size.
        n => n as usize,
      };

      // Invalid box size.
      if box_size < 8 {
        break;
      }
      let Some(next) = offset.checked_add(box_size) else {
        break;
      };
      if next > len {
        break;
      }

      offset = next;
      if offset == len {
        return bytes;
      }
    }

    if offset > 0 {
      &bytes[..offset]
    } else {
      bytes
    }
  }

  fn dimensions_for_format(
    bytes: &[u8],
    format: ImageFormat,
  ) -> (Option<(u32, u32)>, Option<String>) {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match format {
      ImageFormat::Png => image::codecs::png::PngDecoder::new(Cursor::new(bytes))
        .ok()
        .map(|d| d.dimensions()),
      ImageFormat::Jpeg => image::codecs::jpeg::JpegDecoder::new(Cursor::new(bytes))
        .ok()
        .map(|d| d.dimensions()),
      ImageFormat::Gif => image::codecs::gif::GifDecoder::new(Cursor::new(bytes))
        .ok()
        .map(|d| d.dimensions()),
      ImageFormat::WebP => image::codecs::webp::WebPDecoder::new(Cursor::new(bytes))
        .ok()
        .map(|d| d.dimensions()),
      #[cfg(feature = "avif")]
      ImageFormat::Avif => {
        // `avif_parse` includes debug assertions that can panic when the payload includes trailing
        // bytes (which is tolerated by other decoders and observed on pageset content). Trim and
        // catch panics so image probing doesn't crash the renderer in debug builds.
        let trimmed = Self::trim_isobmff_trailing_bytes(bytes);
        let mut cursor = Cursor::new(trimmed);
        let data = AvifData::from_reader(&mut cursor).ok()?;
        let meta = data.primary_item_metadata().ok()?;
        Some((meta.max_frame_width.get(), meta.max_frame_height.get()))
      }
      #[cfg(not(feature = "avif"))]
      ImageFormat::Avif => None,
      _ => None,
    })) {
      Ok(dims) => (dims, None),
      Err(panic) => (
        None,
        Some(format!(
          "{format:?} dimensions probe panicked: {}",
          panic_payload_to_reason(&*panic)
        )),
      ),
    }
  }

  fn enforce_decode_limits(&self, width: u32, height: u32, url: &str) -> Result<()> {
    if self.config.max_decoded_dimension > 0
      && (width > self.config.max_decoded_dimension || height > self.config.max_decoded_dimension)
    {
      return Err(Error::Image(ImageError::DecodeFailed {
        url: url.to_string(),
        reason: format!(
          "Image dimensions {}x{} exceed maximum dimension {}",
          width, height, self.config.max_decoded_dimension
        ),
      }));
    }

    if self.config.max_decoded_pixels > 0 {
      let pixels = u64::from(width) * u64::from(height);
      if pixels > self.config.max_decoded_pixels {
        return Err(Error::Image(ImageError::DecodeFailed {
          url: url.to_string(),
          reason: format!(
            "Image dimensions {}x{} exceed pixel budget of {}",
            width, height, self.config.max_decoded_pixels
          ),
        }));
      }
    }

    // Bound decoded images even when the caller disables `max_decoded_*` limits so we don't
    // allocate unbounded buffers that can abort the process on OOM.
    let pixels = u64::from(width) * u64::from(height);
    let bytes = pixels.checked_mul(4).ok_or_else(|| {
      Error::Image(ImageError::DecodeFailed {
        url: url.to_string(),
        reason: format!(
          "Image dimensions {}x{} overflow decoded byte size",
          width, height
        ),
      })
    })?;
    if bytes > MAX_PIXMAP_BYTES {
      return Err(Error::Image(ImageError::DecodeFailed {
        url: url.to_string(),
        reason: format!(
          "Image dimensions {}x{} require {} bytes (limit {})",
          width, height, bytes, MAX_PIXMAP_BYTES
        ),
      }));
    }

    Ok(())
  }

  #[cfg(feature = "avif")]
  fn decode_avif(bytes: &[u8]) -> std::result::Result<DynamicImage, AvifDecodeError> {
    let decode_inner = |payload: &[u8]| -> std::result::Result<DynamicImage, AvifDecodeError> {
      check_root(RenderStage::Paint).map_err(AvifDecodeError::from)?;
      let decoder = AvifDecoder::from_avif(payload)
        .map_err(|err| AvifDecodeError::Image(Self::avif_error(err)))?;
      check_root(RenderStage::Paint).map_err(AvifDecodeError::from)?;
      let image = decoder
        .to_image()
        .map_err(|err| AvifDecodeError::Image(Self::avif_error(err)))?;
      let mut deadline_counter = 0usize;
      check_root(RenderStage::Paint).map_err(AvifDecodeError::from)?;
      Self::avif_image_to_dynamic(image, &mut deadline_counter)
    };

    let try_decode = |payload: &[u8]| -> std::result::Result<DynamicImage, AvifDecodeError> {
      match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| decode_inner(payload))) {
        Ok(result) => result,
        Err(panic) => {
          let message = panic_payload_to_reason(&*panic);
          Err(AvifDecodeError::Image(Self::avif_error(format!(
            "avif decode panicked: {message}"
          ))))
        }
      }
    };

    let original = match try_decode(bytes) {
      Ok(img) => return Ok(img),
      Err(err) => err,
    };

    if matches!(original, AvifDecodeError::Timeout(_)) {
      return Err(original);
    }

    let trimmed = Self::trim_isobmff_trailing_bytes(bytes);
    if trimmed.len() < bytes.len() {
      match try_decode(trimmed) {
        Ok(img) => Ok(img),
        Err(err) => match err {
          AvifDecodeError::Timeout(_) => Err(err),
          AvifDecodeError::Image(_) => Err(original),
        },
      }
    } else {
      Err(original)
    }
  }

  #[cfg(feature = "avif")]
  fn reserve_for_bytes(bytes: u64, context: &str) -> std::result::Result<usize, image::ImageError> {
    if bytes > MAX_PIXMAP_BYTES {
      return Err(image::ImageError::IoError(io::Error::new(
        io::ErrorKind::Other,
        format!("{context}: buffer would require {bytes} bytes (limit {MAX_PIXMAP_BYTES})"),
      )));
    }
    usize::try_from(bytes).map_err(|_| {
      image::ImageError::IoError(io::Error::new(
        io::ErrorKind::Other,
        format!("{context}: buffer size {bytes} does not fit in usize"),
      ))
    })
  }

  #[cfg(feature = "avif")]
  fn reserve_image_buffer(
    bytes: u64,
    context: &str,
  ) -> std::result::Result<Vec<u8>, image::ImageError> {
    let len = Self::reserve_for_bytes(bytes, context)?;
    let mut buf = Vec::new();
    buf.try_reserve_exact(len).map_err(|err| {
      image::ImageError::IoError(io::Error::new(
        io::ErrorKind::Other,
        format!("{context}: buffer allocation failed for {bytes} bytes: {err}"),
      ))
    })?;
    Ok(buf)
  }

  #[cfg(feature = "avif")]
  fn reserve_image_buffer_u16(
    bytes: u64,
    context: &str,
  ) -> std::result::Result<Vec<u16>, image::ImageError> {
    if bytes % 2 != 0 {
      return Err(image::ImageError::IoError(io::Error::new(
        io::ErrorKind::Other,
        format!("{context}: buffer size {bytes} is not aligned to u16"),
      )));
    }
    let len = Self::reserve_for_bytes(bytes, context)? / 2;
    let mut buf: Vec<u16> = Vec::new();
    buf.try_reserve_exact(len).map_err(|err| {
      image::ImageError::IoError(io::Error::new(
        io::ErrorKind::Other,
        format!("{context}: buffer allocation failed for {bytes} bytes: {err}"),
      ))
    })?;
    Ok(buf)
  }

  #[cfg(feature = "avif")]
  fn avif_image_to_dynamic(
    image: AvifImage,
    deadline_counter: &mut usize,
  ) -> std::result::Result<DynamicImage, AvifDecodeError> {
    let dimension_error = || {
      AvifDecodeError::Image(image::ImageError::Parameter(
        image::error::ParameterError::from_kind(
          image::error::ParameterErrorKind::DimensionMismatch,
        ),
      ))
    };

    match image {
      AvifImage::Rgb8(img) => {
        let (width, height) =
          Self::avif_dimensions(img.width(), img.height()).map_err(AvifDecodeError::Image)?;
        let Some(bytes) = u64::from(width)
          .checked_mul(u64::from(height))
          .and_then(|px| px.checked_mul(3))
        else {
          return Err(AvifDecodeError::Image(Self::avif_error(
            "RGB8 dimensions overflow",
          )));
        };
        let mut buf = Self::reserve_image_buffer(bytes, "avif rgb8 data")?;
        for px in img.buf() {
          check_root_periodic(
            deadline_counter,
            IMAGE_DECODE_DEADLINE_STRIDE,
            RenderStage::Paint,
          )
          .map_err(AvifDecodeError::from)?;
          buf.extend_from_slice(&[px.r, px.g, px.b]);
        }
        image::RgbImage::from_vec(width, height, buf)
          .map(DynamicImage::ImageRgb8)
          .ok_or_else(dimension_error)
      }
      AvifImage::Rgb16(img) => {
        let (width, height) =
          Self::avif_dimensions(img.width(), img.height()).map_err(AvifDecodeError::Image)?;
        // `avif_decode` exposes >8-bit pixels via 16-bit channel types, but the values are in the
        // image's native bit depth range (AVIF only supports 8/10/12-bit). The `image` crate's
        // `to_rgba8()` conversion expects 16-bit channels to already span the full 0..=65535 range,
        // and otherwise will effectively shift the data down (turning 10-bit images nearly black).
        //
        // Normalize 10/12-bit channels up to 16-bit before handing them off to `image`. The AVIF
        // bit depth is not exposed by `avif_decode`, so infer it from the observed channel range.
        let scale_channel_max = |max_value: u16| -> Option<u16> {
          match max_value {
            0..=1023 => Some(1023),
            1024..=4095 => Some(4095),
            _ => None,
          }
        };
        let Some(bytes) = u64::from(width)
          .checked_mul(u64::from(height))
          .and_then(|px| px.checked_mul(3))
          .and_then(|px| px.checked_mul(2))
        else {
          return Err(AvifDecodeError::Image(Self::avif_error(
            "RGB16 dimensions overflow",
          )));
        };
        let mut buf = Self::reserve_image_buffer_u16(bytes, "avif rgb16 data")?;
        let mut max_value = 0u16;
        for px in img.buf() {
          check_root_periodic(
            deadline_counter,
            IMAGE_DECODE_DEADLINE_STRIDE,
            RenderStage::Paint,
          )
          .map_err(AvifDecodeError::from)?;
          max_value = max_value.max(px.r).max(px.g).max(px.b);
          buf.extend_from_slice(&[px.r, px.g, px.b]);
        }
        if let Some(max_in) = scale_channel_max(max_value) {
          let max_in = u32::from(max_in);
          for value in &mut buf {
            check_root_periodic(
              deadline_counter,
              IMAGE_DECODE_DEADLINE_STRIDE,
              RenderStage::Paint,
            )
            .map_err(AvifDecodeError::from)?;
            let v = u32::from(*value);
            *value = ((v * u32::from(u16::MAX) + max_in / 2) / max_in) as u16;
          }
        }
        image::ImageBuffer::from_vec(width, height, buf)
          .map(DynamicImage::ImageRgb16)
          .ok_or_else(dimension_error)
      }
      AvifImage::Rgba8(img) => {
        let (width, height) =
          Self::avif_dimensions(img.width(), img.height()).map_err(AvifDecodeError::Image)?;
        let Some(bytes) = u64::from(width)
          .checked_mul(u64::from(height))
          .and_then(|px| px.checked_mul(4))
        else {
          return Err(AvifDecodeError::Image(Self::avif_error(
            "RGBA8 dimensions overflow",
          )));
        };
        let mut buf = Self::reserve_image_buffer(bytes, "avif rgba8 data")?;
        for px in img.buf() {
          check_root_periodic(
            deadline_counter,
            IMAGE_DECODE_DEADLINE_STRIDE,
            RenderStage::Paint,
          )
          .map_err(AvifDecodeError::from)?;
          buf.extend_from_slice(&[px.r, px.g, px.b, px.a]);
        }
        image::RgbaImage::from_vec(width, height, buf)
          .map(DynamicImage::ImageRgba8)
          .ok_or_else(dimension_error)
      }
      AvifImage::Rgba16(img) => {
        let (width, height) =
          Self::avif_dimensions(img.width(), img.height()).map_err(AvifDecodeError::Image)?;
        let scale_channel_max = |max_value: u16| -> Option<u16> {
          match max_value {
            0..=1023 => Some(1023),
            1024..=4095 => Some(4095),
            _ => None,
          }
        };
        let Some(bytes) = u64::from(width)
          .checked_mul(u64::from(height))
          .and_then(|px| px.checked_mul(4))
          .and_then(|px| px.checked_mul(2))
        else {
          return Err(AvifDecodeError::Image(Self::avif_error(
            "RGBA16 dimensions overflow",
          )));
        };
        let mut buf = Self::reserve_image_buffer_u16(bytes, "avif rgba16 data")?;
        let mut max_rgb = 0u16;
        let mut max_alpha = 0u16;
        for px in img.buf() {
          check_root_periodic(
            deadline_counter,
            IMAGE_DECODE_DEADLINE_STRIDE,
            RenderStage::Paint,
          )
          .map_err(AvifDecodeError::from)?;
          max_rgb = max_rgb.max(px.r).max(px.g).max(px.b);
          max_alpha = max_alpha.max(px.a);
          buf.extend_from_slice(&[px.r, px.g, px.b, px.a]);
        }
        let max_in_rgb = scale_channel_max(max_rgb).map(u32::from);
        let max_in_alpha = scale_channel_max(max_alpha).map(u32::from);
        if max_in_rgb.is_some() || max_in_alpha.is_some() {
          for channels in buf.chunks_mut(4) {
            check_root_periodic(
              deadline_counter,
              IMAGE_DECODE_DEADLINE_STRIDE,
              RenderStage::Paint,
            )
            .map_err(AvifDecodeError::from)?;
            if let Some(max_in) = max_in_rgb {
              channels[0] =
                ((u32::from(channels[0]) * u32::from(u16::MAX) + max_in / 2) / max_in) as u16;
              channels[1] =
                ((u32::from(channels[1]) * u32::from(u16::MAX) + max_in / 2) / max_in) as u16;
              channels[2] =
                ((u32::from(channels[2]) * u32::from(u16::MAX) + max_in / 2) / max_in) as u16;
            }
            if let Some(max_in) = max_in_alpha {
              channels[3] =
                ((u32::from(channels[3]) * u32::from(u16::MAX) + max_in / 2) / max_in) as u16;
            }
          }
        }
        image::ImageBuffer::from_vec(width, height, buf)
          .map(DynamicImage::ImageRgba16)
          .ok_or_else(dimension_error)
      }
      AvifImage::Gray8(img) => {
        let (width, height) =
          Self::avif_dimensions(img.width(), img.height()).map_err(AvifDecodeError::Image)?;
        let Some(bytes) = u64::from(width).checked_mul(u64::from(height)) else {
          return Err(AvifDecodeError::Image(Self::avif_error(
            "Gray8 dimensions overflow",
          )));
        };
        let mut buf = Self::reserve_image_buffer(bytes, "avif gray8 data")?;
        for px in img.buf() {
          check_root_periodic(
            deadline_counter,
            IMAGE_DECODE_DEADLINE_STRIDE,
            RenderStage::Paint,
          )
          .map_err(AvifDecodeError::from)?;
          buf.push(px.value());
        }
        image::ImageBuffer::from_vec(width, height, buf)
          .map(DynamicImage::ImageLuma8)
          .ok_or_else(dimension_error)
      }
      AvifImage::Gray16(img) => {
        let (width, height) =
          Self::avif_dimensions(img.width(), img.height()).map_err(AvifDecodeError::Image)?;
        let scale_channel_max = |max_value: u16| -> Option<u16> {
          match max_value {
            0..=1023 => Some(1023),
            1024..=4095 => Some(4095),
            _ => None,
          }
        };
        let Some(bytes) = u64::from(width)
          .checked_mul(u64::from(height))
          .and_then(|px| px.checked_mul(2))
        else {
          return Err(AvifDecodeError::Image(Self::avif_error(
            "Gray16 dimensions overflow",
          )));
        };
        let mut buf = Self::reserve_image_buffer_u16(bytes, "avif gray16 data")?;
        let mut max_value = 0u16;
        for px in img.buf() {
          check_root_periodic(
            deadline_counter,
            IMAGE_DECODE_DEADLINE_STRIDE,
            RenderStage::Paint,
          )
          .map_err(AvifDecodeError::from)?;
          let v = px.value();
          max_value = max_value.max(v);
          buf.push(v);
        }
        if let Some(max_in) = scale_channel_max(max_value) {
          let max_in = u32::from(max_in);
          for value in &mut buf {
            check_root_periodic(
              deadline_counter,
              IMAGE_DECODE_DEADLINE_STRIDE,
              RenderStage::Paint,
            )
            .map_err(AvifDecodeError::from)?;
            let v = u32::from(*value);
            *value = ((v * u32::from(u16::MAX) + max_in / 2) / max_in) as u16;
          }
        }
        image::ImageBuffer::from_vec(width, height, buf)
          .map(DynamicImage::ImageLuma16)
          .ok_or_else(dimension_error)
      }
    }
  }

  fn avif_dimensions(
    width: usize,
    height: usize,
  ) -> std::result::Result<(u32, u32), image::ImageError> {
    let to_u32 = |v: usize| {
      u32::try_from(v).map_err(|_| {
        image::ImageError::Parameter(image::error::ParameterError::from_kind(
          image::error::ParameterErrorKind::DimensionMismatch,
        ))
      })
    };
    Ok((to_u32(width)?, to_u32(height)?))
  }

  fn avif_error(err: impl std::fmt::Display) -> image::ImageError {
    image::ImageError::Decoding(image::error::DecodingError::new(
      ImageFormat::Avif.into(),
      err.to_string(),
    ))
  }

  fn orientation_from_exif(value: u16) -> Option<OrientationTransform> {
    match value {
      1 => Some(OrientationTransform::IDENTITY),
      2 => Some(OrientationTransform {
        quarter_turns: 0,
        flip_x: true,
      }),
      3 => Some(OrientationTransform {
        quarter_turns: 2,
        flip_x: false,
      }),
      4 => Some(OrientationTransform {
        quarter_turns: 2,
        flip_x: true,
      }),
      5 => Some(OrientationTransform {
        quarter_turns: 1,
        flip_x: true,
      }),
      6 => Some(OrientationTransform {
        quarter_turns: 1,
        flip_x: false,
      }),
      7 => Some(OrientationTransform {
        quarter_turns: 3,
        flip_x: true,
      }),
      8 => Some(OrientationTransform {
        quarter_turns: 3,
        flip_x: false,
      }),
      _ => None,
    }
  }

  fn exif_metadata(bytes: &[u8]) -> (Option<OrientationTransform>, Option<f32>) {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      let mut cursor = std::io::Cursor::new(bytes);
      let Ok(exif) = exif::Reader::new().read_from_container(&mut cursor) else {
        return (None, None);
      };

      let orientation = exif
        .get_field(exif::Tag::Orientation, exif::In::PRIMARY)
        .and_then(|f| f.value.get_uint(0))
        .and_then(|v| Self::orientation_from_exif(v as u16));

      let resolution_unit = exif
        .get_field(exif::Tag::ResolutionUnit, exif::In::PRIMARY)
        .and_then(|f| f.value.get_uint(0))
        .unwrap_or(0);

      let rational_to_f32 = |r: exif::Rational| -> Option<f32> {
        if r.denom == 0 {
          None
        } else {
          Some(r.num as f32 / r.denom as f32)
        }
      };

      let x_res = exif
        .get_field(exif::Tag::XResolution, exif::In::PRIMARY)
        .and_then(|f| {
          if let exif::Value::Rational(ref vals) = f.value {
            vals.first().copied()
          } else {
            None
          }
        })
        .and_then(rational_to_f32);
      let y_res = exif
        .get_field(exif::Tag::YResolution, exif::In::PRIMARY)
        .and_then(|f| {
          if let exif::Value::Rational(ref vals) = f.value {
            vals.first().copied()
          } else {
            None
          }
        })
        .and_then(rational_to_f32);
      let avg_res = match (x_res, y_res) {
        (Some(x), Some(y)) if x.is_finite() && y.is_finite() && x > 0.0 && y > 0.0 => {
          Some((x + y) / 2.0)
        }
        (Some(v), None) | (None, Some(v)) if v.is_finite() && v > 0.0 => Some(v),
        _ => None,
      };

      let resolution = avg_res.and_then(|res| match resolution_unit {
        2 => Some(res / 96.0),          // inch -> dppx
        3 => Some((res * 2.54) / 96.0), // cm -> dppx
        _ => None,
      });

      (orientation, resolution)
    }))
    .unwrap_or((None, None))
  }

  #[allow(dead_code)]
  fn exif_orientation(bytes: &[u8]) -> Option<OrientationTransform> {
    Self::exif_metadata(bytes).0
  }

  /// Renders raw SVG content to a raster image, returning any parsed intrinsic aspect ratio
  /// information.
  pub fn render_svg_to_image(
    &self,
    svg_content: &str,
  ) -> Result<(DynamicImage, Option<f32>, bool)> {
    let (image, ratio, aspect_none, _has_size) =
      self.render_svg_to_image_with_url(svg_content, "SVG content")?;
    Ok((image, ratio, aspect_none))
  }

  fn render_svg_to_image_with_url(
    &self,
    svg_content: &str,
    url: &str,
  ) -> Result<(DynamicImage, Option<f32>, bool, bool)> {
    use resvg::usvg;

    check_root(RenderStage::Paint).map_err(Error::Render)?;
    let url_no_fragment = strip_url_fragment(url);

    // Parse SVG
    let options = usvg_options_for_url(url_no_fragment.as_ref());
    self.enforce_svg_resource_policy(svg_content, url_no_fragment.as_ref())?;
    let svg_preprocessed = self.preprocess_svg_markup(svg_content, url)?;
    let svg_content = svg_preprocessed.as_ref();
    let (meta_width, meta_height, meta_ratio, aspect_ratio_none) =
      svg_intrinsic_metadata(svg_content, 16.0, 16.0).unwrap_or((None, None, None, false));
    let svg_has_intrinsic_size =
      meta_width.filter(|w| *w > 0.0).is_some() || meta_height.filter(|h| *h > 0.0).is_some();
    let svg_for_parse = svg_markup_for_roxmltree(svg_content);
    let tree = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      usvg::Tree::from_str(svg_for_parse.as_ref(), &options)
    })) {
      Ok(Ok(tree)) => tree,
      Ok(Err(e)) => {
        return Err(Error::Image(ImageError::DecodeFailed {
          url: url.to_string(),
          reason: format!("Failed to parse SVG: {}", e),
        }));
      }
      Err(panic) => {
        return Err(Error::Image(ImageError::DecodeFailed {
          url: url.to_string(),
          reason: format!("SVG parse panicked: {}", panic_payload_to_reason(&*panic)),
        }));
      }
    };

    let size = tree.size();
    let source_width = size.width();
    let source_height = size.height();
    if source_width <= 0.0 || source_height <= 0.0 {
      return Err(Error::Render(RenderError::CanvasCreationFailed {
        width: source_width as u32,
        height: source_height as u32,
      }));
    }

    let ratio = meta_ratio.filter(|r| *r > 0.0);
    let (target_width, target_height) =
      svg_intrinsic_target_dimensions(meta_width, meta_height, ratio);

    let render_width = target_width.max(1.0).round() as u32;
    let render_height = target_height.max(1.0).round() as u32;

    self.enforce_decode_limits(render_width, render_height, url)?;
    check_root(RenderStage::Paint).map_err(Error::Render)?;

    // Render SVG to pixmap, scaling to the target intrinsic dimensions when needed
    let mut pixmap = new_pixmap(render_width, render_height).ok_or(Error::Render(
      RenderError::CanvasCreationFailed {
        width: render_width,
        height: render_height,
      },
    ))?;

    let transform = match svg_view_box_root_transform(
      svg_content,
      source_width,
      source_height,
      render_width as f32,
      render_height as f32,
    ) {
      Some(transform) => transform,
      None => {
        let scale_x = render_width as f32 / source_width;
        let scale_y = render_height as f32 / source_height;
        tiny_skia::Transform::from_scale(scale_x, scale_y)
      }
    };
    check_root(RenderStage::Paint).map_err(Error::Render)?;
    if let Err(panic) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      resvg::render(&tree, transform, &mut pixmap.as_mut());
    })) {
      return Err(Error::Image(ImageError::DecodeFailed {
        url: url.to_string(),
        reason: format!("SVG render panicked: {}", panic_payload_to_reason(&*panic)),
      }));
    }
    check_root(RenderStage::Paint).map_err(Error::Render)?;

    // Convert pixmap to image
    // `tiny_skia::Pixmap` stores premultiplied-alpha RGBA, while `image::RgbaImage` expects
    // straight/unpremultiplied RGBA. Unpremultiply here so downstream code (which premultiplies
    // `DynamicImage` buffers when converting back into a pixmap for painting) doesn't accidentally
    // double-premultiply SVG pixels, which visibly darkens semi-transparent edges.
    let mut rgba_data = pixmap.take();
    debug_assert_eq!(rgba_data.len() % 4, 0);
    for px in rgba_data.chunks_exact_mut(4) {
      let a = px[3];
      if a == 0 {
        px[0] = 0;
        px[1] = 0;
        px[2] = 0;
        continue;
      }
      if a == 255 {
        continue;
      }

      // Match `image_output`'s unpremultiplication semantics exactly:
      // - compute alpha as f32
      // - divide each channel by alpha
      // - clamp to 255 and truncate toward zero.
      let alpha = a as f32 / 255.0;
      px[0] = ((px[0] as f32 / alpha).min(255.0)) as u8;
      px[1] = ((px[1] as f32 / alpha).min(255.0)) as u8;
      px[2] = ((px[2] as f32 / alpha).min(255.0)) as u8;
    }
    let img =
      image::RgbaImage::from_raw(render_width, render_height, rgba_data).ok_or_else(|| {
        Error::Image(ImageError::DecodeFailed {
          url: url.to_string(),
          reason: "Failed to create image from SVG pixmap".to_string(),
        })
      })?;

    let ratio = if aspect_ratio_none {
      None
    } else {
      ratio.or_else(|| {
        if render_height > 0 {
          Some(render_width as f32 / render_height as f32)
        } else {
          None
        }
      })
    };

    Ok((
      image::DynamicImage::ImageRgba8(img),
      ratio,
      aspect_ratio_none,
      svg_has_intrinsic_size,
    ))
  }
}

fn svg_intrinsic_target_dimensions(
  meta_width: Option<f32>,
  meta_height: Option<f32>,
  intrinsic_ratio: Option<f32>,
) -> (f32, f32) {
  const DEFAULT_WIDTH: f32 = 300.0;
  const DEFAULT_HEIGHT: f32 = 150.0;

  let width = meta_width.filter(|w| *w > 0.0);
  let height = meta_height.filter(|h| *h > 0.0);
  let ratio = intrinsic_ratio.filter(|r| *r > 0.0);

  match (width, height, ratio) {
    (Some(w), Some(h), _) => (w, h),
    (Some(w), None, Some(r)) => (w, (w / r).max(1.0)),
    (None, Some(h), Some(r)) => ((h * r).max(1.0), h),
    (Some(w), None, None) => (w, DEFAULT_HEIGHT),
    (None, Some(h), None) => (DEFAULT_WIDTH, h),
    // When the SVG root doesn't specify an absolute intrinsic size (missing or percentage
    // widths/heights), use the default object size (300x150) but preserve the intrinsic ratio by
    // applying a "contain" constraint inside that box.
    //
    // This matches Blink/WebKit behavior for SVG images with only a viewBox (no width/height):
    // a 1:1 SVG ends up with an intrinsic size of 150x150, not 300x150.
    (None, None, Some(r)) => {
      let default_ratio = DEFAULT_WIDTH / DEFAULT_HEIGHT;
      if r >= default_ratio {
        (DEFAULT_WIDTH, (DEFAULT_WIDTH / r).max(1.0))
      } else {
        ((DEFAULT_HEIGHT * r).max(1.0), DEFAULT_HEIGHT)
      }
    }
    (None, None, None) => (DEFAULT_WIDTH, DEFAULT_HEIGHT),
  }
}

fn svg_find_root_start_tag(svg_content: &str) -> Option<&str> {
  // We only need the root `<svg ...>` start tag to extract `width`/`height`/`viewBox` and
  // `preserveAspectRatio`. Parsing the entire document with `roxmltree` can be *extremely* slow in
  // debug builds for large/complex SVGs (e.g. Illustrator exports), which makes pageset fixtures
  // time out during image metadata probing.
  //
  // Do a lightweight scan for the first `<svg` start tag, skipping XML prologs (`<?...?>`),
  // doctypes/comments (`<!...>`), and end tags (`</...>`). If we fail to find a plausible root tag
  // we fall back to the slower `roxmltree` parser.
  let bytes = svg_content.as_bytes();
  let mut i = 0usize;
  while i + 4 <= bytes.len() {
    if bytes[i] != b'<' {
      i += 1;
      continue;
    }
    match bytes.get(i + 1).copied() {
      Some(b'!' | b'?' | b'/') | None => {
        i += 1;
        continue;
      }
      _ => {}
    }

    if bytes
      .get(i + 1)
      .is_some_and(|b| b.to_ascii_lowercase() == b's')
      && bytes
        .get(i + 2)
        .is_some_and(|b| b.to_ascii_lowercase() == b'v')
      && bytes
        .get(i + 3)
        .is_some_and(|b| b.to_ascii_lowercase() == b'g')
    {
      // Ensure we're not matching `<svgFoo>`; require a boundary.
      let boundary = bytes.get(i + 4).copied().unwrap_or(b'>');
      if !(boundary.is_ascii_whitespace() || matches!(boundary, b'>' | b'/' | b':')) {
        i += 1;
        continue;
      }

      let mut quote: Option<u8> = None;
      let mut j = i + 4;
      while j < bytes.len() {
        let b = bytes[j];
        if let Some(q) = quote {
          if b == q {
            quote = None;
          }
          j += 1;
          continue;
        }

        match b {
          b'\'' | b'"' => quote = Some(b),
          b'>' => {
            j += 1;
            return svg_content.get(i..j);
          }
          _ => {}
        }
        j += 1;
      }
      return None;
    }

    i += 1;
  }
  None
}

fn svg_extract_root_attr<'a>(start_tag: &'a str, target: &str) -> Option<&'a str> {
  let bytes = start_tag.as_bytes();
  if bytes.len() < 5 || bytes[0] != b'<' {
    return None;
  }

  // Skip tag name (`<svg` or `<svg:svg` etc).
  let mut i = 1usize;
  while i < bytes.len() {
    let b = bytes[i];
    if b.is_ascii_whitespace() || matches!(b, b'>' | b'/') {
      break;
    }
    i += 1;
  }

  while i < bytes.len() {
    // Skip whitespace.
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
      i += 1;
    }
    if i >= bytes.len() || matches!(bytes[i], b'>' | b'/') {
      break;
    }

    // Attribute name.
    let name_start = i;
    while i < bytes.len()
      && !bytes[i].is_ascii_whitespace()
      && !matches!(bytes[i], b'=' | b'>' | b'/')
    {
      i += 1;
    }
    let name_end = i;

    // Skip whitespace.
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
      i += 1;
    }
    if i >= bytes.len() || bytes[i] != b'=' {
      continue;
    }
    i += 1; // '='
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
      i += 1;
    }
    if i >= bytes.len() {
      break;
    }

    let (value_start, value_end) = match bytes[i] {
      b'\'' | b'"' => {
        let quote = bytes[i];
        i += 1;
        let start = i;
        while i < bytes.len() && bytes[i] != quote {
          i += 1;
        }
        let end = i.min(bytes.len());
        if i < bytes.len() {
          i += 1; // closing quote
        }
        (start, end)
      }
      _ => {
        let start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() && !matches!(bytes[i], b'>' | b'/')
        {
          i += 1;
        }
        (start, i)
      }
    };

    if name_end > name_start
      && start_tag
        .get(name_start..name_end)
        .is_some_and(|name| name == target)
    {
      return start_tag.get(value_start..value_end);
    }
  }

  None
}

/// Returns intrinsic metadata extracted from the SVG root element: explicit width/height when
/// present (including common font-relative units when `font_size`/`root_font_size` are provided),
/// an intrinsic aspect ratio (if not disabled), and whether preserveAspectRatio="none" was
/// specified.
fn svg_intrinsic_metadata(
  svg_content: &str,
  font_size: f32,
  root_font_size: f32,
) -> Option<(Option<f32>, Option<f32>, Option<f32>, bool)> {
  if let Some(start_tag) = svg_find_root_start_tag(svg_content) {
    let intrinsic = svg_intrinsic_dimensions_from_attributes(
      svg_extract_root_attr(start_tag, "width"),
      svg_extract_root_attr(start_tag, "height"),
      svg_extract_root_attr(start_tag, "viewBox"),
      svg_extract_root_attr(start_tag, "preserveAspectRatio"),
      font_size,
      root_font_size,
    );

    return Some((
      intrinsic.width,
      intrinsic.height,
      intrinsic.aspect_ratio,
      intrinsic.aspect_ratio_none,
    ));
  }

  // Slow fallback: full XML parse.
  std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    let svg_for_parse = svg_markup_for_roxmltree(svg_content);
    let doc = Document::parse(svg_for_parse.as_ref()).ok()?;
    let root = doc.root_element();
    if !root.tag_name().name().eq_ignore_ascii_case("svg") {
      return None;
    }

    let intrinsic = svg_intrinsic_dimensions_from_attributes(
      root.attribute("width"),
      root.attribute("height"),
      root.attribute("viewBox"),
      root.attribute("preserveAspectRatio"),
      font_size,
      root_font_size,
    );

    Some((
      intrinsic.width,
      intrinsic.height,
      intrinsic.aspect_ratio,
      intrinsic.aspect_ratio_none,
    ))
  }))
  .ok()
  .flatten()
}

// ============================================================================
// URL Resolution
// ============================================================================

pub(crate) fn resolve_against_base(base: &str, reference: &str) -> Option<String> {
  // Normalize file:// bases that point to directories so Url::join keeps the directory segment.
  let mut base_candidate = base.to_string();
  if base_candidate.starts_with("file://") {
    let path = &base_candidate["file://".len()..];
    if Path::new(path).is_dir() && !base_candidate.ends_with('/') {
      base_candidate.push('/');
    }
  }

  let mut base_url = Url::parse(&base_candidate)
    .or_else(|_| {
      Url::from_file_path(&base_candidate).map_err(|()| url::ParseError::RelativeUrlWithoutBase)
    })
    .ok()?;

  if base_url.scheme() == "file" {
    if let Ok(path) = base_url.to_file_path() {
      if path.is_dir() && !base_url.path().ends_with('/') {
        let mut path_str = base_url.path().to_string();
        path_str.push('/');
        base_url.set_path(&path_str);
      }
    }
  }

  let normalized = normalize_url_reference_for_resolution(reference);
  if normalized.as_ref() != reference {
    if let Ok(joined) = base_url.join(normalized.as_ref()) {
      return Some(joined.to_string());
    }
  }

  base_url.join(reference).ok().map(|u| u.to_string())
}

// ============================================================================
// Trait Implementations
// ============================================================================

impl Default for ImageCache {
  fn default() -> Self {
    Self::new()
  }
}

impl Clone for ImageCache {
  fn clone(&self) -> Self {
    Self {
      instance_id: self.instance_id,
      epoch: Arc::clone(&self.epoch),
      cache: Arc::clone(&self.cache),
      in_flight: Arc::clone(&self.in_flight),
      meta_cache: Arc::clone(&self.meta_cache),
      raw_cache: Arc::clone(&self.raw_cache),
      gif_timing_cache: Arc::clone(&self.gif_timing_cache),
      meta_in_flight: Arc::clone(&self.meta_in_flight),
      svg_preprocess_cache: Arc::clone(&self.svg_preprocess_cache),
      svg_subresource_cache: Arc::clone(&self.svg_subresource_cache),
      svg_pixmap_cache: Arc::clone(&self.svg_pixmap_cache),
      raster_pixmap_cache: Arc::clone(&self.raster_pixmap_cache),
      base_url: self.base_url.clone(),
      fetcher: Arc::clone(&self.fetcher),
      config: self.config,
      animation_time_ms: self.animation_time_ms,
      diagnostics: self.diagnostics.clone(),
      resource_context: self.resource_context.clone(),
    }
  }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests;

#[cfg(test)]
mod tests_inline {
  use super::*;
  use crate::error::{Error, RenderError, RenderStage};
  use crate::render_control::RenderDeadline;
  use crate::render_control::{
    record_stage, StageAllocationBudget, StageAllocationBudgetGuard, StageHeartbeat,
  };
  use crate::style::types::OrientationTransform;
  use base64::Engine;
  use image::codecs::gif::GifEncoder;
  use image::codecs::jpeg::JpegEncoder;
  use image::codecs::png::PngEncoder;
  use image::ColorType;
  use image::Delay;
  use image::Frame;
  use image::ImageEncoder;
  use image::Rgba as ImageRgba;
  use image::RgbaImage;
  use std::path::PathBuf;
  use std::time::Duration;
  use std::time::SystemTime;
  use tempfile::tempdir;
  use url::Url;

  #[test]
  fn sized_lru_cache_tracks_bytes_and_eviction() {
    let mut cache = SizedLruCache::new(0, 10);
    cache.insert("a".to_string(), 1u32, 6);
    assert_eq!(cache.len(), 1);
    assert_eq!(cache.current_bytes(), 6);
    cache.insert("b".to_string(), 2u32, 6);
    assert_eq!(cache.len(), 1);
    assert_eq!(cache.current_bytes(), 6);
    assert!(cache.get_cloned("a").is_none());
    assert_eq!(cache.get_cloned("b"), Some(2));
  }

  #[test]
  fn sized_lru_cache_replacing_updates_current_bytes() {
    let mut cache = SizedLruCache::new(0, 100);
    cache.insert("a".to_string(), 1u32, 10);
    assert_eq!(cache.current_bytes(), 10);
    cache.insert("a".to_string(), 2u32, 4);
    assert_eq!(cache.len(), 1);
    assert_eq!(cache.current_bytes(), 4);
    assert_eq!(cache.get_cloned("a"), Some(2));
  }

  #[test]
  fn url_looks_like_gif_detects_common_sources() {
    assert!(url_looks_like_gif(
      "data:image/gif;base64,R0lGODlhAQABAAAAACw="
    ));
    assert!(url_looks_like_gif("file:///tmp/x.gif"));
    assert!(url_looks_like_gif("https://example.com/x.gif?query#frag"));
    assert!(!url_looks_like_gif("https://example.com/x.png"));
  }

  #[test]
  fn svg_text_renders_with_fontdb_configured() {
    let cache = ImageCache::new();
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="80" height="30">
      <text x="0" y="20" font-family="Cantarell" font-size="20" fill="red">F</text>
    </svg>"#;

    let pixmap = cache
      .render_svg_pixmap_at_size(svg, 80, 30, "svg-text", 1.0)
      .expect("render svg pixmap");

    let has_red_text_pixel = pixmap
      .data()
      .chunks_exact(4)
      .any(|px| px[3] > 0 && px[0] > 0);
    assert!(
      has_red_text_pixel,
      "expected SVG <text> to produce non-transparent red pixels; missing usvg fontdb?"
    );
  }

  #[test]
  fn svg_with_doctype_renders_via_usvg() {
    let cache = ImageCache::new();
    // `roxmltree` rejects `<!DOCTYPE ...>` declarations and `usvg` uses `roxmltree` internally.
    // If we don't strip/blank doctypes, external SVG logos commonly disappear.
    //
    // Ensure the fast-path renderer is *not* used (`<rect>` is unsupported there), so the test
    // exercises the `usvg` path.
    let svg = r#"<?xml version="1.0" encoding="utf-8"?>
<!DOCTYPE svg PUBLIC "-//W3C//DTD SVG 1.1//EN" "http://www.w3.org/Graphics/SVG/1.1/DTD/svg11.dtd">
<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10">
  <rect width="10" height="10" fill="red" />
</svg>"#;

    let (img, _ratio, _aspect_none) = cache.render_svg_to_image(svg).expect("render svg to image");
    let rgba = img.to_rgba8();
    assert_eq!(rgba.dimensions(), (10, 10));
    let px = rgba.get_pixel(5, 5).0;
    assert!(
      px[3] > 200 && px[0] > 200 && px[1] < 50 && px[2] < 50,
      "expected opaque red pixel; got {px:?}"
    );

    let pixmap = cache
      .render_svg_pixmap_at_size(svg, 10, 10, "svg-doctype", 1.0)
      .expect("render svg pixmap");
    let idx = (5 + 5 * 10) * 4;
    let px = &pixmap.data()[idx..idx + 4];
    assert!(
      px[3] > 200 && px[0] > 200 && px[1] < 50 && px[2] < 50,
      "expected opaque red pixel; got {px:?}"
    );
  }

  #[cfg(feature = "direct_network")]
  #[test]
  fn svg_image_href_resolves_against_svg_url() {
    let dir = tempdir().expect("temp dir");
    let png_path = dir.path().join("img.png");
    let png = RgbaImage::from_pixel(4, 4, ImageRgba([255, 0, 0, 255]));
    png.save(&png_path).expect("write png");

    let svg_path = dir.path().join("icon.svg");
    let svg_content = r#"
      <svg xmlns="http://www.w3.org/2000/svg" width="4" height="4">
        <image href="img.png" width="4" height="4" />
      </svg>
    "#;
    std::fs::write(&svg_path, svg_content).expect("write svg");

    let svg_url = Url::from_file_path(&svg_path).unwrap().to_string();

    let mut cache = ImageCache::new();
    cache.set_base_url("file:///not-used-for-svg-base/");

    let image = cache.load(&svg_url).expect("render svg with image href");
    let rgba = image.image.to_rgba8();

    assert_eq!(rgba.dimensions(), (4, 4));
    assert_eq!(*rgba.get_pixel(0, 0), ImageRgba([255, 0, 0, 255]));
    assert_eq!(*rgba.get_pixel(3, 3), ImageRgba([255, 0, 0, 255]));
  }

  #[test]
  fn svg_image_href_supports_data_url() {
    let cache = ImageCache::new();

    let data_image = RgbaImage::from_pixel(2, 2, ImageRgba([0, 0, 255, 255]));
    let mut buf = Vec::new();
    data_image
      .write_to(&mut Cursor::new(&mut buf), ImageFormat::Png)
      .expect("encode png");
    let data_url = format!(
      "data:image/png;base64,{}",
      base64::engine::general_purpose::STANDARD.encode(&buf)
    );

    let svg = format!(
      r#"<svg xmlns="http://www.w3.org/2000/svg" width="2" height="2">
          <image href="{data_url}" width="2" height="2" />
        </svg>"#
    );

    let (rendered, _, _) = cache.render_svg_to_image(&svg).expect("render svg");
    let rgba = rendered.to_rgba8();
    assert_eq!(*rgba.get_pixel(0, 0), ImageRgba([0, 0, 255, 255]));
    assert_eq!(*rgba.get_pixel(1, 1), ImageRgba([0, 0, 255, 255]));
  }

  fn composite_pixel_over_white(px: ImageRgba<u8>) -> ImageRgba<u8> {
    let a = px[3] as u16;
    let inv_a = 255u16.saturating_sub(a);
    // `render_svg_to_image` rasterizes via `tiny-skia`, which stores pixels as premultiplied RGBA.
    // We only sample fully-opaque / fully-transparent pixels in these tests, but compositing over a
    // white background makes the expected colors easier to express (red vs white).
    ImageRgba([
      (px[0] as u16 + inv_a).min(255) as u8,
      (px[1] as u16 + inv_a).min(255) as u8,
      (px[2] as u16 + inv_a).min(255) as u8,
      255,
    ])
  }

  fn render_svg(svg: &str) -> RgbaImage {
    let cache = ImageCache::new();
    let (img, _ratio, _aspect_ratio_none) = cache.render_svg_to_image(svg).expect("render svg");
    img.to_rgba8()
  }

  #[test]
  fn svg_render_to_image_preserve_aspect_ratio_xmin_ymin_meet() {
    let svg = r#"
      <svg xmlns="http://www.w3.org/2000/svg"
           width="20" height="10"
           viewBox="0 0 10 10"
           preserveAspectRatio="xMinYMin meet"
           shape-rendering="crispEdges">
        <rect x="0" y="0" width="10" height="10" fill="red" />
      </svg>
    "#;

    let rgba = render_svg(svg);
    assert_eq!(rgba.dimensions(), (20, 10));

    // With `meet`, the 10x10 viewBox fits into a 20x10 viewport without scaling (height is the
    // limiting dimension), leaving 10px of horizontal space. `xMinYMin` aligns the content to the
    // left.
    assert_eq!(
      composite_pixel_over_white(*rgba.get_pixel(2, 5)),
      ImageRgba([255, 0, 0, 255]),
      "expected left side to be red"
    );
    assert_eq!(
      composite_pixel_over_white(*rgba.get_pixel(18, 5)),
      ImageRgba([255, 255, 255, 255]),
      "expected right side to be empty (white when composited)"
    );
  }

  #[test]
  fn svg_render_to_image_preserve_aspect_ratio_xmax_ymin_meet() {
    let svg = r#"
      <svg xmlns="http://www.w3.org/2000/svg"
           width="20" height="10"
           viewBox="0 0 10 10"
           preserveAspectRatio="xMaxYMin meet"
           shape-rendering="crispEdges">
        <rect x="0" y="0" width="10" height="10" fill="red" />
      </svg>
    "#;

    let rgba = render_svg(svg);
    assert_eq!(rgba.dimensions(), (20, 10));

    // `xMaxYMin` aligns the viewBox content to the right.
    assert_eq!(
      composite_pixel_over_white(*rgba.get_pixel(2, 5)),
      ImageRgba([255, 255, 255, 255]),
      "expected left side to be empty (white when composited)"
    );
    assert_eq!(
      composite_pixel_over_white(*rgba.get_pixel(18, 5)),
      ImageRgba([255, 0, 0, 255]),
      "expected right side to be red"
    );
  }

  #[test]
  fn probe_svg_content_extracts_intrinsic_dimensions_without_roxmltree() {
    let cache = ImageCache::new();
    // Real-world SVGs often start with a DOCTYPE which `roxmltree` rejects. Our probe path should
    // still be able to extract intrinsic dimensions from the root start tag without needing a full
    // XML parse.
    let svg = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE svg PUBLIC "-//W3C//DTD SVG 1.1//EN"
  "http://www.w3.org/Graphics/SVG/1.1/DTD/svg11.dtd">
<svg width="300" height="150" viewBox="0 0 300 150" xmlns="http://www.w3.org/2000/svg"></svg>"#;
    let meta = cache
      .probe_svg_content(svg, "test.svg")
      .expect("probe svg content");
    assert_eq!(meta.width, 300);
    assert_eq!(meta.height, 150);
    assert_eq!(meta.intrinsic_ratio, Some(2.0));
  }

  #[test]
  fn probe_svg_content_extracts_viewbox_aspect_ratio_without_intrinsic_size() {
    let cache = ImageCache::new();
    let svg = r#"<svg viewBox="0 0 100 50" xmlns="http://www.w3.org/2000/svg"></svg>"#;
    let meta = cache
      .probe_svg_content(svg, "ratio.svg")
      .expect("probe svg content");
    // Without width/height, SVG images behave like other replaced elements: 300x150 with a
    // separately tracked intrinsic aspect ratio.
    assert_eq!(meta.width, 300);
    assert_eq!(meta.height, 150);
    assert_eq!(meta.intrinsic_ratio, Some(2.0));
  }

  #[test]
  #[cfg(feature = "avif")]
  fn avif_fixture_asset_decodes() {
    let cache = ImageCache::new();
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
      .join("tests/pages/fixtures/gitlab.com/assets/22f6f99a9d5ab37d1feb616188f30106.avif");
    let bytes = std::fs::read(&path).expect("read avif fixture");
    let (img, has_alpha) = cache
      .decode_bitmap(&bytes, Some("image/avif"), &path.to_string_lossy())
      .expect("decode avif fixture");
    assert!(img.width() > 0 && img.height() > 0);
    assert!(
      !ImageCache::decoded_bitmap_is_single_transparent_pixel(&img, has_alpha),
      "expected avif fixture to decode into real pixels (not missing-image sentinel)"
    );
  }

  #[test]
  fn resolve_against_base_normalizes_pipe_character() {
    let resolved = resolve_against_base("https://example.com/dir/", "a|b.svg").expect("resolved");
    assert_eq!(resolved, "https://example.com/dir/a%7Cb.svg");
  }

  #[test]
  fn resolve_against_base_preserves_nbsp() {
    let nbsp = "\u{00A0}";
    let reference = format!("a{nbsp}b.svg");
    let resolved = resolve_against_base("https://example.com/dir/", &reference).expect("resolved");
    assert_eq!(resolved, "https://example.com/dir/a%C2%A0b.svg");
  }

  #[test]
  fn resolve_against_base_percent_encodes_pipe() {
    let base = "https://example.com/a/";
    let reference = "b|c.png";
    let resolved = resolve_against_base(base, reference).expect("resolved");
    assert_eq!(resolved, "https://example.com/a/b%7Cc.png");
  }

  #[test]
  fn resolve_against_base_percent_encodes_spaces() {
    let base = "https://example.com/a/";
    let reference = "b c.png";
    let resolved = resolve_against_base(base, reference).expect("resolved");
    assert_eq!(resolved, "https://example.com/a/b%20c.png");
  }

  #[derive(Clone, Default)]
  struct MapFetcher {
    responses: Arc<HashMap<String, FetchedResource>>,
    requests: Arc<Mutex<Vec<(String, FetchDestination, FetchCredentialsMode)>>>,
  }

  impl MapFetcher {
    fn with_entries(entries: impl IntoIterator<Item = (String, FetchedResource)>) -> Self {
      Self {
        responses: Arc::new(entries.into_iter().collect()),
        requests: Arc::new(Mutex::new(Vec::new())),
      }
    }

    fn requests(&self) -> Vec<(String, FetchDestination, FetchCredentialsMode)> {
      self
        .requests
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
    }
  }

  impl ResourceFetcher for MapFetcher {
    fn fetch(&self, url: &str) -> Result<FetchedResource> {
      self
        .responses
        .get(url)
        .cloned()
        .map(|mut res| {
          res.final_url.get_or_insert_with(|| url.to_string());
          res
        })
        .ok_or_else(|| Error::Other(format!("unexpected fetch url {url}")))
    }

    fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
      self
        .requests
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push((req.url.to_string(), req.destination, req.credentials_mode));
      self.fetch(req.url)
    }
  }

  #[derive(Debug, Clone, PartialEq, Eq)]
  struct RecordedFetchRequest {
    url: String,
    destination: FetchDestination,
    referrer_url: Option<String>,
    credentials_mode: FetchCredentialsMode,
  }

  #[derive(Clone, Default)]
  struct RecordingFetcher {
    responses: Arc<HashMap<String, FetchedResource>>,
    requests: Arc<Mutex<Vec<RecordedFetchRequest>>>,
  }

  impl RecordingFetcher {
    fn with_entries(entries: impl IntoIterator<Item = (String, FetchedResource)>) -> Self {
      Self {
        responses: Arc::new(entries.into_iter().collect()),
        requests: Arc::new(Mutex::new(Vec::new())),
      }
    }

    fn requests(&self) -> Vec<RecordedFetchRequest> {
      self
        .requests
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
    }
  }

  impl ResourceFetcher for RecordingFetcher {
    fn fetch(&self, url: &str) -> Result<FetchedResource> {
      self
        .responses
        .get(url)
        .cloned()
        .map(|mut res| {
          res.final_url.get_or_insert_with(|| url.to_string());
          res
        })
        .ok_or_else(|| Error::Other(format!("unexpected fetch url {url}")))
    }

    fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
      self
        .requests
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(RecordedFetchRequest {
          url: req.url.to_string(),
          destination: req.destination,
          referrer_url: req.referrer_url.map(|url| url.to_string()),
          credentials_mode: req.credentials_mode,
        });
      self.fetch(req.url)
    }
  }

  fn gzip_bytes(input: &[u8]) -> Vec<u8> {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;

    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(input).expect("gzip input");
    encoder.finish().expect("finish gzip")
  }

  fn encode_single_pixel_png(rgba: [u8; 4]) -> Vec<u8> {
    let mut pixels = RgbaImage::new(1, 1);
    pixels.pixels_mut().for_each(|p| *p = image::Rgba(rgba));
    let mut png = Vec::new();
    PngEncoder::new(&mut png)
      .write_image(pixels.as_raw(), 1, 1, ColorType::Rgba8.into())
      .expect("encode png");
    png
  }

  fn fixture_file_url(rel_path: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel_path);
    Url::from_file_path(path)
      .unwrap_or_else(|_| panic!("failed to build file URL for {rel_path}"))
      .to_string()
  }

  #[test]
  fn raster_pixmap_allocation_budget_exceeded() {
    let url = "test://alloc-budget.png";
    let bytes = encode_single_pixel_png([0, 0, 0, 255]);
    let mut res = FetchedResource::new(bytes, Some("image/png".to_string()));
    res.status = Some(200);
    res.final_url = Some(url.to_string());
    let fetcher = Arc::new(MapFetcher::with_entries([(url.to_string(), res)]));
    let cache = ImageCache::with_fetcher(fetcher);

    record_stage(StageHeartbeat::PaintRasterize);
    let budget = Arc::new(StageAllocationBudget::new(5));
    let _guard = StageAllocationBudgetGuard::install(Some(&budget));
    let err = cache
      .load_raster_pixmap(url, OrientationTransform::IDENTITY, false)
      .unwrap_err();
    match err {
      Error::Render(RenderError::StageAllocationBudgetExceeded {
        stage, heartbeat, ..
      }) => {
        assert_eq!(stage, RenderStage::Paint);
        assert_eq!(heartbeat, StageHeartbeat::PaintRasterize);
      }
      other => panic!("expected StageAllocationBudgetExceeded, got {other:?}"),
    }
  }

  #[test]
  fn png_iccp_profile_is_converted_to_srgb() {
    // Regression test for the ietf.org fixture: some PNGs embed an ICC profile (e.g. Display P3),
    // and Chrome converts them to sRGB when rendering. If we ignore the embedded profile, images
    // render with noticeably different colors.
    let png = include_bytes!(
      "../tests/pages/fixtures/ietf.org/assets/d569cb1453b4178adc5736e882edf968.png"
    );
    let cache = ImageCache::new();
    let (decoded, _) = cache
      .decode_with_format(png, ImageFormat::Png, "test://iccp.png")
      .expect("decode png with icc profile");
    let rgba = decoded.to_rgba8();
    let px = rgba.get_pixel(414, 67).0;
    assert_eq!([px[0], px[1], px[2]], [61, 136, 219]);
  }

  #[test]
  fn webp_decoding_matches_libwebp_for_discord_fixture() {
    // Regression test for the discord.com fixture: the hero wumpus WebP decoded with the `image`
    // crate backend produced incorrect alpha blocks, causing visible transparent stripes. Decode
    // via libwebp and assert a representative pixel is opaque.
    let webp = include_bytes!(
      "../tests/pages/fixtures/discord.com/assets/852884fdf69f7e64b05e3b8af3493f06.webp"
    );
    let cache = ImageCache::new();
    let (decoded, has_alpha) = cache
      .decode_with_format(webp, ImageFormat::WebP, "test://discord-wumpus.webp")
      .expect("decode webp");
    assert!(has_alpha, "expected WebP to be reported as having alpha");

    let rgba = decoded.to_rgba8();
    assert_eq!(rgba.dimensions(), (245, 192));
    let px = rgba.get_pixel(140, 33).0;
    assert_eq!(px, [200, 231, 250, 255]);
  }

  #[cfg(feature = "direct_network")]
  #[test]
  fn image_cache_preserves_gif_alpha_metadata() {
    let cache = ImageCache::new();

    // This GIF includes a Graphics Control Extension with the transparency flag set.
    let gif_with_alpha =
      fixture_file_url("tests/pages/fixtures/ft.com/assets/ef1955ae757c8b966c83248350331bd3.gif");
    let img = cache.load(&gif_with_alpha).expect("load gif with alpha");
    assert!(
      img.has_alpha,
      "expected gif transparency metadata to be preserved (has_alpha=true)"
    );

    // This GIF omits transparency (GCE packed field bit 0 is unset), so it should be treated as a
    // luminance mask under `mask-mode: match-source`.
    let gif_without_alpha = fixture_file_url(
      "tests/pages/fixtures/slashdot.org/assets/4e0705327480ad2323cb03d9c450ffca.gif",
    );
    let img = cache
      .load(&gif_without_alpha)
      .expect("load gif without alpha");
    assert!(
      !img.has_alpha,
      "expected gif without transparency to report has_alpha=false"
    );
  }

  #[cfg(feature = "direct_network")]
  #[test]
  fn image_cache_preserves_webp_alpha_metadata() {
    let cache = ImageCache::new();

    // Lossless WebP fixture with the alpha flag set in the VP8L header.
    let webp_with_alpha = fixture_file_url("tests/fixtures/avif/solid.webp");
    let img = cache.load(&webp_with_alpha).expect("load webp with alpha");
    assert!(
      img.has_alpha,
      "expected webp alpha metadata to be preserved (has_alpha=true)"
    );

    // Lossless WebP without alpha (VP8L header alpha bit unset).
    let webp_without_alpha = fixture_file_url(
      "tests/pages/fixtures/foxnews.com/assets/25aa2ed5a0afc0c31b18c51de6df7437.webp",
    );
    let img = cache
      .load(&webp_without_alpha)
      .expect("load webp without alpha");
    assert!(
      !img.has_alpha,
      "expected webp without alpha to report has_alpha=false"
    );
  }

  #[cfg(all(feature = "direct_network", feature = "avif"))]
  #[test]
  fn image_cache_decodes_avif_pixels() {
    let cache = ImageCache::new();

    // How-To Geek fixtures are AVIF-heavy; if AVIF decodes fail we end up with blank (white) pages.
    let url = fixture_file_url(
      "tests/pages/fixtures/howtogeek.com/assets/fb5e72f6b237f006dd9df83e1aa8e008.avif",
    );
    let img = cache.load(&url).expect("load avif fixture");

    let rgba = img.image.to_rgba8();
    let bytes = rgba.as_raw();
    assert!(
      !bytes.is_empty(),
      "decoded AVIF should contain RGBA pixels (got empty buffer)"
    );

    let mut alpha_max = 0u8;
    let mut has_non_white = false;
    for px in bytes.chunks_exact(4) {
      alpha_max = alpha_max.max(px[3]);
      has_non_white |= px[0] != 0xFF || px[1] != 0xFF || px[2] != 0xFF;
      if alpha_max == 0xFF && has_non_white {
        break;
      }
    }

    assert!(
      alpha_max > 0,
      "decoded AVIF unexpectedly has alpha=0 for all pixels (fully transparent)"
    );
    assert!(
      has_non_white,
      "decoded AVIF unexpectedly contains only white pixels"
    );
  }

  #[test]
  fn offline_fixture_placeholder_png_is_detected_as_placeholder_image_after_probe_cache_reuse() {
    let url = "test://missing.png";
    let bytes = encode_single_pixel_png([0, 0, 0, 0]);
    assert_ne!(
      bytes.as_slice(),
      crate::resource::offline_placeholder_png_bytes(),
      "test requires non-canonical placeholder bytes"
    );
    let mut res = FetchedResource::new(
      bytes,
      Some(crate::resource::offline_placeholder_png_content_type().to_string()),
    );
    res.status = Some(200);
    res.final_url = Some(url.to_string());

    let fetcher = MapFetcher::with_entries([(url.to_string(), res)]);
    let cache = ImageCache::with_fetcher(Arc::new(fetcher.clone()));

    // Probe should cache the small bytes so a later decode can reuse them without fetching again.
    cache.probe(url).expect("probe should succeed");
    assert_eq!(
      fetcher.requests().len(),
      1,
      "expected probe to fetch exactly once"
    );

    // Load should reuse the probed bytes and, because the response is marked as an offline
    // placeholder sentinel, return the shared `about:` placeholder image.
    let img = cache.load(url).expect("image should load");
    assert_eq!(
      fetcher.requests().len(),
      1,
      "expected load() to reuse the probed bytes without issuing another fetch"
    );
    assert!(
      cache.is_placeholder_image(&img),
      "offline placeholder PNG payload should map to the shared placeholder image"
    );
  }

  #[cfg(feature = "direct_network")]
  #[test]
  fn image_cache_load_empty_file_url_returns_shared_placeholder_image() {
    // Offline fixtures frequently represent missing images as empty files. Our file fetcher
    // substitutes those bytes with a deterministic 1×1 transparent PNG so downstream code can keep
    // working with a stable intrinsic size. Ensure `ImageCache` maps that sentinel back to the
    // shared placeholder image so painters can reliably detect it (e.g. to render UA broken-image
    // UI for `<img>` elements).
    let tmp = tempdir().expect("tempdir");
    let assets = tmp.path().join("assets");
    std::fs::create_dir(&assets).expect("create assets dir");
    let index_path = tmp.path().join("index.html");
    std::fs::write(&index_path, "<!doctype html>").expect("write index.html");
    let missing = assets.join("missing.bin");
    std::fs::write(&missing, &[]).expect("write missing image");

    let base_url = Url::from_file_path(&index_path)
      .expect("index.html file URL")
      .to_string();
    let cache = ImageCache::with_base_url(base_url);

    let img = cache
      .load("assets/missing.bin")
      .expect("empty fixture image should load as placeholder");
    assert!(
      cache.is_placeholder_image(&img),
      "expected empty fixture image to map to the shared placeholder image"
    );
  }

  #[cfg(feature = "direct_network")]
  #[test]
  fn image_cache_load_file_url_1x1_transparent_png_with_non_image_content_type_maps_to_placeholder()
  {
    // Older offline fixtures can contain 1×1 transparent PNG "placeholder pixel" files without the
    // explicit `fastrender-placeholder=1` content-type marker (because file:// loads infer their
    // MIME type from the path extension). Ensure we still map these to the shared placeholder image
    // so `<img>` elements render UA broken-image UI instead of silently painting a stretched
    // transparent pixel.
    let png = encode_single_pixel_png([0, 0, 0, 0]);
    assert_ne!(
      png.as_slice(),
      crate::resource::offline_placeholder_png_bytes(),
      "test requires non-canonical placeholder bytes"
    );

    let tmp = tempdir().expect("tempdir");
    let index_path = tmp.path().join("index.html");
    std::fs::write(&index_path, "<!doctype html>").expect("write index.html");
    let placeholder_path = tmp.path().join("placeholder.html");
    std::fs::write(&placeholder_path, png).expect("write placeholder pixel");

    let base_url = Url::from_file_path(&index_path)
      .expect("index.html file URL")
      .to_string();
    let cache = ImageCache::with_base_url(base_url);

    let img = cache
      .load("placeholder.html")
      .expect("placeholder pixel should load");
    assert!(
      cache.is_placeholder_image(&img),
      "expected 1×1 transparent non-image file payload to map to the shared placeholder image"
    );
  }

  #[derive(Clone)]
  struct ReferrerAwarePngFetcher {
    calls: Arc<std::sync::atomic::AtomicUsize>,
    variants: Arc<HashMap<(Option<String>, ReferrerPolicy), Vec<u8>>>,
  }

  impl ReferrerAwarePngFetcher {
    fn new(variants: HashMap<(Option<String>, ReferrerPolicy), Vec<u8>>) -> Self {
      Self {
        calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        variants: Arc::new(variants),
      }
    }

    fn calls(&self) -> usize {
      self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
  }

  impl ResourceFetcher for ReferrerAwarePngFetcher {
    fn fetch(&self, _url: &str) -> Result<FetchedResource> {
      panic!("expected ImageCache image load to use fetch_with_request");
    }

    fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
      self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
      let key = (req.referrer_url.map(|v| v.to_string()), req.referrer_policy);
      let bytes = self
        .variants
        .get(&key)
        .cloned()
        .ok_or_else(|| Error::Other(format!("unexpected image fetch request {key:?}")))?;
      let mut res = FetchedResource::new(bytes, Some("image/png".to_string()));
      res.status = Some(200);
      res.final_url = Some(req.url.to_string());
      Ok(res)
    }
  }

  #[test]
  fn svg_pixmap_key_canonicalizes_negative_zero() {
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg"></svg>"#;
    let url = "https://example.com/negzero.svg";
    let key = svg_pixmap_key(svg, url, 0.0, 10, 10);
    let key_neg = svg_pixmap_key(svg, url, -0.0, 10, 10);
    assert_eq!(key, key_neg);
  }

  #[test]
  fn image_fetch_strips_http_fragment() {
    let url = "https://example.test/icon.svg";
    let url_with_fragment = "https://example.test/icon.svg#frag";
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"></svg>"#;

    let mut res = FetchedResource::new(svg.as_bytes().to_vec(), Some("image/svg+xml".to_string()));
    res.status = Some(200);
    res.final_url = Some(url.to_string());

    let fetcher = MapFetcher::with_entries([(url.to_string(), res)]);
    let cache = ImageCache::with_fetcher(Arc::new(fetcher.clone()));

    let img = cache.load(url_with_fragment).expect("image should load");
    assert!(img.is_vector, "expected SVG image");
    assert_eq!(img.dimensions(), (1, 1));
    assert_eq!(
      fetcher.requests(),
      vec![(
        url.to_string(),
        FetchDestination::Image,
        FetchCredentialsMode::Include,
      )]
    );
  }

  #[test]
  fn data_svg_url_with_fragment_decodes() {
    let url = "data:image/svg+xml,%3Csvg%20xmlns='http://www.w3.org/2000/svg'%20width='1'%20height='1'%3E%3C/svg%3E#ignored";
    let cache = ImageCache::new();

    let img = cache.load(url).expect("data SVG should decode");
    assert!(img.is_vector, "expected SVG image");
    assert_eq!(img.dimensions(), (1, 1));
  }

  #[test]
  fn probe_svg_content_handles_doctype_with_px_intrinsic_size() {
    let cache = ImageCache::new();
    let svg = r#"<?xml version="1.0" encoding="utf-8"?>
<!DOCTYPE svg PUBLIC "-//W3C//DTD SVG 1.1//EN" "http://www.w3.org/Graphics/SVG/1.1/DTD/svg11.dtd">
<svg xmlns="http://www.w3.org/2000/svg" width="863.5px" height="700.17px" viewBox="0 0 863.5 700.17"></svg>
"#;

    let meta = cache
      .probe_svg_content(svg, "https://doc.test/doctype.svg")
      .expect("probe_svg_content should succeed");

    // SVG intrinsic sizing follows HTML replaced element defaults: explicit px widths/heights are
    // taken verbatim (rounded to device pixels during decode).
    assert_eq!(meta.width, 864);
    assert_eq!(meta.height, 700);
  }

  #[test]
  fn gif_animation_time_selects_frame_with_looping() {
    fn insert_netscape_loop_extension(bytes: &mut Vec<u8>, loop_count: u16) {
      // Insert the extension immediately after the global color table.
      if bytes.len() < 13 {
        return;
      }
      let packed = bytes[10];
      let mut offset = 13usize;
      if packed & 0x80 != 0 {
        let table_bits = (packed & 0x07) as usize;
        let entries = 1usize << (table_bits + 1);
        offset = offset.saturating_add(3usize.saturating_mul(entries));
      }
      if offset > bytes.len() {
        return;
      }

      let extension = [
        0x21,
        0xFF,
        0x0B,
        b'N',
        b'E',
        b'T',
        b'S',
        b'C',
        b'A',
        b'P',
        b'E',
        b'2',
        b'.',
        b'0',
        0x03,
        0x01,
        (loop_count & 0xFF) as u8,
        (loop_count >> 8) as u8,
        0x00,
      ];
      bytes.splice(offset..offset, extension);
    }

    let mut bytes = Vec::new();
    {
      let red = RgbaImage::from_pixel(1, 1, ImageRgba([255, 0, 0, 255]));
      let blue = RgbaImage::from_pixel(1, 1, ImageRgba([0, 0, 255, 255]));
      let delay = Delay::from_numer_denom_ms(100, 1);

      let mut encoder = GifEncoder::new(&mut bytes);
      encoder
        .encode_frame(Frame::from_parts(red, 0, 0, delay))
        .expect("encode gif frame 0");
      encoder
        .encode_frame(Frame::from_parts(blue, 0, 0, delay))
        .expect("encode gif frame 1");
    }
    insert_netscape_loop_extension(&mut bytes, 0);

    let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
    let url = format!("data:image/gif;base64,{encoded}");

    let mut cache = ImageCache::new();

    cache.set_animation_time_ms(Some(0.0));
    let img0 = cache.load(&url).expect("gif frame at t=0 should decode");
    assert_eq!(img0.image.to_rgba8().get_pixel(0, 0).0, [255, 0, 0, 255]);

    cache.set_animation_time_ms(Some(150.0));
    let img1 = cache.load(&url).expect("gif frame at t=150 should decode");
    assert_eq!(img1.image.to_rgba8().get_pixel(0, 0).0, [0, 0, 255, 255]);

    cache.set_animation_time_ms(Some(250.0));
    let img2 = cache.load(&url).expect("gif frame at t=250 should decode");
    assert_eq!(img2.image.to_rgba8().get_pixel(0, 0).0, [255, 0, 0, 255]);
  }

  #[test]
  fn gif_animation_time_cache_reuses_frames_by_index() {
    fn insert_netscape_loop_extension(bytes: &mut Vec<u8>, loop_count: u16) {
      // Insert the extension immediately after the global color table.
      if bytes.len() < 13 {
        return;
      }
      let packed = bytes[10];
      let mut offset = 13usize;
      if packed & 0x80 != 0 {
        let table_bits = (packed & 0x07) as usize;
        let entries = 1usize << (table_bits + 1);
        offset = offset.saturating_add(3usize.saturating_mul(entries));
      }
      if offset > bytes.len() {
        return;
      }

      let extension = [
        0x21,
        0xFF,
        0x0B,
        b'N',
        b'E',
        b'T',
        b'S',
        b'C',
        b'A',
        b'P',
        b'E',
        b'2',
        b'.',
        b'0',
        0x03,
        0x01,
        (loop_count & 0xFF) as u8,
        (loop_count >> 8) as u8,
        0x00,
      ];
      bytes.splice(offset..offset, extension);
    }
    let mut bytes = Vec::new();
    {
      let red = RgbaImage::from_pixel(1, 1, ImageRgba([255, 0, 0, 255]));
      let blue = RgbaImage::from_pixel(1, 1, ImageRgba([0, 0, 255, 255]));
      let delay = Delay::from_numer_denom_ms(100, 1);

      let mut encoder = GifEncoder::new(&mut bytes);
      encoder
        .encode_frame(Frame::from_parts(red, 0, 0, delay))
        .expect("encode gif frame 0");
      encoder
        .encode_frame(Frame::from_parts(blue, 0, 0, delay))
        .expect("encode gif frame 1");
    }
    insert_netscape_loop_extension(&mut bytes, 0);

    let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
    let url = format!("data:image/gif;base64,{encoded}");

    let mut cache = ImageCache::new();

    cache.set_animation_time_ms(Some(0.0));
    let img0 = cache.load(&url).expect("gif frame at t=0 should decode");
    assert_eq!(img0.image.to_rgba8().get_pixel(0, 0).0, [255, 0, 0, 255]);
    let len_after_first = cache
      .cache
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .len();

    // Different times within the same frame interval should map to the same cache entry.
    cache.set_animation_time_ms(Some(50.0));
    let img0b = cache.load(&url).expect("gif frame at t=50 should decode");
    assert_eq!(img0b.image.to_rgba8().get_pixel(0, 0).0, [255, 0, 0, 255]);
    assert!(
      Arc::ptr_eq(&img0, &img0b),
      "expected frame reuse for same selected frame index"
    );
    let len_after_second = cache
      .cache
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .len();
    assert_eq!(len_after_second, len_after_first);

    // Times that select a different frame should produce a separate cached image.
    cache.set_animation_time_ms(Some(150.0));
    let img1 = cache.load(&url).expect("gif frame at t=150 should decode");
    assert_eq!(img1.image.to_rgba8().get_pixel(0, 0).0, [0, 0, 255, 255]);
    assert!(
      !Arc::ptr_eq(&img0, &img1),
      "expected different cached image for different selected frame index"
    );
    let len_after_third = cache
      .cache
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .len();
    assert_eq!(len_after_third, len_after_first + 1);
  }

  #[test]
  fn probe_metadata_detects_animated_gif() {
    let mut bytes = Vec::new();
    {
      let red = RgbaImage::from_pixel(1, 1, ImageRgba([255, 0, 0, 255]));
      let blue = RgbaImage::from_pixel(1, 1, ImageRgba([0, 0, 255, 255]));
      let delay = Delay::from_numer_denom_ms(100, 1);

      let mut encoder = GifEncoder::new(&mut bytes);
      encoder
        .encode_frame(Frame::from_parts(red, 0, 0, delay))
        .expect("encode gif frame 0");
      encoder
        .encode_frame(Frame::from_parts(blue, 0, 0, delay))
        .expect("encode gif frame 1");
    }

    let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
    let url = format!("data:image/gif;base64,{encoded}");

    let cache = ImageCache::new();
    let meta = cache.probe(&url).expect("probe animated gif");
    assert!(
      meta.is_animated,
      "expected GIF probe to detect multiple frames"
    );
  }

  #[test]
  fn probe_after_load_detects_animated_gif() {
    let mut bytes = Vec::new();
    {
      let red = RgbaImage::from_pixel(1, 1, ImageRgba([255, 0, 0, 255]));
      let blue = RgbaImage::from_pixel(1, 1, ImageRgba([0, 0, 255, 255]));
      let delay = Delay::from_numer_denom_ms(100, 1);

      let mut encoder = GifEncoder::new(&mut bytes);
      encoder
        .encode_frame(Frame::from_parts(red, 0, 0, delay))
        .expect("encode gif frame 0");
      encoder
        .encode_frame(Frame::from_parts(blue, 0, 0, delay))
        .expect("encode gif frame 1");
    }

    let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
    let url = format!("data:image/gif;base64,{encoded}");

    let cache = ImageCache::new();
    cache.load(&url).expect("load animated gif");
    let meta = cache.probe(&url).expect("probe animated gif after load");
    assert!(meta.is_animated);
  }

  #[test]
  fn probe_metadata_non_gif_is_not_animated() {
    let cache = ImageCache::new();

    let png = RgbaImage::from_pixel(1, 1, ImageRgba([0, 255, 0, 255]));
    let mut png_bytes = Vec::new();
    PngEncoder::new(&mut png_bytes)
      .write_image(png.as_raw(), 1, 1, ColorType::Rgba8)
      .expect("encode png");
    let png_url = format!(
      "data:image/png;base64,{}",
      base64::engine::general_purpose::STANDARD.encode(&png_bytes)
    );
    let png_meta = cache.probe(&png_url).expect("probe png");
    assert!(!png_meta.is_animated);

    let rgb = [0u8, 0, 0];
    let mut jpeg_bytes = Vec::new();
    JpegEncoder::new(&mut jpeg_bytes)
      .write_image(&rgb, 1, 1, ColorType::Rgb8.into())
      .expect("encode jpeg");
    let jpeg_url = format!(
      "data:image/jpeg;base64,{}",
      base64::engine::general_purpose::STANDARD.encode(&jpeg_bytes)
    );
    let jpeg_meta = cache.probe(&jpeg_url).expect("probe jpeg");
    assert!(!jpeg_meta.is_animated);
  }

  fn svg_policy_cache_same_origin_only(doc_url: &str) -> ImageCache {
    let doc_origin = origin_from_url(doc_url).expect("document origin");
    let mut cache = ImageCache::new();
    let mut ctx = ResourceContext::default();
    ctx.document_url = Some(doc_url.to_string());
    ctx.policy.document_origin = Some(doc_origin);
    ctx.policy.same_origin_only = true;
    cache.set_resource_context(Some(ctx));
    cache
  }

  #[test]
  fn svg_policy_ignores_display_none_image_src() {
    use crate::html::content_security_policy::CspPolicy;

    let doc_url = "https://doc.test/";
    let doc_origin = origin_from_url(doc_url).expect("document origin");

    let mut cache = ImageCache::new();
    let mut ctx = ResourceContext::default();
    ctx.document_url = Some(doc_url.to_string());
    ctx.policy.document_origin = Some(doc_origin);
    ctx.policy.same_origin_only = false;
    ctx.csp = CspPolicy::from_values(["img-src 'self'"]);
    assert!(ctx.csp.is_some(), "CSP should parse");
    cache.set_resource_context(Some(ctx));

    // Real-world SVGs (notably embedded in HTML) sometimes include `<image src="...">` elements
    // guarded by legacy IE hacks like `display:none \\9`. Modern parsers treat that as non-displayed
    // content, so policy enforcement should not fail the entire SVG document.
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg"><image src="https://cross.test/a.png" style="border:none;display:none \9" width="10" height="10"/></svg>"#;
    cache
      .probe_svg_content(svg, "inline-svg")
      .expect("hidden external <image src> should not trigger CSP/policy checks");
  }

  #[test]
  fn svg_policy_blocks_external_href() {
    let cache = svg_policy_cache_same_origin_only("https://doc.test/");
    let svg =
      r#"<svg xmlns="http://www.w3.org/2000/svg"><image href="https://cross.test/a.png"/></svg>"#;
    let err = cache
      .probe_svg_content(svg, "https://doc.test/icon.svg")
      .expect_err("expected SVG subresource policy failure");
    match err {
      Error::Image(ImageError::LoadFailed { url, reason }) => {
        assert_eq!(url, "https://cross.test/a.png");
        assert!(
          reason.contains("Blocked cross-origin subresource"),
          "{reason}"
        );
      }
      other => panic!("expected ImageError::LoadFailed, got {other:?}"),
    }
  }

  #[test]
  fn svg_policy_blocks_external_css_url() {
    let cache = svg_policy_cache_same_origin_only("https://doc.test/");
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg"><style>rect{fill:url(https://cross.test/a.png)}</style><rect width="10" height="10"/></svg>"#;
    let err = cache
      .probe_svg_content(svg, "https://doc.test/icon.svg")
      .expect_err("expected SVG CSS subresource policy failure");
    match err {
      Error::Image(ImageError::LoadFailed { url, reason }) => {
        assert_eq!(url, "https://cross.test/a.png");
        assert!(
          reason.contains("Blocked cross-origin subresource"),
          "{reason}"
        );
      }
      other => panic!("expected ImageError::LoadFailed, got {other:?}"),
    }
  }

  #[test]
  fn svg_policy_blocks_external_css_import_by_csp() {
    use crate::html::content_security_policy::CspPolicy;

    let doc_url = "https://doc.test/";
    let doc_origin = origin_from_url(doc_url).expect("document origin");

    // Allow cross-origin network loads by default, but use CSP to block cross-origin stylesheets.
    let mut cache = ImageCache::new();
    let mut ctx = ResourceContext::default();
    ctx.document_url = Some(doc_url.to_string());
    ctx.policy.document_origin = Some(doc_origin);
    ctx.policy.same_origin_only = false;
    ctx.csp = CspPolicy::from_values(["style-src 'self'; img-src *"]);
    assert!(ctx.csp.is_some(), "CSP should parse");
    cache.set_resource_context(Some(ctx));

    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg"><style>@import url(https://cross.test/a.css); rect{fill:red}</style><rect width="10" height="10"/></svg>"#;
    let err = cache
      .probe_svg_content(svg, "https://doc.test/icon.svg")
      .expect_err("expected SVG CSS @import to be blocked by CSP style-src");

    match err {
      Error::Image(ImageError::LoadFailed { url, reason }) => {
        assert_eq!(url, "https://cross.test/a.css");
        assert!(
          reason.contains("Content-Security-Policy") && reason.contains("style-src"),
          "{reason}"
        );
      }
      other => panic!("expected ImageError::LoadFailed, got {other:?}"),
    }
  }

  #[test]
  fn svg_policy_blocks_external_font_face_by_csp_font_src() {
    use crate::html::content_security_policy::CspPolicy;

    let doc_url = "https://doc.test/";
    let doc_origin = origin_from_url(doc_url).expect("document origin");

    // Allow cross-origin network loads by default, but use CSP to block cross-origin fonts loaded
    // through SVG-embedded CSS `@font-face`.
    let mut cache = ImageCache::new();
    let mut ctx = ResourceContext::default();
    ctx.document_url = Some(doc_url.to_string());
    ctx.policy.document_origin = Some(doc_origin);
    ctx.policy.same_origin_only = false;
    ctx.csp = CspPolicy::from_values(["font-src 'self'; img-src *; style-src *"]);
    assert!(ctx.csp.is_some(), "CSP should parse");
    cache.set_resource_context(Some(ctx));

    let svg = r#"
      <svg xmlns="http://www.w3.org/2000/svg">
        <style>
          @font-face { font-family: X; src: url(https://cross.test/a.woff2); }
          text { font-family: X; }
        </style>
        <text x="0" y="10">A</text>
      </svg>
    "#;

    let err = cache
      .probe_svg_content(svg, "https://doc.test/icon.svg")
      .expect_err("expected SVG CSS @font-face to be blocked by CSP font-src");

    match err {
      Error::Image(ImageError::LoadFailed { url, reason }) => {
        assert_eq!(url, "https://cross.test/a.woff2");
        assert!(
          reason.contains("Content-Security-Policy") && reason.contains("font-src"),
          "{reason}"
        );
      }
      other => panic!("expected ImageError::LoadFailed, got {other:?}"),
    }
  }

  #[test]
  fn svg_policy_blocks_external_url_in_fill_attribute() {
    let cache = svg_policy_cache_same_origin_only("https://doc.test/");
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg"><rect fill="url(https://cross.test/a.png)" width="10" height="10"/></svg>"#;
    let err = cache
      .probe_svg_content(svg, "https://doc.test/icon.svg")
      .expect_err("expected SVG attribute url() subresource policy failure");
    match err {
      Error::Image(ImageError::LoadFailed { url, reason }) => {
        assert_eq!(url, "https://cross.test/a.png");
        assert!(
          reason.contains("Blocked cross-origin subresource"),
          "{reason}"
        );
      }
      other => panic!("expected ImageError::LoadFailed, got {other:?}"),
    }
  }

  #[test]
  fn svg_policy_blocks_external_url_in_filter_attribute() {
    let cache = svg_policy_cache_same_origin_only("https://doc.test/");
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg"><rect filter="url(https://cross.test/f.svg#f)" width="10" height="10"/></svg>"#;
    let err = cache
      .probe_svg_content(svg, "https://doc.test/icon.svg")
      .expect_err("expected SVG attribute url() subresource policy failure");
    match err {
      Error::Image(ImageError::LoadFailed { url, reason }) => {
        assert_eq!(url, "https://cross.test/f.svg#f");
        assert!(
          reason.contains("Blocked cross-origin subresource"),
          "{reason}"
        );
      }
      other => panic!("expected ImageError::LoadFailed, got {other:?}"),
    }
  }

  #[test]
  fn svg_policy_blocks_scheme_relative_href_in_inline_svg() {
    let cache = svg_policy_cache_same_origin_only("https://doc.test/");
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg"><image href="//cross.test/a.png"/></svg>"#;
    let err = cache
      .probe_svg_content(svg, "inline-svg")
      .expect_err("expected SVG subresource policy failure");
    match err {
      Error::Image(ImageError::LoadFailed { url, reason }) => {
        assert_eq!(url, "https://cross.test/a.png");
        assert!(
          reason.contains("Blocked cross-origin subresource"),
          "{reason}"
        );
      }
      other => panic!("expected ImageError::LoadFailed, got {other:?}"),
    }
  }

  #[test]
  fn svg_policy_blocks_scheme_relative_css_url_in_inline_svg() {
    let cache = svg_policy_cache_same_origin_only("https://doc.test/");
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg"><style>rect{fill:url(//cross.test/a.png)}</style><rect width="10" height="10"/></svg>"#;
    let err = cache
      .probe_svg_content(svg, "inline-svg")
      .expect_err("expected SVG CSS subresource policy failure");
    match err {
      Error::Image(ImageError::LoadFailed { url, reason }) => {
        assert_eq!(url, "https://cross.test/a.png");
        assert!(
          reason.contains("Blocked cross-origin subresource"),
          "{reason}"
        );
      }
      other => panic!("expected ImageError::LoadFailed, got {other:?}"),
    }
  }

  #[test]
  fn svg_policy_ignores_fragment_only_url_in_attributes() {
    let cache = svg_policy_cache_same_origin_only("https://doc.test/");
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg"><rect fill="url(#grad)" width="10" height="10"/></svg>"#;
    cache
      .probe_svg_content(svg, "https://doc.test/icon.svg")
      .expect("expected fragment-only url() references to be ignored");
  }

  #[test]
  fn svg_policy_allows_data_urls_and_fragments() {
    let cache = svg_policy_cache_same_origin_only("https://doc.test/");
    let svg = r##"<svg xmlns="http://www.w3.org/2000/svg"><defs><g id="id"/></defs><use href="#id"/><image href="data:image/png;base64,AAAA"/></svg>"##;
    cache
      .probe_svg_content(svg, "https://doc.test/icon.svg")
      .expect("expected data URLs and fragment-only hrefs to be ignored");
  }

  #[test]
  fn svg_policy_allows_large_non_css_attribute_values() {
    let cache = svg_policy_cache_same_origin_only("https://doc.test/");

    // Large non-CSS attribute values (e.g. huge path data) should not consume the embedded CSS scan
    // budget when they cannot contain `url(...)` references.
    let d = "M".repeat(600 * 1024);
    assert!(
      d.len() > 512 * 1024,
      "expected test path data to exceed scan budget"
    );
    let svg = format!(
      r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><path d="{d}"/></svg>"#
    );

    cache
      .probe_svg_content(&svg, "https://doc.test/icon.svg")
      .expect("expected large non-CSS attribute values to be allowed");
  }

  #[test]
  fn image_cache_diagnostics_survives_poisoned_lock() {
    let result = std::panic::catch_unwind(|| {
      let _guard = IMAGE_CACHE_DIAGNOSTICS.lock().unwrap();
      panic!("poison image cache diagnostics lock");
    });
    assert!(result.is_err(), "expected panic to be caught");

    assert!(
      IMAGE_CACHE_DIAGNOSTICS.is_poisoned(),
      "expected image cache diagnostics mutex to be poisoned"
    );

    enable_image_cache_diagnostics();
    record_image_cache_request();
    let stats = take_image_cache_diagnostics().expect("diagnostics enabled");
    assert_eq!(stats.requests, 1);
  }

  #[test]
  fn decode_inflight_recovers_from_poisoned_lock() {
    let inflight = DecodeInFlight::new();
    let result = std::panic::catch_unwind(|| {
      let _guard = inflight.result.lock().unwrap();
      panic!("poison decode inflight lock");
    });
    assert!(result.is_err(), "expected panic to be caught");

    let deadline = RenderDeadline::new(Some(Duration::from_millis(50)), None);
    render_control::with_deadline(Some(&deadline), || {
      inflight.set(SharedImageResult::Error(Error::Render(
        RenderError::Timeout {
          stage: RenderStage::Paint,
          elapsed: Duration::from_millis(0),
        },
      )));
      let err = match inflight.wait("https://example.com/image.png") {
        Ok(_) => panic!("expected error result"),
        Err(err) => err,
      };
      assert!(matches!(err, Error::Render(RenderError::Timeout { .. })));
    });
  }

  #[test]
  fn probe_inflight_recovers_from_poisoned_lock() {
    let inflight = ProbeInFlight::new();
    let result = std::panic::catch_unwind(|| {
      let _guard = inflight.result.lock().unwrap();
      panic!("poison probe inflight lock");
    });
    assert!(result.is_err(), "expected panic to be caught");

    let deadline = RenderDeadline::new(Some(Duration::from_millis(50)), None);
    render_control::with_deadline(Some(&deadline), || {
      inflight.set(SharedMetaResult::Error(Error::Render(
        RenderError::Timeout {
          stage: RenderStage::Paint,
          elapsed: Duration::from_millis(0),
        },
      )));
      let err = match inflight.wait("https://example.com/image.png") {
        Ok(_) => panic!("expected error result"),
        Err(err) => err,
      };
      assert!(matches!(err, Error::Render(RenderError::Timeout { .. })));
    });
  }

  #[cfg(feature = "direct_network")]
  #[test]
  fn image_decode_uses_root_deadline_instead_of_nested_budget() {
    // The paint pipeline installs nested deadlines to allocate small time budgets to internal
    // phases (e.g. display-list construction). Image decoding can be triggered inside those
    // phases, but should still be bounded by the overall render timeout (root deadline) rather
    // than the internal sub-budget.
    let root = RenderDeadline::new(Some(Duration::from_secs(5)), None);
    render_control::with_deadline(Some(&root), || {
      // A nested deadline that is immediately expired should not prevent file:// image decoding.
      let nested = RenderDeadline::new(Some(Duration::ZERO), None);
      render_control::with_deadline(Some(&nested), || {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
          .join("tests/fixtures/accuracy_png/baseline.png");
        let url = Url::from_file_path(path)
          .expect("baseline.png file URL")
          .to_string();
        let cache = ImageCache::new();
        let img = cache
          .load(&url)
          .expect("image should load under root deadline");
        assert!(
          !cache.is_placeholder_image(&img),
          "loaded image should not be the about: placeholder"
        );
      });
    });
  }

  #[test]
  fn img_crossorigin_enforces_acao_when_toggle_enabled() {
    let _toggles_guard = runtime::set_runtime_toggles(Arc::new(runtime::RuntimeToggles::from_map(
      HashMap::from([("FASTR_FETCH_ENFORCE_CORS".to_string(), "1".to_string())]),
    )));

    let doc_url = "https://doc.test/";
    let doc_origin = crate::resource::origin_from_url(doc_url).expect("document origin");
    // `Access-Control-Allow-Origin` is compared byte-for-byte against the serialized request origin
    // used in the `Origin` header. That serialization omits default ports (e.g. `:443` for HTTPS),
    // so avoid using `DocumentOrigin`'s Display impl here (which includes the effective port).
    let doc_origin_str = {
      let scheme = doc_origin.scheme().to_ascii_lowercase();
      match doc_origin.host() {
        Some(host) => {
          let host = host.to_ascii_lowercase();
          let host = if host.contains(':') && !host.starts_with('[') {
            format!("[{host}]")
          } else {
            host
          };
          let port = match (scheme.as_str(), doc_origin.port()) {
            ("http", Some(80)) | ("https", Some(443)) => None,
            (_, Some(port)) => Some(port),
            _ => None,
          };
          match port {
            Some(port) => format!("{scheme}://{host}:{port}"),
            None => format!("{scheme}://{host}"),
          }
        }
        None => "null".to_string(),
      }
    };

    let mut pixels = RgbaImage::new(1, 1);
    pixels
      .pixels_mut()
      .for_each(|p| *p = image::Rgba([255, 0, 0, 255]));
    let mut png = Vec::new();
    PngEncoder::new(&mut png)
      .write_image(pixels.as_raw(), 1, 1, ColorType::Rgba8.into())
      .expect("encode png");

    let cross_no_acao = "https://img.test/no-acao.png";
    let same_no_acao = "https://doc.test/same.png";
    let cross_star = "https://img.test/star.png";
    let cred_missing = "https://img.test/cred-missing.png";
    let cred_ok = "https://img.test/cred-ok.png";

    let make_res = |url: &str, acao: Option<&str>, acac: bool| {
      let mut res = FetchedResource::new(png.clone(), Some("image/png".to_string()));
      res.status = Some(200);
      res.final_url = Some(url.to_string());
      res.access_control_allow_origin = acao.map(|v| v.to_string());
      res.access_control_allow_credentials = acac;
      res
    };

    let fetcher = MapFetcher::with_entries([
      (
        cross_no_acao.to_string(),
        make_res(cross_no_acao, None, false),
      ),
      (
        same_no_acao.to_string(),
        make_res(same_no_acao, None, false),
      ),
      (
        cross_star.to_string(),
        make_res(cross_star, Some("*"), false),
      ),
      (
        cred_missing.to_string(),
        make_res(cred_missing, Some(doc_origin_str.as_str()), false),
      ),
      (
        cred_ok.to_string(),
        make_res(cred_ok, Some(doc_origin_str.as_str()), true),
      ),
    ]);

    let mut cache = ImageCache::with_fetcher(Arc::new(fetcher.clone()));
    let mut ctx = ResourceContext::default();
    ctx.document_url = Some(doc_url.to_string());
    ctx.policy.document_origin = Some(doc_origin);
    cache.set_resource_context(Some(ctx));

    // Baseline: cross-origin image without `crossorigin` should still load (no CORS enforcement).
    cache
      .load_with_crossorigin(cross_no_acao, CrossOriginAttribute::None)
      .expect("no-cors image should load");

    // CORS-mode fetch should fail without ACAO.
    let err = match cache.load_with_crossorigin(cross_no_acao, CrossOriginAttribute::Anonymous) {
      Ok(_) => panic!("crossorigin=anonymous should enforce ACAO"),
      Err(err) => err,
    };
    match err {
      Error::Image(ImageError::LoadFailed { reason, .. }) => {
        assert!(reason.contains("Access-Control-Allow-Origin"));
      }
      other => panic!("expected CORS image load failure, got {other:?}"),
    }

    // Same-origin images should always pass CORS checks.
    cache
      .load_with_crossorigin(same_no_acao, CrossOriginAttribute::Anonymous)
      .expect("same-origin image should pass CORS enforcement");

    // Anonymous mode accepts `*`.
    cache
      .load_with_crossorigin(cross_star, CrossOriginAttribute::Anonymous)
      .expect("crossorigin=anonymous with ACAO=* should load");

    // Credentialed mode requires exact origin match and ACAC=true.
    let err = match cache.load_with_crossorigin(cred_missing, CrossOriginAttribute::UseCredentials)
    {
      Ok(_) => panic!("credentialed CORS without ACAC should fail"),
      Err(err) => err,
    };
    match err {
      Error::Image(ImageError::LoadFailed { reason, .. }) => {
        assert!(reason.contains("Access-Control-Allow-Credentials"));
      }
      other => panic!("expected credentialed CORS failure, got {other:?}"),
    }
    cache
      .load_with_crossorigin(cred_ok, CrossOriginAttribute::UseCredentials)
      .expect("credentialed CORS with ACAO match + ACAC should load");

    let requests = fetcher.requests();
    assert_eq!(
      requests
        .iter()
        .filter(|(url, _, _)| url == cross_no_acao)
        .map(|(_, dest, _)| *dest)
        .collect::<Vec<_>>(),
      vec![FetchDestination::Image, FetchDestination::ImageCors],
      "no-cors and cors-mode loads must not share the same fetch profile"
    );
    assert!(
      requests
        .iter()
        .any(|(url, dest, _)| url == same_no_acao && *dest == FetchDestination::ImageCors),
      "same-origin crossorigin images should still use ImageCors destination"
    );
    assert!(
      requests.iter().any(|(url, dest, credentials)| {
        url == cross_star
          && *dest == FetchDestination::ImageCors
          && *credentials == FetchCredentialsMode::SameOrigin
      }),
      "crossorigin=anonymous requests must use SameOrigin credentials mode"
    );
  }

  #[test]
  fn img_crossorigin_anonymous_uses_same_origin_credentials_mode() {
    #[derive(Clone, Debug)]
    struct ExpectedRequest {
      url: String,
      destination: FetchDestination,
      credentials_mode: FetchCredentialsMode,
    }

    #[derive(Clone)]
    struct CredentialsModeFetcher {
      expected: Arc<Mutex<Vec<ExpectedRequest>>>,
      png: Arc<Vec<u8>>,
    }

    impl ResourceFetcher for CredentialsModeFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        panic!("expected ImageCache load to use fetch_with_request");
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        let expected = self
          .expected
          .lock()
          .unwrap_or_else(|poisoned| poisoned.into_inner())
          .pop()
          .expect("unexpected fetch_with_request call");
        assert_eq!(req.url, expected.url);
        assert_eq!(req.destination, expected.destination);
        assert_eq!(req.credentials_mode, expected.credentials_mode);
        let mut res = FetchedResource::new((*self.png).clone(), Some("image/png".to_string()));
        res.status = Some(200);
        res.final_url = Some(req.url.to_string());
        // Ensure this test remains stable even when `FASTR_FETCH_ENFORCE_CORS` is enabled globally
        // by other concurrent tests.
        res.access_control_allow_origin = Some("*".to_string());
        Ok(res)
      }
    }

    let doc_url = "https://example.com/doc.html";
    let same_origin_url = "https://example.com/same.png";
    let cross_origin_url = "https://cross.test/cross.png";
    let no_cors_url = "https://example.com/no-cors.png";

    let png = encode_single_pixel_png([255, 0, 0, 255]);

    // Use a stack so we can `pop()` in call order.
    let expected = vec![
      ExpectedRequest {
        url: no_cors_url.to_string(),
        destination: FetchDestination::Image,
        credentials_mode: FetchCredentialsMode::Include,
      },
      ExpectedRequest {
        url: cross_origin_url.to_string(),
        destination: FetchDestination::ImageCors,
        credentials_mode: FetchCredentialsMode::SameOrigin,
      },
      ExpectedRequest {
        url: same_origin_url.to_string(),
        destination: FetchDestination::ImageCors,
        credentials_mode: FetchCredentialsMode::SameOrigin,
      },
    ];

    let fetcher = CredentialsModeFetcher {
      expected: Arc::new(Mutex::new(expected)),
      png: Arc::new(png),
    };

    let mut cache = ImageCache::with_fetcher(Arc::new(fetcher.clone()));
    let mut ctx = ResourceContext::default();
    ctx.document_url = Some(doc_url.to_string());
    ctx.policy.document_origin = crate::resource::origin_from_url(doc_url);
    cache.set_resource_context(Some(ctx));

    cache
      .load_with_crossorigin(same_origin_url, CrossOriginAttribute::Anonymous)
      .expect("same-origin crossorigin=anonymous should load");
    cache
      .load_with_crossorigin(cross_origin_url, CrossOriginAttribute::Anonymous)
      .expect("cross-origin crossorigin=anonymous should load");
    cache
      .load_with_crossorigin(no_cors_url, CrossOriginAttribute::None)
      .expect("no-cors image should load");

    assert!(
      fetcher
        .expected
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .is_empty(),
      "expected all fetch_with_request expectations to be consumed"
    );
  }

  #[test]
  fn probe_metadata_artifacts_are_partitioned_by_client_origin_for_cors_images() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone, Debug)]
    struct StoredArtifact {
      bytes: Vec<u8>,
      access_control_allow_origin: Option<String>,
      access_control_allow_credentials: bool,
      final_url: Option<String>,
    }

    #[derive(Clone, Default)]
    struct ArtifactFetcher {
      calls: Arc<AtomicUsize>,
      png: Arc<Vec<u8>>,
      artifacts: Arc<Mutex<HashMap<(String, Option<String>), StoredArtifact>>>,
    }

    impl ArtifactFetcher {
      fn origin_key_from_client_origin(
        origin: Option<&crate::resource::DocumentOrigin>,
      ) -> Option<String> {
        let origin = origin?;
        if !origin.is_http_like() {
          return Some("null".to_string());
        }
        let host = origin.host()?;
        let mut origin_str = format!("{}://{}", origin.scheme(), host);
        if let Some(port) = origin.port() {
          let default_port = match origin.scheme() {
            "http" => 80,
            "https" => 443,
            _ => port,
          };
          if port != default_port {
            origin_str.push_str(&format!(":{port}"));
          }
        }
        Some(origin_str)
      }
    }

    impl ResourceFetcher for ArtifactFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        panic!("expected ImageCache probe to use fetch_with_request/fetch_partial_with_request");
      }

      fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
        self.fetch_partial_with_request(req, usize::MAX)
      }

      fn fetch_partial_with_request(
        &self,
        req: FetchRequest<'_>,
        max_bytes: usize,
      ) -> Result<FetchedResource> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(
          req.destination,
          FetchDestination::ImageCors,
          "expected probe to use ImageCors destination"
        );
        let origin = Self::origin_key_from_client_origin(req.client_origin)
          .expect("expected client origin to be provided for CORS-mode image probe");
        let mut bytes = (*self.png).clone();
        if bytes.len() > max_bytes {
          bytes.truncate(max_bytes);
        }
        let mut res = FetchedResource::new(bytes, Some("image/png".to_string()));
        res.status = Some(200);
        res.final_url = Some(req.url.to_string());
        res.access_control_allow_origin = Some(origin);
        Ok(res)
      }

      fn read_cache_artifact(
        &self,
        _kind: FetchContextKind,
        _url: &str,
        _artifact: CacheArtifactKind,
      ) -> Option<FetchedResource> {
        panic!("expected ImageCache probe to call read_cache_artifact_with_request");
      }

      fn write_cache_artifact(
        &self,
        _kind: FetchContextKind,
        _url: &str,
        _artifact: CacheArtifactKind,
        _bytes: &[u8],
        _source: Option<&FetchedResource>,
      ) {
        panic!("expected ImageCache probe to call write_cache_artifact_with_request");
      }

      fn remove_cache_artifact(
        &self,
        _kind: FetchContextKind,
        _url: &str,
        _artifact: CacheArtifactKind,
      ) {
        panic!("expected ImageCache probe to call remove_cache_artifact_with_request");
      }

      fn read_cache_artifact_with_request(
        &self,
        req: FetchRequest<'_>,
        artifact: CacheArtifactKind,
      ) -> Option<FetchedResource> {
        assert_eq!(artifact, CacheArtifactKind::ImageProbeMetadata);
        let key = (
          req.url.to_string(),
          Self::origin_key_from_client_origin(req.client_origin),
        );
        let stored = self
          .artifacts
          .lock()
          .unwrap_or_else(|poisoned| poisoned.into_inner())
          .get(&key)
          .cloned()?;
        let mut res = FetchedResource::new(stored.bytes, None);
        res.status = Some(200);
        res.final_url = stored.final_url;
        res.access_control_allow_origin = stored.access_control_allow_origin;
        res.access_control_allow_credentials = stored.access_control_allow_credentials;
        Some(res)
      }

      fn write_cache_artifact_with_request(
        &self,
        req: FetchRequest<'_>,
        artifact: CacheArtifactKind,
        bytes: &[u8],
        source: Option<&FetchedResource>,
      ) {
        assert_eq!(artifact, CacheArtifactKind::ImageProbeMetadata);
        let key = (
          req.url.to_string(),
          Self::origin_key_from_client_origin(req.client_origin),
        );
        let stored = StoredArtifact {
          bytes: bytes.to_vec(),
          access_control_allow_origin: source.and_then(|s| s.access_control_allow_origin.clone()),
          access_control_allow_credentials: source
            .map(|s| s.access_control_allow_credentials)
            .unwrap_or(false),
          final_url: source
            .and_then(|s| s.final_url.clone())
            .or_else(|| Some(req.url.to_string())),
        };
        self
          .artifacts
          .lock()
          .unwrap_or_else(|poisoned| poisoned.into_inner())
          .insert(key, stored);
      }

      fn remove_cache_artifact_with_request(
        &self,
        req: FetchRequest<'_>,
        artifact: CacheArtifactKind,
      ) {
        assert_eq!(artifact, CacheArtifactKind::ImageProbeMetadata);
        let key = (
          req.url.to_string(),
          Self::origin_key_from_client_origin(req.client_origin),
        );
        self
          .artifacts
          .lock()
          .unwrap_or_else(|poisoned| poisoned.into_inner())
          .remove(&key);
      }
    }

    let toggles = Arc::new(runtime::RuntimeToggles::from_map(HashMap::from([(
      "FASTR_FETCH_ENFORCE_CORS".to_string(),
      "1".to_string(),
    )])));
    runtime::with_thread_runtime_toggles(toggles, || {
      let mut pixels = RgbaImage::new(1, 1);
      pixels
        .pixels_mut()
        .for_each(|p| *p = image::Rgba([255, 0, 0, 255]));
      let mut png = Vec::new();
      PngEncoder::new(&mut png)
        .write_image(pixels.as_raw(), 1, 1, ColorType::Rgba8.into())
        .expect("encode png");

      let fetcher = ArtifactFetcher {
        calls: Arc::new(AtomicUsize::new(0)),
        png: Arc::new(png),
        artifacts: Arc::new(Mutex::new(HashMap::new())),
      };
      let url = "https://img.test/probe.png";

      let mut cache = ImageCache::with_fetcher(Arc::new(fetcher.clone()));

      for doc_url in ["http://a.test/page", "http://b.test/page"] {
        let mut ctx = ResourceContext::default();
        ctx.document_url = Some(doc_url.to_string());
        ctx.policy.document_origin = crate::resource::origin_from_url(doc_url);
        cache.set_resource_context(Some(ctx));
        cache
          .probe_resolved_with_crossorigin(url, CrossOriginAttribute::Anonymous)
          .unwrap_or_else(|err| panic!("probe should succeed for {doc_url}: {err:?}"));
      }

      assert_eq!(
        fetcher.calls.load(Ordering::SeqCst),
        2,
        "expected first-time probes for distinct document origins to fetch separately"
      );

      // A new cache instance should be able to reuse the persisted artifacts. Each origin should be
      // resolved independently without triggering more network fetches.
      let mut cache_again = ImageCache::with_fetcher(Arc::new(fetcher.clone()));
      for doc_url in ["http://a.test/page", "http://b.test/page"] {
        let mut ctx = ResourceContext::default();
        ctx.document_url = Some(doc_url.to_string());
        ctx.policy.document_origin = crate::resource::origin_from_url(doc_url);
        cache_again.set_resource_context(Some(ctx));
        cache_again
          .probe_resolved_with_crossorigin(url, CrossOriginAttribute::Anonymous)
          .unwrap_or_else(|err| panic!("probe artifact should succeed for {doc_url}: {err:?}"));
      }

      assert_eq!(
        fetcher.calls.load(Ordering::SeqCst),
        2,
        "expected probe artifacts to satisfy follow-up requests without additional fetches"
      );
    });
  }

  #[test]
  fn image_cache_partitions_by_referrer_url_for_no_cors_images() {
    let url = "https://img.test/pixel.png";
    let doc_a = "https://a.test/page";
    let doc_b = "https://b.test/page";

    let red_png = encode_single_pixel_png([255, 0, 0, 255]);
    let green_png = encode_single_pixel_png([0, 255, 0, 255]);

    let fetcher = ReferrerAwarePngFetcher::new(HashMap::from([
      (
        (Some(doc_a.to_string()), ReferrerPolicy::EmptyString),
        red_png.clone(),
      ),
      (
        (Some(doc_b.to_string()), ReferrerPolicy::EmptyString),
        green_png.clone(),
      ),
    ]));

    let mut cache = ImageCache::with_fetcher(Arc::new(fetcher.clone()));

    let mut ctx_a = ResourceContext::default();
    ctx_a.document_url = Some(doc_a.to_string());
    cache.set_resource_context(Some(ctx_a));

    let img_a = cache.load(url).expect("image load should succeed");
    let pix_a = img_a.image.as_ref().to_rgba8().get_pixel(0, 0).0;
    assert_eq!(pix_a, [255, 0, 0, 255]);
    let pixmap_a = cache
      .load_raster_pixmap(url, OrientationTransform::IDENTITY, false)
      .expect("pixmap load should succeed")
      .expect("expected raster pixmap");
    assert_eq!(&pixmap_a.data()[..4], &[255, 0, 0, 255]);

    let mut ctx_b = ResourceContext::default();
    ctx_b.document_url = Some(doc_b.to_string());
    cache.set_resource_context(Some(ctx_b));

    let img_b = cache.load(url).expect("image load should succeed");
    let pix_b = img_b.image.as_ref().to_rgba8().get_pixel(0, 0).0;
    assert_eq!(pix_b, [0, 255, 0, 255]);
    let pixmap_b = cache
      .load_raster_pixmap(url, OrientationTransform::IDENTITY, false)
      .expect("pixmap load should succeed")
      .expect("expected raster pixmap");
    assert_eq!(&pixmap_b.data()[..4], &[0, 255, 0, 255]);

    assert_eq!(
      fetcher.calls(),
      2,
      "expected referrer-partitioned image cache to fetch twice"
    );
  }

  #[test]
  fn image_cache_partitions_by_referrer_policy_for_no_cors_images() {
    let url = "https://img.test/pixel.png";
    let doc_url = "https://doc.test/page";

    let red_png = encode_single_pixel_png([255, 0, 0, 255]);
    let green_png = encode_single_pixel_png([0, 255, 0, 255]);

    let fetcher = ReferrerAwarePngFetcher::new(HashMap::from([
      (
        (Some(doc_url.to_string()), ReferrerPolicy::EmptyString),
        red_png.clone(),
      ),
      (
        (Some(doc_url.to_string()), ReferrerPolicy::NoReferrer),
        green_png.clone(),
      ),
    ]));

    let mut cache = ImageCache::with_fetcher(Arc::new(fetcher.clone()));

    let mut ctx_default = ResourceContext::default();
    ctx_default.document_url = Some(doc_url.to_string());
    cache.set_resource_context(Some(ctx_default));

    let img_default = cache.load(url).expect("image load should succeed");
    let pix_default = img_default.image.as_ref().to_rgba8().get_pixel(0, 0).0;
    assert_eq!(pix_default, [255, 0, 0, 255]);
    let pixmap_default = cache
      .load_raster_pixmap(url, OrientationTransform::IDENTITY, false)
      .expect("pixmap load should succeed")
      .expect("expected raster pixmap");
    assert_eq!(&pixmap_default.data()[..4], &[255, 0, 0, 255]);

    let mut ctx_no_referrer = ResourceContext::default();
    ctx_no_referrer.document_url = Some(doc_url.to_string());
    ctx_no_referrer.referrer_policy = ReferrerPolicy::NoReferrer;
    cache.set_resource_context(Some(ctx_no_referrer));

    let img_no_referrer = cache.load(url).expect("image load should succeed");
    let pix_no_referrer = img_no_referrer.image.as_ref().to_rgba8().get_pixel(0, 0).0;
    assert_eq!(pix_no_referrer, [0, 255, 0, 255]);
    let pixmap_no_referrer = cache
      .load_raster_pixmap(url, OrientationTransform::IDENTITY, false)
      .expect("pixmap load should succeed")
      .expect("expected raster pixmap");
    assert_eq!(&pixmap_no_referrer.data()[..4], &[0, 255, 0, 255]);

    assert_eq!(
      fetcher.calls(),
      2,
      "expected policy-partitioned image cache to fetch twice"
    );
  }

  #[cfg(feature = "direct_network")]
  #[test]
  fn http_403_image_reports_resource_error_with_status() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    let listener = match TcpListener::bind("127.0.0.1:0") {
      Ok(listener) => listener,
      Err(err)
        if matches!(
          err.kind(),
          std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::AddrNotAvailable
        ) =>
      {
        eprintln!("skipping http_403_image_reports_resource_error_with_status: cannot bind localhost: {err}");
        return;
      }
      Err(err) => panic!("bind localhost: {err}"),
    };
    let addr = listener.local_addr().expect("listener addr");
    let url = format!("http://{addr}/blocked.png");

    let server = std::thread::spawn(move || {
      let (mut stream, _) = listener.accept().expect("accept");
      let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
      let mut buf = [0u8; 1024];
      let _ = stream.read(&mut buf);

      let body = "<html>Forbidden</html>";
      let response = format!(
        "HTTP/1.1 403 Forbidden\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
      );
      stream
        .write_all(response.as_bytes())
        .expect("write response");
      let _ = stream.flush();
    });

    let diagnostics = Arc::new(Mutex::new(RenderDiagnostics::default()));
    let mut cache = ImageCache::with_fetcher(Arc::new(HttpFetcher::new()));
    cache.set_diagnostics_sink(Some(Arc::clone(&diagnostics)));

    let err = match cache.load(&url) {
      Ok(_) => panic!("image load should fail"),
      Err(err) => err,
    };
    match err {
      Error::Resource(ref res) => {
        assert_eq!(res.status, Some(403));
        assert_eq!(res.final_url.as_deref(), Some(url.as_str()));
      }
      other => panic!("expected resource error, got {other:?}"),
    }

    let diag = diagnostics.lock().unwrap().clone();
    let entry = diag
      .fetch_errors
      .iter()
      .find(|e| e.kind == ResourceKind::Image && e.url == url)
      .expect("diagnostics entry");
    assert_eq!(entry.status, Some(403));
    assert_eq!(entry.final_url.as_deref(), Some(url.as_str()));

    server.join().unwrap();
  }

  #[cfg(feature = "direct_network")]
  #[test]
  fn http_200_html_for_jpg_is_reported_as_resource_error() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    let listener = match TcpListener::bind("127.0.0.1:0") {
      Ok(listener) => listener,
      Err(err)
        if matches!(
          err.kind(),
          std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::AddrNotAvailable
        ) =>
      {
        eprintln!("skipping http_200_html_for_jpg_is_reported_as_resource_error: cannot bind localhost: {err}");
        return;
      }
      Err(err) => panic!("bind localhost: {err}"),
    };
    let addr = listener.local_addr().expect("listener addr");
    let url = format!("http://{addr}/photo.jpg");

    let server = std::thread::spawn(move || {
      let (mut stream, _) = listener.accept().expect("accept");
      let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
      let mut buf = [0u8; 1024];
      let _ = stream.read(&mut buf);

      let body = "<!doctype html><html><body>blocked</body></html>";
      let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
      );
      stream
        .write_all(response.as_bytes())
        .expect("write response");
      let _ = stream.flush();
    });

    let diagnostics = Arc::new(Mutex::new(RenderDiagnostics::default()));
    let mut cache = ImageCache::with_fetcher(Arc::new(HttpFetcher::new()));
    cache.set_diagnostics_sink(Some(Arc::clone(&diagnostics)));

    let err = match cache.load(&url) {
      Ok(_) => panic!("image load should fail"),
      Err(err) => err,
    };
    match err {
      Error::Resource(ref res) => {
        assert_eq!(res.status, Some(200));
        assert!(
          res.message.contains("unexpected content-type"),
          "unexpected error message: {}",
          res.message
        );
      }
      other => panic!("expected resource error, got {other:?}"),
    }

    let diag = diagnostics.lock().unwrap().clone();
    let entry = diag
      .fetch_errors
      .iter()
      .find(|e| e.kind == ResourceKind::Image && e.url == url)
      .expect("diagnostics entry");
    assert_eq!(entry.status, Some(200));
    assert!(
      diag.invalid_images.iter().all(|u| u != &url),
      "expected invalid_images to not contain url"
    );

    server.join().unwrap();
  }

  #[cfg(feature = "direct_network")]
  #[test]
  fn http_200_html_for_jpg_with_image_mime_is_reported_as_resource_error() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    let listener = match TcpListener::bind("127.0.0.1:0") {
      Ok(listener) => listener,
      Err(err)
        if matches!(
          err.kind(),
          std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::AddrNotAvailable
        ) =>
      {
        eprintln!("skipping http_200_html_for_jpg_with_image_mime_is_reported_as_resource_error: cannot bind localhost: {err}");
        return;
      }
      Err(err) => panic!("bind localhost: {err}"),
    };
    let addr = listener.local_addr().expect("listener addr");
    let url = format!("http://{addr}/photo.jpg");

    let server = std::thread::spawn(move || {
      let (mut stream, _) = listener.accept().expect("accept");
      let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
      let mut buf = [0u8; 1024];
      let _ = stream.read(&mut buf);

      let body = "<!doctype html><html><body>blocked</body></html>";
      let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
      );
      stream
        .write_all(response.as_bytes())
        .expect("write response");
      let _ = stream.flush();
    });

    let diagnostics = Arc::new(Mutex::new(RenderDiagnostics::default()));
    let mut cache = ImageCache::with_fetcher(Arc::new(HttpFetcher::new()));
    cache.set_diagnostics_sink(Some(Arc::clone(&diagnostics)));

    let err = match cache.load(&url) {
      Ok(_) => panic!("image load should fail"),
      Err(err) => err,
    };
    match err {
      Error::Resource(ref res) => {
        assert_eq!(res.status, Some(200));
        assert!(
          res.message.contains("unexpected markup"),
          "unexpected error message: {}",
          res.message
        );
      }
      other => panic!("expected resource error, got {other:?}"),
    }

    let diag = diagnostics.lock().unwrap().clone();
    let entry = diag
      .fetch_errors
      .iter()
      .find(|e| e.kind == ResourceKind::Image && e.url == url)
      .expect("diagnostics entry");
    assert_eq!(entry.status, Some(200));
    assert!(
      diag.invalid_images.iter().all(|u| u != &url),
      "expected invalid_images to not contain url"
    );

    server.join().unwrap();
  }

  #[cfg(feature = "direct_network")]
  #[test]
  fn http_200_html_for_jpg_probe_is_reported_as_resource_error() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    let listener = match TcpListener::bind("127.0.0.1:0") {
      Ok(listener) => listener,
      Err(err)
        if matches!(
          err.kind(),
          std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::AddrNotAvailable
        ) =>
      {
        eprintln!(
          "skipping http_200_html_for_jpg_probe_is_reported_as_resource_error: cannot bind localhost: {err}"
        );
        return;
      }
      Err(err) => panic!("bind localhost: {err}"),
    };
    let addr = listener.local_addr().expect("listener addr");
    let url = format!("http://{addr}/photo.jpg");

    let server = std::thread::spawn(move || {
      let (mut stream, _) = listener.accept().expect("accept");
      let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
      let mut buf = [0u8; 1024];
      let _ = stream.read(&mut buf);

      let body = "<!doctype html><html><body>blocked</body></html>";
      let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
      );
      stream
        .write_all(response.as_bytes())
        .expect("write response");
      let _ = stream.flush();
    });

    let diagnostics = Arc::new(Mutex::new(RenderDiagnostics::default()));
    let mut cache = ImageCache::with_fetcher(Arc::new(HttpFetcher::new()));
    cache.set_diagnostics_sink(Some(Arc::clone(&diagnostics)));

    let err = match cache.probe(&url) {
      Ok(_) => panic!("image probe should fail"),
      Err(err) => err,
    };
    match err {
      Error::Resource(ref res) => {
        assert_eq!(res.status, Some(200));
        assert!(
          res.message.contains("unexpected content-type"),
          "unexpected error message: {}",
          res.message
        );
      }
      other => panic!("expected resource error, got {other:?}"),
    }

    let diag = diagnostics.lock().unwrap().clone();
    let entry = diag
      .fetch_errors
      .iter()
      .find(|e| e.kind == ResourceKind::Image && e.url == url)
      .expect("diagnostics entry");
    assert_eq!(entry.status, Some(200));
    assert!(
      diag.invalid_images.iter().all(|u| u != &url),
      "expected invalid_images to not contain url"
    );

    server.join().unwrap();
  }

  #[cfg(feature = "direct_network")]
  #[test]
  fn http_200_html_for_jpg_with_image_mime_probe_is_reported_as_resource_error() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    let listener = match TcpListener::bind("127.0.0.1:0") {
      Ok(listener) => listener,
      Err(err)
        if matches!(
          err.kind(),
          std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::AddrNotAvailable
        ) =>
      {
        eprintln!("skipping http_200_html_for_jpg_with_image_mime_probe_is_reported_as_resource_error: cannot bind localhost: {err}");
        return;
      }
      Err(err) => panic!("bind localhost: {err}"),
    };
    let addr = listener.local_addr().expect("listener addr");
    let url = format!("http://{addr}/photo.jpg");

    let server = std::thread::spawn(move || {
      let (mut stream, _) = listener.accept().expect("accept");
      let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
      let mut buf = [0u8; 1024];
      let _ = stream.read(&mut buf);

      let body = "<!doctype html><html><body>blocked</body></html>";
      let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
      );
      stream
        .write_all(response.as_bytes())
        .expect("write response");
      let _ = stream.flush();
    });

    let diagnostics = Arc::new(Mutex::new(RenderDiagnostics::default()));
    let mut cache = ImageCache::with_fetcher(Arc::new(HttpFetcher::new()));
    cache.set_diagnostics_sink(Some(Arc::clone(&diagnostics)));

    let err = match cache.probe(&url) {
      Ok(_) => panic!("image probe should fail"),
      Err(err) => err,
    };
    match err {
      Error::Resource(ref res) => {
        assert_eq!(res.status, Some(200));
        assert!(
          res.message.contains("unexpected markup"),
          "unexpected error message: {}",
          res.message
        );
      }
      other => panic!("expected resource error, got {other:?}"),
    }

    let diag = diagnostics.lock().unwrap().clone();
    let entry = diag
      .fetch_errors
      .iter()
      .find(|e| e.kind == ResourceKind::Image && e.url == url)
      .expect("diagnostics entry");
    assert_eq!(entry.status, Some(200));
    assert!(
      diag.invalid_images.iter().all(|u| u != &url),
      "expected invalid_images to not contain url"
    );

    server.join().unwrap();
  }

  #[test]
  fn image_cache_load_about_blank_returns_transparent_placeholder() {
    #[derive(Clone)]
    struct PanicFetcher;

    impl ResourceFetcher for PanicFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        panic!("fetch should not be called for about: URLs");
      }

      fn fetch_partial_with_context(
        &self,
        _kind: FetchContextKind,
        _url: &str,
        _max_bytes: usize,
      ) -> Result<FetchedResource> {
        panic!("partial fetch should not be called for about: URLs");
      }
    }

    let cache = ImageCache::with_fetcher(Arc::new(PanicFetcher));
    let image = cache
      .load("about:blank")
      .expect("about:blank placeholder loads");
    assert!(!image.is_vector);
    assert_eq!(image.dimensions(), (1, 1));

    let rgba = image.image.to_rgba8();
    assert_eq!(rgba.dimensions(), (1, 1));
    assert_eq!(rgba.get_pixel(0, 0).0, [0, 0, 0, 0]);
  }

  #[test]
  fn image_cache_load_empty_url_returns_transparent_placeholder() {
    #[derive(Clone)]
    struct PanicFetcher;

    impl ResourceFetcher for PanicFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        panic!("fetch should not be called for empty URLs");
      }

      fn fetch_partial_with_context(
        &self,
        _kind: FetchContextKind,
        _url: &str,
        _max_bytes: usize,
      ) -> Result<FetchedResource> {
        panic!("partial fetch should not be called for empty URLs");
      }
    }

    let cache = ImageCache::with_fetcher(Arc::new(PanicFetcher));
    let image = cache.load(" \t\r\n").expect("empty url placeholder loads");
    assert!(!image.is_vector);
    assert_eq!(image.dimensions(), (1, 1));

    let rgba = image.image.to_rgba8();
    assert_eq!(rgba.dimensions(), (1, 1));
    assert_eq!(rgba.get_pixel(0, 0).0, [0, 0, 0, 0]);
  }

  #[test]
  fn image_cache_load_reuses_offline_placeholder_png_from_raw_cache() {
    // Offline fixtures substitute missing file payloads with a deterministic 1×1 transparent PNG.
    // When image dimensions are probed first, the raw bytes can be cached and later decoded via
    // `decode_resource_into_cache`. Ensure that decode path still recognizes the placeholder and
    // returns the shared `about:` placeholder image so replaced-content paint can render UA fallback
    // UI instead of a silent transparent 1×1.
    let url = "https://example.com/missing.png";
    let mut res = FetchedResource::new(
      crate::resource::offline_placeholder_png_bytes().to_vec(),
      Some("image/png".to_string()),
    );
    res.status = Some(200);
    res.final_url = Some(url.to_string());

    let fetcher = MapFetcher::with_entries([(url.to_string(), res)]);
    let cache = ImageCache::with_fetcher(Arc::new(fetcher));

    // Populate the probe/raw caches first to force `load()` down the `decode_resource_into_cache`
    // path.
    cache.probe(url).expect("probe should succeed");
    let image = cache.load(url).expect("load should succeed");
    assert!(
      cache.is_placeholder_image(&image),
      "expected offline placeholder PNG bytes to map to the shared placeholder image"
    );
  }

  #[test]
  fn image_meta_cache_is_bounded_by_items() {
    #[derive(Clone)]
    struct PanicFetcher;

    impl ResourceFetcher for PanicFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        panic!("fetch should not be called")
      }
    }

    let config = ImageCacheConfig::default()
      .with_max_cached_metadata_items(2)
      .with_max_cached_metadata_bytes(1024 * 1024);
    let cache = ImageCache::with_fetcher_and_config(Arc::new(PanicFetcher), config);

    let meta = Arc::new(CachedImageMetadata {
      width: 1,
      height: 1,
      orientation: None,
      resolution: None,
      is_vector: false,
      is_animated: false,
      intrinsic_ratio: Some(1.0),
      aspect_ratio_none: false,
    });

    let mut guard = cache
      .meta_cache
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard.clear();
    for i in 0..16 {
      let key = format!("test://meta/{i}");
      let bytes = ImageCache::estimate_meta_cache_entry_bytes(&key, meta.as_ref());
      guard.insert(key, Arc::clone(&meta), bytes);
      assert!(
        guard.len() <= config.max_cached_metadata_items,
        "meta cache should be bounded by items"
      );
      assert!(
        guard.current_bytes() <= config.max_cached_metadata_bytes,
        "meta cache bytes should not exceed configured limit"
      );
    }
    assert_eq!(guard.len(), config.max_cached_metadata_items);
  }

  #[test]
  fn image_raw_cache_is_bounded_by_bytes() {
    #[derive(Clone)]
    struct PanicFetcher;

    impl ResourceFetcher for PanicFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        panic!("fetch should not be called")
      }
    }

    let config = ImageCacheConfig::default()
      .with_max_raw_cached_items(16)
      .with_max_raw_cached_bytes(180);
    let cache = ImageCache::with_fetcher_and_config(Arc::new(PanicFetcher), config);

    let mut guard = cache
      .raw_cache
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard.clear();

    let mut last_key = String::new();
    for i in 0..8 {
      let key = format!("test://raw/{i}");
      last_key = key.clone();
      let resource = Arc::new(FetchedResource::new(vec![0u8; 120], None));
      let bytes = ImageCache::estimate_raw_cache_entry_bytes(&key, resource.as_ref());
      guard.insert(key, resource, bytes);
      assert!(
        guard.current_bytes() <= config.max_raw_cached_bytes,
        "raw cache bytes should not exceed configured limit"
      );
      assert!(
        guard.len() <= config.max_raw_cached_items,
        "raw cache should be bounded by items"
      );
    }

    // With an 180-byte budget and ~120-byte payloads, the cache should only retain the most recent
    // entry after eviction.
    assert_eq!(guard.len(), 1);
    assert!(guard.get_cloned(last_key.as_str()).is_some());
    assert!(guard.get_cloned("test://raw/0").is_none());
  }

  #[test]
  fn image_cache_probe_about_blank_returns_placeholder_metadata() {
    #[derive(Clone)]
    struct PanicFetcher;

    impl ResourceFetcher for PanicFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        panic!("fetch should not be called for about: URLs");
      }

      fn fetch_partial_with_context(
        &self,
        _kind: FetchContextKind,
        _url: &str,
        _max_bytes: usize,
      ) -> Result<FetchedResource> {
        panic!("partial fetch should not be called for about: URLs");
      }
    }

    let cache = ImageCache::with_fetcher(Arc::new(PanicFetcher));
    let meta = cache
      .probe("about:blank")
      .expect("about:blank probe succeeds");
    assert!(!meta.is_vector);
    assert_eq!(meta.dimensions(), (1, 1));
    assert_eq!(
      meta.intrinsic_ratio(OrientationTransform::IDENTITY),
      Some(1.0)
    );
  }

  #[test]
  fn image_cache_load_offline_placeholder_probe_bytes_returns_placeholder_image() {
    #[derive(Clone)]
    struct OfflinePlaceholderFetcher;

    impl ResourceFetcher for OfflinePlaceholderFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        Ok(FetchedResource::with_final_url(
          crate::resource::offline_placeholder_png_bytes().to_vec(),
          Some("image/png".to_string()),
          Some(url.to_string()),
        ))
      }
    }

    let cache = ImageCache::with_fetcher(Arc::new(OfflinePlaceholderFetcher));
    let url = "https://example.com/offline-placeholder.png";
    let _ = cache.probe(url).expect("probe succeeds");
    let image = cache.load(url).expect("load succeeds");
    assert!(
      cache.is_placeholder_image(&image),
      "expected offline placeholder probe bytes to load as the shared placeholder image"
    );
  }

  #[test]
  fn image_cache_load_empty_http_body_returns_placeholder_without_diagnostics() {
    #[derive(Clone)]
    struct EmptyBodyFetcher;

    impl ResourceFetcher for EmptyBodyFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        Err(Error::Resource(
          crate::error::ResourceError::new(url, "empty HTTP response body").with_status(200),
        ))
      }
    }

    let diagnostics = Arc::new(Mutex::new(RenderDiagnostics::default()));
    let mut cache = ImageCache::with_fetcher(Arc::new(EmptyBodyFetcher));
    cache.set_diagnostics_sink(Some(Arc::clone(&diagnostics)));

    let image = cache
      .load("https://example.com/pixel.png")
      .expect("empty-body image loads as placeholder");
    assert_eq!(image.dimensions(), (1, 1));

    let diag = diagnostics.lock().unwrap().clone();
    assert!(
      diag.fetch_errors.is_empty(),
      "placeholder images should not be recorded as fetch errors"
    );
  }

  #[test]
  fn image_cache_probe_empty_http_body_returns_placeholder_metadata_without_diagnostics() {
    #[derive(Clone)]
    struct EmptyBodyFetcher;

    impl ResourceFetcher for EmptyBodyFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        Err(Error::Resource(
          crate::error::ResourceError::new(url, "empty HTTP response body").with_status(200),
        ))
      }
    }

    let diagnostics = Arc::new(Mutex::new(RenderDiagnostics::default()));
    let mut cache = ImageCache::with_fetcher(Arc::new(EmptyBodyFetcher));
    cache.set_diagnostics_sink(Some(Arc::clone(&diagnostics)));

    let meta = cache
      .probe("https://example.com/pixel.png")
      .expect("empty-body image probe returns placeholder");
    assert_eq!(meta.dimensions(), (1, 1));

    let diag = diagnostics.lock().unwrap().clone();
    assert!(diag.fetch_errors.is_empty());
    assert!(
      diag.invalid_images.is_empty(),
      "empty-body placeholder images should not be treated as invalid images"
    );
  }

  #[test]
  fn image_cache_raw_cached_offline_placeholder_png_is_recognized_as_placeholder() {
    #[derive(Clone)]
    struct PanicFetcher;

    impl ResourceFetcher for PanicFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        panic!("fetch should not be called when raw cache is populated");
      }

      fn fetch_partial_with_context(
        &self,
        _kind: FetchContextKind,
        _url: &str,
        _max_bytes: usize,
      ) -> Result<FetchedResource> {
        panic!("partial fetch should not be called when raw cache is populated");
      }
    }

    let cache = ImageCache::with_fetcher(Arc::new(PanicFetcher));
    let url = "https://example.com/offline-placeholder.png";
    let cache_key = cache.cache_key_for_crossorigin(url, CrossOriginAttribute::None, None);

    let mut resource = FetchedResource::with_final_url(
      crate::resource::offline_placeholder_png_bytes().to_vec(),
      Some("image/png".to_string()),
      Some(url.to_string()),
    );
    resource.status = Some(200);
    let resource = Arc::new(resource);

    let mut guard = cache
      .raw_cache
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard.clear();
    let bytes = ImageCache::estimate_raw_cache_entry_bytes(&cache_key, resource.as_ref());
    guard.insert(cache_key, resource, bytes);
    drop(guard);

    let image = cache.load(url).expect("image should load from raw cache");
    assert!(
      cache.is_placeholder_image(&image),
      "offline fixture placeholder PNG should load as the about: placeholder image"
    );
  }

  #[test]
  fn image_cache_html_payload_returns_placeholder_without_diagnostics() {
    #[derive(Clone)]
    struct HtmlFetcher;

    impl ResourceFetcher for HtmlFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        let mut res = FetchedResource::new(
          b"<!doctype html><html><head></head><body>not an image</body></html>".to_vec(),
          Some("image/png".to_string()),
        );
        res.status = Some(200);
        Ok(res)
      }
    }

    let diagnostics = Arc::new(Mutex::new(RenderDiagnostics::default()));
    let mut cache = ImageCache::with_fetcher(Arc::new(HtmlFetcher));
    cache.set_diagnostics_sink(Some(Arc::clone(&diagnostics)));

    let image = cache
      .load("https://example.com/not-really.html")
      .expect("HTML body treated as placeholder image");
    assert_eq!(image.dimensions(), (1, 1));

    let meta = cache
      .probe("https://example.com/not-really.html")
      .expect("HTML body treated as placeholder metadata");
    assert_eq!(meta.dimensions(), (1, 1));

    let diag = diagnostics.lock().unwrap().clone();
    assert!(diag.fetch_errors.is_empty());
    assert!(
      diag
        .invalid_images
        .iter()
        .any(|u| u == "https://example.com/not-really.html"),
      "invalid image URLs should be tracked in diagnostics.invalid_images"
    );
  }

  #[test]
  fn image_cache_html_payload_with_206_status_returns_placeholder_without_diagnostics() {
    #[derive(Clone)]
    struct PartialHtmlFetcher;

    impl ResourceFetcher for PartialHtmlFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        panic!("full fetch should not be required for HTML payload probe");
      }

      fn fetch_partial_with_context(
        &self,
        _kind: FetchContextKind,
        _url: &str,
        _max_bytes: usize,
      ) -> Result<FetchedResource> {
        let mut res = FetchedResource::new(
          b"<!doctype html><html><body>partial content</body></html>".to_vec(),
          Some("image/png".to_string()),
        );
        res.status = Some(206);
        Ok(res)
      }
    }

    let diagnostics = Arc::new(Mutex::new(RenderDiagnostics::default()));
    let mut cache = ImageCache::with_fetcher(Arc::new(PartialHtmlFetcher));
    cache.set_diagnostics_sink(Some(Arc::clone(&diagnostics)));

    let meta = cache
      .probe("https://example.com/not-an-image.html")
      .expect("HTML probe returns placeholder metadata");
    assert_eq!(meta.dimensions(), (1, 1));

    let diag = diagnostics.lock().unwrap().clone();
    assert!(diag.fetch_errors.is_empty());
    assert!(
      diag
        .invalid_images
        .iter()
        .any(|u| u == "https://example.com/not-an-image.html"),
      "invalid image URLs should be tracked in diagnostics.invalid_images"
    );
  }

  fn padded_png() -> Vec<u8> {
    // 1x1 RGBA PNG, padded with trailing bytes so that the probe must use a prefix fetch.
    const PNG_1X1: &[u8] = &[
      0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
      0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x04, 0x00, 0x00, 0x00, 0xB5,
      0x1C, 0x0C, 0x02, 0x00, 0x00, 0x00, 0x0B, 0x49, 0x44, 0x41, 0x54, 0x78, 0xDA, 0x63, 0xFC,
      0xFF, 0x1F, 0x00, 0x03, 0x03, 0x01, 0x02, 0x94, 0x60, 0xC4, 0x1B, 0x00, 0x00, 0x00, 0x00,
      0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    let mut bytes = PNG_1X1.to_vec();
    bytes.resize(128 * 1024, 0);
    bytes
  }

  #[cfg(feature = "disk_cache")]
  #[test]
  fn image_probe_persists_metadata_to_disk_cache_and_reuses_it() {
    use crate::resource::{CachingFetcherConfig, DiskCacheConfig, DiskCachingFetcher};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[derive(Clone)]
    struct CountingPartialFetcher {
      calls: Arc<AtomicUsize>,
      body: Arc<Vec<u8>>,
    }

    impl ResourceFetcher for CountingPartialFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        panic!("expected partial fetch only");
      }

      fn fetch_partial_with_context(
        &self,
        _kind: FetchContextKind,
        url: &str,
        max_bytes: usize,
      ) -> Result<FetchedResource> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut res =
          FetchedResource::new(self.body.as_ref().clone(), Some("image/png".to_string()));
        res.status = Some(200);
        res.final_url = Some(url.to_string());
        if res.bytes.len() > max_bytes {
          res.bytes.truncate(max_bytes);
        }
        Ok(res)
      }
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let cache_dir = tmp.path().join("assets");
    let url = "https://example.com/probe.png";
    let body = Arc::new(padded_png());

    let calls = Arc::new(AtomicUsize::new(0));
    let disk = DiskCachingFetcher::with_configs(
      CountingPartialFetcher {
        calls: Arc::clone(&calls),
        body: Arc::clone(&body),
      },
      &cache_dir,
      CachingFetcherConfig {
        honor_http_cache_freshness: true,
        ..CachingFetcherConfig::default()
      },
      DiskCacheConfig {
        max_bytes: 0,
        ..DiskCacheConfig::default()
      },
    );
    let cache = ImageCache::with_fetcher(Arc::new(disk));
    let meta = cache.probe(url).expect("probe succeeds");
    assert_eq!(meta.dimensions(), (1, 1));
    assert_eq!(
      calls.load(Ordering::SeqCst),
      1,
      "first probe should hit the network via fetch_partial"
    );

    let calls2 = Arc::new(AtomicUsize::new(0));
    let disk2 = DiskCachingFetcher::with_configs(
      CountingPartialFetcher {
        calls: Arc::clone(&calls2),
        body: Arc::clone(&body),
      },
      &cache_dir,
      CachingFetcherConfig {
        honor_http_cache_freshness: true,
        ..CachingFetcherConfig::default()
      },
      DiskCacheConfig {
        max_bytes: 0,
        ..DiskCacheConfig::default()
      },
    );
    let cache2 = ImageCache::with_fetcher(Arc::new(disk2));
    let meta2 = cache2.probe(url).expect("probe succeeds from disk");
    assert_eq!(meta2.dimensions(), (1, 1));
    assert_eq!(
      calls2.load(Ordering::SeqCst),
      0,
      "second probe should reuse persisted probe metadata without network calls"
    );
  }

  #[cfg(feature = "disk_cache")]
  #[test]
  fn image_probe_resolved_reuses_persisted_metadata_to_disk_cache() {
    use crate::resource::{CachingFetcherConfig, DiskCacheConfig, DiskCachingFetcher};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[derive(Clone)]
    struct CountingPartialFetcher {
      calls: Arc<AtomicUsize>,
      body: Arc<Vec<u8>>,
    }

    impl ResourceFetcher for CountingPartialFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        panic!("expected partial fetch only");
      }

      fn fetch_partial_with_context(
        &self,
        _kind: FetchContextKind,
        url: &str,
        max_bytes: usize,
      ) -> Result<FetchedResource> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut res =
          FetchedResource::new(self.body.as_ref().clone(), Some("image/png".to_string()));
        res.status = Some(200);
        res.final_url = Some(url.to_string());
        if res.bytes.len() > max_bytes {
          res.bytes.truncate(max_bytes);
        }
        Ok(res)
      }
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let cache_dir = tmp.path().join("assets");
    let url = "https://example.com/probe_resolved.png";
    let body = Arc::new(padded_png());

    let calls = Arc::new(AtomicUsize::new(0));
    let disk = DiskCachingFetcher::with_configs(
      CountingPartialFetcher {
        calls: Arc::clone(&calls),
        body: Arc::clone(&body),
      },
      &cache_dir,
      CachingFetcherConfig {
        honor_http_cache_freshness: true,
        ..CachingFetcherConfig::default()
      },
      DiskCacheConfig {
        max_bytes: 0,
        ..DiskCacheConfig::default()
      },
    );
    let cache = ImageCache::with_fetcher(Arc::new(disk));
    let meta = cache.probe_resolved(url).expect("probe succeeds");
    assert_eq!(meta.dimensions(), (1, 1));
    assert_eq!(
      calls.load(Ordering::SeqCst),
      1,
      "first resolved probe should hit the network via fetch_partial"
    );

    let calls2 = Arc::new(AtomicUsize::new(0));
    let disk2 = DiskCachingFetcher::with_configs(
      CountingPartialFetcher {
        calls: Arc::clone(&calls2),
        body: Arc::clone(&body),
      },
      &cache_dir,
      CachingFetcherConfig {
        honor_http_cache_freshness: true,
        ..CachingFetcherConfig::default()
      },
      DiskCacheConfig {
        max_bytes: 0,
        ..DiskCacheConfig::default()
      },
    );
    let cache2 = ImageCache::with_fetcher(Arc::new(disk2));
    let meta2 = cache2
      .probe_resolved(url)
      .expect("probe succeeds from disk");
    assert_eq!(meta2.dimensions(), (1, 1));
    assert_eq!(
      calls2.load(Ordering::SeqCst),
      0,
      "second resolved probe should reuse persisted probe metadata without network calls"
    );
  }

  #[cfg(feature = "disk_cache")]
  #[test]
  fn image_probe_artifact_inherits_stored_at_from_cached_resource() {
    use crate::resource::{
      CachingFetcherConfig, DiskCacheConfig, DiskCachingFetcher, FetchDestination, FetchRequest,
    };
    use std::fs;
    use std::sync::Arc;

    #[derive(Clone)]
    struct FullFetcher {
      body: Arc<Vec<u8>>,
    }

    impl ResourceFetcher for FullFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        let mut res =
          FetchedResource::new(self.body.as_ref().clone(), Some("image/png".to_string()));
        res.status = Some(200);
        res.final_url = Some(url.to_string());
        Ok(res)
      }
    }

    #[derive(Clone)]
    struct PanicFetcher;

    impl ResourceFetcher for PanicFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        panic!("network fetch should not be called");
      }

      fn fetch_partial_with_context(
        &self,
        _kind: FetchContextKind,
        _url: &str,
        _max_bytes: usize,
      ) -> Result<FetchedResource> {
        panic!("network partial fetch should not be called");
      }
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let cache_dir = tmp.path().join("assets");
    let url = "https://example.com/aged.png";
    let body = Arc::new(padded_png());

    // Persist the full image bytes into the disk cache so the probe can later derive metadata from
    // disk without touching the network.
    let disk = DiskCachingFetcher::with_configs(
      FullFetcher {
        body: Arc::clone(&body),
      },
      &cache_dir,
      CachingFetcherConfig {
        honor_http_cache_freshness: true,
        ..CachingFetcherConfig::default()
      },
      DiskCacheConfig {
        max_bytes: 0,
        ..DiskCacheConfig::default()
      },
    );
    disk
      .fetch_with_request(FetchRequest::new(url, FetchDestination::Image))
      .expect("seed fetch");

    // Locate the primary cached image entry and force its `stored_at` to be very old so we can
    // assert the derived probe artifact inherits that age rather than refreshing it.
    let mut resource_meta_path = None;
    for entry in fs::read_dir(&cache_dir).expect("read cache dir") {
      let path = entry.expect("dir entry").path();
      if !path.to_string_lossy().ends_with(".bin.meta") {
        continue;
      }
      let bytes = fs::read(&path).expect("read meta");
      let value: serde_json::Value = serde_json::from_slice(&bytes).expect("parse meta json");
      let ct = value
        .get("content_type")
        .and_then(|v| v.as_str())
        .unwrap_or("");
      if ct == "image/png" {
        resource_meta_path = Some(path);
        break;
      }
    }
    let resource_meta_path = resource_meta_path.expect("cached image meta file");
    let meta_bytes = fs::read(&resource_meta_path).expect("read meta bytes");
    let mut value: serde_json::Value =
      serde_json::from_slice(&meta_bytes).expect("parse meta json");
    value["stored_at"] = serde_json::Value::from(0u64);
    fs::write(
      &resource_meta_path,
      serde_json::to_vec(&value).expect("serialize meta"),
    )
    .expect("write meta");

    // New fetcher instance (empty memory cache). The probe should be satisfied entirely from disk.
    let disk2 = DiskCachingFetcher::with_configs(
      PanicFetcher,
      &cache_dir,
      CachingFetcherConfig {
        honor_http_cache_freshness: true,
        ..CachingFetcherConfig::default()
      },
      DiskCacheConfig {
        max_bytes: 0,
        ..DiskCacheConfig::default()
      },
    );
    let cache = ImageCache::with_fetcher(Arc::new(disk2));
    let meta = cache.probe(url).expect("probe succeeds");
    assert_eq!(meta.dimensions(), (1, 1));

    // The persisted probe metadata should inherit the primary resource `stored_at` timestamp so it
    // becomes stale alongside the cached image bytes.
    let mut probe_meta_path = None;
    for entry in fs::read_dir(&cache_dir).expect("read cache dir") {
      let path = entry.expect("dir entry").path();
      if !path.to_string_lossy().ends_with(".bin.meta") {
        continue;
      }
      let bytes = fs::read(&path).expect("read meta");
      let value: serde_json::Value = serde_json::from_slice(&bytes).expect("parse meta json");
      let ct = value
        .get("content_type")
        .and_then(|v| v.as_str())
        .unwrap_or("");
      if ct == "application/x-fastrender-image-probe+json" {
        probe_meta_path = Some(path);
        break;
      }
    }
    let probe_meta_path = probe_meta_path.expect("probe meta file");
    let probe_bytes = fs::read(&probe_meta_path).expect("read probe meta");
    let probe_value: serde_json::Value =
      serde_json::from_slice(&probe_bytes).expect("parse probe meta json");
    assert_eq!(
      probe_value.get("stored_at").and_then(|v| v.as_u64()),
      Some(0),
      "probe metadata should inherit stored_at from the cached resource"
    );
  }

  #[cfg(feature = "disk_cache")]
  #[test]
  fn image_probe_disk_cache_respects_staleness_and_recovers_from_corruption() {
    use crate::resource::{CachingFetcherConfig, DiskCacheConfig, DiskCachingFetcher};
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    #[derive(Clone)]
    struct CountingPartialFetcher {
      calls: Arc<AtomicUsize>,
      body: Arc<Vec<u8>>,
    }

    impl ResourceFetcher for CountingPartialFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        panic!("expected partial fetch only");
      }

      fn fetch_partial_with_context(
        &self,
        _kind: FetchContextKind,
        url: &str,
        max_bytes: usize,
      ) -> Result<FetchedResource> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut res =
          FetchedResource::new(self.body.as_ref().clone(), Some("image/png".to_string()));
        res.status = Some(200);
        res.final_url = Some(url.to_string());
        if res.bytes.len() > max_bytes {
          res.bytes.truncate(max_bytes);
        }
        Ok(res)
      }
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let cache_dir = tmp.path().join("assets");
    let url = "https://example.com/stale.png";
    let body = Arc::new(padded_png());

    let calls = Arc::new(AtomicUsize::new(0));
    let disk = DiskCachingFetcher::with_configs(
      CountingPartialFetcher {
        calls: Arc::clone(&calls),
        body: Arc::clone(&body),
      },
      &cache_dir,
      CachingFetcherConfig {
        honor_http_cache_freshness: true,
        ..CachingFetcherConfig::default()
      },
      DiskCacheConfig {
        max_bytes: 0,
        max_age: Some(Duration::from_secs(1)),
        ..DiskCacheConfig::default()
      },
    );
    let cache = ImageCache::with_fetcher(Arc::new(disk));
    let meta = cache.probe(url).expect("probe succeeds");
    assert_eq!(meta.dimensions(), (1, 1));
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // Locate the probe-metadata cache entry and force it to be stale by setting stored_at=0.
    let mut meta_path = None;
    for entry in fs::read_dir(&cache_dir).expect("read cache dir") {
      let path = entry.expect("dir entry").path();
      if !path.to_string_lossy().ends_with(".bin.meta") {
        continue;
      }
      let bytes = fs::read(&path).expect("read meta");
      let value: serde_json::Value = serde_json::from_slice(&bytes).expect("parse meta json");
      let ct = value
        .get("content_type")
        .and_then(|v| v.as_str())
        .unwrap_or("");
      if ct == "application/x-fastrender-image-probe+json" {
        meta_path = Some(path);
        break;
      }
    }
    let meta_path = meta_path.expect("probe metadata entry meta file");
    let meta_bytes = fs::read(&meta_path).expect("read meta bytes");
    let mut value: serde_json::Value =
      serde_json::from_slice(&meta_bytes).expect("parse meta json");
    value["stored_at"] = serde_json::Value::from(0u64);
    fs::write(
      &meta_path,
      serde_json::to_vec(&value).expect("serialize meta"),
    )
    .expect("write meta");

    let calls2 = Arc::new(AtomicUsize::new(0));
    let disk2 = DiskCachingFetcher::with_configs(
      CountingPartialFetcher {
        calls: Arc::clone(&calls2),
        body: Arc::clone(&body),
      },
      &cache_dir,
      CachingFetcherConfig {
        honor_http_cache_freshness: true,
        ..CachingFetcherConfig::default()
      },
      DiskCacheConfig {
        max_bytes: 0,
        max_age: Some(Duration::from_secs(1)),
        ..DiskCacheConfig::default()
      },
    );
    let cache2 = ImageCache::with_fetcher(Arc::new(disk2));
    let meta2 = cache2.probe(url).expect("probe succeeds after staleness");
    assert_eq!(meta2.dimensions(), (1, 1));
    assert_eq!(
      calls2.load(Ordering::SeqCst),
      1,
      "stale probe metadata should trigger a network refresh"
    );

    // Corrupt the on-disk probe data while keeping the length intact, then ensure we can recover.
    let len = value.get("len").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let meta_string = meta_path.to_string_lossy();
    let data_path =
      std::path::PathBuf::from(meta_string.strip_suffix(".meta").expect("meta path suffix"));
    fs::write(&data_path, vec![0u8; len.max(1)]).expect("write corrupt data");

    let calls3 = Arc::new(AtomicUsize::new(0));
    let disk3 = DiskCachingFetcher::with_configs(
      CountingPartialFetcher {
        calls: Arc::clone(&calls3),
        body: Arc::clone(&body),
      },
      &cache_dir,
      CachingFetcherConfig {
        honor_http_cache_freshness: true,
        ..CachingFetcherConfig::default()
      },
      DiskCacheConfig {
        max_bytes: 0,
        max_age: Some(Duration::from_secs(1)),
        ..DiskCacheConfig::default()
      },
    );
    let cache3 = ImageCache::with_fetcher(Arc::new(disk3));
    let meta3 = cache3.probe(url).expect("probe succeeds after corruption");
    assert_eq!(meta3.dimensions(), (1, 1));
    assert_eq!(
      calls3.load(Ordering::SeqCst),
      1,
      "corrupt probe metadata should be treated as a miss and refreshed"
    );
  }

  fn read_http_headers(stream: &mut std::net::TcpStream) -> String {
    use std::io::Read;

    let mut buf = Vec::new();
    let mut scratch = [0u8; 1024];
    loop {
      match stream.read(&mut scratch) {
        Ok(0) => break,
        Ok(n) => {
          buf.extend_from_slice(&scratch[..n]);
          if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 32 * 1024 {
            break;
          }
        }
        Err(_) => break,
      }
    }
    String::from_utf8_lossy(&buf).to_string()
  }

  fn trim_http_whitespace(value: &str) -> &str {
    value.trim_matches(|c: char| matches!(c, ' ' | '\t'))
  }

  fn extract_range_header(req: &str) -> Option<String> {
    req.lines().find_map(|line| {
      let line = line.trim_end_matches('\r');
      let (name, value) = line.split_once(':')?;
      if trim_http_whitespace(name).eq_ignore_ascii_case("range") {
        Some(trim_http_whitespace(value).to_string())
      } else {
        None
      }
    })
  }

  fn parse_range_end(range: &str) -> Option<usize> {
    let range = trim_http_whitespace(range);
    let range = range.strip_prefix("bytes=")?;
    let (_start, end) = range.split_once('-')?;
    trim_http_whitespace(end).parse::<usize>().ok()
  }

  #[cfg(feature = "direct_network")]
  #[test]
  fn image_probe_uses_http_range_requests() {
    use std::io::Write;
    use std::net::TcpListener;
    use std::time::Duration;

    let listener = match TcpListener::bind("127.0.0.1:0") {
      Ok(listener) => listener,
      Err(err)
        if matches!(
          err.kind(),
          std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::AddrNotAvailable
        ) =>
      {
        eprintln!("skipping image_probe_uses_http_range_requests: cannot bind localhost: {err}");
        return;
      }
      Err(err) => panic!("bind localhost: {err}"),
    };
    let addr = listener.local_addr().expect("listener addr");
    let url = format!("http://{addr}/probe.png");
    let body = padded_png();

    let server = std::thread::spawn(move || {
      let (mut stream, _) = listener.accept().expect("accept");
      let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
      let req = read_http_headers(&mut stream);

      let range = extract_range_header(&req).expect("missing Range header");
      let end = parse_range_end(&range).expect("invalid Range header");
      let prefix_len = (end + 1).min(body.len());
      let prefix = &body[..prefix_len];

      let header = format!(
        "HTTP/1.1 206 Partial Content\r\nContent-Type: image/png\r\nContent-Length: {}\r\nContent-Range: bytes 0-{}/{}\r\nConnection: close\r\n\r\n",
        prefix.len(),
        prefix.len().saturating_sub(1),
        body.len()
      );
      stream.write_all(header.as_bytes()).expect("write header");
      stream.write_all(prefix).expect("write body");
      let _ = stream.flush();
    });

    let cache = ImageCache::with_fetcher(Arc::new(HttpFetcher::new()));
    let meta = cache.probe(&url).expect("probe succeeds");
    assert_eq!(meta.dimensions(), (1, 1));

    server.join().unwrap();
  }

  #[cfg(feature = "direct_network")]
  #[test]
  fn image_probe_partial_fetch_handles_range_ignored() {
    use std::io::Write;
    use std::net::TcpListener;
    use std::time::Duration;

    let listener = match TcpListener::bind("127.0.0.1:0") {
      Ok(listener) => listener,
      Err(err)
        if matches!(
          err.kind(),
          std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::AddrNotAvailable
        ) =>
      {
        eprintln!(
          "skipping image_probe_partial_fetch_handles_range_ignored: cannot bind localhost: {err}"
        );
        return;
      }
      Err(err) => panic!("bind localhost: {err}"),
    };
    let addr = listener.local_addr().expect("listener addr");
    let url = format!("http://{addr}/probe.png");
    let body = padded_png();

    let server = std::thread::spawn(move || {
      let (mut stream, _) = listener.accept().expect("accept");
      let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
      let req = read_http_headers(&mut stream);

      let range = extract_range_header(&req).expect("missing Range header");
      let end = parse_range_end(&range).expect("invalid Range header");
      let prefix_len = (end + 1).min(body.len());
      let prefix = &body[..prefix_len];

      // Ignore Range and respond with 200 + the full content-length. Send only the prefix
      // immediately; if the client tried to read the entire body it would stall and hit the
      // request timeout below.
      let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
      );
      stream.write_all(header.as_bytes()).expect("write header");
      stream.write_all(prefix).expect("write prefix");
      let _ = stream.flush();
      std::thread::sleep(Duration::from_millis(500));
      let _ = stream.write_all(&body[prefix_len..]);
      let _ = stream.flush();
    });

    let cache = ImageCache::with_fetcher(Arc::new(
      HttpFetcher::new().with_timeout(Duration::from_millis(150)),
    ));
    let meta = cache.probe(&url).expect("probe succeeds");
    assert_eq!(meta.dimensions(), (1, 1));

    server.join().unwrap();
  }

  #[cfg(feature = "direct_network")]
  #[test]
  fn image_probe_partial_fetch_falls_back_on_http_405() {
    use std::io::Write;
    use std::net::TcpListener;
    use std::time::Duration;

    let listener = match TcpListener::bind("127.0.0.1:0") {
      Ok(listener) => listener,
      Err(err)
        if matches!(
          err.kind(),
          std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::AddrNotAvailable
        ) =>
      {
        eprintln!(
          "skipping image_probe_partial_fetch_falls_back_on_http_405: cannot bind localhost: {err}"
        );
        return;
      }
      Err(err) => panic!("bind localhost: {err}"),
    };
    let addr = listener.local_addr().expect("listener addr");
    let url = format!("http://{addr}/probe.png");
    let body = padded_png();

    let server = std::thread::spawn(move || {
      // First request should be the partial probe with a Range header.
      {
        let (mut stream, _) = listener.accept().expect("accept range request");
        let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
        let req = read_http_headers(&mut stream);
        assert!(
          extract_range_header(&req).is_some(),
          "expected Range header on probe request"
        );

        let header = "HTTP/1.1 405 Method Not Allowed\r\nContent-Type: text/html\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        stream.write_all(header.as_bytes()).expect("write header");
        let _ = stream.flush();
      }

      // Second request should be the fallback full fetch (no Range header).
      {
        let (mut stream, _) = listener.accept().expect("accept full request");
        let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
        let req = read_http_headers(&mut stream);
        assert!(
          extract_range_header(&req).is_none(),
          "unexpected Range header on fallback full fetch"
        );

        let header = format!(
          "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
          body.len()
        );
        stream.write_all(header.as_bytes()).expect("write header");
        stream.write_all(&body).expect("write body");
        let _ = stream.flush();
      }
    });

    enable_image_cache_diagnostics();
    let cache = ImageCache::with_fetcher(Arc::new(HttpFetcher::new()));
    let meta = cache.probe(&url).expect("probe succeeds");
    assert_eq!(meta.dimensions(), (1, 1));

    let stats = take_image_cache_diagnostics().expect("diagnostics enabled");
    assert_eq!(stats.probe_partial_requests, 1);
    assert_eq!(stats.probe_partial_fallback_full, 1);

    server.join().unwrap();
  }

  #[test]
  fn svgz_load_with_svg_content_type() {
    let url = "https://example.com/icon.svg";
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="20"></svg>"#;
    let svgz = gzip_bytes(svg.as_bytes());

    let mut res = FetchedResource::new(svgz, Some("image/svg+xml".to_string()));
    res.status = Some(200);
    let fetcher = MapFetcher::with_entries([(url.to_string(), res)]);
    let cache = ImageCache::with_fetcher(Arc::new(fetcher));

    let img = cache
      .load(url)
      .expect("load svgz content via svg content-type");
    assert!(img.is_vector);
    assert_eq!(img.dimensions(), (10, 20));
  }

  #[test]
  fn svgz_load_with_octet_stream_content_type() {
    let url = "https://example.com/icon.SVGZ?version=1";
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="20"></svg>"#;
    let svgz = gzip_bytes(svg.as_bytes());

    let mut res = FetchedResource::new(svgz, Some("application/octet-stream".to_string()));
    res.status = Some(200);
    let fetcher = MapFetcher::with_entries([(url.to_string(), res)]);
    let cache = ImageCache::with_fetcher(Arc::new(fetcher));

    let img = cache
      .load(url)
      .expect("load svgz content via .svgz URL + octet-stream content-type");
    assert!(img.is_vector);
    assert_eq!(img.dimensions(), (10, 20));
  }

  #[test]
  fn svgz_probe_metadata() {
    let url = "https://example.com/icon.svgz";
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="20"></svg>"#;
    let svgz = gzip_bytes(svg.as_bytes());

    let mut res = FetchedResource::new(svgz, Some("application/octet-stream".to_string()));
    res.status = Some(200);
    let fetcher = MapFetcher::with_entries([(url.to_string(), res)]);
    let cache = ImageCache::with_fetcher(Arc::new(fetcher));

    let meta = cache
      .probe_resolved(url)
      .expect("probe svgz content should succeed");
    assert!(meta.is_vector);
    assert_eq!(meta.dimensions(), (10, 20));
  }

  #[test]
  fn svgz_load_uses_final_url_suffix() {
    let requested = "https://example.com/icon";
    let final_url = "https://example.com/icon.svgz?redirect=1";
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="20"></svg>"#;
    let svgz = gzip_bytes(svg.as_bytes());

    let mut res = FetchedResource::new(svgz, Some("application/octet-stream".to_string()));
    res.status = Some(200);
    res.final_url = Some(final_url.to_string());
    let fetcher = MapFetcher::with_entries([(requested.to_string(), res)]);
    let cache = ImageCache::with_fetcher(Arc::new(fetcher));

    let img = cache
      .load(requested)
      .expect("load svgz content via final URL .svgz suffix");
    assert!(img.is_vector);
    assert_eq!(img.dimensions(), (10, 20));
  }

  #[test]
  fn svgz_probe_uses_final_url_suffix() {
    let requested = "https://example.com/icon";
    let final_url = "https://example.com/icon.svgz?redirect=1";
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="20"></svg>"#;
    let svgz = gzip_bytes(svg.as_bytes());

    let mut res = FetchedResource::new(svgz, Some("application/octet-stream".to_string()));
    res.status = Some(200);
    res.final_url = Some(final_url.to_string());
    let fetcher = MapFetcher::with_entries([(requested.to_string(), res)]);
    let cache = ImageCache::with_fetcher(Arc::new(fetcher));

    let meta = cache
      .probe_resolved(requested)
      .expect("probe svgz content via final URL .svgz suffix");
    assert!(meta.is_vector);
    assert_eq!(meta.dimensions(), (10, 20));
  }

  #[test]
  fn svgz_gzipped_non_svg_payload_is_not_misidentified() {
    let url = "https://example.com/bad.svgz";
    let svgz = gzip_bytes(b"not an svg");

    let mut res = FetchedResource::new(svgz, Some("application/octet-stream".to_string()));
    res.status = Some(200);
    let fetcher = MapFetcher::with_entries([(url.to_string(), res)]);
    let cache = ImageCache::with_fetcher(Arc::new(fetcher));

    assert!(
      cache.load(url).is_err(),
      "gzipped non-svg payload must not decode as SVG"
    );
    assert!(
      cache.probe_resolved(url).is_err(),
      "gzipped non-svg payload must not probe as SVG"
    );
  }

  #[test]
  fn svg_viewbox_renders_with_default_letterboxing() {
    let cache = ImageCache::new();
    let svg = "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 100 100'><rect width='100' height='100' fill='red'/></svg>";

    let pixmap = cache
      .render_svg_pixmap_at_size(svg, 200, 100, "test://svg", 1.0)
      .expect("render svg");

    let left = pixmap.pixel(10, 50).expect("left padding");
    assert_eq!(left.alpha(), 0, "letterboxed area should be transparent");

    let center = pixmap.pixel(100, 50).expect("center pixel");
    assert_eq!(
      (center.red(), center.green(), center.blue(), center.alpha()),
      (255, 0, 0, 255)
    );
  }

  #[test]
  fn svg_viewbox_none_stretches_to_viewport() {
    let cache = ImageCache::new();
    let svg = "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 100 100' preserveAspectRatio='none'><rect width='100' height='100' fill='red'/></svg>";

    let pixmap = cache
      .render_svg_pixmap_at_size(svg, 200, 100, "test://svg", 1.0)
      .expect("render svg");

    let left = pixmap.pixel(10, 50).expect("left pixel");
    assert_eq!(
      (left.red(), left.green(), left.blue(), left.alpha()),
      (255, 0, 0, 255)
    );
  }

  #[test]
  fn svg_viewbox_aligns_min_min_meet() {
    let cache = ImageCache::new();
    let svg = "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 100 100' preserveAspectRatio='xMinYMin meet'><rect width='100' height='100' fill='red'/></svg>";

    let pixmap = cache
      .render_svg_pixmap_at_size(svg, 200, 100, "test://svg", 1.0)
      .expect("render svg");

    let left = pixmap.pixel(10, 50).expect("left pixel");
    assert_eq!(left.alpha(), 255);

    let right = pixmap.pixel(190, 50).expect("right padding");
    assert_eq!(right.alpha(), 0);
  }

  #[test]
  fn svg_viewbox_slice_fills_viewport() {
    let cache = ImageCache::new();
    let svg = "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 100 100' preserveAspectRatio='xMidYMid slice'><circle cx='50' cy='50' r='50' fill='red'/></svg>";

    let pixmap = cache
      .render_svg_pixmap_at_size(svg, 200, 100, "test://svg", 1.0)
      .expect("render svg");

    let left = pixmap.pixel(10, 50).expect("left pixel");
    assert_eq!(left.alpha(), 255);
    assert_eq!(left.red(), 255);
  }

  #[test]
  fn inline_svg_renders_with_style_attributes_and_rgba_colors() {
    let cache = ImageCache::new();
    // Inline `<svg>` replaced elements are serialized with computed styles re-emitted via
    // `style=""` attributes. Ensure `rgba()` colors in those style attributes are accepted by the
    // SVG renderer.
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10" style="fill: none; color: rgba(66,84,102,1.000); font-family: sohne-var, &quot;Helvetica Neue&quot;, Arial, sans-serif"><rect width="10" height="10" style="fill: rgba(0,0,0,1.000)"/></svg>"#;

    let pixmap = cache
      .render_svg_pixmap_at_size(svg, 10, 10, "inline-svg", 1.0)
      .expect("render svg with style attributes");

    let pixel = pixmap.pixel(5, 5).expect("center pixel");
    assert_eq!(pixel.alpha(), 255);
    assert_eq!((pixel.red(), pixel.green(), pixel.blue()), (0, 0, 0));
  }

  #[test]
  fn simple_svg_fast_path_stroke_renders() {
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><path d="M0 0 L10 10" stroke="red" stroke-width="2" fill="none"/></svg>"#;
    let pixmap = try_render_simple_svg_pixmap(svg, 20, 20)
      .expect("render should not error")
      .expect("expected simple SVG to use fast-path");

    let pixel = pixmap.pixel(10, 10).expect("diagonal pixel");
    assert!(
      pixel.alpha() > 0,
      "expected diagonal stroke pixel to be non-transparent"
    );
    assert_eq!(pixel.green(), 0);
    assert_eq!(pixel.blue(), 0);
    assert!(
      pixel.red().abs_diff(pixel.alpha()) <= 1,
      "expected premultiplied red to match alpha (got rgba=({}, {}, {}, {}))",
      pixel.red(),
      pixel.green(),
      pixel.blue(),
      pixel.alpha()
    );
  }

  #[test]
  fn simple_svg_fast_path_dasharray_renders() {
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><path d="M0 5 L10 5" stroke="red" stroke-width="2" stroke-dasharray="2 2" fill="none"/></svg>"#;
    let pixmap = try_render_simple_svg_pixmap(svg, 20, 20)
      .expect("render should not error")
      .expect("expected simple SVG to use fast-path");

    let dash = pixmap.pixel(2, 10).expect("dash pixel");
    assert!(
      dash.alpha() > 0,
      "expected dash pixel to be non-transparent"
    );
    assert_eq!(dash.green(), 0);
    assert_eq!(dash.blue(), 0);
    assert!(
      dash.red().abs_diff(dash.alpha()) <= 1,
      "expected premultiplied red to match alpha (got rgba=({}, {}, {}, {}))",
      dash.red(),
      dash.green(),
      dash.blue(),
      dash.alpha()
    );

    let gap = pixmap.pixel(6, 10).expect("gap pixel");
    assert_eq!(
      (gap.red(), gap.green(), gap.blue(), gap.alpha()),
      (0, 0, 0, 0),
      "expected dash gap pixel to be transparent"
    );
  }

  #[test]
  fn simple_svg_fast_path_rejects_filter_attribute() {
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><path d="M0 0 L10 10" stroke="red" stroke-width="2" fill="none" filter="url(#f)"/></svg>"#;
    assert!(
      try_render_simple_svg_pixmap(svg, 20, 20)
        .expect("render should not error")
        .is_none(),
      "expected SVG filter attribute to force slow-path fallback"
    );
  }

  #[test]
  fn svg_width_height_set_intrinsic_size_and_ratio() {
    let cache = ImageCache::new();
    let svg = "<svg xmlns='http://www.w3.org/2000/svg' width='200' height='100'></svg>";
    let img = cache.render_svg(svg).expect("rendered");

    assert_eq!(img.width(), 200);
    assert_eq!(img.height(), 100);
    assert_eq!(
      img.intrinsic_ratio(OrientationTransform::IDENTITY),
      Some(2.0)
    );
    assert!(!img.aspect_ratio_none);
  }

  #[test]
  fn intrinsic_ratio_fallback_respects_orientation_transform() {
    let img = CachedImage {
      image: Arc::new(DynamicImage::ImageRgba8(RgbaImage::new(100, 200))),
      orientation: None,
      resolution: None,
      is_animated: false,
      has_alpha: true,
      is_vector: false,
      intrinsic_ratio: None,
      aspect_ratio_none: false,
      svg_content: None,
      svg_has_intrinsic_size: true,
    };

    assert_eq!(
      img.intrinsic_ratio(OrientationTransform::IDENTITY),
      Some(0.5)
    );
    assert_eq!(
      img.intrinsic_ratio(OrientationTransform {
        quarter_turns: 1,
        flip_x: false,
      }),
      Some(2.0)
    );
  }

  #[test]
  fn intrinsic_ratio_fallback_respects_orientation_transform_metadata() {
    let meta = CachedImageMetadata {
      width: 100,
      height: 200,
      orientation: None,
      resolution: None,
      is_vector: false,
      is_animated: false,
      intrinsic_ratio: None,
      aspect_ratio_none: false,
    };

    assert_eq!(
      meta.intrinsic_ratio(OrientationTransform::IDENTITY),
      Some(0.5)
    );
    assert_eq!(
      meta.intrinsic_ratio(OrientationTransform {
        quarter_turns: 1,
        flip_x: false,
      }),
      Some(2.0)
    );
  }

  #[test]
  fn svg_viewbox_defaults_to_300x150_with_ratio() {
    let cache = ImageCache::new();
    let svg = "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 40 20'></svg>";
    let img = cache.render_svg(svg).expect("rendered");

    assert_eq!(img.width(), 300);
    assert_eq!(img.height(), 150);
    assert_eq!(
      img.intrinsic_ratio(OrientationTransform::IDENTITY),
      Some(2.0)
    );
    assert!(!img.aspect_ratio_none);
  }

  #[test]
  fn svg_viewbox_doctype_preserves_intrinsic_ratio() {
    let svg = r#"<!DOCTYPE svg PUBLIC "-//W3C//DTD SVG 1.1//EN" "http://www.w3.org/Graphics/SVG/1.1/DTD/svg11.dtd"><svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 450 175"></svg>"#;

    let (meta_width, meta_height, ratio, aspect_ratio_none) =
      svg_intrinsic_metadata(svg, 16.0, 16.0).expect("parse svg intrinsic metadata");
    assert_eq!(meta_width, None);
    assert_eq!(meta_height, None);
    assert!(!aspect_ratio_none);
    assert!(
      (ratio.unwrap_or(0.0) - (450.0 / 175.0)).abs() < 1e-6,
      "expected ratio from viewBox with doctype"
    );
  }

  #[test]
  fn svg_viewbox_square_defaults_to_150x150_in_probe() {
    let cache = ImageCache::new();
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"></svg>"#;

    let meta = cache
      .probe_svg_content(svg, "inline viewBox-only svg")
      .expect("probe svg");
    assert_eq!(meta.width, 150);
    assert_eq!(meta.height, 150);
    assert_eq!(meta.intrinsic_ratio, Some(1.0));
    assert!(!meta.aspect_ratio_none);
  }

  #[test]
  fn svg_viewbox_square_defaults_to_150x150_in_render_svg_to_image() {
    let cache = ImageCache::new();
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"></svg>"#;

    let (image, ratio, aspect_none) = cache.render_svg_to_image(svg).expect("render svg");
    assert_eq!((image.width(), image.height()), (150, 150));
    assert_eq!(ratio, Some(1.0));
    assert!(!aspect_none);
  }

  #[test]
  fn render_svg_to_image_viewbox_only_defaults_to_150x150() {
    let cache = ImageCache::new();
    let svg = "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 100 100'><rect width='100' height='100' fill='red'/></svg>";
    let (image, _, _) = cache.render_svg_to_image(svg).expect("render svg");

    assert_eq!((image.width(), image.height()), (150, 150));

    let rgba = image.to_rgba8();
    assert_eq!(rgba.get_pixel(75, 75).0, [255, 0, 0, 255]);
    assert_eq!(
      rgba.get_pixel(10, 75).0,
      [255, 0, 0, 255],
      "viewBox-only SVG should not introduce padding when the intrinsic ratio matches the target"
    );
  }

  #[test]
  fn render_svg_to_image_unpremultiplies_alpha() {
    let cache = ImageCache::new();
    let svg = "<svg xmlns='http://www.w3.org/2000/svg' width='10' height='10'>\
      <rect width='10' height='10' fill='red' fill-opacity='0.5'/>\
    </svg>";
    let (image, _, _) = cache.render_svg_to_image(svg).expect("render svg");

    let rgba = image.to_rgba8();
    let px = rgba.get_pixel(5, 5).0;
    assert!(
      (1..=254).contains(&px[3]),
      "expected a semi-transparent alpha channel, got {px:?}"
    );
    assert!(
      px[0] >= 250 && px[1] <= 5 && px[2] <= 5,
      "expected straight/unpremultiplied red channel, got {px:?}"
    );
  }

  #[test]
  fn probe_svg_content_viewbox_only_defaults_to_150x150() {
    let cache = ImageCache::new();
    let svg = "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 100 100'><rect width='100' height='100' fill='red'/></svg>";

    let meta = cache
      .probe_svg_content(svg, "test://svg")
      .expect("probe svg content");
    assert_eq!(meta.width, 150);
    assert_eq!(meta.height, 150);
    assert_eq!(meta.intrinsic_ratio, Some(1.0));
  }

  #[test]
  fn svg_viewbox_only_reports_no_css_natural_dimensions() {
    let cache = ImageCache::new();
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"></svg>"#;
    let image = cache.render_svg(svg).expect("rendered");

    let (w, h) = image.css_natural_dimensions(
      OrientationTransform::IDENTITY,
      &crate::style::types::ImageResolution::default(),
      1.0,
      None,
    );
    assert_eq!(w, None);
    assert_eq!(h, None);
  }

  #[test]
  fn svg_with_explicit_size_reports_css_natural_dimensions() {
    let cache = ImageCache::new();
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="20" viewBox="0 0 40 20"></svg>"#;
    let image = cache.render_svg(svg).expect("rendered");

    let (w, h) = image.css_natural_dimensions(
      OrientationTransform::IDENTITY,
      &crate::style::types::ImageResolution::default(),
      1.0,
      None,
    );
    assert_eq!(w, Some(40.0));
    assert_eq!(h, Some(20.0));
  }

  #[test]
  fn render_svg_to_image_preserve_aspect_ratio_alignment_respected() {
    let cache = ImageCache::new();

    let svg_min = "<svg xmlns='http://www.w3.org/2000/svg' width='300' height='150' viewBox='0 0 100 100' preserveAspectRatio='xMinYMin meet'><rect width='100' height='100' fill='red'/></svg>";
    let (image, _, _) = cache.render_svg_to_image(svg_min).expect("render svg xMin");
    let rgba = image.to_rgba8();
    assert_eq!(rgba.get_pixel(10, 75).0, [255, 0, 0, 255]);
    assert_eq!(rgba.get_pixel(290, 75).0[3], 0);

    let svg_max = "<svg xmlns='http://www.w3.org/2000/svg' width='300' height='150' viewBox='0 0 100 100' preserveAspectRatio='xMaxYMin meet'><rect width='100' height='100' fill='red'/></svg>";
    let (image, _, _) = cache.render_svg_to_image(svg_max).expect("render svg xMax");
    let rgba = image.to_rgba8();
    assert_eq!(rgba.get_pixel(10, 75).0[3], 0);
    assert_eq!(rgba.get_pixel(290, 75).0, [255, 0, 0, 255]);
  }

  #[test]
  fn render_svg_to_image_preserve_aspect_ratio_slice_respected() {
    let cache = ImageCache::new();
    let svg = "<svg xmlns='http://www.w3.org/2000/svg' width='300' height='150' viewBox='0 0 100 100' preserveAspectRatio='xMidYMid slice'>\
      <rect x='0' y='0' width='100' height='10' fill='red'/>\
      <rect x='0' y='45' width='100' height='10' fill='blue'/>\
      <rect x='0' y='90' width='100' height='10' fill='green'/>\
    </svg>";
    let (image, _, _) = cache.render_svg_to_image(svg).expect("render svg slice");
    let rgba = image.to_rgba8();

    assert_eq!(rgba.get_pixel(150, 5).0[3], 0);
    assert_eq!(rgba.get_pixel(150, 75).0, [0, 0, 255, 255]);
    assert_eq!(rgba.get_pixel(150, 145).0[3], 0);
  }

  #[test]
  fn render_svg_to_image_viewbox_min_xy_translation_respected() {
    let cache = ImageCache::new();
    let svg =
      "<svg xmlns='http://www.w3.org/2000/svg' width='100' height='100' viewBox='50 50 100 100'>\
      <rect x='50' y='50' width='100' height='100' fill='red'/>\
    </svg>";
    let (image, _, _) = cache.render_svg_to_image(svg).expect("render svg");
    let rgba = image.to_rgba8();
    assert_eq!(rgba.get_pixel(10, 10).0, [255, 0, 0, 255]);
  }

  #[test]
  fn svg_preserve_aspect_ratio_none_disables_ratio() {
    let cache = ImageCache::new();
    let svg = "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 100 50' preserveAspectRatio='none'></svg>";
    let img = cache.render_svg(svg).expect("rendered");

    assert_eq!(img.width(), 300);
    assert_eq!(img.height(), 150);
    assert!(img.aspect_ratio_none);
    assert_eq!(img.intrinsic_ratio(OrientationTransform::IDENTITY), None);
  }

  #[test]
  fn svg_fast_path_without_viewbox_scales_non_uniformly() {
    let cache = ImageCache::new();
    let svg = "<svg xmlns='http://www.w3.org/2000/svg' width='100' height='100'>\
      <path d='M0 0 H100 V100 H0 Z' fill='red'/>\
    </svg>";

    let pixmap = cache
      .render_svg_pixmap_at_size(svg, 200, 100, "test://fast-path", 1.0)
      .expect("rendered pixmap");
    let pixel = pixmap.pixel(10, 50).expect("pixel");

    assert_eq!(
      (pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()),
      (255, 0, 0, 255)
    );
  }

  #[test]
  fn svg_percent_width_height_ignored_defaults_with_viewbox_ratio() {
    let cache = ImageCache::new();
    let svg = "<svg xmlns='http://www.w3.org/2000/svg' width='50%' height='25%' viewBox='0 0 200 100'></svg>";
    let img = cache.render_svg(svg).expect("rendered");

    // Percent lengths are ignored; fall back to 300x150 but keep the viewBox ratio (2:1).
    assert_eq!(img.width(), 300);
    assert_eq!(img.height(), 150);
    assert_eq!(
      img.intrinsic_ratio(OrientationTransform::IDENTITY),
      Some(2.0)
    );
  }

  #[test]
  fn svg_absolute_units_convert_to_px() {
    let cache = ImageCache::new();
    let svg = "<svg xmlns='http://www.w3.org/2000/svg' width='1in' height='0.5in'></svg>";

    let meta = cache
      .probe_svg_content(svg, "inline inches")
      .expect("probe inches svg");
    assert_eq!(meta.width, 96);
    assert_eq!(meta.height, 48);

    let img = cache.render_svg(svg).expect("rendered");
    assert_eq!(img.width(), 96);
    assert_eq!(img.height(), 48);
    assert_eq!(
      img.intrinsic_ratio(OrientationTransform::IDENTITY),
      Some(2.0)
    );
  }

  #[test]
  fn svg_em_units_use_default_font_size_when_probing() {
    let cache = ImageCache::new();
    let svg = "<svg xmlns='http://www.w3.org/2000/svg' width='1em' height='1em'></svg>";

    let meta = cache
      .probe_svg_content(svg, "inline em")
      .expect("probe em svg");
    assert_eq!(meta.width, 16);
    assert_eq!(meta.height, 16);
  }

  #[test]
  fn svg_metric_units_convert_to_px() {
    let cache = ImageCache::new();
    let svg = "<svg xmlns='http://www.w3.org/2000/svg' width='2.54cm' height='25.4mm'></svg>";

    let meta = cache
      .probe_svg_content(svg, "inline metric")
      .expect("probe metric svg");
    assert_eq!(meta.width, 96);
    assert_eq!(meta.height, 96);

    let img = cache.render_svg(svg).expect("rendered");
    assert_eq!(img.width(), 96);
    assert_eq!(img.height(), 96);
  }

  #[test]
  fn render_inline_svg_returns_image() {
    let cache = ImageCache::new();
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="5"></svg>"#;
    let (image, ratio, aspect_none) = cache.render_svg_to_image(svg).expect("svg render");
    assert_eq!(image.width(), 10);
    assert_eq!(image.height(), 5);
    assert_eq!(ratio, Some(2.0));
    assert!(!aspect_none);
  }

  #[test]
  fn non_ascii_whitespace_svg_text_looks_like_markup_does_not_trim_nbsp() {
    assert!(svg_text_looks_like_markup("<svg></svg>"));
    assert!(svg_text_looks_like_markup("\n\t<svg></svg>"));
    assert!(!svg_text_looks_like_markup("\u{00A0}<svg></svg>"));
  }

  #[test]
  fn render_svg_to_image_width_only_viewbox_preserves_aspect_ratio() {
    let cache = ImageCache::new();
    let svg = r#"
      <svg xmlns='http://www.w3.org/2000/svg' width='200' viewBox='0 0 100 100'>
        <rect x='0' y='0' width='100' height='100' fill='red'/>
      </svg>
    "#;
    let (image, _, _) = cache.render_svg_to_image(svg).expect("render svg");

    assert_eq!((image.width(), image.height()), (200, 200));
    let rgba = image.to_rgba8();
    assert_eq!(rgba.get_pixel(100, 10).0, [255, 0, 0, 255]);
    assert_eq!(rgba.get_pixel(100, 100).0, [255, 0, 0, 255]);
  }

  #[test]
  fn render_svg_to_image_respects_preserve_aspect_ratio_none_when_scaling() {
    let cache = ImageCache::new();
    let svg = r#"
      <svg xmlns='http://www.w3.org/2000/svg' width='200' viewBox='0 0 100 100' preserveAspectRatio='none'>
        <rect x='0' y='0' width='100' height='100' fill='red'/>
      </svg>
    "#;
    let (image, _, aspect_none) = cache.render_svg_to_image(svg).expect("render svg");

    assert!(aspect_none);
    assert_eq!((image.width(), image.height()), (200, 150));
    let rgba = image.to_rgba8();
    assert_eq!(
      rgba.get_pixel(100, 10).0,
      [255, 0, 0, 255],
      "preserveAspectRatio='none' should stretch to the top edge"
    );
  }

  #[test]
  fn render_svg_to_image_height_only_viewbox_preserves_aspect_ratio() {
    let cache = ImageCache::new();
    let svg = r#"
      <svg xmlns='http://www.w3.org/2000/svg' height='200' viewBox='0 0 100 100'>
        <rect x='0' y='0' width='100' height='100' fill='red'/>
      </svg>
    "#;
    let (image, _, _) = cache.render_svg_to_image(svg).expect("render svg");

    assert_eq!((image.width(), image.height()), (200, 200));
    let rgba = image.to_rgba8();
    assert_eq!(rgba.get_pixel(10, 100).0, [255, 0, 0, 255]);
    assert_eq!(rgba.get_pixel(100, 100).0, [255, 0, 0, 255]);
  }

  #[test]
  fn load_svg_data_url() {
    let cache = ImageCache::new();
    let data_url =
            "data:image/svg+xml,%3Csvg%20xmlns=%22http://www.w3.org/2000/svg%22%20width=%221%22%20height=%221%22%3E%3C/svg%3E";
    let image = cache.load(data_url).expect("decode data URL");
    assert_eq!(image.width(), 1);
    assert_eq!(image.height(), 1);
  }

  #[test]
  fn svg_fragment_identifier_renders_symbol_sprite() {
    let fetch_url = "https://example.test/sprite.svg";
    let url = "https://example.test/sprite.svg#icon";

    let sprite_svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><symbol id="icon" viewBox="0 0 1 1"><rect width="1" height="1" fill="red"/></symbol></svg>"#;

    let mut res = FetchedResource::new(
      sprite_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    res.status = Some(200);
    res.final_url = Some(fetch_url.to_string());

    let fetcher = MapFetcher::with_entries([(fetch_url.to_string(), res)]);
    let cache = ImageCache::with_fetcher(Arc::new(fetcher.clone()));

    let image = cache.load(url).expect("load svg fragment");
    assert!(image.is_vector);
    assert_eq!((image.width(), image.height()), (1, 1));
    let rgba = image.image.to_rgba8();
    assert_eq!(
      rgba.get_pixel(0, 0).0,
      [255, 0, 0, 255],
      "symbol-only sprite should render when fragment identifier is applied"
    );

    let pixmap = cache
      .render_svg_pixmap_at_size(sprite_svg, 1, 1, url, 1.0)
      .expect("render svg fragment pixmap");
    let pixel = pixmap.pixel(0, 0).expect("pixmap pixel");
    assert_eq!(
      (pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()),
      (255, 0, 0, 255)
    );

    let meta = cache.probe(url).expect("probe svg fragment");
    assert!(meta.is_vector);
    assert_eq!((meta.width, meta.height), (1, 1));

    for (req_url, _, _) in fetcher.requests() {
      assert!(
        !req_url.contains('#'),
        "SVG fetches must strip fragments; got request url {req_url}"
      );
    }
  }

  #[test]
  fn svg_fragment_identifier_renders_defs_g_element() {
    let fetch_url = "https://example.test/sprite.svg";
    let url = "https://example.test/sprite.svg#icon";

    let sprite_svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><defs><g id="icon"><rect width="1" height="1" fill="red"/></g></defs></svg>"#;

    let mut res = FetchedResource::new(
      sprite_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    res.status = Some(200);
    res.final_url = Some(fetch_url.to_string());

    let fetcher = MapFetcher::with_entries([(fetch_url.to_string(), res)]);
    let cache = ImageCache::with_fetcher(Arc::new(fetcher));

    let image = cache.load(url).expect("load svg fragment");
    assert!(image.is_vector);
    assert_eq!((image.width(), image.height()), (1, 1));
    let rgba = image.image.to_rgba8();
    assert_eq!(
      rgba.get_pixel(0, 0).0,
      [255, 0, 0, 255],
      "<g> sprite inside <defs> should render when fragment identifier is applied"
    );

    let pixmap = cache
      .render_svg_pixmap_at_size(sprite_svg, 1, 1, url, 1.0)
      .expect("render svg fragment pixmap");
    let pixel = pixmap.pixel(0, 0).expect("pixmap pixel");
    assert_eq!(
      (pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()),
      (255, 0, 0, 255)
    );
  }

  #[test]
  fn svg_fragment_identifier_prefixed_defs_g_renders() {
    let fetch_url = "https://example.test/sprite.svg";
    let url = "https://example.test/sprite.svg#icon";

    let sprite_svg = r#"<svg:svg xmlns:svg="http://www.w3.org/2000/svg" width="1" height="1"><svg:defs><svg:g id="icon"><svg:rect width="1" height="1" fill="red"/></svg:g></svg:defs></svg:svg>"#;

    let mut res = FetchedResource::new(
      sprite_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    res.status = Some(200);
    res.final_url = Some(fetch_url.to_string());

    let fetcher = MapFetcher::with_entries([(fetch_url.to_string(), res)]);
    let cache = ImageCache::with_fetcher(Arc::new(fetcher));

    let image = cache.load(url).expect("load svg fragment");
    assert!(image.is_vector);
    assert_eq!((image.width(), image.height()), (1, 1));
    let rgba = image.image.to_rgba8();
    assert_eq!(
      rgba.get_pixel(0, 0).0,
      [255, 0, 0, 255],
      "prefixed <g> sprite inside <defs> should render when fragment identifier is applied"
    );
  }

  #[test]
  fn svg_external_use_sprite_renders() {
    let sprite_url = "https://example.test/sprite.svg";
    let main_url = "https://example.test/main.svg";

    let sprite_svg = r#"<svg xmlns="http://www.w3.org/2000/svg"><symbol id="icon"><rect width="1" height="1" fill="red"/></symbol></svg>"#;
    let main_svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><use href="/sprite.svg#icon"/></svg>"#;

    let mut main_res = FetchedResource::new(
      main_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    main_res.status = Some(200);
    main_res.final_url = Some(main_url.to_string());

    let mut sprite_res = FetchedResource::new(
      sprite_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    sprite_res.status = Some(200);
    sprite_res.final_url = Some(sprite_url.to_string());

    let fetcher = MapFetcher::with_entries([
      (main_url.to_string(), main_res),
      (sprite_url.to_string(), sprite_res),
    ]);
    let cache = ImageCache::with_fetcher(Arc::new(fetcher));

    let image = cache.load(main_url).expect("load main svg");
    assert_eq!((image.width(), image.height()), (1, 1));
    let rgba = image.image.to_rgba8();
    assert_eq!(
      rgba.get_pixel(0, 0).0,
      [255, 0, 0, 255],
      "external <use href> sprite should inline and render red pixel"
    );
  }

  #[test]
  fn svg_external_use_sprite_prefixed_svg_renders() {
    let sprite_url = "https://example.test/sprite.svg";
    let main_url = "https://example.test/main.svg";

    let sprite_svg = r#"<svg:svg xmlns:svg="http://www.w3.org/2000/svg"><svg:symbol id="icon" viewBox="0 0 1 1"><svg:rect width="1" height="1" fill="red"/></svg:symbol></svg:svg>"#;
    let main_svg = r#"<svg:svg xmlns:svg="http://www.w3.org/2000/svg" width="1" height="1"><svg:use href="/sprite.svg#icon"/></svg:svg>"#;

    let mut main_res = FetchedResource::new(
      main_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    main_res.status = Some(200);
    main_res.final_url = Some(main_url.to_string());

    let mut sprite_res = FetchedResource::new(
      sprite_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    sprite_res.status = Some(200);
    sprite_res.final_url = Some(sprite_url.to_string());

    let fetcher = MapFetcher::with_entries([
      (main_url.to_string(), main_res),
      (sprite_url.to_string(), sprite_res),
    ]);
    let cache = ImageCache::with_fetcher(Arc::new(fetcher));

    let image = cache.load(main_url).expect("load main svg");
    assert_eq!((image.width(), image.height()), (1, 1));
    let rgba = image.image.to_rgba8();
    assert_eq!(
      rgba.get_pixel(0, 0).0,
      [255, 0, 0, 255],
      "prefixed external <use href> sprite should inline and render red pixel"
    );
  }

  #[test]
  fn svg_external_use_sprite_renders_with_xlink_href() {
    let sprite_url = "https://example.test/sprite.svg";
    let main_url = "https://example.test/main.svg";

    let sprite_svg = r#"<svg xmlns="http://www.w3.org/2000/svg"><symbol id="icon"><rect width="1" height="1" fill="red"/></symbol></svg>"#;
    let main_svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><use xlink:href="/sprite.svg#icon" xmlns:xlink="http://www.w3.org/1999/xlink"/></svg>"#;

    let mut main_res = FetchedResource::new(
      main_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    main_res.status = Some(200);
    main_res.final_url = Some(main_url.to_string());

    let mut sprite_res = FetchedResource::new(
      sprite_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    sprite_res.status = Some(200);
    sprite_res.final_url = Some(sprite_url.to_string());

    let fetcher = MapFetcher::with_entries([
      (main_url.to_string(), main_res),
      (sprite_url.to_string(), sprite_res),
    ]);
    let cache = ImageCache::with_fetcher(Arc::new(fetcher));

    let image = cache.load(main_url).expect("load main svg");
    assert_eq!((image.width(), image.height()), (1, 1));
    let rgba = image.image.to_rgba8();
    assert_eq!(
      rgba.get_pixel(0, 0).0,
      [255, 0, 0, 255],
      "external <use xlink:href> sprite should inline and render red pixel"
    );
  }

  #[test]
  fn svg_external_use_sprite_svgz_renders() {
    let sprite_url = "https://example.test/sprite.svgz";
    let main_url = "https://example.test/main.svg";

    let sprite_svg = r#"<svg xmlns="http://www.w3.org/2000/svg"><symbol id="icon"><rect width="1" height="1" fill="red"/></symbol></svg>"#;
    let sprite_svgz = gzip_bytes(sprite_svg.as_bytes());
    let main_svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><use href="/sprite.svgz#icon"/></svg>"#;

    let mut main_res = FetchedResource::new(
      main_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    main_res.status = Some(200);
    main_res.final_url = Some(main_url.to_string());

    let mut sprite_res =
      FetchedResource::new(sprite_svgz, Some("application/octet-stream".to_string()));
    sprite_res.status = Some(200);
    sprite_res.final_url = Some(sprite_url.to_string());

    let fetcher = MapFetcher::with_entries([
      (main_url.to_string(), main_res),
      (sprite_url.to_string(), sprite_res),
    ]);
    let cache = ImageCache::with_fetcher(Arc::new(fetcher));

    let image = cache.load(main_url).expect("load main svg");
    assert_eq!((image.width(), image.height()), (1, 1));
    let rgba = image.image.to_rgba8();
    assert_eq!(
      rgba.get_pixel(0, 0).0,
      [255, 0, 0, 255],
      "external <use> sprite should inline and render red pixel when sprite is gzipped"
    );
  }

  #[test]
  fn svg_external_image_href_renders() {
    let main_url = "https://example.test/main.svg";
    let img_url = "https://example.test/img.png";

    let main_svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><image href="/img.png" width="1" height="1"/></svg>"#;

    let mut main_res = FetchedResource::new(
      main_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    main_res.status = Some(200);
    main_res.final_url = Some(main_url.to_string());

    let mut img_res = FetchedResource::new(
      encode_single_pixel_png([255, 0, 0, 255]),
      Some("image/png".to_string()),
    );
    img_res.status = Some(200);
    img_res.final_url = Some(img_url.to_string());

    let fetcher = MapFetcher::with_entries([
      (main_url.to_string(), main_res),
      (img_url.to_string(), img_res),
    ]);
    let cache = ImageCache::with_fetcher(Arc::new(fetcher));

    let image = cache.load(main_url).expect("load main svg");
    assert_eq!((image.width(), image.height()), (1, 1));
    let rgba = image.image.to_rgba8();
    assert_eq!(
      rgba.get_pixel(0, 0).0,
      [255, 0, 0, 255],
      "external <image href> should inline into a data URL and render red pixel"
    );
  }

  #[test]
  fn svg_external_image_href_renders_with_child_elements() {
    let main_url = "https://example.test/main.svg";
    let img_url = "https://example.test/img.png";

    let main_svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><image href="/img.png" width="1" height="1"><desc>hi</desc></image></svg>"#;

    let mut main_res = FetchedResource::new(
      main_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    main_res.status = Some(200);
    main_res.final_url = Some(main_url.to_string());

    let mut img_res = FetchedResource::new(
      encode_single_pixel_png([255, 0, 0, 255]),
      Some("image/png".to_string()),
    );
    img_res.status = Some(200);
    img_res.final_url = Some(img_url.to_string());

    let fetcher = MapFetcher::with_entries([
      (main_url.to_string(), main_res),
      (img_url.to_string(), img_res),
    ]);
    let cache = ImageCache::with_fetcher(Arc::new(fetcher));

    let image = cache.load(main_url).expect("load main svg");
    assert_eq!((image.width(), image.height()), (1, 1));
    let rgba = image.image.to_rgba8();
    assert_eq!(
      rgba.get_pixel(0, 0).0,
      [255, 0, 0, 255],
      "external <image href> should inline even when <image> has child elements"
    );
  }

  #[test]
  fn svg_external_feimage_href_renders() {
    let main_url = "https://example.test/main.svg";
    let img_url = "https://example.test/img.png";

    let main_svg = r#"
      <svg xmlns="http://www.w3.org/2000/svg" width="1" height="1">
        <filter id="f" x="0" y="0" width="1" height="1" filterUnits="userSpaceOnUse">
          <feImage href="/img.png" x="0" y="0" width="1" height="1" preserveAspectRatio="none"/>
        </filter>
        <rect width="1" height="1" filter="url(#f)"/>
      </svg>
    "#;

    let mut main_res = FetchedResource::new(
      main_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    main_res.status = Some(200);
    main_res.final_url = Some(main_url.to_string());

    let mut img_res = FetchedResource::new(
      encode_single_pixel_png([255, 0, 0, 255]),
      Some("image/png".to_string()),
    );
    img_res.status = Some(200);
    img_res.final_url = Some(img_url.to_string());

    let fetcher = MapFetcher::with_entries([
      (main_url.to_string(), main_res),
      (img_url.to_string(), img_res),
    ]);
    let cache = ImageCache::with_fetcher(Arc::new(fetcher));

    let image = cache.load(main_url).expect("load main svg");
    assert_eq!((image.width(), image.height()), (1, 1));
    let rgba = image.image.to_rgba8();
    assert_eq!(
      rgba.get_pixel(0, 0).0,
      [255, 0, 0, 255],
      "external <feImage href> should inline into a data URL and render red pixel"
    );
  }

  #[test]
  fn svg_external_feimage_xlink_href_renders() {
    let main_url = "https://example.test/main.svg";
    let img_url = "https://example.test/img.png";

    let main_svg = r#"
      <svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="1" height="1">
        <filter id="f" x="0" y="0" width="1" height="1" filterUnits="userSpaceOnUse">
          <feImage xlink:href="/img.png" x="0" y="0" width="1" height="1" preserveAspectRatio="none"/>
        </filter>
        <rect width="1" height="1" filter="url(#f)"/>
      </svg>
    "#;

    let mut main_res = FetchedResource::new(
      main_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    main_res.status = Some(200);
    main_res.final_url = Some(main_url.to_string());

    let mut img_res = FetchedResource::new(
      encode_single_pixel_png([255, 0, 0, 255]),
      Some("image/png".to_string()),
    );
    img_res.status = Some(200);
    img_res.final_url = Some(img_url.to_string());

    let fetcher = MapFetcher::with_entries([
      (main_url.to_string(), main_res),
      (img_url.to_string(), img_res),
    ]);
    let cache = ImageCache::with_fetcher(Arc::new(fetcher));

    let image = cache.load(main_url).expect("load main svg");
    assert_eq!((image.width(), image.height()), (1, 1));
    let rgba = image.image.to_rgba8();
    assert_eq!(
      rgba.get_pixel(0, 0).0,
      [255, 0, 0, 255],
      "external <feImage xlink:href> should inline into a data URL and render red pixel"
    );
  }

  #[test]
  fn svg_external_image_xlink_href_renders() {
    let main_url = "https://example.test/main.svg";
    let img_url = "https://example.test/img.png";

    let main_svg = r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="1" height="1"><image xlink:href="/img.png" width="1" height="1"/></svg>"#;

    let mut main_res = FetchedResource::new(
      main_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    main_res.status = Some(200);
    main_res.final_url = Some(main_url.to_string());

    let mut img_res = FetchedResource::new(
      encode_single_pixel_png([255, 0, 0, 255]),
      Some("image/png".to_string()),
    );
    img_res.status = Some(200);
    img_res.final_url = Some(img_url.to_string());

    let fetcher = MapFetcher::with_entries([
      (main_url.to_string(), main_res),
      (img_url.to_string(), img_res),
    ]);
    let cache = ImageCache::with_fetcher(Arc::new(fetcher));

    let image = cache.load(main_url).expect("load main svg");
    assert_eq!((image.width(), image.height()), (1, 1));
    let rgba = image.image.to_rgba8();
    assert_eq!(
      rgba.get_pixel(0, 0).0,
      [255, 0, 0, 255],
      "external <image xlink:href> should inline into a data URL and render red pixel"
    );
  }

  #[test]
  fn svg_external_image_href_fragment_preserved() {
    let main_url = "https://example.test/main.svg";
    let img_url = "https://example.test/img.png";

    let main_svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><image href="/img.png#frag" width="1" height="1"/></svg>"#;

    let mut main_res = FetchedResource::new(
      main_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    main_res.status = Some(200);
    main_res.final_url = Some(main_url.to_string());

    let mut img_res = FetchedResource::new(
      encode_single_pixel_png([255, 0, 0, 255]),
      Some("image/png".to_string()),
    );
    img_res.status = Some(200);
    img_res.final_url = Some(img_url.to_string());

    let fetcher = MapFetcher::with_entries([
      (main_url.to_string(), main_res),
      (img_url.to_string(), img_res),
    ]);
    let cache = ImageCache::with_fetcher(Arc::new(fetcher.clone()));

    let image = cache.load(main_url).expect("load main svg");
    assert_eq!((image.width(), image.height()), (1, 1));
    let rgba = image.image.to_rgba8();
    assert_eq!(
      rgba.get_pixel(0, 0).0,
      [255, 0, 0, 255],
      "external <image href> should inline and render even when the URL has a fragment"
    );

    let requests = fetcher.requests();
    assert!(
      requests.iter().any(|(url, _, _)| url == img_url),
      "expected fragment-less subresource fetch request"
    );
    assert!(
      requests
        .iter()
        .all(|(url, _, _)| url != &format!("{img_url}#frag")),
      "fetch URL must not include fragments"
    );
  }

  #[test]
  fn svg_external_image_href_renders_without_content_type() {
    let main_url = "https://example.test/main.svg";
    let img_url = "https://example.test/img.png";

    let main_svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><image href="/img.png" width="1" height="1"/></svg>"#;

    let mut main_res = FetchedResource::new(
      main_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    main_res.status = Some(200);
    main_res.final_url = Some(main_url.to_string());

    // Some endpoints omit Content-Type for images. The SVG preprocessor should sniff common formats
    // so the generated `data:` URL uses an image/* mime that resvg/usvg accept.
    let mut img_res = FetchedResource::new(encode_single_pixel_png([255, 0, 0, 255]), None);
    img_res.status = Some(200);
    img_res.final_url = Some(img_url.to_string());

    let fetcher = MapFetcher::with_entries([
      (main_url.to_string(), main_res),
      (img_url.to_string(), img_res),
    ]);
    let cache = ImageCache::with_fetcher(Arc::new(fetcher));

    let image = cache.load(main_url).expect("load main svg");
    assert_eq!((image.width(), image.height()), (1, 1));
    let rgba = image.image.to_rgba8();
    assert_eq!(
      rgba.get_pixel(0, 0).0,
      [255, 0, 0, 255],
      "external <image href> should render even when the fetched image has no Content-Type"
    );
  }

  #[test]
  fn svg_external_image_blocked_by_policy() {
    let doc_url = "https://example.test/";
    let main_url = "https://example.test/main.svg";
    let img_url = "https://cross.test/img.png";

    let main_svg = format!(
      r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><image href="{img_url}" width="1" height="1"/></svg>"#
    );

    let mut main_res = FetchedResource::new(
      main_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    main_res.status = Some(200);
    main_res.final_url = Some(main_url.to_string());

    let mut img_res = FetchedResource::new(
      encode_single_pixel_png([255, 0, 0, 255]),
      Some("image/png".to_string()),
    );
    img_res.status = Some(200);
    img_res.final_url = Some(img_url.to_string());

    let fetcher = MapFetcher::with_entries([
      (main_url.to_string(), main_res),
      (img_url.to_string(), img_res),
    ]);

    let mut cache = ImageCache::with_fetcher(Arc::new(fetcher.clone()));
    let doc_origin = crate::resource::origin_from_url(doc_url).expect("document origin");
    let mut ctx = ResourceContext::default();
    ctx.document_url = Some(doc_url.to_string());
    ctx.policy.document_origin = Some(doc_origin);
    ctx.policy.same_origin_only = true;
    cache.set_resource_context(Some(ctx));

    let load_err = match cache.load(main_url) {
      Ok(_) => panic!("cross-origin SVG image href should be blocked"),
      Err(err) => err,
    };
    match load_err {
      Error::Image(ImageError::LoadFailed { reason, .. }) => {
        assert!(
          reason.contains("Blocked cross-origin subresource"),
          "unexpected policy reason: {reason}"
        );
      }
      other => panic!("expected ImageError::LoadFailed, got {other:?}"),
    }

    let probe_err = match cache.probe(main_url) {
      Ok(_) => panic!("cross-origin SVG image href should be blocked during probe"),
      Err(err) => err,
    };
    match probe_err {
      Error::Image(ImageError::LoadFailed { reason, .. }) => {
        assert!(
          reason.contains("Blocked cross-origin subresource"),
          "unexpected policy reason: {reason}"
        );
      }
      other => panic!("expected ImageError::LoadFailed, got {other:?}"),
    }

    assert!(
      fetcher.requests().iter().all(|(url, _, _)| url != img_url),
      "blocked image should not be fetched"
    );
  }

  #[test]
  fn svg_anchor_href_not_blocked_by_policy() {
    let doc_url = "https://example.test/";
    let main_url = "https://example.test/main.svg";
    let cross_url = "https://cross.test/";

    let main_svg = format!(
      r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><a href="{cross_url}"><rect width="1" height="1" fill="red"/></a></svg>"#
    );

    let mut main_res = FetchedResource::new(
      main_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    main_res.status = Some(200);
    main_res.final_url = Some(main_url.to_string());

    let fetcher = MapFetcher::with_entries([(main_url.to_string(), main_res)]);

    let mut cache = ImageCache::with_fetcher(Arc::new(fetcher.clone()));
    let doc_origin = crate::resource::origin_from_url(doc_url).expect("document origin");
    let mut ctx = ResourceContext::default();
    ctx.document_url = Some(doc_url.to_string());
    ctx.policy.document_origin = Some(doc_origin);
    ctx.policy.same_origin_only = true;
    cache.set_resource_context(Some(ctx));

    let image = cache
      .load(main_url)
      .expect("SVG with <a href> should not be blocked");
    assert_eq!((image.width(), image.height()), (1, 1));
    let rgba = image.image.to_rgba8();
    assert_eq!(
      rgba.get_pixel(0, 0).0,
      [255, 0, 0, 255],
      "<a href> hyperlink should not affect rendered output"
    );

    assert!(
      fetcher
        .requests()
        .iter()
        .all(|(url, _, _)| url != cross_url),
      "hyperlink should not be fetched"
    );
  }

  #[test]
  fn inline_svg_use_includes_sprite_defs_dependencies() {
    let sprite_url = "https://example.test/sprite.svg";
    let main_url = "https://example.test/main.svg";

    let sprite_svg = r#"<svg xmlns="http://www.w3.org/2000/svg">
      <defs>
        <linearGradient id="g">
          <stop offset="0" stop-color="red"/>
          <stop offset="1" stop-color="red"/>
        </linearGradient>
      </defs>
      <symbol id="icon" viewBox="0 0 1 1">
        <rect width="1" height="1" fill="url(#g)"/>
      </symbol>
    </svg>"#;
    let mut sprite_res = FetchedResource::new(
      sprite_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    sprite_res.status = Some(200);
    sprite_res.final_url = Some(sprite_url.to_string());

    let fetcher = MapFetcher::with_entries([(sprite_url.to_string(), sprite_res)]);

    let main_svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><use href="/sprite.svg#icon" width="1" height="1"/></svg>"#;
    let expanded =
      inline_svg_use_references(main_svg, main_url, &fetcher, None, None).expect("expand");
    assert!(
      expanded.contains("linearGradient") && expanded.contains("id=\"g\""),
      "expected sprite <defs> dependency to be injected, got: {expanded}"
    );

    let cache = ImageCache::with_fetcher(Arc::new(fetcher));
    let pixmap = cache
      .render_svg_pixmap_at_size(main_svg, 1, 1, main_url, 1.0)
      .expect("rendered pixmap");
    let pixel = pixmap.pixel(0, 0).expect("pixel");
    assert_eq!(
      (pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()),
      (255, 0, 0, 255),
      "expected gradient-painted icon to render red after <use> expansion"
    );
  }

  #[test]
  fn inline_svg_use_skips_display_none() {
    let sprite_url = "https://example.test/sprite.svg";
    let main_url = "https://example.test/main.svg";
    let sprite_svg = r#"<svg xmlns="http://www.w3.org/2000/svg"><symbol id="icon"><rect width="1" height="1" fill="red"/></symbol></svg>"#;
    let mut sprite_res = FetchedResource::new(
      sprite_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    sprite_res.status = Some(200);
    sprite_res.final_url = Some(sprite_url.to_string());
    let fetcher = MapFetcher::with_entries([(sprite_url.to_string(), sprite_res)]);

    let main_svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><use href="https://example.test/sprite.svg#icon" style="display:none" /></svg>"#;
    let expanded =
      inline_svg_use_references(main_svg, main_url, &fetcher, None, None).expect("expand");
    assert_eq!(expanded.as_ref(), main_svg);
    assert!(
      fetcher.requests().is_empty(),
      "expected no fetches for display:none <use>"
    );
  }

  #[test]
  fn non_ascii_whitespace_inline_svg_use_references_does_not_trim_nbsp_in_sprite_id() {
    let nbsp = "\u{00A0}";
    let sprite_url = "https://example.test/sprite.svg";
    let main_url = "https://example.test/main.svg";

    let sprite_svg = format!(
      r#"<svg xmlns="http://www.w3.org/2000/svg"><symbol id="{nbsp}icon"><rect width="1" height="1" fill="red"/></symbol></svg>"#
    );
    let mut sprite_res = FetchedResource::new(
      sprite_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    sprite_res.status = Some(200);
    sprite_res.final_url = Some(sprite_url.to_string());

    let fetcher = MapFetcher::with_entries([(sprite_url.to_string(), sprite_res)]);

    let main_svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><use href="/sprite.svg#icon"/></svg>"#;
    let expanded =
      inline_svg_use_references(main_svg, main_url, &fetcher, None, None).expect("expand");
    assert_eq!(
      expanded.as_ref(),
      main_svg,
      "NBSP must not be treated as whitespace when indexing sprite ids for <use> expansion"
    );
  }

  #[test]
  fn inline_svg_use_references_propagates_render_errors_from_fetcher() {
    struct RenderErrorFetcher;

    impl ResourceFetcher for RenderErrorFetcher {
      fn fetch(&self, _url: &str) -> crate::error::Result<FetchedResource> {
        Err(Error::Render(RenderError::Timeout {
          stage: RenderStage::Paint,
          elapsed: Duration::from_millis(0),
        }))
      }
    }

    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg"><use href="https://example.test/sprite.svg#icon"/></svg>"#;
    let err = inline_svg_use_references(
      svg,
      "https://example.test/main.svg",
      &RenderErrorFetcher,
      None,
      None,
    )
    .expect_err("expected render error to propagate");
    assert!(
      matches!(
        err,
        Error::Render(RenderError::Timeout {
          stage: RenderStage::Paint,
          ..
        })
      ),
      "expected paint-stage render timeout, got {err:?}"
    );
  }

  #[test]
  fn svg_external_use_sprite_blocked_by_policy() {
    let doc_url = "https://example.test/";
    let main_url = "https://example.test/main.svg";
    let sprite_url = "https://cross.test/sprite.svg";

    let main_svg = format!(
      r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><use href="{sprite_url}#icon"/></svg>"#
    );
    let sprite_svg = r#"<svg xmlns="http://www.w3.org/2000/svg"><symbol id="icon"><rect width="1" height="1" fill="red"/></symbol></svg>"#;

    let mut main_res = FetchedResource::new(
      main_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    main_res.status = Some(200);
    main_res.final_url = Some(main_url.to_string());

    let mut sprite_res = FetchedResource::new(
      sprite_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    sprite_res.status = Some(200);
    sprite_res.final_url = Some(sprite_url.to_string());

    let fetcher = MapFetcher::with_entries([
      (main_url.to_string(), main_res),
      (sprite_url.to_string(), sprite_res),
    ]);

    let mut cache = ImageCache::with_fetcher(Arc::new(fetcher.clone()));
    let doc_origin = crate::resource::origin_from_url(doc_url).expect("document origin");
    let mut ctx = ResourceContext::default();
    ctx.document_url = Some(doc_url.to_string());
    ctx.policy.document_origin = Some(doc_origin);
    ctx.policy.same_origin_only = true;
    cache.set_resource_context(Some(ctx));

    let err = match cache.load(main_url) {
      Ok(_) => panic!("cross-origin sprite should be blocked"),
      Err(err) => err,
    };
    match err {
      Error::Image(ImageError::LoadFailed { reason, .. }) => {
        assert!(
          reason.contains("Blocked cross-origin subresource"),
          "unexpected policy reason: {reason}"
        );
      }
      other => panic!("expected ImageError::LoadFailed, got {other:?}"),
    }

    assert!(
      fetcher
        .requests()
        .iter()
        .all(|(url, _, _)| url != sprite_url),
      "blocked sprite should not be fetched"
    );
  }

  #[test]
  fn svg_external_use_sprite_redirect_blocked_by_policy() {
    let doc_url = "https://example.test/";
    let main_url = "https://example.test/main.svg";
    let sprite_url = "https://example.test/sprite.svg";
    let sprite_final_url = "https://cross.test/sprite.svg";

    let main_svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><use href="/sprite.svg#icon"/></svg>"#;
    let sprite_svg = r#"<svg xmlns="http://www.w3.org/2000/svg"><symbol id="icon"><rect width="1" height="1" fill="red"/></symbol></svg>"#;

    let mut main_res = FetchedResource::new(
      main_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    main_res.status = Some(200);
    main_res.final_url = Some(main_url.to_string());

    let mut sprite_res = FetchedResource::new(
      sprite_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    sprite_res.status = Some(200);
    sprite_res.final_url = Some(sprite_final_url.to_string());

    let fetcher = MapFetcher::with_entries([
      (main_url.to_string(), main_res),
      (sprite_url.to_string(), sprite_res),
    ]);

    let mut cache = ImageCache::with_fetcher(Arc::new(fetcher));
    let doc_origin = crate::resource::origin_from_url(doc_url).expect("document origin");
    let mut ctx = ResourceContext::default();
    ctx.document_url = Some(doc_url.to_string());
    ctx.policy.document_origin = Some(doc_origin);
    ctx.policy.same_origin_only = true;
    cache.set_resource_context(Some(ctx));

    let err = match cache.load(main_url) {
      Ok(_) => panic!("redirected cross-origin sprite should be blocked"),
      Err(err) => err,
    };
    match err {
      Error::Image(ImageError::LoadFailed { reason, .. }) => {
        assert!(
          reason.contains("Blocked cross-origin subresource"),
          "unexpected policy reason: {reason}"
        );
        assert!(
          reason.contains(sprite_final_url),
          "policy reason should mention final URL: {reason}"
        );
      }
      other => panic!("expected ImageError::LoadFailed, got {other:?}"),
    }
  }

  #[test]
  fn inline_svg_use_external_without_fragment_renders() {
    let icon_url = "https://example.test/icon.svg";
    let main_url = "https://example.test/main.svg";

    let icon_svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><rect width="1" height="1" fill="red"/></svg>"#;
    let mut icon_res = FetchedResource::new(
      icon_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    icon_res.status = Some(200);
    icon_res.final_url = Some(icon_url.to_string());

    let fetcher = MapFetcher::with_entries([(icon_url.to_string(), icon_res)]);
    let cache = ImageCache::with_fetcher(Arc::new(fetcher.clone()));

    let main_svg = format!(
      r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><use href="{icon_url}"/></svg>"#
    );

    let pixmap = cache
      .render_svg_pixmap_at_size(&main_svg, 1, 1, main_url, 1.0)
      .expect("rendered pixmap");
    let pixel = pixmap.pixel(0, 0).expect("pixel");
    assert_eq!(
      (pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()),
      (255, 0, 0, 255)
    );

    let requests = fetcher.requests();
    assert_eq!(
      requests.len(),
      1,
      "expected exactly one fetch request for the external SVG, got: {requests:?}"
    );
    assert_eq!(requests[0].0, icon_url);
    assert_eq!(requests[0].1, FetchDestination::Image);
  }

  #[test]
  fn inline_svg_external_use_sprite_uses_document_url_as_base() {
    let doc_url = "https://example.test/page.html";
    let sprite_url = "https://example.test/sprite.svg";

    let sprite_svg = r#"<svg xmlns="http://www.w3.org/2000/svg"><symbol id="icon"><rect width="1" height="1" fill="red"/></symbol></svg>"#;
    let main_svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><use href="/sprite.svg#icon"/></svg>"#;

    let mut sprite_res = FetchedResource::new(
      sprite_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    sprite_res.status = Some(200);
    sprite_res.final_url = Some(sprite_url.to_string());

    let fetcher = MapFetcher::with_entries([(sprite_url.to_string(), sprite_res)]);
    let mut cache = ImageCache::with_fetcher(Arc::new(fetcher.clone()));
    let doc_origin = crate::resource::origin_from_url(doc_url).expect("document origin");
    let mut ctx = ResourceContext::default();
    ctx.document_url = Some(doc_url.to_string());
    ctx.policy.document_origin = Some(doc_origin);
    cache.set_resource_context(Some(ctx));

    let pixmap = cache
      .render_svg_pixmap_at_size(main_svg, 1, 1, "inline-svg", 1.0)
      .expect("rendered pixmap");
    let pixel = pixmap.pixel(0, 0).expect("pixel");
    assert_eq!(
      (pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()),
      (255, 0, 0, 255)
    );
  }

  #[test]
  fn inline_svg_external_use_sprite_uses_xml_base_as_base() {
    let doc_url = "https://example.test/page.html";
    let sprite_url = "https://example.test/assets/sprite.svg";

    let sprite_svg = r#"<svg xmlns="http://www.w3.org/2000/svg"><symbol id="icon"><rect width="1" height="1" fill="red"/></symbol></svg>"#;
    let main_svg = r#"<svg xmlns="http://www.w3.org/2000/svg" xml:base="assets/" width="1" height="1"><use href="sprite.svg#icon"/></svg>"#;

    let mut sprite_res = FetchedResource::new(
      sprite_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    sprite_res.status = Some(200);
    sprite_res.final_url = Some(sprite_url.to_string());

    let fetcher = MapFetcher::with_entries([(sprite_url.to_string(), sprite_res)]);
    let mut cache = ImageCache::with_fetcher(Arc::new(fetcher.clone()));
    let doc_origin = crate::resource::origin_from_url(doc_url).expect("document origin");
    let mut ctx = ResourceContext::default();
    ctx.document_url = Some(doc_url.to_string());
    ctx.policy.document_origin = Some(doc_origin);
    cache.set_resource_context(Some(ctx));

    let pixmap = cache
      .render_svg_pixmap_at_size(main_svg, 1, 1, "inline-svg", 1.0)
      .expect("rendered pixmap");

    let requests = fetcher.requests();
    assert!(
      requests
        .iter()
        .any(|(url, dest, _)| url == sprite_url && *dest == FetchDestination::Image),
      "expected fetch for xml:base sprite href {sprite_url}, got: {requests:?}"
    );

    let pixel = pixmap.pixel(0, 0).expect("pixel");
    assert_eq!(
      (pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()),
      (255, 0, 0, 255)
    );
  }

  #[test]
  fn inline_svg_external_use_sprite_inlines_sprite_images_with_sprite_base() {
    let doc_url = "https://example.test/page.html";
    let sprite_url = "https://example.test/assets/sprite.svg";
    let img_url = "https://example.test/assets/img.png";

    let sprite_svg = r#"<svg xmlns="http://www.w3.org/2000/svg"><symbol id="icon"><image href="img.png" width="1" height="1"/></symbol></svg>"#;
    let main_svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><use href="/assets/sprite.svg#icon"/></svg>"#;

    let mut sprite_res = FetchedResource::new(
      sprite_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    sprite_res.status = Some(200);
    sprite_res.final_url = Some(sprite_url.to_string());

    let mut img_res = FetchedResource::new(
      encode_single_pixel_png([255, 0, 0, 255]),
      Some("image/png".to_string()),
    );
    img_res.status = Some(200);
    img_res.final_url = Some(img_url.to_string());

    let fetcher = MapFetcher::with_entries([
      (sprite_url.to_string(), sprite_res),
      (img_url.to_string(), img_res),
    ]);
    let mut cache = ImageCache::with_fetcher(Arc::new(fetcher.clone()));
    let doc_origin = crate::resource::origin_from_url(doc_url).expect("document origin");
    let mut ctx = ResourceContext::default();
    ctx.document_url = Some(doc_url.to_string());
    ctx.policy.document_origin = Some(doc_origin);
    cache.set_resource_context(Some(ctx));

    let pixmap = cache
      .render_svg_pixmap_at_size(main_svg, 1, 1, "inline-svg", 1.0)
      .expect("rendered pixmap");

    let requests = fetcher.requests();
    assert!(
      requests
        .iter()
        .any(|(url, dest, _)| url == sprite_url && *dest == FetchDestination::Image),
      "expected fetch for sprite URL {sprite_url}, got: {requests:?}"
    );
    assert!(
      requests
        .iter()
        .any(|(url, dest, _)| url == img_url && *dest == FetchDestination::Image),
      "expected fetch for sprite-nested image URL {img_url}, got: {requests:?}"
    );

    let pixel = pixmap.pixel(0, 0).expect("pixel");
    assert_eq!(
      (pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()),
      (255, 0, 0, 255)
    );
  }

  #[test]
  fn svg_external_url_fragment_inlines_nested_image_relative_to_external_doc() {
    let main_url = "https://example.test/b/main.svg";
    let defs_url = "https://example.test/a/defs.svg";
    let img_url = "https://example.test/a/img.png";

    let defs_svg = r#"
      <svg xmlns="http://www.w3.org/2000/svg" xml:base="./">
        <defs>
          <pattern id="p" patternUnits="userSpaceOnUse" width="1" height="1">
            <image href="img.png" width="1" height="1"/>
          </pattern>
        </defs>
      </svg>
    "#;

    let main_svg = format!(
      r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><rect width="1" height="1" fill="url({defs_url}#p)"/></svg>"#
    );

    let mut main_res = FetchedResource::new(
      main_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    main_res.status = Some(200);
    main_res.final_url = Some(main_url.to_string());

    let mut defs_res = FetchedResource::new(
      defs_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    defs_res.status = Some(200);
    defs_res.final_url = Some(defs_url.to_string());

    let mut img_res = FetchedResource::new(
      encode_single_pixel_png([255, 0, 0, 255]),
      Some("image/png".to_string()),
    );
    img_res.status = Some(200);
    img_res.final_url = Some(img_url.to_string());

    let fetcher = MapFetcher::with_entries([
      (main_url.to_string(), main_res),
      (defs_url.to_string(), defs_res),
      (img_url.to_string(), img_res),
    ]);
    let cache = ImageCache::with_fetcher(Arc::new(fetcher.clone()));

    let image = cache
      .load(main_url)
      .expect("load main svg with external url(#id)");
    assert_eq!((image.width(), image.height()), (1, 1));
    let rgba = image.image.to_rgba8();
    assert_eq!(
      rgba.get_pixel(0, 0).0,
      [255, 0, 0, 255],
      "external pattern fill should resolve nested <image> relative to the external defs document"
    );

    let requests = fetcher.requests();
    assert!(
      requests.iter().any(|(url, _, _)| url == img_url),
      "expected fetch for nested external image {img_url}, got: {requests:?}"
    );
    assert!(
      requests
        .iter()
        .all(|(url, _, _)| url != "https://example.test/b/img.png"),
      "nested image must not resolve relative to the host SVG URL, got: {requests:?}"
    );
  }

  #[test]
  fn inline_svg_image_references_inlines_file_scheme_when_called_from_use_inliner() {
    let tmp = tempdir().expect("tempdir");
    let icons_dir = tmp.path().join("icons");
    std::fs::create_dir_all(&icons_dir).expect("create icons dir");

    let sprite_path = icons_dir.join("sprite.svg");
    let img_path = icons_dir.join("img.png");
    let main_path = tmp.path().join("main.svg");

    let sprite_url = Url::from_file_path(&sprite_path)
      .expect("sprite.svg file URL")
      .to_string();
    let img_url = Url::from_file_path(&img_path)
      .expect("img.png file URL")
      .to_string();
    let main_url = Url::from_file_path(&main_path)
      .expect("main.svg file URL")
      .to_string();

    let sprite_svg = r#"<svg xmlns="http://www.w3.org/2000/svg"><symbol id="icon"><image href="img.png" width="1" height="1"/></symbol></svg>"#;
    let main_svg = format!(
      r#"<svg xmlns="http://www.w3.org/2000/svg"><use href="{}#icon"/></svg>"#,
      sprite_url
    );

    let mut sprite_res = FetchedResource::new(
      sprite_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    sprite_res.status = Some(200);
    sprite_res.final_url = Some(sprite_url.to_string());

    let mut img_res = FetchedResource::new(
      encode_single_pixel_png([255, 0, 0, 255]),
      Some("image/png".to_string()),
    );
    img_res.status = Some(200);
    img_res.final_url = Some(img_url.to_string());

    let fetcher = MapFetcher::with_entries([
      (sprite_url.to_string(), sprite_res),
      (img_url.to_string(), img_res),
    ]);

    let inlined =
      inline_svg_use_references(&main_svg, &main_url, &fetcher, None, None).expect("inlined svg");
    assert!(
      inlined.as_ref().contains("data:image/png;base64,"),
      "expected sprite-nested <image> to be inlined for file:// sprites, got: {}",
      inlined.as_ref()
    );
  }

  #[test]
  fn svg_style_import_rewrites_relative_url_tokens_to_absolute() {
    let svg_url = "https://example.test/b/main.svg";
    let css_url = "https://example.test/a/style.css";

    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg"><style>@import url("../a/style.css");</style><rect class="r" width="10" height="10"/></svg>"#;

    let css = b".r{fill:url(img.svg#grad);}";
    let mut css_res = FetchedResource::new(css.to_vec(), Some("text/css".to_string()));
    css_res.status = Some(200);
    css_res.final_url = Some(css_url.to_string());

    let fetcher = MapFetcher::with_entries([(css_url.to_string(), css_res)]);

    let processed =
      inline_svg_style_imports(svg, svg_url, &fetcher, None).expect("inlined style imports");

    assert!(
      processed
        .as_ref()
        .contains("https://example.test/a/img.svg#grad"),
      "expected imported CSS url() to be absolutized, got: {}",
      processed.as_ref()
    );

    let requests = fetcher.requests();
    assert!(
      requests
        .iter()
        .any(|(url, dest, _)| url == css_url && *dest == FetchDestination::Style),
      "expected stylesheet fetch for {css_url}, got: {requests:?}"
    );
  }

  #[test]
  fn svg_style_import_fetch_uses_svg_url_as_referrer_when_parseable() {
    let svg_url = "https://example.test/main.svg";
    let style_url = "https://example.test/style.css";
    let svg =
      r#"<svg xmlns="http://www.w3.org/2000/svg"><style>@import url("style.css");</style></svg>"#;

    let mut css_res = FetchedResource::new(b"/* empty */".to_vec(), Some("text/css".to_string()));
    css_res.status = Some(200);
    css_res.final_url = Some(style_url.to_string());

    let fetcher = RecordingFetcher::with_entries([(style_url.to_string(), css_res)]);

    let _processed =
      inline_svg_style_imports(svg, svg_url, &fetcher, None).expect("inlined style imports");

    let requests = fetcher.requests();
    assert!(
      requests.iter().any(|req| {
        req.url == style_url
          && req.destination == FetchDestination::Style
          && req.referrer_url.as_deref() == Some(svg_url)
      }),
      "expected stylesheet fetch for {style_url} to use referrer_url={svg_url}, got: {requests:?}"
    );
  }

  #[test]
  fn svg_style_import_nested_fetch_uses_importer_final_url_as_referrer() {
    let svg_url = "https://example.test/main.svg";
    let requested_url = "https://example.test/style.css";
    let final_url = "https://example.test/assets/style.css";
    let nested_url = "https://example.test/assets/nested.css";
    let svg =
      r#"<svg xmlns="http://www.w3.org/2000/svg"><style>@import url("style.css");</style></svg>"#;

    let mut redirected_res = FetchedResource::new(
      b"@import \"nested.css\";".to_vec(),
      Some("text/css".to_string()),
    );
    redirected_res.status = Some(200);
    redirected_res.final_url = Some(final_url.to_string());

    let mut nested_res =
      FetchedResource::new(b"/* nested */".to_vec(), Some("text/css".to_string()));
    nested_res.status = Some(200);
    nested_res.final_url = Some(nested_url.to_string());

    let fetcher = RecordingFetcher::with_entries([
      (requested_url.to_string(), redirected_res),
      (nested_url.to_string(), nested_res),
    ]);

    let _processed =
      inline_svg_style_imports(svg, svg_url, &fetcher, None).expect("inlined style imports");

    let requests = fetcher.requests();
    assert!(
      requests.iter().any(|req| {
        req.url == requested_url
          && req.destination == FetchDestination::Style
          && req.referrer_url.as_deref() == Some(svg_url)
      }),
      "expected stylesheet fetch for {requested_url} to use referrer_url={svg_url}, got: {requests:?}"
    );
    assert!(
      requests.iter().any(|req| {
        req.url == nested_url
          && req.destination == FetchDestination::Style
          && req.referrer_url.as_deref() == Some(final_url)
      }),
      "expected nested stylesheet fetch for {nested_url} to use referrer_url={final_url}, got: {requests:?}"
    );
  }

  #[test]
  fn svg_style_import_fetch_uses_document_url_as_referrer_when_svg_url_is_not_a_url() {
    let svg_url = "inline-svg";
    let doc_url = "https://example.test/page.html";
    let style_url = "https://example.test/style.css";
    let svg =
      r#"<svg xmlns="http://www.w3.org/2000/svg"><style>@import url("style.css");</style></svg>"#;

    let mut css_res = FetchedResource::new(b"/* empty */".to_vec(), Some("text/css".to_string()));
    css_res.status = Some(200);
    css_res.final_url = Some(style_url.to_string());

    let fetcher = RecordingFetcher::with_entries([(style_url.to_string(), css_res)]);

    let mut ctx = ResourceContext::default();
    ctx.document_url = Some(doc_url.to_string());

    let _processed =
      inline_svg_style_imports(svg, svg_url, &fetcher, Some(&ctx)).expect("inlined style imports");

    let requests = fetcher.requests();
    assert!(
      requests.iter().any(|req| {
        req.url == style_url
          && req.destination == FetchDestination::Style
          && req.referrer_url.as_deref() == Some(doc_url)
      }),
      "expected stylesheet fetch for {style_url} to use referrer_url={doc_url}, got: {requests:?}"
    );
  }

  #[test]
  fn svg_subresource_fetches_use_svg_url_as_referrer_when_parseable() {
    let svg_url = "https://example.test/sprite.svg";
    let doc_url = "https://example.test/page.html";
    let img_url = "https://example.test/img.png";

    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1">
      <image href="img.png" width="1" height="1"/>
    </svg>"#;

    let mut img_res = FetchedResource::new(
      encode_single_pixel_png([0, 0, 0, 255]),
      Some("image/png".to_string()),
    );
    img_res.status = Some(200);
    img_res.final_url = Some(img_url.to_string());

    let fetcher = RecordingFetcher::with_entries([(img_url.to_string(), img_res)]);
    let mut ctx = ResourceContext::default();
    ctx.document_url = Some(doc_url.to_string());

    let _ = inline_svg_image_references(svg, svg_url, &fetcher, Some(&ctx), None)
      .expect("inline svg <image>");

    let requests = fetcher.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url, img_url);
    assert_eq!(requests[0].destination, FetchDestination::Image);
    assert_eq!(requests[0].referrer_url.as_deref(), Some(svg_url));
  }

  #[test]
  fn svg_use_sprite_fetch_uses_host_svg_url_as_referrer() {
    let main_svg_url = "https://example.test/main.svg";
    let sprite_url = "https://example.test/sprite.svg";
    let doc_url = "https://example.test/page.html";

    let main_svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1">
      <use href="sprite.svg#icon" width="1" height="1"/>
    </svg>"#;
    let sprite_svg = r#"<svg xmlns="http://www.w3.org/2000/svg">
      <symbol id="icon"><rect width="1" height="1" fill="red"/></symbol>
    </svg>"#;

    let mut sprite_res = FetchedResource::new(
      sprite_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    sprite_res.status = Some(200);
    sprite_res.final_url = Some(sprite_url.to_string());

    let fetcher = RecordingFetcher::with_entries([(sprite_url.to_string(), sprite_res)]);
    let mut ctx = ResourceContext::default();
    ctx.document_url = Some(doc_url.to_string());

    let _ = inline_svg_use_references(main_svg, main_svg_url, &fetcher, Some(&ctx), None)
      .expect("inline external <use>");

    let requests = fetcher.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url, sprite_url);
    assert_eq!(requests[0].destination, FetchDestination::Image);
    assert_eq!(requests[0].referrer_url.as_deref(), Some(main_svg_url));
  }

  #[test]
  fn svg_external_url_fragment_fetch_uses_host_svg_url_as_referrer() {
    let main_svg_url = "https://example.test/main.svg";
    let defs_url = "https://example.test/defs.svg";
    let doc_url = "https://example.test/page.html";

    let main_svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1">
      <rect width="1" height="1" fill="url(defs.svg#p)"/>
    </svg>"#;

    let defs_svg = r#"<svg xmlns="http://www.w3.org/2000/svg">
      <defs><pattern id="p"><rect width="1" height="1"/></pattern></defs>
    </svg>"#;

    let mut defs_res = FetchedResource::new(
      defs_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    defs_res.status = Some(200);
    defs_res.final_url = Some(defs_url.to_string());

    let fetcher = RecordingFetcher::with_entries([(defs_url.to_string(), defs_res)]);
    let mut ctx = ResourceContext::default();
    ctx.document_url = Some(doc_url.to_string());

    let _ = inline_svg_external_url_fragment_references(
      main_svg,
      main_svg_url,
      &fetcher,
      Some(&ctx),
      None,
    )
    .expect("inline external url(#id)");

    let requests = fetcher.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url, defs_url);
    assert_eq!(requests[0].destination, FetchDestination::Image);
    assert_eq!(requests[0].referrer_url.as_deref(), Some(main_svg_url));
  }

  #[test]
  fn svg_subresource_fetches_fall_back_to_document_url_when_svg_url_invalid() {
    let svg_url = "inline-svg";
    let doc_url = "https://example.test/page.html";
    let img_url = "https://example.test/img.png";

    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1">
      <image href="img.png" width="1" height="1"/>
    </svg>"#;

    let mut img_res = FetchedResource::new(
      encode_single_pixel_png([0, 0, 0, 255]),
      Some("image/png".to_string()),
    );
    img_res.status = Some(200);
    img_res.final_url = Some(img_url.to_string());

    let fetcher = RecordingFetcher::with_entries([(img_url.to_string(), img_res)]);
    let mut ctx = ResourceContext::default();
    ctx.document_url = Some(doc_url.to_string());

    let _ = inline_svg_image_references(svg, svg_url, &fetcher, Some(&ctx), None)
      .expect("inline svg <image>");

    let requests = fetcher.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url, img_url);
    assert_eq!(requests[0].destination, FetchDestination::Image);
    assert_eq!(requests[0].referrer_url.as_deref(), Some(doc_url));
  }

  #[test]
  fn svg_external_image_href_is_fetched_and_renders() {
    let main_url = "https://example.test/main.svg";
    let img_url = "https://example.test/img.png";

    let main_svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><image href="/img.png" width="1" height="1"/></svg>"#;

    let mut main_res = FetchedResource::new(
      main_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    main_res.status = Some(200);
    main_res.final_url = Some(main_url.to_string());

    let mut img_res = FetchedResource::new(
      encode_single_pixel_png([255, 0, 0, 255]),
      Some("image/png".to_string()),
    );
    img_res.status = Some(200);
    img_res.final_url = Some(img_url.to_string());

    let fetcher = MapFetcher::with_entries([
      (main_url.to_string(), main_res),
      (img_url.to_string(), img_res),
    ]);

    let cache = ImageCache::with_fetcher(Arc::new(fetcher.clone()));
    let image = cache.load(main_url).expect("main svg should render");
    assert_eq!(image.dimensions(), (1, 1));
    let rgba = image.image.to_rgba8();
    assert_eq!(rgba.get_pixel(0, 0).0, [255, 0, 0, 255]);

    let requests = fetcher.requests();
    assert!(
      requests
        .iter()
        .any(|(url, dest, _)| url == img_url && *dest == FetchDestination::Image),
      "expected fetch for image href {img_url}, got: {requests:?}"
    );
  }

  #[test]
  fn svg_image_inliner_caches_data_urls_across_sizes() {
    let main_url = "https://example.test/main.svg";
    let img_url = "https://example.test/img.png";

    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="2" height="2"><image href="https://example.test/img.png" width="2" height="2"/></svg>"#;

    let mut img_res = FetchedResource::new(
      encode_single_pixel_png([255, 0, 0, 255]),
      Some("image/png".to_string()),
    );
    img_res.status = Some(200);
    img_res.final_url = Some(img_url.to_string());

    let fetcher = MapFetcher::with_entries([(img_url.to_string(), img_res)]);
    let cache = ImageCache::with_fetcher(Arc::new(fetcher.clone()));

    cache
      .render_svg_pixmap_at_size(svg, 1, 1, main_url, 1.0)
      .expect("render at 1x1");
    cache
      .render_svg_pixmap_at_size(svg, 2, 2, main_url, 1.0)
      .expect("render at 2x2");

    let requests = fetcher.requests();
    let fetches = requests
      .iter()
      .filter(|(url, dest, _)| url == img_url && *dest == FetchDestination::Image)
      .count();
    assert_eq!(
      fetches, 1,
      "expected a single FetchDestination::Image request for {img_url}, got: {requests:?}"
    );
  }

  #[test]
  fn svg_use_inliner_caches_external_sprite_across_sizes() {
    let main_url = "https://example.test/main.svg";
    let sprite_url = "https://example.test/sprite.svg";

    let main_svg = format!(
      r#"<svg xmlns="http://www.w3.org/2000/svg" width="2" height="2"><use href="{sprite_url}#icon" width="2" height="2"/></svg>"#
    );
    let sprite_svg = r#"<svg xmlns="http://www.w3.org/2000/svg"><symbol id="icon" viewBox="0 0 1 1"><rect width="1" height="1" fill="red"/></symbol></svg>"#;

    let mut sprite_res = FetchedResource::new(
      sprite_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    sprite_res.status = Some(200);
    sprite_res.final_url = Some(sprite_url.to_string());

    let fetcher = MapFetcher::with_entries([(sprite_url.to_string(), sprite_res)]);
    let cache = ImageCache::with_fetcher(Arc::new(fetcher.clone()));

    cache
      .render_svg_pixmap_at_size(&main_svg, 1, 1, main_url, 1.0)
      .expect("render at 1x1");
    cache
      .render_svg_pixmap_at_size(&main_svg, 2, 2, main_url, 1.0)
      .expect("render at 2x2");

    let requests = fetcher.requests();
    let fetches = requests
      .iter()
      .filter(|(url, dest, _)| url == sprite_url && *dest == FetchDestination::Image)
      .count();
    assert_eq!(
      fetches, 1,
      "expected a single FetchDestination::Image request for {sprite_url}, got: {requests:?}"
    );
  }

  #[test]
  fn svg_preprocess_cache_reuse_survives_multiple_calls() {
    let main_url = "https://example.test/main.svg";
    let sprite_url = "https://example.test/sprite.svg";
    let img_url = "https://example.test/img.png";

    let main_svg = format!(
      r#"<svg xmlns="http://www.w3.org/2000/svg" width="2" height="2"><use href="{sprite_url}#icon" width="2" height="2"/><image href="{img_url}" width="2" height="2"/></svg>"#
    );
    let sprite_svg = r#"<svg xmlns="http://www.w3.org/2000/svg"><symbol id="icon" viewBox="0 0 1 1"><rect width="1" height="1" fill="red"/></symbol></svg>"#;

    let mut sprite_res = FetchedResource::new(
      sprite_svg.as_bytes().to_vec(),
      Some("image/svg+xml".to_string()),
    );
    sprite_res.status = Some(200);
    sprite_res.final_url = Some(sprite_url.to_string());

    let mut img_res = FetchedResource::new(
      encode_single_pixel_png([255, 0, 0, 255]),
      Some("image/png".to_string()),
    );
    img_res.status = Some(200);
    img_res.final_url = Some(img_url.to_string());

    let fetcher = MapFetcher::with_entries([
      (sprite_url.to_string(), sprite_res),
      (img_url.to_string(), img_res),
    ]);
    let cache = ImageCache::with_fetcher(Arc::new(fetcher.clone()));

    cache
      .render_svg_pixmap_at_size(&main_svg, 1, 1, main_url, 1.0)
      .expect("render at 1x1");
    cache
      .render_svg_pixmap_at_size(&main_svg, 2, 2, main_url, 1.0)
      .expect("render at 2x2");

    let requests = fetcher.requests();
    let sprite_fetches = requests
      .iter()
      .filter(|(url, dest, _)| url == sprite_url && *dest == FetchDestination::Image)
      .count();
    let img_fetches = requests
      .iter()
      .filter(|(url, dest, _)| url == img_url && *dest == FetchDestination::Image)
      .count();
    assert_eq!(
      sprite_fetches, 1,
      "expected sprite to be fetched once, got: {requests:?}"
    );
    assert_eq!(
      img_fetches, 1,
      "expected image to be fetched once, got: {requests:?}"
    );
  }

  #[test]
  fn svg_style_import_applies_rules() {
    let main_url = "https://example.test/main.svg";
    let style_url = "https://example.test/style.css";

    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><style>@import url("style.css");</style><rect class="r" width="1" height="1" fill="blue"/></svg>"#;

    let mut css_res = FetchedResource::new(
      b".r{fill:red !important;}".to_vec(),
      Some("text/css".to_string()),
    );
    css_res.status = Some(200);
    css_res.final_url = Some(style_url.to_string());

    let fetcher = MapFetcher::with_entries([(style_url.to_string(), css_res)]);
    let cache = ImageCache::with_fetcher(Arc::new(fetcher.clone()));

    let pixmap = cache
      .render_svg_pixmap_at_size(svg, 1, 1, main_url, 1.0)
      .expect("rendered pixmap");
    let pixel = pixmap.pixel(0, 0).expect("pixel");
    assert_eq!(
      (pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()),
      (255, 0, 0, 255)
    );

    let requests = fetcher.requests();
    assert_eq!(
      requests.len(),
      1,
      "expected exactly one fetch for the imported stylesheet, got: {requests:?}"
    );
    assert_eq!(requests[0].0, style_url);
    assert_eq!(requests[0].1, FetchDestination::Style);
  }

  #[test]
  fn svg_style_import_works_with_cdata_and_literal_angle_brackets() {
    let main_url = "https://example.test/main.svg";
    let style_url = "https://example.test/style.css";

    // `<style>` content can appear inside CDATA sections in the wild. CDATA allows literal `<`
    // characters, so the import inliner must not treat those as XML tag boundaries when patching
    // the original SVG source string.
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><style><![CDATA[@import url("style.css"); /* < */]]></style><rect class="r" width="1" height="1" fill="blue"/></svg>"#;

    let mut css_res = FetchedResource::new(
      b".r{fill:red !important;}".to_vec(),
      Some("text/css".to_string()),
    );
    css_res.status = Some(200);
    css_res.final_url = Some(style_url.to_string());

    let fetcher = MapFetcher::with_entries([(style_url.to_string(), css_res)]);
    let cache = ImageCache::with_fetcher(Arc::new(fetcher.clone()));

    let pixmap = cache
      .render_svg_pixmap_at_size(svg, 1, 1, main_url, 1.0)
      .expect("rendered pixmap");
    let pixel = pixmap.pixel(0, 0).expect("pixel");
    assert_eq!(
      (pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()),
      (255, 0, 0, 255)
    );
  }

  #[test]
  fn svg_style_import_respects_xml_base() {
    let main_url = "https://example.test/main.svg";
    let style_url = "https://example.test/assets/theme.css";

    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" xml:base="assets/" width="1" height="1"><style>@import "theme.css";</style><rect class="r" width="1" height="1" fill="blue"/></svg>"#;

    let mut css_res = FetchedResource::new(
      b".r{fill:red !important;}".to_vec(),
      Some("text/css".to_string()),
    );
    css_res.status = Some(200);
    css_res.final_url = Some(style_url.to_string());

    let fetcher = MapFetcher::with_entries([(style_url.to_string(), css_res)]);
    let cache = ImageCache::with_fetcher(Arc::new(fetcher.clone()));

    let pixmap = cache
      .render_svg_pixmap_at_size(svg, 1, 1, main_url, 1.0)
      .expect("rendered pixmap");
    let pixel = pixmap.pixel(0, 0).expect("pixel");
    assert_eq!(
      (pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()),
      (255, 0, 0, 255)
    );

    let requests = fetcher.requests();
    assert!(
      requests
        .iter()
        .any(|(url, dest, _)| url == style_url && *dest == FetchDestination::Style),
      "expected fetch for xml:base-resolved stylesheet URL {style_url}, got: {requests:?}"
    );
  }

  #[test]
  fn svg_style_import_chain_resolves_relative_to_import_url() {
    let main_url = "https://example.test/main.svg";
    let a_url = "https://example.test/a/a.css";
    let b_url = "https://example.test/a/b.css";

    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><style>@import url("a/a.css");</style><rect class="r" width="1" height="1" fill="blue"/></svg>"#;

    let mut a_res = FetchedResource::new(
      b"@import \"b.css\"; .r{fill:red !important;}".to_vec(),
      Some("text/css".to_string()),
    );
    a_res.status = Some(200);
    a_res.final_url = Some(a_url.to_string());

    let mut b_res = FetchedResource::new(
      b".r{fill:green !important;}".to_vec(),
      Some("text/css".to_string()),
    );
    b_res.status = Some(200);
    b_res.final_url = Some(b_url.to_string());

    let fetcher =
      MapFetcher::with_entries([(a_url.to_string(), a_res), (b_url.to_string(), b_res)]);
    let cache = ImageCache::with_fetcher(Arc::new(fetcher.clone()));

    let _pixmap = cache
      .render_svg_pixmap_at_size(svg, 1, 1, main_url, 1.0)
      .expect("rendered pixmap");

    let requests = fetcher.requests();
    assert!(
      requests
        .iter()
        .any(|(url, dest, _)| url == a_url && *dest == FetchDestination::Style),
      "expected fetch for first imported stylesheet URL {a_url}, got: {requests:?}"
    );
    assert!(
      requests
        .iter()
        .any(|(url, dest, _)| url == b_url && *dest == FetchDestination::Style),
      "expected nested @import to resolve relative to {a_url} (fetch {b_url}), got: {requests:?}"
    );
  }

  #[test]
  fn svg_style_import_chain_resolves_relative_to_import_final_url_after_redirect() {
    let main_url = "https://example.test/main.svg";
    let requested_url = "https://example.test/style.css";
    let final_url = "https://example.test/assets/style.css";
    let nested_url = "https://example.test/assets/nested.css";

    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><style>@import url("style.css");</style><rect class="r" width="1" height="1" fill="blue"/></svg>"#;

    let mut redirected_res = FetchedResource::new(
      b"@import \"nested.css\";".to_vec(),
      Some("text/css".to_string()),
    );
    redirected_res.status = Some(200);
    redirected_res.final_url = Some(final_url.to_string());

    let mut nested_res = FetchedResource::new(
      b".r{fill:red !important;}".to_vec(),
      Some("text/css".to_string()),
    );
    nested_res.status = Some(200);
    nested_res.final_url = Some(nested_url.to_string());

    let fetcher = MapFetcher::with_entries([
      (requested_url.to_string(), redirected_res),
      (nested_url.to_string(), nested_res),
    ]);
    let cache = ImageCache::with_fetcher(Arc::new(fetcher.clone()));

    let pixmap = cache
      .render_svg_pixmap_at_size(svg, 1, 1, main_url, 1.0)
      .expect("rendered pixmap");
    let pixel = pixmap.pixel(0, 0).expect("pixel");
    assert_eq!(
      (pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()),
      (255, 0, 0, 255)
    );

    let requests = fetcher.requests();
    assert!(
      requests
        .iter()
        .any(|(url, dest, _)| url == requested_url && *dest == FetchDestination::Style),
      "expected fetch for first imported stylesheet URL {requested_url}, got: {requests:?}"
    );
    assert!(
      requests
        .iter()
        .any(|(url, dest, _)| url == nested_url && *dest == FetchDestination::Style),
      "expected nested @import to resolve relative to final URL {final_url} (fetch {nested_url}), got: {requests:?}"
    );
  }

  #[test]
  fn svg_style_import_policy_checks_final_url_after_redirect() {
    let doc_url = "https://example.test/page.html";
    let main_url = "https://example.test/main.svg";
    let requested_url = "https://example.test/style.css";
    let final_url = "https://cross.test/style.css";

    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><style>@import url("style.css");</style><rect class="r" width="1" height="1" fill="blue"/></svg>"#;

    // Redirect to a cross-origin final URL; strict same-origin policy should block after fetch.
    let mut css_res = FetchedResource::new(
      b".r{fill:red !important;}".to_vec(),
      Some("text/css".to_string()),
    );
    css_res.status = Some(200);
    css_res.final_url = Some(final_url.to_string());

    let fetcher = MapFetcher::with_entries([(requested_url.to_string(), css_res)]);
    let mut cache = ImageCache::with_fetcher(Arc::new(fetcher.clone()));
    let doc_origin = origin_from_url(doc_url).expect("document origin");
    let mut ctx = ResourceContext::default();
    ctx.document_url = Some(doc_url.to_string());
    ctx.policy.document_origin = Some(doc_origin);
    ctx.policy.same_origin_only = true;
    cache.set_resource_context(Some(ctx));

    let err = cache
      .render_svg_pixmap_at_size(svg, 1, 1, main_url, 1.0)
      .expect_err("expected redirect-to-cross-origin stylesheet to be blocked");
    match err {
      Error::Image(ImageError::LoadFailed { url, reason }) => {
        assert_eq!(url, requested_url);
        assert!(
          reason.contains("Blocked cross-origin subresource") && reason.contains(final_url),
          "unexpected policy reason: {reason}"
        );
      }
      other => panic!("expected ImageError::LoadFailed, got {other:?}"),
    }

    let requests = fetcher.requests();
    assert!(
      requests
        .iter()
        .any(|(url, dest, _)| url == requested_url && *dest == FetchDestination::Style),
      "expected stylesheet fetch for {requested_url} before final-url policy check, got: {requests:?}"
    );
  }

  #[test]
  fn svg_style_import_policy_injected_style_respects_xml_base_for_cache_safety() {
    let doc_url = "https://doc.test/page.html";
    let svg_url = "inline-svg";
    let css_url = "https://cross.test/a.css";

    // The injected `<style>` element is inserted under the SVG root. Its import resolution should
    // inherit the SVG's xml:base, and policy enforcement must take that into account even for
    // cached pixmaps.
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" xml:base="//cross.test/" width="1" height="1"><rect class="r" width="1" height="1" fill="blue"/></svg>"#;
    let insert_pos = svg.find('>').expect("svg root tag end") + 1;
    let style_element = r#"<style>@import "a.css";</style>"#;

    let mut css_res = FetchedResource::new(
      b".r{fill:red !important;}".to_vec(),
      Some("text/css".to_string()),
    );
    css_res.status = Some(200);
    css_res.final_url = Some(css_url.to_string());

    let fetcher = MapFetcher::with_entries([(css_url.to_string(), css_res)]);
    let mut cache = ImageCache::with_fetcher(Arc::new(fetcher.clone()));

    // First render: allow cross-origin so the pixmap is populated + cached.
    let doc_origin = origin_from_url(doc_url).expect("document origin");
    let mut ctx = ResourceContext::default();
    ctx.document_url = Some(doc_url.to_string());
    ctx.policy.document_origin = Some(doc_origin.clone());
    ctx.policy.same_origin_only = false;
    cache.set_resource_context(Some(ctx));

    let pixmap = cache
      .render_svg_pixmap_at_size_with_injected_style(
        svg,
        insert_pos,
        style_element,
        1,
        1,
        svg_url,
        1.0,
      )
      .expect("rendered pixmap with injected style");
    let pixel = pixmap.pixel(0, 0).expect("pixel");
    assert_eq!(
      (pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()),
      (255, 0, 0, 255)
    );

    // Second render: switch to same-origin-only and ensure we *don't* get a cache hit that bypasses
    // policy enforcement.
    let mut ctx = ResourceContext::default();
    ctx.document_url = Some(doc_url.to_string());
    ctx.policy.document_origin = Some(doc_origin);
    ctx.policy.same_origin_only = true;
    cache.set_resource_context(Some(ctx));

    let err = cache
      .render_svg_pixmap_at_size_with_injected_style(
        svg,
        insert_pos,
        style_element,
        1,
        1,
        svg_url,
        1.0,
      )
      .expect_err("expected policy block for injected cross-origin stylesheet");
    match err {
      Error::Image(ImageError::LoadFailed { url, reason }) => {
        assert_eq!(url, css_url);
        assert!(
          reason.contains("Blocked SVG subresource"),
          "unexpected policy reason: {reason}"
        );
      }
      other => panic!("expected ImageError::LoadFailed, got {other:?}"),
    }
  }

  #[test]
  fn svg_style_import_wraps_media_list_in_output() {
    let main_url = "https://example.test/main.svg";
    let style_url = "https://example.test/style.css";

    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><style>@import url("style.css") screen;</style></svg>"#;

    let mut css_res = FetchedResource::new(
      b".r{fill:red !important;}".to_vec(),
      Some("text/css".to_string()),
    );
    css_res.status = Some(200);
    css_res.final_url = Some(style_url.to_string());

    let fetcher = MapFetcher::with_entries([(style_url.to_string(), css_res)]);
    let out = inline_svg_style_imports(svg, main_url, &fetcher, None).expect("inlined svg CSS");
    assert!(
      out.as_ref().contains("@media screen"),
      "expected media list to be preserved via @media wrapper, got: {}",
      out.as_ref()
    );
    assert!(
      out.as_ref().contains(".r{fill:red !important;}"),
      "expected imported CSS to be inlined, got: {}",
      out.as_ref()
    );
    assert!(
      !out.as_ref().contains("@import"),
      "expected @import rule to be replaced, got: {}",
      out.as_ref()
    );
  }

  #[test]
  fn svg_style_import_cycle_is_bounded() {
    let main_url = "https://example.test/main.svg";
    let a_url = "https://example.test/a/a.css";
    let b_url = "https://example.test/a/b.css";

    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><style>@import url("a/a.css");</style><rect class="r" width="1" height="1" fill="blue"/></svg>"#;

    let mut a_res =
      FetchedResource::new(b"@import \"b.css\";".to_vec(), Some("text/css".to_string()));
    a_res.status = Some(200);
    a_res.final_url = Some(a_url.to_string());

    let mut b_res = FetchedResource::new(
      b"@import \"a.css\"; .r{fill:red !important;}".to_vec(),
      Some("text/css".to_string()),
    );
    b_res.status = Some(200);
    b_res.final_url = Some(b_url.to_string());

    let fetcher =
      MapFetcher::with_entries([(a_url.to_string(), a_res), (b_url.to_string(), b_res)]);
    let cache = ImageCache::with_fetcher(Arc::new(fetcher.clone()));

    let pixmap = cache
      .render_svg_pixmap_at_size(svg, 1, 1, main_url, 1.0)
      .expect("rendered pixmap");
    let pixel = pixmap.pixel(0, 0).expect("pixel");
    assert_eq!(
      (pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()),
      (255, 0, 0, 255),
      "expected non-cyclic portion of the import chain to apply"
    );

    let requests = fetcher.requests();
    let style_requests = requests
      .iter()
      .filter(|(_, dest, _)| *dest == FetchDestination::Style)
      .count();
    assert_eq!(
      style_requests, 2,
      "expected cyclic @import to be skipped (only a.css and b.css fetched), got: {requests:?}"
    );
  }

  #[test]
  fn inline_svg_external_image_uses_document_url_as_base() {
    let doc_url = "https://example.test/page.html";
    let img_url = "https://example.test/img.png";

    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><image href="/img.png" width="1" height="1"/></svg>"#;

    let mut img_res = FetchedResource::new(
      encode_single_pixel_png([255, 0, 0, 255]),
      Some("image/png".to_string()),
    );
    img_res.status = Some(200);
    img_res.final_url = Some(img_url.to_string());

    let fetcher = MapFetcher::with_entries([(img_url.to_string(), img_res)]);
    let mut cache = ImageCache::with_fetcher(Arc::new(fetcher.clone()));
    let doc_origin = crate::resource::origin_from_url(doc_url).expect("document origin");
    let mut ctx = ResourceContext::default();
    ctx.document_url = Some(doc_url.to_string());
    ctx.policy.document_origin = Some(doc_origin);
    cache.set_resource_context(Some(ctx));

    let pixmap = cache
      .render_svg_pixmap_at_size(svg, 1, 1, "inline-svg", 1.0)
      .expect("rendered pixmap");
    let pixel = pixmap.pixel(0, 0).expect("pixel");
    assert_eq!(
      (pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()),
      (255, 0, 0, 255)
    );

    let requests = fetcher.requests();
    assert!(
      requests
        .iter()
        .any(|(url, dest, _)| url == img_url && *dest == FetchDestination::Image),
      "expected fetch for inline svg image href {img_url}, got: {requests:?}"
    );
  }

  #[test]
  fn inline_svg_external_image_uses_xml_base_as_base() {
    let doc_url = "https://example.test/page.html";
    let img_url = "https://example.test/assets/img.png";

    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" xml:base="assets/" width="1" height="1"><image href="img.png" width="1" height="1"/></svg>"#;

    let mut img_res = FetchedResource::new(
      encode_single_pixel_png([255, 0, 0, 255]),
      Some("image/png".to_string()),
    );
    img_res.status = Some(200);
    img_res.final_url = Some(img_url.to_string());

    let fetcher = MapFetcher::with_entries([(img_url.to_string(), img_res)]);
    let mut cache = ImageCache::with_fetcher(Arc::new(fetcher.clone()));
    let doc_origin = crate::resource::origin_from_url(doc_url).expect("document origin");
    let mut ctx = ResourceContext::default();
    ctx.document_url = Some(doc_url.to_string());
    ctx.policy.document_origin = Some(doc_origin);
    cache.set_resource_context(Some(ctx));

    let pixmap = cache
      .render_svg_pixmap_at_size(svg, 1, 1, "inline-svg", 1.0)
      .expect("rendered pixmap");

    let requests = fetcher.requests();
    assert!(
      requests
        .iter()
        .any(|(url, dest, _)| url == img_url && *dest == FetchDestination::Image),
      "expected fetch for xml:base image href {img_url}, got: {requests:?}"
    );

    let pixel = pixmap.pixel(0, 0).expect("pixel");
    assert_eq!(
      (pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()),
      (255, 0, 0, 255)
    );
  }

  #[test]
  fn inline_svg_image_src_is_rewritten_to_href_and_renders() {
    let main_url = "https://example.test/main.svg";
    let img_url = "https://example.test/img.png";

    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><image src="https://example.test/img.png" width="1" height="1" /></svg>"#;

    let mut img_res = FetchedResource::new(
      encode_single_pixel_png([255, 0, 0, 255]),
      Some("image/png".to_string()),
    );
    img_res.status = Some(200);
    img_res.final_url = Some(img_url.to_string());

    let fetcher = MapFetcher::with_entries([(img_url.to_string(), img_res)]);
    let inlined =
      inline_svg_image_references(svg, main_url, &fetcher, None, None).expect("inlined svg");
    assert!(
      inlined.as_ref().contains("href=\"data:image/png;base64,"),
      "expected href data URL rewrite, got: {}",
      inlined.as_ref()
    );
    assert!(
      !inlined
        .as_ref()
        .contains("src=\"https://example.test/img.png\""),
      "expected src attribute to be rewritten, got: {}",
      inlined.as_ref()
    );

    // Verify the rewritten SVG is self-contained and renders without any network access.
    let cache = ImageCache::with_fetcher(Arc::new(MapFetcher::default()));
    let pixmap = cache
      .render_svg_pixmap_at_size(inlined.as_ref(), 1, 1, "inline-svg", 1.0)
      .expect("rendered pixmap");

    let pixel = pixmap.pixel(0, 0).expect("pixel");
    assert_eq!(
      (pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()),
      (255, 0, 0, 255)
    );
  }

  #[test]
  fn inline_svg_image_skips_display_none() {
    let main_url = "https://example.test/main.svg";
    let img_url = "https://example.test/img.png";

    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg"><image href="https://example.test/img.png" style="display:none" width="1" height="1" /></svg>"#;

    let mut img_res = FetchedResource::new(
      encode_single_pixel_png([255, 0, 0, 255]),
      Some("image/png".to_string()),
    );
    img_res.status = Some(200);
    img_res.final_url = Some(img_url.to_string());

    let fetcher = MapFetcher::with_entries([(img_url.to_string(), img_res)]);

    let inlined =
      inline_svg_image_references(svg, main_url, &fetcher, None, None).expect("inlined svg");
    assert_eq!(inlined.as_ref(), svg);
    assert!(
      !inlined.as_ref().contains("data:image/"),
      "expected no data URL rewrite for display:none <image>, got: {}",
      inlined.as_ref()
    );
    assert!(
      fetcher.requests().is_empty(),
      "expected no fetches for display:none <image>"
    );
  }

  #[test]
  fn inline_svg_external_fe_image_href_is_inlined() {
    let main_url = "https://example.test/main.svg";
    let img_url = "https://example.test/img.png";

    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg"><defs><filter id="f"><feImage href="/img.png" x="0" y="0" width="1" height="1"/></filter></defs><rect width="1" height="1" filter="url(#f)"/></svg>"#;

    let mut img_res = FetchedResource::new(
      encode_single_pixel_png([255, 0, 0, 255]),
      Some("image/png".to_string()),
    );
    img_res.status = Some(200);
    img_res.final_url = Some(img_url.to_string());

    let fetcher = MapFetcher::with_entries([(img_url.to_string(), img_res)]);
    let inlined =
      inline_svg_image_references(svg, main_url, &fetcher, None, None).expect("inlined svg");
    assert!(
      inlined.as_ref().contains("data:image/png;base64,"),
      "expected data URL rewrite, got: {}",
      inlined.as_ref()
    );

    let requests = fetcher.requests();
    assert!(
      requests
        .iter()
        .any(|(url, dest, _)| url == img_url && *dest == FetchDestination::Image),
      "expected fetch for feImage href {img_url}, got: {requests:?}"
    );
  }

  #[test]
  fn inline_svg_image_references_decompresses_svgz() {
    let main_url = "https://example.test/main.svg";
    let nested_url = "https://example.test/nested.svgz";

    let nested_svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><rect width="1" height="1" fill="red"/></svg>"#;
    let nested_svgz = gzip_bytes(nested_svg.as_bytes());

    let main_svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><image href="/nested.svgz" width="1" height="1"/></svg>"#;

    let mut nested_res =
      FetchedResource::new(nested_svgz, Some("application/octet-stream".to_string()));
    nested_res.status = Some(200);
    nested_res.final_url = Some(nested_url.to_string());

    let fetcher = MapFetcher::with_entries([(nested_url.to_string(), nested_res)]);

    let inlined =
      inline_svg_image_references(main_svg, main_url, &fetcher, None, None).expect("inlined svg");
    let output = inlined.as_ref();
    let prefix = "data:image/svg+xml;base64,";
    let start = output
      .find(prefix)
      .unwrap_or_else(|| panic!("expected SVG data URL rewrite, got: {output}"));
    let b64_after_prefix = &output[start + prefix.len()..];
    let end = b64_after_prefix.find('"').expect("closing quote");
    let payload_b64 = &b64_after_prefix[..end];
    let decoded = base64::engine::general_purpose::STANDARD
      .decode(payload_b64)
      .expect("decoded base64 payload");
    assert!(
      decoded.starts_with(b"<svg"),
      "expected decoded payload to start with <svg, got: {:?}",
      &decoded[..decoded.len().min(8)]
    );

    let cache = ImageCache::with_fetcher(Arc::new(fetcher));
    let pixmap = cache
      .render_svg_pixmap_at_size(main_svg, 1, 1, main_url, 1.0)
      .expect("rendered pixmap");
    let pixel = pixmap.pixel(0, 0).expect("pixel");
    assert_eq!(
      (pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()),
      (255, 0, 0, 255)
    );
  }

  #[test]
  fn inline_svg_image_references_propagates_render_errors_from_fetcher() {
    struct RenderErrorFetcher;

    impl ResourceFetcher for RenderErrorFetcher {
      fn fetch(&self, _url: &str) -> crate::error::Result<FetchedResource> {
        Err(Error::Render(RenderError::Timeout {
          stage: RenderStage::Paint,
          elapsed: Duration::from_millis(0),
        }))
      }
    }

    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg"><image href="https://example.test/img.png" width="1" height="1"/></svg>"#;
    let err = inline_svg_image_references(
      svg,
      "https://example.test/main.svg",
      &RenderErrorFetcher,
      None,
      None,
    )
    .expect_err("expected render error to propagate");
    assert!(
      matches!(
        err,
        Error::Render(RenderError::Timeout {
          stage: RenderStage::Paint,
          ..
        })
      ),
      "expected paint-stage render timeout, got {err:?}"
    );
  }

  #[test]
  fn svg_policy_blocks_external_image_during_render() {
    let doc_url = "https://doc.test/";
    let blocked_url = "https://cross.test/a.png";

    let fetcher = MapFetcher::default();
    let mut cache = ImageCache::with_fetcher(Arc::new(fetcher.clone()));
    let doc_origin = origin_from_url(doc_url).expect("document origin");
    let mut ctx = ResourceContext::default();
    ctx.document_url = Some(doc_url.to_string());
    ctx.policy.document_origin = Some(doc_origin);
    ctx.policy.same_origin_only = true;
    cache.set_resource_context(Some(ctx));

    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><image href="https://cross.test/a.png" width="1" height="1"/></svg>"#;

    let err = cache
      .render_svg_pixmap_at_size(svg, 1, 1, "inline-svg", 1.0)
      .expect_err("expected policy block during render");
    match err {
      Error::Image(ImageError::LoadFailed { url, reason }) => {
        assert_eq!(url, blocked_url);
        assert!(
          reason.contains("Blocked cross-origin subresource"),
          "unexpected policy reason: {reason}"
        );
      }
      other => panic!("expected ImageError::LoadFailed, got {other:?}"),
    }

    let requests = fetcher.requests();
    assert!(
      requests.is_empty(),
      "fetcher should not be called for policy-blocked URLs, got: {requests:?}"
    );
  }

  #[test]
  fn svg_policy_scan_respects_xml_base() {
    let doc_url = "https://doc.test/";
    let svg_url = "https://doc.test/main.svg";
    let blocked_url = "https://cross.test/a.png";

    let fetcher = MapFetcher::default();
    let mut cache = ImageCache::with_fetcher(Arc::new(fetcher));
    let doc_origin = origin_from_url(doc_url).expect("document origin");
    let mut ctx = ResourceContext::default();
    ctx.document_url = Some(doc_url.to_string());
    ctx.policy.document_origin = Some(doc_origin);
    ctx.policy.same_origin_only = true;
    cache.set_resource_context(Some(ctx));

    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" xml:base="https://cross.test/" width="1" height="1"><image href="a.png" width="1" height="1"/></svg>"#;

    let err = cache
      .enforce_svg_resource_policy(svg, svg_url)
      .expect_err("expected policy block based on xml:base");
    match err {
      Error::Image(ImageError::LoadFailed { url, reason }) => {
        assert_eq!(url, blocked_url);
        assert!(
          reason.contains("Blocked SVG subresource"),
          "unexpected policy reason: {reason}"
        );
      }
      other => panic!("expected ImageError::LoadFailed, got {other:?}"),
    }
  }

  #[test]
  fn svg_policy_scan_respects_xml_base_with_inline_svg_url() {
    let doc_url = "https://doc.test/page.html";
    let svg_url = "inline-svg";
    let blocked_url = "https://cross.test/a.png";

    let fetcher = MapFetcher::default();
    let mut cache = ImageCache::with_fetcher(Arc::new(fetcher));
    let doc_origin = origin_from_url(doc_url).expect("document origin");
    let mut ctx = ResourceContext::default();
    ctx.document_url = Some(doc_url.to_string());
    ctx.policy.document_origin = Some(doc_origin);
    ctx.policy.same_origin_only = true;
    cache.set_resource_context(Some(ctx));

    // Use a "scheme-relative" xml:base that relies on the base URL to fill in the scheme. When
    // the SVG is rendered with a dummy `svg_url` (`inline-svg`), we must still apply xml:base
    // relative to the document URL; otherwise policy scans can miss cross-origin references and
    // cached pixmaps might bypass the policy.
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" xml:base="//cross.test/" width="1" height="1"><image href="a.png" width="1" height="1"/></svg>"#;

    let err = cache
      .enforce_svg_resource_policy(svg, svg_url)
      .expect_err("expected policy block based on xml:base");
    match err {
      Error::Image(ImageError::LoadFailed { url, reason }) => {
        assert_eq!(url, blocked_url);
        assert!(
          reason.contains("Blocked SVG subresource"),
          "unexpected policy reason: {reason}"
        );
      }
      other => panic!("expected ImageError::LoadFailed, got {other:?}"),
    }
  }

  #[test]
  fn svg_mask_application() {
    let cache = ImageCache::new();
    let svg = r#"
      <svg xmlns='http://www.w3.org/2000/svg' width='20' height='20'>
        <defs>
          <mask id='m'>
            <rect width='100%' height='100%' fill='white'/>
            <rect width='10' height='20' fill='black'/>
          </mask>
        </defs>
        <rect width='20' height='20' fill='red' mask='url(#m)' />
      </svg>
    "#;

    let image = cache.render_svg(svg).expect("render svg mask");
    let rgba = image.image.to_rgba8();
    let left_alpha = rgba.get_pixel(5, 10)[3];
    let right_alpha = rgba.get_pixel(15, 10)[3];
    assert!(
      left_alpha < right_alpha,
      "mask should reduce left side opacity"
    );
  }

  #[test]
  fn exposes_exif_orientation() {
    let cache = ImageCache::new();
    let image = cache
      .load("tests/fixtures/image_orientation/orientation-6.jpg")
      .expect("load oriented image");
    assert_eq!(
      image.orientation,
      Some(OrientationTransform {
        quarter_turns: 1,
        flip_x: false
      })
    );
  }

  #[test]
  fn jpeg_decode_matches_chrome_pixel_values() {
    // Regression test: JPEG decoding should match Chrome/libjpeg output closely. Even small
    // per-channel differences show up as huge fixture diffs because the page-loop diffing uses
    // tolerance=0.
    let cache = ImageCache::new();
    let image = cache
      .load("tests/fixtures/image_orientation/orientation-6.jpg")
      .expect("load oriented image");
    assert_eq!(image.width(), 2);
    assert_eq!(image.height(), 1);
    let rgba = image.image.to_rgba8();
    assert_eq!(rgba.get_pixel(0, 0).0, [254, 0, 0, 255]);
    assert_eq!(rgba.get_pixel(1, 0).0, [0, 255, 1, 255]);
  }

  #[test]
  fn jpeg_decode_420_matches_chrome_pixel_values() {
    // Regression test: baseline 4:2:0 (chroma subsampled) JPEG decoding should match
    // Chrome/libjpeg-turbo output exactly. Tiny per-channel differences (often +/-1..3) can show
    // up as large fixture diffs because the page-loop diffing uses tolerance=0.
    let bytes = include_bytes!("../tests/fixtures/jpeg/nbcnews_80x80_sof0_420.jpg");
    let cache = ImageCache::new();
    let (image, has_alpha) = cache
      .decode_with_format(bytes, ImageFormat::Jpeg, "nbcnews_80x80_sof0_420")
      .expect("decode 4:2:0 baseline jpeg");
    assert!(!has_alpha);
    assert_eq!(image.width(), 80);
    assert_eq!(image.height(), 80);
    let rgba = image.to_rgba8();

    // Reference pixels sampled from Chrome (headless) output.
    for (x, y, expected) in [
      (0, 0, [28, 62, 107, 255]),     // top-left
      (30, 40, [207, 200, 174, 255]), // interior
      (40, 30, [0, 1, 10, 255]),      // interior (dark)
      (79, 0, [9, 41, 80, 255]),      // top-right
      (0, 79, [0, 0, 2, 255]),        // bottom-left
      (79, 79, [0, 0, 0, 255]),       // bottom-right
    ] {
      assert_eq!(
        rgba.get_pixel(x, y).0,
        expected,
        "unexpected pixel at ({x},{y})"
      );
    }
  }

  #[test]
  fn resolves_relative_urls_against_base() {
    let mut cache = ImageCache::new();
    let mut path: PathBuf = std::env::temp_dir();
    path.push(format!(
      "fastrender_base_url_test_{}_{}.png",
      std::process::id(),
      std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
    ));
    let dir = path.parent().unwrap().to_path_buf();
    let image = RgbaImage::from_raw(1, 1, vec![255, 0, 0, 255]).expect("build 1x1");
    image.save(&path).expect("encode png");
    let base_url = Url::from_directory_path(&dir).unwrap().to_string();
    cache.set_base_url(base_url);

    let image = cache
      .load(path.file_name().unwrap().to_str().unwrap())
      .expect("load via base");
    assert_eq!(image.width(), 1);
    assert_eq!(image.height(), 1);
  }

  #[test]
  fn resolves_relative_paths_against_http_base() {
    let cache = ImageCache::with_base_url("https://example.com/a/b/".to_string());
    assert_eq!(
      cache.resolve_url("../img.png"),
      "https://example.com/a/img.png".to_string()
    );
    assert_eq!(
      cache.resolve_url("./nested/icon.png"),
      "https://example.com/a/b/nested/icon.png".to_string()
    );
  }

  #[test]
  fn resolves_protocol_relative_urls_using_base_scheme() {
    let cache = ImageCache::with_base_url("https://example.com/base/".to_string());
    assert_eq!(
      cache.resolve_url("//cdn.example.com/asset.png"),
      "https://cdn.example.com/asset.png".to_string()
    );
  }

  #[test]
  fn load_preserves_non_ascii_whitespace_in_urls() {
    let nbsp = "\u{00A0}";
    let expected_url = "https://example.com/base/foo%C2%A0".to_string();
    let mut resource = FetchedResource::new(
      encode_single_pixel_png([0, 0, 0, 0]),
      Some("image/png".to_string()),
    );
    resource.status = Some(200);

    let fetcher = Arc::new(MapFetcher::with_entries([(expected_url.clone(), resource)]));
    let cache = ImageCache::with_base_url_and_fetcher(
      "https://example.com/base/".to_string(),
      Arc::clone(&fetcher) as Arc<dyn ResourceFetcher>,
    );

    cache
      .load(&format!("foo{nbsp}"))
      .expect("expected image load");

    let requests = fetcher.requests();
    assert!(
      requests.iter().any(|(url, _, _)| *url == expected_url),
      "expected fetcher to request {expected_url}, got: {requests:?}"
    );
  }

  #[test]
  fn image_format_from_content_type_does_not_trim_non_ascii_whitespace() {
    let nbsp = "\u{00A0}";
    let content_type = format!("{nbsp}image/png");
    assert_eq!(
      ImageCache::format_from_content_type(Some(&content_type)),
      None
    );
    assert_eq!(
      ImageCache::format_from_content_type(Some("image/png")),
      Some(ImageFormat::Png)
    );
  }

  #[test]
  fn non_ascii_whitespace_svg_parse_fill_color_does_not_trim_nbsp() {
    let nbsp = "\u{00A0}";
    assert_eq!(
      svg_parse_fill_color(&format!("{nbsp}none")),
      None,
      "NBSP must not be treated as whitespace when parsing SVG fill colors"
    );
    assert_eq!(
      svg_parse_fill_color("none"),
      Some(Rgba::new(0, 0, 0, 0.0)),
      "baseline: none should parse to a transparent color sentinel"
    );
  }

  #[test]
  fn resolves_file_base_without_trailing_slash_as_directory() {
    let mut dir: PathBuf = std::env::temp_dir();
    dir.push(format!(
      "fastrender_url_base_{}_{}",
      std::process::id(),
      SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
    ));
    std::fs::create_dir_all(dir.join("assets")).expect("create temp dir");
    let mut base_url = Url::from_directory_path(&dir).unwrap();
    let trimmed = base_url.path().trim_end_matches('/').to_string();
    base_url.set_path(&trimmed);
    let base = base_url.to_string();
    let cache = ImageCache::with_base_url(base);

    let resolved = cache.resolve_url("assets/image.png");
    assert!(
      resolved.ends_with("/assets/image.png"),
      "resolved path should keep directory: {}",
      resolved
    );

    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn with_fetcher_uses_custom_fetcher() {
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    struct CountingFetcher {
      count: AtomicUsize,
    }

    impl ResourceFetcher for CountingFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        self.count.fetch_add(1, Ordering::SeqCst);
        // Return a minimal valid PNG
        let png_data = vec![
          0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, // PNG signature
          0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, // IHDR chunk
          0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, // 1x1
          0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53, 0xde, 0x00, 0x00, 0x00, 0x0c, 0x49, 0x44,
          0x41, 0x54, 0x08, 0xd7, 0x63, 0xf8, 0xff, 0xff, 0x3f, 0x00, 0x05, 0xfe, 0x02, 0xfe, 0xdc,
          0xcc, 0x59, 0xe7, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
        ];
        Ok(FetchedResource::new(
          png_data,
          Some("image/png".to_string()),
        ))
      }
    }

    let fetcher = Arc::new(CountingFetcher {
      count: AtomicUsize::new(0),
    });
    let cache = ImageCache::with_fetcher(Arc::clone(&fetcher) as Arc<dyn ResourceFetcher>);

    let _ = cache.load("test://image.png");
    assert_eq!(fetcher.count.load(Ordering::SeqCst), 1);

    // Second load should use cache
    let _ = cache.load("test://image.png");
    assert_eq!(fetcher.count.load(Ordering::SeqCst), 1);

    // Different URL should fetch again
    let _ = cache.load("test://other.png");
    assert_eq!(fetcher.count.load(Ordering::SeqCst), 2);
  }

  struct StaticFetcher {
    bytes: Vec<u8>,
    content_type: Option<String>,
  }

  impl ResourceFetcher for StaticFetcher {
    fn fetch(&self, _url: &str) -> Result<FetchedResource> {
      Ok(FetchedResource::new(
        self.bytes.clone(),
        self.content_type.clone(),
      ))
    }
  }

  #[cfg(feature = "avif")]
  fn avif_fixture_bytes() -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/avif/solid.avif");
    std::fs::read(&path).expect("read avif fixture")
  }

  #[cfg(feature = "avif")]
  fn assert_green_pixel(pixel: [u8; 4]) {
    assert!(pixel[1] >= 180, "expected green channel, got {pixel:?}");
    assert!(
      pixel[0] < 50 && pixel[2] < 50,
      "expected low red/blue, got {pixel:?}"
    );
  }

  #[cfg(feature = "avif")]
  #[test]
  fn decodes_avif_with_declared_content_type() {
    let bytes = avif_fixture_bytes();
    let fetcher = Arc::new(StaticFetcher {
      bytes: bytes.clone(),
      content_type: Some("image/avif".to_string()),
    });
    let cache = ImageCache::with_fetcher(fetcher);

    let image = cache.load("test://avif.declared").expect("decode avif");
    assert_eq!(image.width(), 4);
    assert_eq!(image.height(), 4);

    let pixel = image.image.to_rgba8().get_pixel(0, 0).0;
    assert_green_pixel(pixel);
  }

  #[cfg(feature = "avif")]
  #[test]
  fn decodes_avif_when_content_type_is_incorrect() {
    let bytes = avif_fixture_bytes();
    let fetcher = Arc::new(StaticFetcher {
      bytes: bytes.clone(),
      content_type: Some("image/png".to_string()),
    });
    let cache = ImageCache::with_fetcher(fetcher);

    let image = cache
      .load("test://avif.sniff")
      .expect("decode avif via sniffing");
    assert_eq!(image.width(), 4);
    assert_eq!(image.height(), 4);

    let pixel = image.image.to_rgba8().get_pixel(2, 2).0;
    assert_green_pixel(pixel);
  }

  #[test]
  fn decode_inflight_wait_respects_render_deadline() {
    use std::sync::mpsc;
    use std::sync::Barrier;
    use std::thread;

    struct BlockingFetcher {
      started: Arc<Barrier>,
      release: Arc<Barrier>,
      url: String,
    }

    impl ResourceFetcher for BlockingFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        if url == self.url {
          self.started.wait();
          self.release.wait();
          return Ok(FetchedResource::new(
            b"not an image".to_vec(),
            Some("image/png".to_string()),
          ));
        }

        Err(Error::Resource(crate::error::ResourceError::new(
          url.to_string(),
          "unexpected url",
        )))
      }
    }

    let started = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let url = "https://example.com/blocked.png".to_string();
    let cache = ImageCache::with_fetcher(Arc::new(BlockingFetcher {
      started: Arc::clone(&started),
      release: Arc::clone(&release),
      url: url.clone(),
    }));

    let owner_cache = cache.clone();
    let owner_url = url.clone();
    let owner_handle = thread::spawn(move || owner_cache.load(&owner_url));

    // Wait until the owner has entered the fetcher (and therefore registered the in-flight entry).
    started.wait();

    let waiter_cache = cache.clone();
    let waiter_url = url.clone();
    let (tx, rx) = mpsc::channel();
    let waiter_handle = thread::spawn(move || {
      let deadline = render_control::RenderDeadline::new(Some(Duration::from_millis(50)), None);
      let start = Instant::now();
      let result =
        render_control::with_deadline(Some(&deadline), || waiter_cache.load(&waiter_url));
      tx.send((result, start.elapsed())).unwrap();
    });

    let (result, elapsed) = match rx.recv_timeout(Duration::from_secs(1)) {
      Ok(value) => value,
      Err(err) => {
        // Make sure we don't leave the owner thread blocked on the barrier.
        release.wait();
        let _ = owner_handle.join();
        drop(waiter_handle);
        panic!("waiter decode did not complete under deadline: {err}");
      }
    };

    let err = match result {
      Ok(_) => panic!("waiter decode should fail under deadline"),
      Err(err) => err,
    };
    match err {
      Error::Render(RenderError::Timeout { stage, .. }) => {
        assert_eq!(stage, RenderStage::Paint);
      }
      other => panic!("unexpected error after {elapsed:?}: {other:?}"),
    }

    // Let the owner thread exit so it can resolve the in-flight entry.
    release.wait();
    let _ = owner_handle.join();
    let _ = waiter_handle.join();
  }

  fn assert_decode_or_timeout(err: Error) {
    match err {
      Error::Image(_) | Error::Render(RenderError::Timeout { .. }) => {}
      other => panic!("unexpected error kind: {other:?}"),
    }
  }

  #[test]
  fn webp_decode_matches_libwebp() {
    let bytes = include_bytes!("../tests/fixtures/webp/lossy_gradient.webp");
    let cache = ImageCache::new();
    let (img, has_alpha) = cache
      .decode_with_format(bytes, ImageFormat::WebP, "lossy_gradient.webp")
      .expect("decode webp");
    assert!(!has_alpha, "expected lossy_gradient.webp to have no alpha");

    let rgba = img.to_rgba8();
    assert_eq!(rgba.dimensions(), (16, 16));

    // Values sampled from libwebp decode output (matches Chrome).
    assert_eq!(rgba.get_pixel(0, 0).0, [7, 6, 13, 255]);
    assert_eq!(rgba.get_pixel(5, 7).0, [109, 152, 139, 255]);
    assert_eq!(rgba.get_pixel(15, 15).0, [39, 103, 111, 255]);
  }

  #[test]
  fn image_decode_uses_root_deadline_over_nested_budget_deadline() {
    let mut pixels = RgbaImage::new(1, 1);
    pixels
      .pixels_mut()
      .for_each(|p| *p = image::Rgba([0, 255, 0, 255]));
    let mut png = Vec::new();
    PngEncoder::new(&mut png)
      .write_image(pixels.as_raw(), 1, 1, ColorType::Rgba8.into())
      .expect("encode png");

    let url = "test://nested-deadline.png".to_string();
    let resource = FetchedResource::new(png, Some("image/png".to_string()));
    let fetcher = Arc::new(MapFetcher::with_entries([(url.clone(), resource)]));
    let cache = ImageCache::with_fetcher(fetcher);

    // Install a root deadline with enough budget, then a nested budget deadline that is already
    // expired. Image decoding should use the root deadline, so it still succeeds.
    let root_deadline = RenderDeadline::new(Some(Duration::from_secs(1)), None);
    render_control::with_deadline(Some(&root_deadline), || {
      let nested_deadline = RenderDeadline::new(Some(Duration::from_millis(0)), None);
      let image = render_control::with_deadline(Some(&nested_deadline), || cache.load(&url))
        .expect("decode should ignore nested deadline budget");
      assert_eq!(image.width(), 1);
      assert_eq!(image.height(), 1);
    });
  }

  #[test]
  fn truncated_bitmap_headers_do_not_panic() {
    let cases: &[(&str, &[u8])] = &[
      ("png", b"\x89PNG\r\n\x1a\n"),
      ("jpeg", b"\xff\xd8\xff"),
      ("gif", b"GIF89a"),
      ("webp", b"RIFF\x00\x00\x00\x00WEBP"),
    ];

    let entries = cases.iter().map(|(name, bytes)| {
      (
        format!("test://truncated.{name}"),
        FetchedResource::new(bytes.to_vec(), None),
      )
    });
    let fetcher = Arc::new(MapFetcher::with_entries(entries));
    let cache = ImageCache::with_fetcher_and_config(
      fetcher,
      ImageCacheConfig::default()
        .with_max_decoded_dimension(1024)
        .with_max_decoded_pixels(1024 * 1024),
    );

    for (name, _) in cases {
      let url = format!("test://truncated.{name}");

      let probe = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cache.probe(&url)));
      let probe = probe.unwrap_or_else(|_| panic!("probe panicked for {name}"));
      let probe_err = match probe {
        Ok(_) => panic!("probe should fail for truncated header {name}"),
        Err(err) => err,
      };
      assert_decode_or_timeout(probe_err);

      let load = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cache.load(&url)));
      let load = load.unwrap_or_else(|_| panic!("load panicked for {name}"));
      let load_err = match load {
        Ok(_) => panic!("load should fail for truncated header {name}"),
        Err(err) => err,
      };
      assert_decode_or_timeout(load_err);
    }
  }

  #[test]
  fn enforce_decode_limits_rejects_images_exceeding_max_pixmap_bytes() {
    // Even if callers disable the user-configurable decode limits, the decoder should still
    // refuse images that would require allocations larger than our global pixmap cap.
    let config = ImageCacheConfig {
      max_decoded_pixels: 0,
      max_decoded_dimension: 0,
      ..ImageCacheConfig::default()
    };
    let cache = ImageCache::with_config(config);

    // width * height * 4 > MAX_PIXMAP_BYTES
    let err = cache
      .enforce_decode_limits(20_000, 20_000, "test://too-big")
      .expect_err("expected size limit failure");
    match err {
      Error::Image(ImageError::DecodeFailed { reason, .. }) => {
        assert!(reason.contains("limit"), "unexpected reason: {reason}");
      }
      other => panic!("unexpected error: {other:?}"),
    }
  }

  #[test]
  fn malformed_isobmff_avif_headers_do_not_panic() {
    // ISO-BMFF box headers that have historically triggered debug-only assertions inside AVIF
    // sniffers/parsers.
    let cases: &[(&str, Vec<u8>)] = &[
      // Box declares an extended size but the header is truncated.
      (
        "short_ext_size",
        vec![
          0x00, 0x00, 0x00, 0x01, b'f', b't', b'y', b'p', 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ],
      ),
      // Extended size is present but invalid (smaller than the 16-byte extended header).
      (
        "invalid_ext_size",
        vec![
          0x00, 0x00, 0x00, 0x01, b'f', b't', b'y', b'p', 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
          0x08,
        ],
      ),
    ];

    let entries = cases.iter().map(|(name, bytes)| {
      (
        format!("test://malformed.{name}.avif"),
        FetchedResource::new(bytes.clone(), Some("image/avif".to_string())),
      )
    });
    let fetcher = Arc::new(MapFetcher::with_entries(entries));
    let cache = ImageCache::with_fetcher_and_config(
      fetcher,
      ImageCacheConfig::default()
        .with_max_decoded_dimension(1024)
        .with_max_decoded_pixels(1024 * 1024),
    );

    for (name, _) in cases {
      let url = format!("test://malformed.{name}.avif");

      let probe = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cache.probe(&url)));
      let probe = probe.unwrap_or_else(|_| panic!("probe panicked for {name}"));
      let probe_err = match probe {
        Ok(_) => panic!("probe should fail for malformed avif header {name}"),
        Err(err) => err,
      };
      assert_decode_or_timeout(probe_err);

      let load = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cache.load(&url)));
      let load = load.unwrap_or_else(|_| panic!("load panicked for {name}"));
      let load_err = match load {
        Ok(_) => panic!("load should fail for malformed avif header {name}"),
        Err(err) => err,
      };
      assert_decode_or_timeout(load_err);
    }
  }

  fn mean_abs_diff(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len());
    let sum: u64 = a
      .iter()
      .zip(b.iter())
      .map(|(lhs, rhs)| i16::from(*lhs).abs_diff(i16::from(*rhs)) as u64)
      .sum();
    sum as f64 / a.len() as f64
  }

  #[test]
  fn raster_pixmap_at_size_matches_draw_scaled_output_with_orientation() {
    let mut image = RgbaImage::new(6, 4);
    for y in 0..image.height() {
      for x in 0..image.width() {
        let r = (x * 40).min(255) as u8;
        let g = (y * 60).min(255) as u8;
        let b = 128u8;
        image.put_pixel(x, y, image::Rgba([r, g, b, 255]));
      }
    }
    // Include one translucent pixel to exercise premultiplication.
    image.put_pixel(2, 1, image::Rgba([255, 0, 0, 128]));

    let mut png = Vec::new();
    PngEncoder::new(&mut png)
      .write_image(
        &image,
        image.width(),
        image.height(),
        ColorType::Rgba8.into(),
      )
      .expect("encode png");
    let src = format!(
      "data:image/png;base64,{}",
      base64::engine::general_purpose::STANDARD.encode(&png)
    );

    let cache = ImageCache::new();
    let orientation = OrientationTransform {
      quarter_turns: 1,
      flip_x: true,
    };

    let full = cache
      .load_raster_pixmap(&src, orientation, false)
      .expect("load full pixmap")
      .expect("raster pixmap");

    for (target_w, target_h) in [(3u32, 4u32), (2u32, 3u32)] {
      let mut expected = Pixmap::new(target_w, target_h).expect("dst pixmap");
      let scale_x = target_w as f32 / full.width() as f32;
      let scale_y = target_h as f32 / full.height() as f32;
      let mut paint = tiny_skia::PixmapPaint::default();
      paint.quality = FilterQuality::Bilinear;
      expected.draw_pixmap(
        0,
        0,
        full.as_ref().as_ref(),
        &paint,
        tiny_skia::Transform::from_row(scale_x, 0.0, 0.0, scale_y, 0.0, 0.0),
        None,
      );

      let scaled = cache
        .load_raster_pixmap_at_size(
          &src,
          orientation,
          false,
          target_w,
          target_h,
          FilterQuality::Bilinear,
        )
        .expect("scaled pixmap")
        .expect("raster pixmap");

      assert_eq!((scaled.width(), scaled.height()), (target_w, target_h));
      let diff = mean_abs_diff(expected.data(), scaled.data());
      assert!(
        diff <= 10.0,
        "expected scaled output to match within tolerance (diff={diff})"
      );
    }

    let scaled_a = cache
      .load_raster_pixmap_at_size(&src, orientation, false, 3, 4, FilterQuality::Bilinear)
      .expect("scaled pixmap")
      .expect("raster pixmap");
    let scaled_a_again = cache
      .load_raster_pixmap_at_size(&src, orientation, false, 3, 4, FilterQuality::Bilinear)
      .expect("scaled pixmap")
      .expect("raster pixmap");
    assert!(Arc::ptr_eq(&scaled_a, &scaled_a_again));

    let scaled_b = cache
      .load_raster_pixmap_at_size(&src, orientation, false, 2, 3, FilterQuality::Bilinear)
      .expect("scaled pixmap")
      .expect("raster pixmap");
    assert!(!Arc::ptr_eq(&scaled_a, &scaled_b));
  }

  #[test]
  fn decode_inflight_wait_respects_cancel_callback() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::sync::Barrier;
    use std::thread;

    struct BlockingFetcher {
      started: Arc<Barrier>,
      release: Arc<Barrier>,
      url: String,
    }

    impl ResourceFetcher for BlockingFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        if url == self.url {
          self.started.wait();
          self.release.wait();
          return Ok(FetchedResource::new(
            b"not an image".to_vec(),
            Some("image/png".to_string()),
          ));
        }

        Err(Error::Resource(crate::error::ResourceError::new(
          url.to_string(),
          "unexpected url",
        )))
      }
    }

    let started = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let url = "https://example.com/blocked-image.png".to_string();
    let cache = ImageCache::with_fetcher(Arc::new(BlockingFetcher {
      started: Arc::clone(&started),
      release: Arc::clone(&release),
      url: url.clone(),
    }));

    let owner_cache = cache.clone();
    let owner_url = url.clone();
    let owner_handle = thread::spawn(move || owner_cache.load(&owner_url));

    // Wait until the owner has entered the fetcher (and therefore registered the in-flight entry).
    started.wait();

    let waiter_cache = cache.clone();
    let waiter_url = url.clone();
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let cancel_flag_worker = Arc::clone(&cancel_flag);
    let cancel: Arc<crate::render_control::CancelCallback> =
      Arc::new(move || cancel_flag_worker.load(Ordering::Relaxed));
    let (tx, rx) = mpsc::channel();
    let waiter_handle = thread::spawn(move || {
      let deadline = render_control::RenderDeadline::new(None, Some(cancel));
      let start = Instant::now();
      let result =
        render_control::with_deadline(Some(&deadline), || waiter_cache.load(&waiter_url));
      tx.send((result, start.elapsed())).unwrap();
    });

    thread::sleep(Duration::from_millis(50));
    cancel_flag.store(true, Ordering::Relaxed);

    let (result, elapsed) = match rx.recv_timeout(Duration::from_secs(1)) {
      Ok(value) => value,
      Err(err) => {
        release.wait();
        let _ = owner_handle.join();
        drop(waiter_handle);
        panic!("waiter decode did not complete under cancel: {err}");
      }
    };

    let err = match result {
      Ok(_) => panic!("waiter decode should fail under cancel callback"),
      Err(err) => err,
    };
    match err {
      Error::Render(RenderError::Timeout { stage, .. }) => {
        assert_eq!(stage, RenderStage::Paint);
      }
      other => panic!("unexpected error after {elapsed:?}: {other:?}"),
    }

    release.wait();
    let _ = owner_handle.join();
    let _ = waiter_handle.join();
  }

  #[test]
  fn probe_inflight_deduplicates_cache_artifact_reads() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Barrier;
    use std::thread;

    struct ArtifactFetcher {
      url: String,
      bytes: Arc<Vec<u8>>,
      calls: Arc<AtomicUsize>,
    }

    impl ResourceFetcher for ArtifactFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        panic!("network fetch should not be called");
      }

      fn fetch_partial_with_context(
        &self,
        _kind: FetchContextKind,
        _url: &str,
        _max_bytes: usize,
      ) -> Result<FetchedResource> {
        panic!("network partial fetch should not be called");
      }

      fn read_cache_artifact(
        &self,
        kind: FetchContextKind,
        url: &str,
        artifact: CacheArtifactKind,
      ) -> Option<FetchedResource> {
        assert_eq!(kind, FetchContextKind::Image);
        assert_eq!(artifact, CacheArtifactKind::ImageProbeMetadata);
        if url != self.url {
          return None;
        }

        let idx = self.calls.fetch_add(1, Ordering::SeqCst);
        if idx == 0 {
          // Simulate a slow disk read so concurrent probes overlap.
          thread::sleep(Duration::from_millis(50));
        }

        let mut res = FetchedResource::new(
          self.bytes.as_ref().clone(),
          Some("application/x-fastrender-image-probe+json".to_string()),
        );
        res.status = Some(200);
        res.final_url = Some(url.to_string());
        Some(res)
      }
    }

    let meta = CachedImageMetadata {
      width: 42,
      height: 24,
      orientation: None,
      resolution: None,
      is_vector: false,
      is_animated: true,
      intrinsic_ratio: None,
      aspect_ratio_none: false,
    };
    let bytes = Arc::new(encode_probe_metadata_for_disk(&meta).expect("encode metadata"));
    let url = "https://example.com/probe-inflight-artifact.png".to_string();
    let calls = Arc::new(AtomicUsize::new(0));
    let cache = ImageCache::with_fetcher(Arc::new(ArtifactFetcher {
      url: url.clone(),
      bytes: Arc::clone(&bytes),
      calls: Arc::clone(&calls),
    }));

    let threads = 8usize;
    let barrier = Arc::new(Barrier::new(threads));
    let mut handles = Vec::new();
    for _ in 0..threads {
      let cache = cache.clone();
      let url = url.clone();
      let barrier = Arc::clone(&barrier);
      handles.push(thread::spawn(move || {
        barrier.wait();
        cache.probe_resolved(&url)
      }));
    }

    for handle in handles {
      let probed = handle
        .join()
        .expect("probe thread panicked")
        .expect("probe ok");
      assert_eq!(probed.dimensions(), (42, 24));
      assert!(probed.is_animated);
    }

    assert_eq!(
      calls.load(Ordering::SeqCst),
      1,
      "cache artifact reads should be single-flight under probe in-flight"
    );
  }

  #[test]
  fn probe_inflight_wait_respects_render_deadline() {
    use std::sync::mpsc;
    use std::sync::Barrier;
    use std::thread;

    struct BlockingFetcher {
      started: Arc<Barrier>,
      release: Arc<Barrier>,
      url: String,
    }

    impl ResourceFetcher for BlockingFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        if url == self.url {
          self.started.wait();
          self.release.wait();
          return Ok(FetchedResource::new(
            b"not an image".to_vec(),
            Some("image/png".to_string()),
          ));
        }

        Err(Error::Resource(crate::error::ResourceError::new(
          url.to_string(),
          "unexpected url",
        )))
      }
    }

    let started = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let url = "https://example.com/blocked-probe.png".to_string();
    let cache = ImageCache::with_fetcher(Arc::new(BlockingFetcher {
      started: Arc::clone(&started),
      release: Arc::clone(&release),
      url: url.clone(),
    }));

    let owner_cache = cache.clone();
    let owner_url = url.clone();
    let owner_handle = thread::spawn(move || owner_cache.probe(&owner_url));

    started.wait();

    let waiter_cache = cache.clone();
    let waiter_url = url.clone();
    let (tx, rx) = mpsc::channel();
    let waiter_handle = thread::spawn(move || {
      let deadline = render_control::RenderDeadline::new(Some(Duration::from_millis(50)), None);
      let start = Instant::now();
      let result =
        render_control::with_deadline(Some(&deadline), || waiter_cache.probe(&waiter_url));
      tx.send((result, start.elapsed())).unwrap();
    });

    let (result, elapsed) = match rx.recv_timeout(Duration::from_secs(1)) {
      Ok(value) => value,
      Err(err) => {
        release.wait();
        let _ = owner_handle.join();
        drop(waiter_handle);
        panic!("waiter probe did not complete under deadline: {err}");
      }
    };

    let err = match result {
      Ok(_) => panic!("waiter probe should fail under deadline"),
      Err(err) => err,
    };
    match err {
      Error::Render(RenderError::Timeout { stage, .. }) => {
        assert_eq!(stage, RenderStage::Paint);
      }
      other => panic!("unexpected error after {elapsed:?}: {other:?}"),
    }

    release.wait();
    let _ = owner_handle.join();
    let _ = waiter_handle.join();
  }

  #[test]
  fn probe_inflight_wait_respects_cancel_callback() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::sync::Barrier;
    use std::thread;

    struct BlockingFetcher {
      started: Arc<Barrier>,
      release: Arc<Barrier>,
      url: String,
    }

    impl ResourceFetcher for BlockingFetcher {
      fn fetch(&self, url: &str) -> Result<FetchedResource> {
        if url == self.url {
          self.started.wait();
          self.release.wait();
          return Ok(FetchedResource::new(
            b"not an image".to_vec(),
            Some("image/png".to_string()),
          ));
        }

        Err(Error::Resource(crate::error::ResourceError::new(
          url.to_string(),
          "unexpected url",
        )))
      }
    }

    let started = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let url = "https://example.com/blocked-probe.png".to_string();
    let cache = ImageCache::with_fetcher(Arc::new(BlockingFetcher {
      started: Arc::clone(&started),
      release: Arc::clone(&release),
      url: url.clone(),
    }));

    let owner_cache = cache.clone();
    let owner_url = url.clone();
    let owner_handle = thread::spawn(move || owner_cache.probe(&owner_url));

    started.wait();

    let waiter_cache = cache.clone();
    let waiter_url = url.clone();
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let cancel_flag_worker = Arc::clone(&cancel_flag);
    let cancel: Arc<crate::render_control::CancelCallback> =
      Arc::new(move || cancel_flag_worker.load(Ordering::Relaxed));
    let (tx, rx) = mpsc::channel();
    let waiter_handle = thread::spawn(move || {
      let deadline = render_control::RenderDeadline::new(None, Some(cancel));
      let start = Instant::now();
      let result =
        render_control::with_deadline(Some(&deadline), || waiter_cache.probe(&waiter_url));
      tx.send((result, start.elapsed())).unwrap();
    });

    thread::sleep(Duration::from_millis(50));
    cancel_flag.store(true, Ordering::Relaxed);

    let (result, elapsed) = match rx.recv_timeout(Duration::from_secs(1)) {
      Ok(value) => value,
      Err(err) => {
        release.wait();
        let _ = owner_handle.join();
        drop(waiter_handle);
        panic!("waiter probe did not complete under cancel: {err}");
      }
    };

    let err = match result {
      Ok(_) => panic!("waiter probe should fail under cancel callback"),
      Err(err) => err,
    };
    match err {
      Error::Render(RenderError::Timeout { stage, .. }) => {
        assert_eq!(stage, RenderStage::Paint);
      }
      other => panic!("unexpected error after {elapsed:?}: {other:?}"),
    }

    release.wait();
    let _ = owner_handle.join();
    let _ = waiter_handle.join();
  }

  #[test]
  fn icc_adobe_rgb_profile_converts_to_srgb_reference_values() {
    let bytes = include_bytes!(
      "../tests/pages/fixtures/arstechnica.com/assets/52a1160fbdf01f8511cf15e24a23ec7e.jpg"
    );
    let icc = extract_jpeg_icc_profile(bytes).expect("extract icc profile");
    let transform = icc_transform_to_srgb(&icc).expect("build ICC transform");

    // Reference values computed via LittleCMS (Pillow/ImageCms) converting Adobe RGB (1998) -> sRGB.
    let (r, g, b) = transform.convert_rgb8(100, 150, 200);
    assert!(
      (r as i32 - 66).abs() <= 2 && (g as i32 - 151).abs() <= 2 && (b as i32 - 203).abs() <= 2,
      "unexpected AdobeRGB->sRGB conversion result: ({r}, {g}, {b})"
    );
  }

  #[test]
  fn image_cache_decodes_adobe_rgb_jpeg_with_color_management() {
    let bytes = include_bytes!(
      "../tests/pages/fixtures/arstechnica.com/assets/52a1160fbdf01f8511cf15e24a23ec7e.jpg"
    );
    let icc = extract_jpeg_icc_profile(bytes).expect("extract icc profile");
    let transform = icc_transform_to_srgb(&icc).expect("build ICC transform");

    let cache = ImageCache::new();
    let (raw, _has_alpha) = cache
      .decode_with_format(bytes, ImageFormat::Jpeg, "icc-adobe-rgb")
      .expect("decode without color management");
    let raw_px = raw.to_rgba8().get_pixel(10, 10).0;
    let expected = transform.convert_rgb8(raw_px[0], raw_px[1], raw_px[2]);

    let (decoded, _has_alpha) = cache
      .decode_bitmap(bytes, Some("image/jpeg"), "icc-adobe-rgb")
      .expect("decode with color management");
    let decoded_px = decoded.to_rgba8().get_pixel(10, 10).0;
    assert_eq!(
      (decoded_px[0], decoded_px[1], decoded_px[2]),
      expected,
      "decoded pixels should be color corrected"
    );
  }

  #[test]
  fn webp_icc_extractor_parses_riff_chunks_and_padding() {
    let icc_payload = b"abc";
    let riff_size: u32 = 4 /* WEBP */ + 8 /* ICCP header */ + icc_payload.len() as u32 + 1 /* pad */;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&riff_size.to_le_bytes());
    bytes.extend_from_slice(b"WEBP");
    bytes.extend_from_slice(b"ICCP");
    bytes.extend_from_slice(&(icc_payload.len() as u32).to_le_bytes());
    bytes.extend_from_slice(icc_payload);
    bytes.push(0); // odd-sized chunks are padded to an even boundary.

    assert_eq!(
      extract_webp_icc_profile(&bytes),
      Some(icc_payload.to_vec()),
      "expected ICCP payload to be extracted"
    );

    // Malformed/truncated payloads should fail cleanly without panicking.
    let mut truncated = bytes.clone();
    truncated.truncate(truncated.len().saturating_sub(2));
    assert!(extract_webp_icc_profile(&truncated).is_none());
  }

  #[test]
  fn image_cache_decodes_adobe_rgb_webp_with_color_management() {
    let bytes = include_bytes!(
      "../tests/pages/fixtures/en.wikipedia.org/assets/9550a19a4c433c52e322f1dd56981c9b.webp"
    );
    let icc = extract_webp_icc_profile(bytes).expect("extract icc profile");
    let transform = icc_transform_to_srgb(&icc).expect("build ICC transform");

    // Decode via libwebp directly to get the raw pixel values before color management.
    let raw = (|| -> RgbaImage {
      use std::ffi::c_int;
      let mut width: c_int = 0;
      let mut height: c_int = 0;
      unsafe {
        assert_ne!(
          libwebp_sys::WebPGetInfo(bytes.as_ptr(), bytes.len(), &mut width, &mut height),
          0,
          "WebPGetInfo failed"
        );
      }
      let width: u32 = width.try_into().expect("width fits u32");
      let height: u32 = height.try_into().expect("height fits u32");
      let stride: usize = width as usize * 4;
      let stride_i32: i32 = stride.try_into().expect("stride fits i32");
      let mut buf = vec![0u8; stride * height as usize];
      unsafe {
        let out = libwebp_sys::WebPDecodeRGBAInto(
          bytes.as_ptr(),
          bytes.len(),
          buf.as_mut_ptr(),
          buf.len(),
          stride_i32,
        );
        assert!(!out.is_null(), "WebPDecodeRGBAInto failed");
      }
      RgbaImage::from_raw(width, height, buf).expect("raw rgba buffer is valid")
    })();

    let (w, h) = raw.dimensions();
    let candidates = [
      (0, 0),
      (w / 2, h / 2),
      (w / 3, h / 3),
      (w.saturating_sub(1), h.saturating_sub(1)),
      (10.min(w.saturating_sub(1)), 10.min(h.saturating_sub(1))),
    ];
    let mut chosen = (0u32, 0u32);
    let mut expected = (0u8, 0u8, 0u8);
    for (x, y) in candidates {
      let raw_px = raw.get_pixel(x, y).0;
      let converted = transform.convert_rgb8(raw_px[0], raw_px[1], raw_px[2]);
      chosen = (x, y);
      expected = converted;
      if converted != (raw_px[0], raw_px[1], raw_px[2]) {
        break;
      }
    }

    let cache = ImageCache::new();
    let (decoded, _has_alpha) = cache
      .decode_bitmap(bytes, Some("image/webp"), "icc-adobe-rgb-webp")
      .expect("decode with color management");
    let decoded_px = decoded.to_rgba8().get_pixel(chosen.0, chosen.1).0;
    assert_eq!(
      (decoded_px[0], decoded_px[1], decoded_px[2]),
      expected,
      "decoded pixels should be color corrected"
    );
  }
}
