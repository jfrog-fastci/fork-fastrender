//! Media loader helpers.
//!
//! This module provides lightweight, best-effort validation before attempting container demux.
//! The primary goal is to avoid passing obvious non-media payloads (HTML error pages, JSON API
//! errors, etc) into demux/decoder logic, which otherwise produces confusing low-level failures.
//!
//! The checks here are intentionally permissive: unknown or generic content-types (including
//! missing/empty headers) are accepted.

use super::demux::webm::WebmDemuxer;
use super::mp4::Mp4Demuxer;
use super::MediaError;
use super::MediaResult;
use std::io::Cursor;

fn trim_http_whitespace(value: &str) -> &str {
  value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000D}' | '\u{0020}'))
}

fn content_type_mime(content_type: &str) -> &str {
  trim_http_whitespace(content_type.split(';').next().unwrap_or(content_type))
}

fn starts_with_ignore_ascii_case(value: &str, prefix: &str) -> bool {
  value
    .get(..prefix.len())
    .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}

fn ends_with_ignore_ascii_case(value: &str, suffix: &str) -> bool {
  let Some(tail) = value
    .len()
    .checked_sub(suffix.len())
    .and_then(|idx| value.get(idx..))
  else {
    return false;
  };
  tail.eq_ignore_ascii_case(suffix)
}

fn url_without_query_fragment(url: &str) -> &str {
  let url = url.trim();
  let url = url.split_once('#').map(|(before, _)| before).unwrap_or(url);
  url.split_once('?').map(|(before, _)| before).unwrap_or(url)
}

fn payload_looks_like_markup(bytes: &[u8]) -> bool {
  let sample = &bytes[..bytes.len().min(256)];
  let mut i = 0;
  if sample.starts_with(b"\xef\xbb\xbf") {
    i = 3;
  }
  while i < sample.len() && sample[i].is_ascii_whitespace() {
    i += 1;
  }
  sample.get(i) == Some(&b'<')
}

fn mime_is_html(mime: &str) -> bool {
  let mime = trim_http_whitespace(mime);
  starts_with_ignore_ascii_case(mime, "text/html")
    || starts_with_ignore_ascii_case(mime, "application/xhtml+xml")
    || starts_with_ignore_ascii_case(mime, "application/html")
    || ends_with_ignore_ascii_case(mime, "+html")
}

fn mime_is_javascript(mime: &str) -> bool {
  let mime = trim_http_whitespace(mime);
  starts_with_ignore_ascii_case(mime, "application/javascript")
    || starts_with_ignore_ascii_case(mime, "text/javascript")
    || starts_with_ignore_ascii_case(mime, "application/ecmascript")
    || starts_with_ignore_ascii_case(mime, "text/ecmascript")
    || starts_with_ignore_ascii_case(mime, "application/x-ecmascript")
    || starts_with_ignore_ascii_case(mime, "application/x-javascript")
}

/// Best-effort MIME sanity check for fetched media (MP4/WebM).
///
/// When strict MIME checks are enabled (`FASTR_FETCH_STRICT_MIME=1`), this rejects content-types
/// that strongly indicate non-media payloads (HTML/JSON/etc). Everything else is accepted.
pub fn ensure_media_mime_sane(url: &str, content_type: Option<&str>) -> MediaResult<()> {
  if !crate::resource::strict_mime_checks_enabled() {
    return Ok(());
  }

  let Some(content_type) = content_type else {
    return Ok(());
  };
  let mime = content_type_mime(content_type);
  if mime.is_empty() {
    return Ok(());
  }

  let url_no_qf = url_without_query_fragment(url);
  let looks_like_mp4 = ends_with_ignore_ascii_case(url_no_qf, ".mp4")
    || ends_with_ignore_ascii_case(url_no_qf, ".m4v")
    || ends_with_ignore_ascii_case(url_no_qf, ".m4a")
    || ends_with_ignore_ascii_case(url_no_qf, ".mov");
  let looks_like_webm =
    ends_with_ignore_ascii_case(url_no_qf, ".webm") || ends_with_ignore_ascii_case(url_no_qf, ".mkv");

  // Accept common, real-world content types for MP4-ish media.
  let allowed_mp4 = mime.eq_ignore_ascii_case("video/mp4")
    || mime.eq_ignore_ascii_case("audio/mp4")
    || mime.eq_ignore_ascii_case("application/mp4")
    || mime.eq_ignore_ascii_case("video/quicktime")
    || mime.eq_ignore_ascii_case("audio/quicktime");
  let allowed_webm = mime.eq_ignore_ascii_case("video/webm")
    || mime.eq_ignore_ascii_case("audio/webm")
    || mime.eq_ignore_ascii_case("video/x-matroska")
    || mime.eq_ignore_ascii_case("audio/x-matroska");

  if (looks_like_mp4 && allowed_mp4) || (looks_like_webm && allowed_webm) {
    return Ok(());
  }

  // Reject content types that are strongly indicative of non-media payloads (HTML error pages,
  // JSON API errors, etc). Keep this conservative: unknown or generic types like
  // `application/octet-stream` are accepted so we don't break misconfigured servers.
  if mime_is_html(mime)
    || starts_with_ignore_ascii_case(mime, "text/")
    || starts_with_ignore_ascii_case(mime, "image/")
    || starts_with_ignore_ascii_case(mime, "font/")
    || mime_is_javascript(mime)
    || mime.eq_ignore_ascii_case("application/json")
    || ends_with_ignore_ascii_case(mime, "+json")
    || mime.eq_ignore_ascii_case("application/xml")
    || mime.eq_ignore_ascii_case("text/xml")
    || ends_with_ignore_ascii_case(mime, "+xml")
  {
    return Err(MediaError::LoadFailed {
      url: url.to_string(),
      reason: format!("unexpected content-type {mime}"),
    });
  }

  Ok(())
}

