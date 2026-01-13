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
pub enum MediaKind {
  Video,
  Audio,
}

/// Backwards compatible alias; older call sites used `MediaElementKind`.
pub type MediaElementKind = MediaKind;

/// Selection inputs describing the rendering environment.
///
/// This struct is intentionally extendable; the selection algorithm may need
/// additional information in the future (e.g. platform decoder capabilities).
#[derive(Clone, Copy, Debug, Default)]
pub struct MediaSelectionContext<'a> {
  pub media_context: Option<&'a MediaContext>,
}

/// Result of selecting a media source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectedMediaSource<'a> {
  pub url: &'a str,
  pub mime: Option<String>,
  pub codecs: Vec<String>,
  /// True when the selection came from a `<source>` element.
  pub from_source: bool,
}

impl<'a> SelectedMediaSource<'a> {
  fn empty() -> Self {
    Self {
      url: "",
      mime: None,
      codecs: Vec::new(),
      from_source: false,
    }
  }
}

/// A `<source>` candidate for media selection.
#[derive(Clone, Copy, Debug)]
pub struct MediaSourceCandidate<'a> {
  pub src: &'a str,
  pub type_attr: Option<&'a str>,
  pub media_attr: Option<&'a str>,
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

fn starts_with_ignore_ascii_case(value: &str, prefix: &str) -> bool {
  value
    .as_bytes()
    .get(..prefix.len())
    .is_some_and(|head| head.eq_ignore_ascii_case(prefix.as_bytes()))
}

fn infer_mime_from_src(kind: MediaKind, src: &str) -> Option<String> {
  let trimmed = trim_ascii_whitespace(src);
  if trimmed.is_empty() {
    return None;
  }

  // Infer from data URLs.
  if starts_with_ignore_ascii_case(trimmed, "data:") {
    let rest = &trimmed["data:".len()..];
    let (metadata, _payload) = rest.split_once(',')?;
    let mediatype = trim_ascii_whitespace(metadata.split(';').next().unwrap_or(""));
    let mediatype = if mediatype.is_empty() { "text/plain" } else { mediatype };
    return Some(mediatype.to_ascii_lowercase());
  }

  // Strip query/fragment before extension checks.
  let cut = trimmed
    .find(|c| matches!(c, '?' | '#'))
    .unwrap_or(trimmed.len());
  let path = &trimmed[..cut];
  let ext = path.rsplit_once('.')?.1.to_ascii_lowercase();

  let inferred = match ext.as_str() {
    "mp4" => match kind {
      MediaKind::Video => "video/mp4",
      MediaKind::Audio => "audio/mp4",
    },
    "m4v" => "video/mp4",
    "m4a" => "audio/mp4",
    "webm" => match kind {
      MediaKind::Video => "video/webm",
      MediaKind::Audio => "audio/webm",
    },
    "ogg" => match kind {
      MediaKind::Video => "video/ogg",
      MediaKind::Audio => "audio/ogg",
    },
    "ogv" => "video/ogg",
    "oga" => "audio/ogg",
    "mp3" => "audio/mpeg",
    "wav" => "audio/wav",
    "aac" => "audio/aac",
    "flac" => "audio/flac",
    "opus" => "audio/opus",
    _ => return None,
  };

  Some(inferred.to_string())
}

fn mime_matches_kind(kind: MediaKind, mime: &str) -> bool {
  match kind {
    MediaKind::Video => mime.starts_with("video/"),
    MediaKind::Audio => mime.starts_with("audio/"),
  }
}

