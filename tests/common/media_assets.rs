//! Tiny offline media assets for integration tests.
//!
//! These are intended for tests that need deterministic, in-repo media without any network access
//! and without pulling large binaries into the repository.
//!
//! Source-of-truth fixture files live under:
//! - `tests/fixtures/media/test_h264_aac.mp4`
//! - `tests/fixtures/media/test_vp9_opus.webm`
//!
//! Licensing: the assets are generated from synthetic FFmpeg sources and are dedicated to the
//! public domain (CC0-1.0). See `tests/fixtures/media/README.md` for generation commands.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

/// Tiny MP4 (H.264 + AAC) test asset bytes.
pub(crate) const TEST_H264_AAC_MP4: &[u8] =
  include_bytes!("../fixtures/media/test_h264_aac.mp4");

/// Tiny WebM (VP9 + Opus) test asset bytes.
pub(crate) const TEST_VP9_OPUS_WEBM: &[u8] =
  include_bytes!("../fixtures/media/test_vp9_opus.webm");

#[derive(Debug, Clone)]
pub(crate) struct MediaAssetPaths {
  pub mp4: PathBuf,
  pub webm: PathBuf,
}

/// Write `test_h264_aac.mp4` into `dir` and return the written path.
pub(crate) fn write_test_h264_aac_mp4(dir: impl AsRef<Path>) -> PathBuf {
  write_asset(dir.as_ref(), "test_h264_aac.mp4", TEST_H264_AAC_MP4)
}

/// Write `test_vp9_opus.webm` into `dir` and return the written path.
pub(crate) fn write_test_vp9_opus_webm(dir: impl AsRef<Path>) -> PathBuf {
  write_asset(dir.as_ref(), "test_vp9_opus.webm", TEST_VP9_OPUS_WEBM)
}

/// Write all supported test media assets into `dir`.
pub(crate) fn write_all_media_assets(dir: impl AsRef<Path>) -> MediaAssetPaths {
  let dir = dir.as_ref();
  MediaAssetPaths {
    mp4: write_test_h264_aac_mp4(dir),
    webm: write_test_vp9_opus_webm(dir),
  }
}

fn write_asset(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
  std::fs::create_dir_all(dir).unwrap_or_else(|err| {
    panic!("create media asset output dir {}: {err}", dir.display());
  });
  let path = dir.join(name);
  std::fs::write(&path, bytes).unwrap_or_else(|err| {
    panic!("write media asset {}: {err}", path.display());
  });
  path
}

#[cfg(test)]
mod tests {
  use super::*;

  // Keep the HTML page fixtures in sync with the shared media fixtures so tests and manual pages
  // exercise identical deterministic content.
  const PAGE_H264_AAC_MP4: &[u8] = include_bytes!("../pages/fixtures/media_mp4_basic/test_h264_aac.mp4");
  const PAGE_VP9_OPUS_WEBM: &[u8] =
    include_bytes!("../pages/fixtures/media_webm_basic/test_vp9_opus.webm");
  const PLAYBACK_H264_AAC_MP4: &[u8] =
    include_bytes!("../pages/fixtures/media_playback/assets/test_h264_aac.mp4");
  const PLAYBACK_VP9_OPUS_WEBM: &[u8] =
    include_bytes!("../pages/fixtures/media_playback/assets/test_vp9_opus.webm");

  #[test]
  fn page_fixtures_match_shared_media_fixtures() {
    assert_eq!(
      TEST_H264_AAC_MP4, PAGE_H264_AAC_MP4,
      "media_mp4_basic fixture should match tests/fixtures/media/test_h264_aac.mp4"
    );
    assert_eq!(
      TEST_VP9_OPUS_WEBM, PAGE_VP9_OPUS_WEBM,
      "media_webm_basic fixture should match tests/fixtures/media/test_vp9_opus.webm"
    );
    assert_eq!(
      TEST_H264_AAC_MP4, PLAYBACK_H264_AAC_MP4,
      "media_playback/assets fixture should match tests/fixtures/media/test_h264_aac.mp4"
    );
    assert_eq!(
      TEST_VP9_OPUS_WEBM, PLAYBACK_VP9_OPUS_WEBM,
      "media_playback/assets fixture should match tests/fixtures/media/test_vp9_opus.webm"
    );
  }
}
