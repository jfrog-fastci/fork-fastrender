use fastrender::dom::DomCompatibilityMode;
use fastrender::resource::bundle::{
  Bundle, BundleManifest, BundleRenderConfig, BundledDocument, BundledFetcher, BundledResourceInfo,
  BUNDLE_MANIFEST, BUNDLE_VERSION,
};
use fastrender::resource::ResourceFetcher;
use fastrender::CompatProfile;
use std::collections::BTreeMap;
use tempfile::TempDir;

fn create_minimal_bundle_with_vary_manifest_key() -> (TempDir, String, Vec<u8>) {
  let dir = TempDir::new().expect("create temp bundle dir");
  let root = dir.path();

  std::fs::write(root.join("doc.html"), b"<!doctype html><html></html>")
    .expect("write bundled document");

  let resource_bytes = vec![0x00, 0x01, 0x02, 0x03, 0x7f, 0xfe, 0xff];
  std::fs::write(root.join("res.bin"), &resource_bytes).expect("write bundled resource");

  let synthetic_key = "https://example.invalid/res.bin@@fastr:bundle:vary_v1@@test-key".to_string();

  let mut resources = BTreeMap::new();
  resources.insert(
    synthetic_key.clone(),
    BundledResourceInfo {
      path: "res.bin".to_string(),
      content_type: Some("application/octet-stream".to_string()),
      nosniff: false,
      status: Some(200),
      final_url: None,
      etag: None,
      last_modified: None,
      response_referrer_policy: None,
      response_headers: None,
      vary: Some("user-agent".to_string()),
      access_control_allow_origin: None,
      timing_allow_origin: None,
      access_control_allow_credentials: false,
    },
  );

  let manifest = BundleManifest {
    version: BUNDLE_VERSION,
    original_url: "https://example.invalid/doc.html".to_string(),
    document: BundledDocument {
      path: "doc.html".to_string(),
      content_type: Some("text/html".to_string()),
      nosniff: false,
      final_url: "https://example.invalid/doc.html".to_string(),
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
      viewport: (800, 600),
      device_pixel_ratio: 1.0,
      scroll_x: 0.0,
      scroll_y: 0.0,
      full_page: false,
      same_origin_subresources: false,
      allowed_subresource_origins: Vec::new(),
      compat_profile: CompatProfile::default(),
      dom_compat_mode: DomCompatibilityMode::default(),
    },
    fetch_profile: Default::default(),
    resources,
  };

  let manifest_bytes = serde_json::to_vec_pretty(&manifest).expect("serialize bundle manifest");
  std::fs::write(root.join(BUNDLE_MANIFEST), manifest_bytes).expect("write bundle manifest");

  (dir, synthetic_key, resource_bytes)
}

#[test]
fn synthetic_vary_manifest_key_can_be_fetched() {
  let (dir, synthetic_key, expected_bytes) = create_minimal_bundle_with_vary_manifest_key();

  let bundle = Bundle::load(dir.path()).expect("load bundle");

  let fetched = bundle
    .fetch_manifest_entry(&synthetic_key)
    .expect("Bundle::fetch_manifest_entry resolves synthetic Vary key");
  assert_eq!(fetched.bytes, expected_bytes);

  let fetcher = BundledFetcher::new(bundle);
  let fetched = fetcher
    .fetch(&synthetic_key)
    .expect("BundledFetcher::fetch resolves synthetic Vary key");
  assert_eq!(fetched.bytes, expected_bytes);
}

#[test]
fn synthetic_vary_manifest_key_missing_variant_has_actionable_error() {
  let (dir, synthetic_key, _expected_bytes) = create_minimal_bundle_with_vary_manifest_key();
  let missing_key = synthetic_key.replace("test-key", "missing-key");

  let bundle = Bundle::load(dir.path()).expect("load bundle");

  let err = bundle
    .fetch_manifest_entry(&missing_key)
    .expect_err("missing Vary variant should error");
  let message = err.to_string();
  assert!(
    message.contains("Vary variant"),
    "unexpected error: {message}"
  );
  assert!(
    message.contains(&missing_key),
    "unexpected error: {message}"
  );

  let fetcher = BundledFetcher::new(bundle);
  let err = fetcher
    .fetch(&missing_key)
    .expect_err("missing Vary variant should error");
  let message = err.to_string();
  assert!(
    message.contains("Vary variant"),
    "unexpected error: {message}"
  );
  assert!(
    message.contains(&missing_key),
    "unexpected error: {message}"
  );
}
