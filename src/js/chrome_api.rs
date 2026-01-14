//! Public re-exports for privileged `chrome` JS bindings + shared helpers.
//!
//! The canonical `vm-js` implementation lives in [`crate::js::vmjs_chrome_api`] (declared in
//! `src/js/mod.rs`). Keep the stable `crate::js::chrome_api::*` path by re-exporting the bindings
//! and related helper utilities here.

pub use super::vmjs_chrome_api::{
  install_chrome_api_bindings_vm_js, ChromeApiHost, ChromeCommand, MAX_CHROME_API_URL_CODE_UNITS,
};

// Host-side URL validation helpers for `chrome.navigation.navigate(url)` and other chrome-driven
// navigation surfaces.
pub use super::chrome_navigation_url::{
  validate_chrome_navigation_url, ChromeApiError, MAX_CHROME_NAVIGATION_URL_CODE_UNITS,
};
