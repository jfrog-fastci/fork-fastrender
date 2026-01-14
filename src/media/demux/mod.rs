//! Container demuxers.
//!
//! This module provides container demuxers that yield compressed
//! [`crate::media::MediaPacket`]s.
//!
//! MP4 (ISO BMFF) demuxing lives in [`crate::media::demux::mp4`] and
//! [`crate::media::demux::mp4parse`].

#[cfg(feature = "media_mp4")]
pub mod mp4;

#[cfg(not(feature = "media_mp4"))]
pub mod mp4 {
  use crate::media::{MediaError, MediaPacket, MediaResult, MediaTrackInfo};
  use std::io::{Read, Seek};

  #[derive(Debug)]
  pub struct Mp4Demuxer;

  impl Mp4Demuxer {
    pub fn open(_reader: impl Read + Seek) -> MediaResult<Self> {
      Err(MediaError::Unsupported(
        "`media_mp4` feature disabled (enable Cargo feature `media_mp4` or `media`)".into(),
      ))
    }

    pub fn from_bytes(_bytes: Vec<u8>) -> MediaResult<Self> {
      Err(MediaError::Unsupported(
        "`media_mp4` feature disabled (enable Cargo feature `media_mp4` or `media`)".into(),
      ))
    }

    pub fn tracks(&self) -> &[MediaTrackInfo] {
      &[]
    }

    pub fn next_packet(&mut self) -> MediaResult<Option<MediaPacket>> {
      Err(MediaError::Unsupported(
        "`media_mp4` feature disabled (enable Cargo feature `media_mp4` or `media`)".into(),
      ))
    }

    pub fn seek(&mut self, _time_ns: u64) -> MediaResult<()> {
      Err(MediaError::Unsupported(
        "`media_mp4` feature disabled (enable Cargo feature `media_mp4` or `media`)".into(),
      ))
    }
  }
}

#[cfg(feature = "media_mp4")]
pub mod mp4parse;

#[cfg(not(feature = "media_mp4"))]
pub mod mp4parse {
  use crate::media::track_selection::TrackFilterMode;
  use crate::media::track_selection::TrackSelectionPolicy;
  use crate::media::{MediaError, MediaPacket, MediaResult, MediaTrackInfo};
  use std::io::{Read, Seek};
  use std::marker::PhantomData;

  #[derive(Debug, Clone, Copy)]
  pub struct Mp4ParseDemuxerOptions {
    pub track_selection_policy: TrackSelectionPolicy,
    pub track_filter: TrackFilterMode,
  }

  impl Default for Mp4ParseDemuxerOptions {
    fn default() -> Self {
      Self {
        track_selection_policy: TrackSelectionPolicy::default(),
        track_filter: TrackFilterMode::PrimaryOnly,
      }
    }
  }

  pub struct Mp4ParseDemuxer<R: Read + Seek> {
    _phantom: PhantomData<R>,
  }

  impl<R: Read + Seek> Mp4ParseDemuxer<R> {
    pub fn open(_reader: R) -> MediaResult<Self> {
      Err(MediaError::Unsupported(
        "`media_mp4` feature disabled (enable Cargo feature `media_mp4` or `media`)".into(),
      ))
    }

    pub fn open_with_options(_reader: R, _options: Mp4ParseDemuxerOptions) -> MediaResult<Self> {
      Err(MediaError::Unsupported(
        "`media_mp4` feature disabled (enable Cargo feature `media_mp4` or `media`)".into(),
      ))
    }

    pub fn tracks(&self) -> &[MediaTrackInfo] {
      &[]
    }

    pub fn next_packet(&mut self) -> MediaResult<Option<MediaPacket>> {
      Err(MediaError::Unsupported(
        "`media_mp4` feature disabled (enable Cargo feature `media_mp4` or `media`)".into(),
      ))
    }

    pub fn seek(&mut self, _time_ns: u64) -> MediaResult<()> {
      Err(MediaError::Unsupported(
        "`media_mp4` feature disabled (enable Cargo feature `media_mp4` or `media`)".into(),
      ))
    }
  }
}

#[cfg(feature = "media_webm")]
pub mod webm;

#[cfg(not(feature = "media_webm"))]
pub mod webm {
  use crate::media::{MediaError, MediaPacket, MediaResult, MediaTrackInfo};
  use std::io::{Read, Seek};
  use std::marker::PhantomData;

  #[derive(Debug, Clone, Copy)]
  pub struct WebmDemuxerOptions {
    pub inter_track_reordering: bool,
    pub per_track_queue_capacity: usize,
  }

  impl Default for WebmDemuxerOptions {
    fn default() -> Self {
      Self {
        inter_track_reordering: true,
        per_track_queue_capacity: 8,
      }
    }
  }

  pub struct WebmDemuxer<R: Read + Seek> {
    _phantom: PhantomData<R>,
  }

  impl<R: Read + Seek> WebmDemuxer<R> {
    pub fn open(_reader: R) -> MediaResult<Self> {
      Err(MediaError::Unsupported(
        "`media_webm` feature disabled (enable Cargo feature `media_webm` or `media`)".into(),
      ))
    }

    pub fn open_with_options(_reader: R, _options: WebmDemuxerOptions) -> MediaResult<Self> {
      Err(MediaError::Unsupported(
        "`media_webm` feature disabled (enable Cargo feature `media_webm` or `media`)".into(),
      ))
    }

    pub fn tracks(&self) -> &[MediaTrackInfo] {
      &[]
    }

    pub fn next_packet(&mut self) -> MediaResult<Option<MediaPacket>> {
      Err(MediaError::Unsupported(
        "`media_webm` feature disabled (enable Cargo feature `media_webm` or `media`)".into(),
      ))
    }

    pub fn seek(&mut self, _time_ns: u64) -> MediaResult<()> {
      Err(MediaError::Unsupported(
        "`media_webm` feature disabled (enable Cargo feature `media_webm` or `media`)".into(),
      ))
    }
  }
}