/// Parses MP4 bytes after running [`ensure_media_mime_sane`].
#[cfg(feature = "media_mp4")]
pub fn open_mp4_demuxer(
  url: &str,
  content_type: Option<&str>,
  bytes: &[u8],
) -> MediaResult<Mp4Demuxer> {
  ensure_media_mime_sane(url, content_type)?;
  if crate::resource::strict_mime_checks_enabled() && payload_looks_like_markup(bytes) {
    return Err(MediaError::LoadFailed {
      url: url.to_string(),
      reason: "unexpected markup response body".to_string(),
    });
  }
  Mp4Demuxer::new(bytes).map_err(|err| MediaError::Demux(err.to_string()))
}

/// Parses MP4 bytes after running [`ensure_media_mime_sane`].
#[cfg(not(feature = "media_mp4"))]
pub fn open_mp4_demuxer(
  url: &str,
  content_type: Option<&str>,
  _bytes: &[u8],
) -> MediaResult<Mp4Demuxer> {
  ensure_media_mime_sane(url, content_type)?;
  Err(MediaError::Unsupported(
    "`media_mp4` feature disabled (enable Cargo feature `media_mp4` or `media`)",
  ))
}

/// Opens a WebM demuxer after running [`ensure_media_mime_sane`].
pub fn open_webm_demuxer<'a>(
  url: &str,
  content_type: Option<&str>,
  bytes: &'a [u8],
) -> MediaResult<WebmDemuxer<Cursor<&'a [u8]>>> {
  ensure_media_mime_sane(url, content_type)?;
  if crate::resource::strict_mime_checks_enabled() && payload_looks_like_markup(bytes) {
    return Err(MediaError::LoadFailed {
      url: url.to_string(),
      reason: "unexpected markup response body".to_string(),
    });
  }
  WebmDemuxer::open(Cursor::new(bytes))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::debug::runtime;
  use std::collections::HashMap;
  use std::sync::Arc;

  fn strict_mime_toggles() -> Arc<runtime::RuntimeToggles> {
    Arc::new(runtime::RuntimeToggles::from_map(HashMap::from([(
      "FASTR_FETCH_STRICT_MIME".to_string(),
      "1".to_string(),
    )])))
  }

  #[test]
  fn media_mime_sanity_allows_common_mp4_content_types() {
    runtime::with_thread_runtime_toggles(strict_mime_toggles(), || {
      let url = "https://example.com/video.mp4";
      for content_type in [
        "video/mp4",
        "video/mp4; charset=binary",
        "application/mp4",
        "video/quicktime",
      ] {
        ensure_media_mime_sane(url, Some(content_type))
          .unwrap_or_else(|err| panic!("expected {content_type} to be allowed: {err:?}"));
      }
      ensure_media_mime_sane(url, None).expect("missing content-type should be allowed");
    });
  }

  #[test]
  fn media_mime_sanity_allows_common_webm_content_types() {
    runtime::with_thread_runtime_toggles(strict_mime_toggles(), || {
      let url = "https://example.com/video.webm";
      for content_type in [
        "video/webm",
        "audio/webm",
        "video/x-matroska",
        "audio/x-matroska",
      ] {
        ensure_media_mime_sane(url, Some(content_type))
          .unwrap_or_else(|err| panic!("expected {content_type} to be allowed: {err:?}"));
      }
      ensure_media_mime_sane(url, None).expect("missing content-type should be allowed");
    });
  }

  #[test]
  fn media_mime_sanity_rejects_html_and_json_content_types() {
    runtime::with_thread_runtime_toggles(strict_mime_toggles(), || {
      let url = "https://example.com/video.mp4";
      for content_type in [
        "text/html",
        "text/html; charset=utf-8",
        "application/json",
        "application/problem+json",
      ] {
        match ensure_media_mime_sane(url, Some(content_type)) {
          Err(MediaError::LoadFailed { .. }) => {}
          Ok(()) => panic!("expected {content_type} to be rejected"),
          Err(other) => panic!("expected LoadFailed for {content_type}, got {other:?}"),
        }
      }
    });
  }

  #[test]
  fn media_loader_checks_mime_before_demux() {
    runtime::with_thread_runtime_toggles(strict_mime_toggles(), || {
      let url = "https://example.com/video.mp4";
      let bytes = b"not an mp4";
      match open_mp4_demuxer(url, Some("text/html"), bytes) {
        Err(MediaError::LoadFailed { .. }) => {}
        Ok(_) => panic!("expected LoadFailed"),
        Err(other) => panic!("expected LoadFailed, got {other:?}"),
      }
    });
  }

  #[test]
  fn media_loader_rejects_markup_payloads_before_demux() {
    runtime::with_thread_runtime_toggles(strict_mime_toggles(), || {
      let url = "https://example.com/video.webm";
      let bytes = b"<!doctype html><html><title>blocked</title></html>";
      match open_webm_demuxer(url, Some("video/webm"), bytes) {
        Err(MediaError::LoadFailed { reason, .. }) => {
          assert!(
            reason.contains("unexpected markup response body"),
            "unexpected reason: {reason}"
          );
        }
        Ok(_) => panic!("expected LoadFailed"),
        Err(other) => panic!("expected LoadFailed, got {other:?}"),
      }
    });
  }
}
