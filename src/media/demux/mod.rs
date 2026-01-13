//! Container demuxers.
//!
//! This module provides container demuxers that yield compressed [`MediaPacket`]s.
//!
pub mod mp4;
//! MP4 (ISO BMFF) demuxing lives in [`crate::media::demux::mp4`] (sample-table helpers in
//! [`crate::media::mp4`]).

pub mod webm;