fn media_attribute_matches(media_attr: Option<&str>, ctx: MediaSelectionContext<'_>) -> bool {
  let Some(raw) = media_attr else {
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

  let Some(media_ctx) = ctx.media_context else {
    // Caller cannot evaluate media queries yet. Treat as match-all for now.
    return true;
  };

  media_ctx.evaluate_list(&list)
}

/// Selects a media source for `<video>`/`<audio>`.
///
/// This is a shared (box-generation + JS) subset of the HTML media element source selection
/// algorithm; see the module-level docs for ordering rules.
pub fn select_media_source<'a>(
  kind: MediaKind,
  element_src: Option<&'a str>,
  sources: impl IntoIterator<Item = MediaSourceCandidate<'a>>,
  ctx: MediaSelectionContext<'_>,
) -> SelectedMediaSource<'a> {
  if let Some(src) = element_src {
    let trimmed = trim_ascii_whitespace(src);
    if !media_src_is_unusable(trimmed) {
      return SelectedMediaSource {
        url: trimmed,
        mime: None,
        codecs: Vec::new(),
        from_source: false,
      };
    }
  }

  let mut fallback: Option<SelectedMediaSource<'a>> = None;

  for candidate in sources {
    let src_trimmed = trim_ascii_whitespace(candidate.src);
    if media_src_is_unusable(src_trimmed) {
      continue;
    }
    if !media_attribute_matches(candidate.media_attr, ctx) {
      continue;
    }

    let type_trimmed = candidate
      .type_attr
      .map(trim_ascii_whitespace)
      .filter(|t| !t.is_empty());
    let parsed = type_trimmed.and_then(parse_type_attribute);

    let (mime, codecs, playability) = if let (Some(type_str), Some(parsed)) = (type_trimmed, parsed)
    {
      (Some(parsed.mime), parsed.codecs, can_play_type(type_str))
    } else {
      let mime = infer_mime_from_src(kind, src_trimmed);
      let playability = mime
        .as_deref()
        .map(can_play_type)
        .unwrap_or(CanPlayType::No);
      (mime, Vec::new(), playability)
    };

    let selected = SelectedMediaSource {
      url: src_trimmed,
      mime,
      codecs,
      from_source: true,
    };

    let preferred = selected
      .mime
      .as_deref()
      .is_some_and(|mime| mime_matches_kind(kind, mime) && playability.is_playable());
    if preferred {
      return selected;
    }

    if fallback.is_none() {
      fallback = Some(selected);
    }
  }

  fallback.unwrap_or_else(SelectedMediaSource::empty)
}

/// Compute the effective media source URL for a `<video>`/`<audio>` element during box generation.
///
/// The returned string is ASCII-whitespace trimmed, matching HTML's attribute parsing behavior.
pub fn effective_media_src(
  styled: &StyledNode,
  kind: MediaElementKind,
  media_context: Option<&MediaContext>,
) -> String {
  let element_src = styled.node.get_attribute_ref("src");
  let sources = styled.children.iter().filter_map(|child| {
    let tag = child.node.tag_name()?;
    if !tag.eq_ignore_ascii_case("source") {
      return None;
    }
    let src = child.node.get_attribute_ref("src")?;
    Some(MediaSourceCandidate {
      src,
      type_attr: child.node.get_attribute_ref("type"),
      media_attr: child.node.get_attribute_ref("media"),
    })
  });

  select_media_source(
    kind,
    element_src,
    sources,
    MediaSelectionContext {
      media_context: media_context,
    },
  )
  .url
  .to_string()
}

#[cfg(test)]
mod tests {
  use super::{
    can_play_type, parse_type_attribute, select_media_source, CanPlayType, MediaKind,
    MediaSelectionContext, MediaSourceCandidate,
  };
  use crate::style::media::MediaContext;

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

  #[test]
  fn media_src_attribute_wins_over_source_children() {
    let selected = select_media_source(
      MediaKind::Video,
      Some("parent.mp4"),
      [MediaSourceCandidate {
        src: "child.mp4",
        type_attr: Some("video/mp4"),
        media_attr: None,
      }],
      MediaSelectionContext { media_context: None },
    );

    assert_eq!(selected.url, "parent.mp4");
    assert!(!selected.from_source);
    assert!(selected.mime.is_none());
    assert!(selected.codecs.is_empty());
  }

  #[test]
  fn media_attribute_filters_sources_when_context_available() {
    let media_ctx = MediaContext::screen(800.0, 600.0);
    let selected = select_media_source(
      MediaKind::Video,
      None,
      [
        MediaSourceCandidate {
          src: "small.mp4",
          type_attr: Some("video/mp4"),
          media_attr: Some("(max-width: 500px)"),
        },
        MediaSourceCandidate {
          src: "large.mp4",
          type_attr: Some("video/mp4"),
          media_attr: None,
        },
      ],
      MediaSelectionContext {
        media_context: Some(&media_ctx),
      },
    );

    assert_eq!(selected.url, "large.mp4");
    assert!(selected.from_source);
  }

  #[test]
  fn selection_prefers_playable_codec_over_unplayable() {
    let selected = select_media_source(
      MediaKind::Video,
      None,
      [
        MediaSourceCandidate {
          src: "bad.mp4",
          type_attr: Some("video/mp4; codecs=badcodec"),
          media_attr: None,
        },
        MediaSourceCandidate {
          src: "good.mp4",
          type_attr: Some("video/mp4; codecs=avc1.42E01E"),
          media_attr: None,
        },
      ],
      MediaSelectionContext { media_context: None },
    );

    assert_eq!(selected.url, "good.mp4");
    assert!(selected.from_source);
  }
}
