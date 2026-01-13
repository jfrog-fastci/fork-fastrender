//! Backwards-compatible JS clock module.
//!
//! The shared monotonic clock abstraction lives in [`crate::clock`]. This module remains as a
//! re-export so existing `crate::js::clock::*` paths keep working.

pub use crate::clock::{Clock, RealClock, VirtualClock};
