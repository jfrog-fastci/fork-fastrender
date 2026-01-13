//! HTML media helpers.
//!
//! This module contains:
//! - best-effort parsing of HTML media `type` attribute values (including common non-standard
//!   quoting forms seen in the wild),
//! - a best-effort `HTMLMediaElement.canPlayType()`-style support check, and
//! - a small, stable subset of `<video>`/`<audio>` source selection used during box generation.
//!
//! FastRender does not currently implement the full HTML "resource selection algorithm" for media
//! elements. For box generation we intentionally implement a small subset:
//!
//! - The element `src` attribute wins when present and not "unusable".
//! - Otherwise scan `<source>` children in DOM order.
//! - Remember the first `<source src>` as a fallback.
//! - Prefer a `<source>` whose `type` parses to the right kind (`video/*` for `<video>`,
//!   `audio/*` for `<audio>`) *and* `can_play_type(type)` is not [`CanPlayType::No`].
//! - If no preferred candidate exists, fall back to the first `<source>` so pages relying on
//!   sniffing do not regress.
//!
//! The API accepts an optional [`MediaContext`] so `<source media>` conditions can be honored when a
//! media-query evaluation context is available.

use crate::style::cascade::StyledNode;
use crate::style::media::{MediaContext, MediaQuery};

/// Result type for `HTMLMediaElement.canPlayType()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanPlayType {
  No,
  Maybe,
  Probably,
}

impl CanPlayType {
  pub fn as_str(self) -> &'static str {
    match self {
      Self::No => "",
      Self::Maybe => "maybe",
      Self::Probably => "probably",
    }
  }

  #[inline]
  pub fn is_playable(self) -> bool {
    !matches!(self, Self::No)
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaElementKind {
  Video,
  Audio,
}

// HTML defines "ASCII whitespace" as: U+0009 TAB, U+000A LF, U+000C FF, U+000D CR, U+0020 SPACE.
fn trim_ascii_whitespace(value: &str) -> &str {
  value.trim_matches(|c: char| {
    matches!(
      c,
      '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | '\u{0020}'
    )
  })
}

/// Parsed representation of an HTML media `type` attribute value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedMediaType {
  /// Lowercased `type/subtype` portion (e.g. `video/mp4`).
  pub mime: String,
  /// Parsed `codecs` parameter values, lowercased and split on commas.
  pub codecs: Vec<String>,
}

fn split_params(value: &str) -> Vec<&str> {
  // Split on semicolons, but ignore semicolons within quoted strings.
  let mut parts = Vec::new();
  let bytes = value.as_bytes();
  let mut quote: Option<u8> = None;
  let mut start = 0usize;
  let mut i = 0usize;
  while i < bytes.len() {
    let b = bytes[i];
    match quote {
      Some(q) => {
        if b == q {
          quote = None;
        }
      }
      None => {
        if b == b'"' || b == b'\'' {
          quote = Some(b);
        } else if b == b';' {
          parts.push(&value[start..i]);
          start = i + 1;
        }
      }
    }
    i += 1;
  }
  parts.push(&value[start..]);
  parts
}

fn parse_param_value(raw: &str) -> &str {
  let raw = trim_ascii_whitespace(raw);
  if raw.len() >= 2 {
    let bytes = raw.as_bytes();
    // Accept both standard `"..."`
    // and common real-world `'...'` quoting for parameter values.
    if (bytes[0] == b'"' && bytes[raw.len() - 1] == b'"')
      || (bytes[0] == b'\'' && bytes[raw.len() - 1] == b'\'')
    {
      // Quotes are ASCII and therefore always lie on UTF-8 boundaries.
      return &raw[1..raw.len() - 1];
    }
  }
  raw
}

fn parse_codecs_list(raw: &str) -> Vec<String> {
  let raw = trim_ascii_whitespace(raw);
  if raw.is_empty() {
    return Vec::new();
  }
  raw
    .split(',')
    .filter_map(|item| {
      let item = trim_ascii_whitespace(item);
      (!item.is_empty()).then_some(item.to_ascii_lowercase())
    })
    .collect()
}

/// Best-effort parse of a `type` attribute value.
///
/// Real-world content often uses non-standard but common quoting forms in MIME parameters, e.g.:
/// `type="video/mp4; codecs='avc1.42E01E, mp4a.40.2'"`.
///
/// The browser ecosystem is lenient here; we follow suit by accepting both single- and
/// double-quoted codec parameter values when parsing.
pub fn parse_type_attribute(value: &str) -> Option<ParsedMediaType> {
  let trimmed = trim_ascii_whitespace(value);
  if trimmed.is_empty() {
    return None;
  }

  let (mime_raw, params_raw) = trimmed
    .split_once(';')
    .map(|(m, p)| (m, Some(p)))
    .unwrap_or((trimmed, None));

  let mime = trim_ascii_whitespace(mime_raw).to_ascii_lowercase();
  if mime.is_empty() {
    return None;
  }

  let mut codecs: Vec<String> = Vec::new();
  if let Some(params_raw) = params_raw {
    for param in split_params(params_raw) {
      let param = trim_ascii_whitespace(param);
      if param.is_empty() {
        continue;
      }
      let Some((name, value)) = param.split_once('=') else {
        continue;
      };
      let name = trim_ascii_whitespace(name);
      if !name.eq_ignore_ascii_case("codecs") {
        continue;
      }
      let value = parse_param_value(value);
      codecs = parse_codecs_list(value);
      break;
    }
  }

  Some(ParsedMediaType { mime, codecs })
}

