//! Media utilities and shared primitives.
//!
//! This module currently exposes timestamp/timebase helpers used by media playback work.
//! For the intended A/V clocking model (audio master clock, UI tick as wake-up only), see
//! `docs/media_clocking.md`.

pub mod timebase;

pub use timebase::{duration_to_ticks, ticks_to_duration, Timebase};
