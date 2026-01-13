use fastrender::resource::bundle::{
  BundleFetchProfile, BundleManifest, BundleRenderConfig, BundledDocument, BUNDLE_MANIFEST,
  BUNDLE_VERSION,
};
use image::GenericImageView;
use std::collections::BTreeMap;
use std::process::Command;
use tempfile::TempDir;

#[test]
fn bundle_page_render_js_executes_inline_scripts() {
  let tmp = TempDir::new().expect("tempdir");
  let bundle_dir = tmp.path().join("bundle");
  std::fs::create_dir_all(&bundle_dir).expect("create bundle dir");

  // Mirror `tests/bin/fetch_and_render_js_test.rs`: use a minimal DOM mutation that is already
  // supported by the JS-enabled pipeline (className toggling) rather than relying on
  // CSSStyleDeclaration integration.
  let html = r#"<!doctype html><html class="no-js"><head><meta charset="utf-8"><style>
html, body { margin: 0; width: 100%; height: 100%; }
html.no-js body { background: rgb(255, 0, 0); }
html.js-enabled body { background: rgb(0, 255, 0); }
</style><script>document.documentElement.className = 'js-enabled';</script></head><body></body></html>"#;
  std::fs::write(bundle_dir.join("document.html"), html).expect("write bundle document");

  let url = "https://example.invalid/";
  let manifest = BundleManifest {
    version: BUNDLE_VERSION,
    original_url: url.to_string(),
    document: BundledDocument {
      path: "document.html".to_string(),
      content_type: Some("text/html; charset=utf-8".to_string()),
      nosniff: false,
      final_url: url.to_string(),
      status: Some(200),
      etag: None,
      last_modified: None,
      response_referrer_policy: None,
      response_headers: None,
      access_control_allow_origin: None,
      timing_allow_origin: None,
      vary: None,
    },
    render: BundleRenderConfig {
      viewport: (64, 64),
      device_pixel_ratio: 1.0,
      scroll_x: 0.0,
      scroll_y: 0.0,
      full_page: false,
      same_origin_subresources: false,
      allowed_subresource_origins: Vec::new(),
      compat_profile: Default::default(),
      dom_compat_mode: Default::default(),
    },
    fetch_profile: BundleFetchProfile::default(),
    resources: BTreeMap::new(),
  };
  std::fs::write(
    bundle_dir.join(BUNDLE_MANIFEST),
    serde_json::to_vec_pretty(&manifest).expect("serialize bundle manifest"),
  )
  .expect("write bundle manifest");

  let out_no_js = tmp.path().join("out_no_js.png");
  let status = Command::new(env!("CARGO_BIN_EXE_bundle_page"))
    .args(["render"])
    .arg(bundle_dir.to_string_lossy().as_ref())
    .args(["--out", out_no_js.to_string_lossy().as_ref()])
    .status()
    .expect("run bundle_page render (no js)");
  assert!(
    status.success(),
    "bundle_page render should succeed (no js), got status={:?}",
    status.code()
  );

  let out_js = tmp.path().join("out_js.png");
  let status = Command::new(env!("CARGO_BIN_EXE_bundle_page"))
    .args(["render"])
    .arg(bundle_dir.to_string_lossy().as_ref())
    .args(["--out", out_js.to_string_lossy().as_ref()])
    .arg("--js")
    .status()
    .expect("run bundle_page render (js)");
  assert!(
    status.success(),
    "bundle_page render should succeed (js), got status={:?}",
    status.code()
  );

  let img_no_js = image::open(&out_no_js).expect("decode no-js PNG");
  let img_js = image::open(&out_js).expect("decode js PNG");
  assert_eq!(img_no_js.dimensions(), (64, 64));
  assert_eq!(img_js.dimensions(), (64, 64));

  let pixel_no_js = img_no_js.to_rgba8().get_pixel(0, 0).0;
  let pixel_js = img_js.to_rgba8().get_pixel(0, 0).0;

  assert_ne!(
    pixel_no_js, pixel_js,
    "expected pixels to differ when JS is enabled"
  );
  assert!(
    pixel_no_js[0] > 200 && pixel_no_js[1] < 80,
    "expected red without JS, got {pixel_no_js:?}"
  );
  assert!(
    pixel_js[1] > 200 && pixel_js[0] < 80,
    "expected green with JS, got {pixel_js:?}"
  );
}
