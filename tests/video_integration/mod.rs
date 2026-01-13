//! Video decode integration tests.
//!
//! These tests validate that our decode stack produces *reasonable* RGBA pixels for the first
//! decoded frame of small MP4 (H.264) and WebM (VP9) fixtures.
//!
//! We intentionally avoid bit-exact pixel assertions because different decoder implementations can
//! produce small output deltas (e.g. rounding differences in YUV→RGB conversion). Instead we assert
//! coarse channel dominance thresholds.

mod rgba_first_frame_stats_test;