/// Best-effort implementation of `HTMLMediaElement.canPlayType()`.
///
/// This intentionally models browser behavior:
/// - For *supported* containers, the absence of a `codecs` parameter yields `Maybe`.
/// - When `codecs` is present, all codecs must be compatible with the container; otherwise `No`.
/// - When `codecs` is present and all codecs are permitted, we return `Probably`.
///
/// Note: This does not attempt to validate codec profile/level details. We treat known codec name
/// prefixes (e.g. `avc1.*`, `mp4a.*`) as supported.
pub fn can_play_type(type_: &str) -> CanPlayType {
  let Some(parsed) = parse_type_attribute(type_) else {
    return CanPlayType::No;
  };

  // Container-aware codec allowlist. This prevents mismatched `audio/*` vs `video/*` queries from
  // producing surprising results (e.g. `audio/webm; codecs=vp9` should be `No`, not `Probably`).
  #[derive(Clone, Copy, Debug, PartialEq, Eq)]
  enum Container {
    AudioMp4,
    VideoMp4,
    AudioWebm,
    VideoWebm,
    AudioOgg,
    VideoOgg,
  }

  let container = match parsed.mime.as_str() {
    "audio/mp4" => Container::AudioMp4,
    "video/mp4" => Container::VideoMp4,
    "audio/webm" => Container::AudioWebm,
    "video/webm" => Container::VideoWebm,
    "audio/ogg" => Container::AudioOgg,
    "video/ogg" => Container::VideoOgg,

    // Common real-world aliases.
    "audio/mpeg" | "audio/mp3" => return CanPlayType::Maybe,

    _ => return CanPlayType::No,
  };

  if parsed.codecs.is_empty() {
    // Supported container, but no explicit codecs list.
    return CanPlayType::Maybe;
  }

  fn is_mp4_audio_codec(codec: &str) -> bool {
    codec == "mp4a" || codec.starts_with("mp4a.")
  }

  fn is_mp4_video_codec(codec: &str) -> bool {
    codec == "avc1"
      || codec.starts_with("avc1.")
      || codec == "avc3"
      || codec.starts_with("avc3.")
      || codec == "hev1"
      || codec.starts_with("hev1.")
      || codec == "hvc1"
      || codec.starts_with("hvc1.")
  }

  fn is_webm_audio_codec(codec: &str) -> bool {
    codec == "opus" || codec == "vorbis"
  }

  fn is_webm_video_codec(codec: &str) -> bool {
    codec == "vp8"
      || codec.starts_with("vp8.")
      || codec == "vp9"
      || codec.starts_with("vp9.")
      || codec.starts_with("vp09")
      || codec.starts_with("av01")
  }

  fn is_ogg_audio_codec(codec: &str) -> bool {
    matches!(codec, "opus" | "vorbis" | "flac" | "speex")
  }

  fn is_ogg_video_codec(codec: &str) -> bool {
    matches!(codec, "theora" | "dirac")
  }

  for codec in &parsed.codecs {
    let allowed = match container {
      Container::AudioMp4 => is_mp4_audio_codec(codec),
      Container::VideoMp4 => is_mp4_audio_codec(codec) || is_mp4_video_codec(codec),
      Container::AudioWebm => is_webm_audio_codec(codec),
      Container::VideoWebm => is_webm_audio_codec(codec) || is_webm_video_codec(codec),
      Container::AudioOgg => is_ogg_audio_codec(codec),
      Container::VideoOgg => is_ogg_audio_codec(codec) || is_ogg_video_codec(codec),
    };

    if !allowed {
      return CanPlayType::No;
    }
  }

  CanPlayType::Probably
}

/// Mirror the box-generation semantics for determining whether a media element `src` attribute is
/// unusable and should fall back to `<source>` children.
pub fn media_src_is_unusable(src: &str) -> bool {
  let trimmed = trim_ascii_whitespace(src);
  if trimmed.is_empty() || trimmed.starts_with('#') {
    return true;
  }
  const ABOUT_BLANK: &str = "about:blank";
  if trimmed
    .get(..ABOUT_BLANK.len())
    .is_some_and(|head| head.eq_ignore_ascii_case(ABOUT_BLANK))
  {
    return matches!(
      trimmed.as_bytes().get(ABOUT_BLANK.len()),
      None | Some(b'#') | Some(b'?')
    );
  }
  false
}

