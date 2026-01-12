//! Dedicated test target for focused paged-media (pagination) regressions.
//!
//! The full integration suite is linked via `tests/integration.rs`. This target exists so
//! automation can run a small slice of paged-media assertions without executing the entire
//! integration test harness.

#[path = "layout/paged_media.rs"]
mod paged_media;

