#![cfg(target_os = "macos")]

use fastrender::api::{FastRender, FastRenderConfig};
use fastrender::resource::{FetchedResource, ResourceFetcher};
use fastrender::text::font_db::FontConfig;
use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::process::Command;
use std::sync::Arc;

#[link(name = "sandbox")]
extern "C" {
  fn sandbox_init(profile: *const c_char, flags: u64, errorbuf: *mut *mut c_char) -> i32;
  fn sandbox_free_error(errorbuf: *mut c_char);
}

const SEATBELT_PROFILE_PURE_COMPUTATION: &str = "pure-computation";
const SANDBOX_NAMED: u64 = 1;

fn apply_seatbelt_named_profile(profile_name: &str) {
  let profile_c = CString::new(profile_name).expect("profile name should not contain NUL bytes");
  let mut error_buf: *mut c_char = std::ptr::null_mut();
  // SAFETY: Calls into Apple Seatbelt (`libsandbox`). `sandbox_init` populates `error_buf` on
  // failure and returns non-zero.
  let rc = unsafe { sandbox_init(profile_c.as_ptr(), SANDBOX_NAMED, &mut error_buf) };
  if rc == 0 {
    return;
  }

  // SAFETY: `sandbox_init` returns an allocated C string when `error_buf` is non-null.
  let err = unsafe {
    let message = if error_buf.is_null() {
      format!("sandbox_init failed with rc={rc}")
    } else {
      CStr::from_ptr(error_buf).to_string_lossy().into_owned()
    };
    if !error_buf.is_null() {
      sandbox_free_error(error_buf);
    }
    message
  };
  panic!("failed to enable Seatbelt profile {profile_name:?}: {err}");
}

#[derive(Debug)]
struct NoNetworkFetcher;

impl ResourceFetcher for NoNetworkFetcher {
  fn fetch(&self, url: &str) -> fastrender::Result<FetchedResource> {
    panic!("sandbox smoke test attempted unexpected fetch for {url}");
  }
}

#[test]
fn sandboxed_render_smoke_seatbelt_profile() {
  const CHILD_ENV: &str = "FASTR_TEST_SEATBELT_RENDER_SMOKE_CHILD";
  let is_child = std::env::var_os(CHILD_ENV).is_some();
  if is_child {
    // Apply the strictest built-in profile (`pure-computation`) so this smoke test fails if the
    // renderer starts depending on filesystem/network access inside the sandbox.
    apply_seatbelt_named_profile(SEATBELT_PROFILE_PURE_COMPUTATION);

    let mut config = FastRenderConfig::new();
    config.font_config = FontConfig::bundled_only();
    // Avoid any optional caches even when `--all-features` enables disk_cache.
    config.resource_cache = None;

    let mut renderer =
      FastRender::with_config_and_fetcher(config, Some(Arc::new(NoNetworkFetcher)))
        .expect("build FastRender under Seatbelt");

    let pixmap = renderer
      .render_html("<html><body>Hello</body></html>", 32, 32)
      .expect("render HTML under Seatbelt");

    assert!(
      !pixmap.data().is_empty(),
      "expected non-empty image buffer"
    );
    assert!(
      pixmap.data().iter().any(|b| *b != 0),
      "expected rendered image to contain non-zero bytes"
    );
    return;
  }

  let exe = std::env::current_exe().expect("current test exe path");
  let test_name = "sandboxed_render_smoke_seatbelt_profile";
  let output = Command::new(exe)
    .env(CHILD_ENV, "1")
    // Keep test harness output deterministic under strict sandboxing.
    .arg("--test-threads=1")
    .arg("--exact")
    .arg(test_name)
    .arg("--nocapture")
    .output()
    .expect("spawn sandboxed child test process");
  assert!(
    output.status.success(),
    "child process should exit successfully (stdout={}, stderr={})",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );
}

