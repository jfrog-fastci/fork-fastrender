//! Container demuxers.
//!
//! This module provides container demuxers that yield compressed [`crate::media::MediaPacket`]s.
//!
/// MP4 (ISO BMFF) demuxing lives in [`crate::media::demux::mp4`] (sample-table helpers in
/// [`crate::media::mp4`]).
pub mod mp4;

/// Additional mp4parse-based track inspection lives in [`crate::media::demux::mp4parse`].
pub mod mp4parse;
pub mod webm;
