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
  let type_trimmed = type_.trim();
  if type_trimmed.is_empty() {
    return CanPlayType::No;
  }

  let mut parts = type_trimmed.split(';');
  let mime = parts
    .next()
    .map(str::trim)
    .filter(|s| !s.is_empty())
    .unwrap_or("");
  if mime.is_empty() {
    return CanPlayType::No;
  }
  let mime = mime.to_ascii_lowercase();

  // Container-aware codec allowlist. This prevents mismatched `audio/*` vs `video/*` queries from
  // producing surprising results (e.g. `audio/webm; codecs=vp9` should be `No`, not `Probably`).
  #[derive(Clone, Copy, Debug, PartialEq, Eq)]
  enum Container {
    AudioMp4,
    VideoMp4,
    AudioWebm,
    VideoWebm,
  }

  let container = match mime.as_str() {
    "audio/mp4" => Container::AudioMp4,
    "video/mp4" => Container::VideoMp4,
    "audio/webm" => Container::AudioWebm,
    "video/webm" => Container::VideoWebm,
    _ => return CanPlayType::No,
  };

  // Find the first `codecs` parameter (case-insensitive).
  let mut codecs_param: Option<&str> = None;
  for param in parts {
    let param = param.trim();
    if param.is_empty() {
      continue;
    }
    let (key, value) = param.split_once('=').unwrap_or((param, ""));
    if key.trim().eq_ignore_ascii_case("codecs") {
      codecs_param = Some(value);
      break;
    }
  }

  let Some(codecs_param) = codecs_param else {
    // Supported container, but no explicit codecs list.
    return CanPlayType::Maybe;
  };

  fn unquote(value: &str) -> Option<&str> {
    let value = value.trim();
    let bytes = value.as_bytes();
    if bytes.len() >= 2 && (bytes[0] == b'"' || bytes[0] == b'\'') {
      let q = bytes[0];
      if bytes[bytes.len() - 1] != q {
        return None;
      }
      return Some(&value[1..value.len() - 1]);
    }
    Some(value)
  }

  let Some(codecs_list) = unquote(codecs_param) else {
    return CanPlayType::No;
  };

  fn is_mp4_audio_codec(codec: &str) -> bool {
    codec == "mp4a" || codec.starts_with("mp4a.")
  }

  fn is_mp4_video_codec(codec: &str) -> bool {
    codec == "avc1" || codec.starts_with("avc1.")
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

  for raw in codecs_list.split(',') {
    let codec = raw.trim();
    if codec.is_empty() {
      // An explicit but empty codec entry is not meaningful; treat as unsupported.
      return CanPlayType::No;
    }
    let codec = codec.to_ascii_lowercase();

    let allowed = match container {
      Container::AudioMp4 => is_mp4_audio_codec(&codec),
      Container::VideoMp4 => is_mp4_audio_codec(&codec) || is_mp4_video_codec(&codec),
      Container::AudioWebm => is_webm_audio_codec(&codec),
      Container::VideoWebm => is_webm_audio_codec(&codec) || is_webm_video_codec(&codec),
    };

    if !allowed {
      return CanPlayType::No;
    }
  }

  CanPlayType::Probably
}

#[cfg(test)]
mod tests {
  use super::{can_play_type, CanPlayType};

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
    assert_eq!(
      can_play_type("video/mp4; codecs=mp4a.40.2"),
      CanPlayType::Probably
    );
  }
}

