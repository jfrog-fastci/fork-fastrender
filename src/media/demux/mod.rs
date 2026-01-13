//! Container demuxers.
//!
//! This module provides container demuxers that yield compressed [`MediaPacket`]s.
//!
//! MP4 (ISO BMFF) parsing/demuxing lives in [`crate::media::mp4`].

pub mod webm;