fn source_media_matches(source: &StyledNode, media_context: Option<&MediaContext>) -> bool {
  let Some(raw) = source.node.get_attribute_ref("media") else {
    return true;
  };

  let trimmed = trim_ascii_whitespace(raw);
  if trimmed.is_empty() {
    return true;
  }

  let Some(list) = MediaQuery::parse_list(trimmed).ok() else {
    // Be conservative: if we cannot parse the media query list, do not exclude the candidate.
    return true;
  };

  let Some(ctx) = media_context else {
    // Caller cannot evaluate media queries yet. Treat as match-all for now.
    return true;
  };

  ctx.evaluate_list(&list)
}

/// Compute the effective media source URL for a `<video>`/`<audio>` element during box generation.
///
/// The returned string is ASCII-whitespace trimmed, matching HTML's attribute parsing behavior.
pub fn effective_media_src(
  styled: &StyledNode,
  kind: MediaElementKind,
  media_context: Option<&MediaContext>,
) -> String {
  let src = styled
    .node
    .get_attribute_ref("src")
    .map(trim_ascii_whitespace)
    .unwrap_or("");
  if !media_src_is_unusable(src) {
    return src.to_string();
  }

  let preferred_prefix = match kind {
    MediaElementKind::Video => "video/",
    MediaElementKind::Audio => "audio/",
  };

  let mut first_any: Option<String> = None;
  for child in &styled.children {
    let Some(tag) = child.node.tag_name() else {
      continue;
    };
    if !tag.eq_ignore_ascii_case("source") {
      continue;
    }
    if !source_media_matches(child, media_context) {
      continue;
    }

    let Some(src_attr) = child.node.get_attribute_ref("src") else {
      continue;
    };
    let src_trimmed = trim_ascii_whitespace(src_attr);
    if src_trimmed.is_empty() {
      continue;
    }
    if first_any.is_none() {
      first_any = Some(src_trimmed.to_string());
    }

    let Some(type_attr) = child.node.get_attribute_ref("type") else {
      continue;
    };
    let type_trimmed = trim_ascii_whitespace(type_attr);
    if type_trimmed.is_empty() {
      continue;
    }

    let Some(parsed) = parse_type_attribute(type_trimmed) else {
      continue;
    };
    if !parsed.mime.starts_with(preferred_prefix) {
      continue;
    }

    if can_play_type(type_trimmed).is_playable() {
      return src_trimmed.to_string();
    }
  }

  first_any.unwrap_or_default()
}

#[cfg(test)]
mod tests {
  use super::{can_play_type, parse_type_attribute, CanPlayType};

  #[test]
  fn type_attribute_parses_single_quoted_codecs_mp4() {
    let parsed = parse_type_attribute("video/mp4; codecs='avc1.42E01E, mp4a.40.2'").unwrap();
    assert_eq!(parsed.mime, "video/mp4");
    assert_eq!(
      parsed.codecs,
      vec!["avc1.42e01e".to_string(), "mp4a.40.2".to_string()]
    );
  }

  #[test]
  fn type_attribute_parses_single_quoted_codecs_webm() {
    let parsed = parse_type_attribute("video/webm; codecs='vp9,opus'").unwrap();
    assert_eq!(parsed.mime, "video/webm");
    assert_eq!(parsed.codecs, vec!["vp9".to_string(), "opus".to_string()]);
  }

  #[test]
  fn audio_webm_rejects_video_codecs() {
    assert_eq!(can_play_type("audio/webm; codecs=vp9"), CanPlayType::No);
  }

  #[test]
  fn audio_mp4_rejects_video_codecs() {
    assert_eq!(
      can_play_type("audio/mp4; codecs=avc1.42E01E"),
      CanPlayType::No
    );
  }

  #[test]
  fn video_webm_accepts_audio_only_codec() {
    // Browsers accept audio-only WebM in a `<video>` element; the container indicates WebM support,
    // and the codec is compatible with WebM.
    assert_eq!(can_play_type("video/webm; codecs=opus"), CanPlayType::Probably);
  }

  #[test]
  fn video_mp4_accepts_audio_only_codec() {
    // We return `Probably` here to align with mainstream browsers: `video/mp4` indicates the ISO
    // BMFF container, which can carry audio-only tracks, and `mp4a.*` is a valid MP4 audio codec.
    assert_eq!(can_play_type("video/mp4; codecs=mp4a.40.2"), CanPlayType::Probably);
  }

  #[test]
  fn rejects_bogus_codecs() {
    assert_eq!(can_play_type("video/webm; codecs=bogus"), CanPlayType::No);
  }
}
