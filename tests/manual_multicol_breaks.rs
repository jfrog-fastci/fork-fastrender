//! Dedicated test target for focused manual multi-column (builder columns) regressions.
//!
//! The full integration suite is linked via `tests/integration.rs`. This target exists so automation
//! can validate manual-column fragmentation behavior in isolation.

#[path = "misc/fragmentation_columns_public_api.rs"]
mod fragmentation_columns_public_api;

