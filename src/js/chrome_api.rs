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

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn chrome_api_public_reexports_compile() {
    // Regression guard: `crate::js::chrome_api` used to be both a module and a re-export alias for
    // `chrome_navigation_url`, which caused an E0255 name collision. Ensure the stable public API is
    // still reachable.
    let _ = MAX_CHROME_API_URL_CODE_UNITS;
    let _ = MAX_CHROME_NAVIGATION_URL_CODE_UNITS;
    let _ = validate_chrome_navigation_url;

    fn _assert_install_is_reexported<Host>()
    where
      Host: ChromeApiHost + crate::js::window_realm::WindowRealmHost + 'static,
    {
      let _ = install_chrome_api_bindings_vm_js::<Host>;
    }

    let _ = ChromeApiError::EmptyUrl;
    let _ = ChromeCommand::Back;
  }
}
