#![cfg(feature = "disk_cache")]

use fastrender::css::parser::parse_stylesheet;
use fastrender::resource::bundle::{Bundle, BundledFetcher};
use fastrender::resource::{
  normalize_user_agent_for_log, CachingFetcherConfig, DiskCacheConfig, DiskCachingFetcher,
  FetchDestination, FetchRequest, FetchedResource, DEFAULT_ACCEPT_LANGUAGE, DEFAULT_USER_AGENT,
};
use fastrender::{Error, ResourceFetcher};
use std::collections::HashMap;
use std::process::Command;
use std::sync::Arc;
use tempfile::TempDir;

fn disk_cache_namespace_for(user_agent: &str, accept_language: &str) -> String {
  let ua = normalize_user_agent_for_log(user_agent).trim();
  let lang = accept_language.trim();
  let browser_headers_enabled = std::env::var("FASTR_HTTP_BROWSER_HEADERS")
    .ok()
    .map(|raw| {
      !matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "0" | "false" | "no" | "off"
      )
    })
    .unwrap_or(true);
  if browser_headers_enabled {
    format!("fetch-profile:contextual-v1\nuser-agent:{ua}\naccept-language:{lang}")
  } else {
    format!(
      "fetch-profile:contextual-v1\nuser-agent:{ua}\naccept-language:{lang}\nhttp-browser-headers:0"
    )
  }
}

fn disk_cache_namespace() -> String {
  disk_cache_namespace_for(DEFAULT_USER_AGENT, DEFAULT_ACCEPT_LANGUAGE)
}

#[derive(Clone)]
struct StaticFetcher {
  responses: Arc<HashMap<String, (Vec<u8>, &'static str)>>,
}

impl ResourceFetcher for StaticFetcher {
  fn fetch(&self, url: &str) -> Result<FetchedResource, Error> {
    let (bytes, content_type) = self
      .responses
      .get(url)
      .ok_or_else(|| Error::Other(format!("unexpected fetch: {url}")))?;
    let mut resource = FetchedResource::with_final_url(
      bytes.clone(),
      Some((*content_type).to_string()),
      Some(url.to_string()),
    );
    resource.status = Some(200);
    resource.access_control_allow_origin = Some("*".to_string());
    resource.timing_allow_origin = Some("https://timing.example".to_string());
    resource.access_control_allow_credentials = true;
    Ok(resource)
  }
}

#[test]
fn bundle_page_cache_captures_from_disk_cache_offline() {
  let tmp = TempDir::new().expect("tempdir");

  let html_dir = tmp.path().join("fetches/html");
  std::fs::create_dir_all(&html_dir).expect("create html dir");
  let asset_dir = tmp.path().join("custom_asset_cache");
  std::fs::create_dir_all(&asset_dir).expect("create asset dir");

  let stem = "example.invalid";
  let page_url = "https://example.invalid/";
  let html_path = html_dir.join(format!("{stem}.html"));
  std::fs::write(
    &html_path,
    "<!doctype html><html><head><link rel=\"stylesheet\" href=\"/a.css\"></head><body><img src=\"img.png\"></body></html>",
  )
  .expect("write html");
  std::fs::write(
    html_path.with_extension("html.meta"),
    format!("content-type: text/html\nurl: {page_url}\n"),
  )
  .expect("write meta");

  let css_url = "https://example.invalid/a.css".to_string();
  let img_url = "https://example.invalid/img.png".to_string();
  let bg_url = "https://example.invalid/bg.png".to_string();

  let mut responses: HashMap<String, (Vec<u8>, &'static str)> = HashMap::new();
  responses.insert(
    css_url.clone(),
    (
      b"body { background-image: url(\"bg.png\"); }".to_vec(),
      "text/css",
    ),
  );
  responses.insert(img_url.clone(), (b"png-bytes-1".to_vec(), "image/png"));
  responses.insert(bg_url.clone(), (b"png-bytes-2".to_vec(), "image/png"));

  let mut disk_config = DiskCacheConfig::default();
  disk_config.namespace = Some(disk_cache_namespace());
  disk_config.allow_no_store = true;

  let cache_writer = DiskCachingFetcher::with_configs(
    StaticFetcher {
      responses: Arc::new(responses),
    },
    asset_dir.clone(),
    CachingFetcherConfig::default(),
    disk_config,
  );

  cache_writer
    .fetch_with_request(FetchRequest::new(&css_url, FetchDestination::Style))
    .expect("warm css");
  cache_writer
    .fetch_with_request(FetchRequest::new(&img_url, FetchDestination::Image))
    .expect("warm img");
  cache_writer
    .fetch_with_request(FetchRequest::new(&bg_url, FetchDestination::Image))
    .expect("warm bg");

  let bundle_dir = tmp.path().join("bundle");
  let status = Command::new(env!("CARGO_BIN_EXE_bundle_page"))
    .current_dir(tmp.path())
    .args(["cache", stem, "--out"])
    .arg(bundle_dir.to_string_lossy().as_ref())
    .args(["--cache-dir"])
    .arg(&asset_dir)
    .status()
    .expect("run bundle_page cache");

  assert!(status.success(), "bundle_page cache should succeed");

  let bundle = Bundle::load(&bundle_dir).expect("load bundle");
  assert!(
    bundle.manifest().resources.contains_key(css_url.as_str()),
    "bundle should include stylesheet"
  );
  assert!(
    bundle.manifest().resources.contains_key(img_url.as_str()),
    "bundle should include image referenced from HTML"
  );
  assert!(
    bundle.manifest().resources.contains_key(bg_url.as_str()),
    "bundle should include image referenced from CSS url()"
  );

  let fetcher = BundledFetcher::new(bundle);
  let css_res = fetcher.fetch(&css_url).expect("fetch css");
  assert_eq!(
    css_res.bytes,
    b"body { background-image: url(\"bg.png\"); }".to_vec()
  );
  assert_eq!(css_res.access_control_allow_origin.as_deref(), Some("*"));
  assert_eq!(
    css_res.timing_allow_origin.as_deref(),
    Some("https://timing.example")
  );
  assert!(css_res.access_control_allow_credentials);
  assert_eq!(
    fetcher.fetch(&img_url).expect("fetch img").bytes,
    b"png-bytes-1".to_vec()
  );
  assert_eq!(
    fetcher.fetch(&bg_url).expect("fetch bg").bytes,
    b"png-bytes-2".to_vec()
  );

  let output_png = tmp.path().join("out.png");
  let status = Command::new(env!("CARGO_BIN_EXE_bundle_page"))
    .current_dir(tmp.path())
    .args(["render"])
    .arg(bundle_dir.to_string_lossy().as_ref())
    .args(["--out", output_png.to_string_lossy().as_ref()])
    .status()
    .expect("run bundle_page render");
  assert!(
    status.success(),
    "bundle_page render should succeed offline"
  );
  let png_bytes = std::fs::read(&output_png).expect("png output");
  assert!(
    png_bytes.starts_with(b"\x89PNG"),
    "expected PNG header, got {} bytes",
    png_bytes.len()
  );
}

#[test]
fn bundle_page_cache_infers_original_url_from_pageset_when_html_meta_lacks_url() {
  let tmp = TempDir::new().expect("tempdir");

  let html_dir = tmp.path().join("fetches/html");
  std::fs::create_dir_all(&html_dir).expect("create html dir");
  let asset_dir = tmp.path().join("fetches/assets");
  std::fs::create_dir_all(&asset_dir).expect("create asset dir");

  // `example.com` is part of the built-in pageset, so the bundle_page cache subcommand can infer
  // the original URL from the stem even when the cached HTML `.meta` sidecar only contains a
  // content-type string (legacy format).
  let stem = "example.com";
  let page_url = "https://example.com";
  let html_path = html_dir.join(format!("{stem}.html"));
  std::fs::write(
    &html_path,
    "<!doctype html><html><head><link rel=\"stylesheet\" href=\"/a.css\"></head><body><img src=\"img.png\"></body></html>",
  )
  .expect("write html");
  std::fs::write(html_path.with_extension("html.meta"), "text/html").expect("write meta");

  let css_url = "https://example.com/a.css".to_string();
  let img_url = "https://example.com/img.png".to_string();
  let bg_url = "https://example.com/bg.png".to_string();

  let mut responses: HashMap<String, (Vec<u8>, &'static str)> = HashMap::new();
  responses.insert(
    css_url.clone(),
    (
      b"body { background-image: url(\"bg.png\"); }".to_vec(),
      "text/css",
    ),
  );
  responses.insert(img_url.clone(), (b"png-bytes-1".to_vec(), "image/png"));
  responses.insert(bg_url.clone(), (b"png-bytes-2".to_vec(), "image/png"));

  let mut disk_config = DiskCacheConfig::default();
  disk_config.namespace = Some(disk_cache_namespace());
  disk_config.allow_no_store = true;

  let cache_writer = DiskCachingFetcher::with_configs(
    StaticFetcher {
      responses: Arc::new(responses),
    },
    asset_dir.clone(),
    CachingFetcherConfig::default(),
    disk_config,
  );

  cache_writer
    .fetch_with_request(FetchRequest::new(&css_url, FetchDestination::Style))
    .expect("warm css");
  cache_writer
    .fetch_with_request(FetchRequest::new(&img_url, FetchDestination::Image))
    .expect("warm img");
  cache_writer
    .fetch_with_request(FetchRequest::new(&bg_url, FetchDestination::Image))
    .expect("warm bg");

  let bundle_dir = tmp.path().join("bundle");
  let status = Command::new(env!("CARGO_BIN_EXE_bundle_page"))
    .current_dir(tmp.path())
    .env_remove("FASTR_PAGESET_URLS")
    .args(["cache", stem, "--out"])
    .arg(bundle_dir.to_string_lossy().as_ref())
    .status()
    .expect("run bundle_page cache");

  assert!(
    status.success(),
    "bundle_page cache should succeed when URL is inferred from pageset stem"
  );

  let bundle = Bundle::load(&bundle_dir).expect("load bundle");
  assert_eq!(bundle.manifest().original_url, page_url);
  assert!(bundle.manifest().resources.contains_key(css_url.as_str()));
  assert!(bundle.manifest().resources.contains_key(img_url.as_str()));
  assert!(bundle.manifest().resources.contains_key(bg_url.as_str()));
}

#[test]
fn bundle_page_cache_infers_original_url_from_pageset_when_cache_stem_is_legacy_www() {
  let tmp = TempDir::new().expect("tempdir");

  let html_dir = tmp.path().join("fetches/html");
  std::fs::create_dir_all(&html_dir).expect("create html dir");
  let asset_dir = tmp.path().join("fetches/assets");
  std::fs::create_dir_all(&asset_dir).expect("create asset dir");

  // Older caches may include `www.` in the cached HTML stem even though pageset normalization
  // strips it (e.g. `fetches/html/www.w3.org.html`). When the meta sidecar is in the legacy
  // content-type-only format, bundle_page should still infer the original URL from pageset
  // metadata so relative subresources resolve to the correct HTTP origin.
  let stem = "www.w3.org";
  let page_url = "https://www.w3.org";
  let html_path = html_dir.join(format!("{stem}.html"));
  std::fs::write(
    &html_path,
    "<!doctype html><html><head><link rel=\"stylesheet\" href=\"/a.css\"></head><body><img src=\"img.png\"></body></html>",
  )
  .expect("write html");
  std::fs::write(html_path.with_extension("html.meta"), "text/html").expect("write meta");

  let css_url = "https://www.w3.org/a.css".to_string();
  let img_url = "https://www.w3.org/img.png".to_string();
  let bg_url = "https://www.w3.org/bg.png".to_string();

  let mut responses: HashMap<String, (Vec<u8>, &'static str)> = HashMap::new();
  responses.insert(
    css_url.clone(),
    (
      b"body { background-image: url(\"bg.png\"); }".to_vec(),
      "text/css",
    ),
  );
  responses.insert(img_url.clone(), (b"png-bytes-1".to_vec(), "image/png"));
  responses.insert(bg_url.clone(), (b"png-bytes-2".to_vec(), "image/png"));

  let mut disk_config = DiskCacheConfig::default();
  disk_config.namespace = Some(disk_cache_namespace());
  disk_config.allow_no_store = true;

  let cache_writer = DiskCachingFetcher::with_configs(
    StaticFetcher {
      responses: Arc::new(responses),
    },
    asset_dir.clone(),
    CachingFetcherConfig::default(),
    disk_config,
  );

  cache_writer
    .fetch_with_request(FetchRequest::new(&css_url, FetchDestination::Style))
    .expect("warm css");
  cache_writer
    .fetch_with_request(FetchRequest::new(&img_url, FetchDestination::Image))
    .expect("warm img");
  cache_writer
    .fetch_with_request(FetchRequest::new(&bg_url, FetchDestination::Image))
    .expect("warm bg");

  let bundle_dir = tmp.path().join("bundle");
  let status = Command::new(env!("CARGO_BIN_EXE_bundle_page"))
    .current_dir(tmp.path())
    .env_remove("FASTR_PAGESET_URLS")
    .args(["cache", stem, "--out"])
    .arg(bundle_dir.to_string_lossy().as_ref())
    .status()
    .expect("run bundle_page cache");

  assert!(
    status.success(),
    "bundle_page cache should succeed when URL is inferred from pageset stem"
  );

  let bundle = Bundle::load(&bundle_dir).expect("load bundle");
  assert_eq!(bundle.manifest().original_url, page_url);
  assert!(bundle.manifest().resources.contains_key(css_url.as_str()));
  assert!(bundle.manifest().resources.contains_key(img_url.as_str()));
  assert!(bundle.manifest().resources.contains_key(bg_url.as_str()));
}

#[test]
fn bundle_page_cache_fails_when_resource_missing() {
  let tmp = TempDir::new().expect("tempdir");

  let html_dir = tmp.path().join("fetches/html");
  std::fs::create_dir_all(&html_dir).expect("create html dir");
  let asset_dir = tmp.path().join("fetches/assets");
  std::fs::create_dir_all(&asset_dir).expect("create asset dir");

  let stem = "example.invalid";
  let page_url = "https://example.invalid/";
  let html_path = html_dir.join(format!("{stem}.html"));
  std::fs::write(
    &html_path,
    "<!doctype html><html><head><link rel=\"stylesheet\" href=\"/a.css\"></head><body><img src=\"img.png\"></body></html>",
  )
  .expect("write html");
  std::fs::write(
    html_path.with_extension("html.meta"),
    format!("content-type: text/html\nurl: {page_url}\n"),
  )
  .expect("write meta");

  let css_url = "https://example.invalid/a.css".to_string();
  let img_url = "https://example.invalid/img.png".to_string();

  let mut responses: HashMap<String, (Vec<u8>, &'static str)> = HashMap::new();
  responses.insert(css_url.clone(), (b"body {}".to_vec(), "text/css"));

  let mut disk_config = DiskCacheConfig::default();
  disk_config.namespace = Some(disk_cache_namespace());
  disk_config.allow_no_store = true;

  let cache_writer = DiskCachingFetcher::with_configs(
    StaticFetcher {
      responses: Arc::new(responses),
    },
    asset_dir.clone(),
    CachingFetcherConfig::default(),
    disk_config,
  );

  cache_writer
    .fetch_with_request(FetchRequest::new(&css_url, FetchDestination::Style))
    .expect("warm css");

  let bundle_dir = tmp.path().join("bundle");
  let output = Command::new(env!("CARGO_BIN_EXE_bundle_page"))
    .current_dir(tmp.path())
    .args(["cache", stem, "--out"])
    .arg(bundle_dir.to_string_lossy().as_ref())
    .output()
    .expect("run bundle_page cache");

  assert!(
    !output.status.success(),
    "expected cache capture to fail when {img_url} is missing"
  );

  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(
    stderr.contains("cache miss (offline)"),
    "expected offline cache capture failures to report a cache miss, got stderr:\n{stderr}"
  );
  assert!(
    !stderr.contains("fetch blocked by policy"),
    "expected offline cache capture to avoid policy errors, got stderr:\n{stderr}"
  );
}

#[test]
fn bundle_page_cache_allow_missing_inserts_typed_placeholders() {
  let tmp = TempDir::new().expect("tempdir");

  let html_dir = tmp.path().join("fetches/html");
  std::fs::create_dir_all(&html_dir).expect("create html dir");
  let asset_dir = tmp.path().join("fetches/assets");
  std::fs::create_dir_all(&asset_dir).expect("create asset dir");

  let stem = "example.invalid";
  let page_url = "https://example.invalid/";
  let html_path = html_dir.join(format!("{stem}.html"));
  std::fs::write(
    &html_path,
    "<!doctype html><html><head><link rel=\"stylesheet\" href=\"/missing.css\"><style>@font-face{font-family:'Test';src:url('/missing.woff2');}</style></head><body><iframe src=\"/frame.html\"></iframe><img src=\"img.png\"></body></html>",
  )
  .expect("write html");
  std::fs::write(
    html_path.with_extension("html.meta"),
    format!("content-type: text/html\nurl: {page_url}\n"),
  )
  .expect("write meta");

  let missing_css_url = "https://example.invalid/missing.css".to_string();
  let missing_img_url = "https://example.invalid/img.png".to_string();
  let missing_font_url = "https://example.invalid/missing.woff2".to_string();
  let missing_frame_url = "https://example.invalid/frame.html".to_string();

  let bundle_dir = tmp.path().join("bundle");
  let status = Command::new(env!("CARGO_BIN_EXE_bundle_page"))
    .current_dir(tmp.path())
    .args(["cache", stem, "--out"])
    .arg(bundle_dir.to_string_lossy().as_ref())
    .args(["--cache-dir"])
    .arg(&asset_dir)
    .arg("--allow-missing")
    .status()
    .expect("run bundle_page cache");

  assert!(
    status.success(),
    "expected cache capture to succeed with --allow-missing"
  );

  let bundle = Bundle::load(&bundle_dir).expect("load bundle");
  for url in [
    missing_css_url.as_str(),
    missing_img_url.as_str(),
    missing_font_url.as_str(),
    missing_frame_url.as_str(),
  ] {
    assert!(
      bundle.manifest().resources.contains_key(url),
      "bundle should include placeholder for missing resource: {url}"
    );
  }

  let fetcher = BundledFetcher::new(bundle);

  let missing_img = fetcher
    .fetch(&missing_img_url)
    .expect("fetch placeholder image");
  assert_eq!(missing_img.content_type.as_deref(), Some("image/png"));
  assert_eq!(
    missing_img.final_url.as_deref(),
    Some(missing_img_url.as_str())
  );
  assert_eq!(
    missing_img.access_control_allow_origin.as_deref(),
    Some("https://example.invalid")
  );
  assert!(
    missing_img.access_control_allow_credentials,
    "expected placeholder image to satisfy credentialed CORS checks"
  );
  assert!(
    missing_img.bytes.starts_with(b"\x89PNG"),
    "expected PNG placeholder bytes"
  );

  let missing_font = fetcher
    .fetch(&missing_font_url)
    .expect("fetch placeholder font");
  assert_eq!(missing_font.content_type.as_deref(), Some("font/woff2"));
  assert_eq!(
    missing_font.final_url.as_deref(),
    Some(missing_font_url.as_str())
  );
  assert_eq!(
    missing_font.access_control_allow_origin.as_deref(),
    Some("https://example.invalid")
  );
  assert!(
    missing_font.access_control_allow_credentials,
    "expected placeholder font to satisfy credentialed CORS checks"
  );
  assert!(
    !missing_font.bytes.is_empty(),
    "font placeholder should be non-empty"
  );

  let missing_css = fetcher
    .fetch(&missing_css_url)
    .expect("fetch placeholder stylesheet");
  assert_eq!(missing_css.content_type.as_deref(), Some("text/css"));
  assert_eq!(
    missing_css.final_url.as_deref(),
    Some(missing_css_url.as_str())
  );
  let css_text =
    std::str::from_utf8(&missing_css.bytes).expect("stylesheet placeholder should be valid UTF-8");
  parse_stylesheet(css_text).expect("stylesheet placeholder should parse");

  let missing_frame = fetcher
    .fetch(&missing_frame_url)
    .expect("fetch placeholder document");
  assert_eq!(
    missing_frame.content_type.as_deref(),
    Some("text/html; charset=utf-8")
  );
  assert_eq!(
    missing_frame.final_url.as_deref(),
    Some(missing_frame_url.as_str())
  );
  assert!(
    missing_frame.bytes.starts_with(b"<!doctype html>"),
    "document placeholder should start with a doctype"
  );
}

#[test]
fn bundle_page_cache_captures_extensionless_css_url_images() {
  let tmp = TempDir::new().expect("tempdir");

  let html_dir = tmp.path().join("fetches/html");
  std::fs::create_dir_all(&html_dir).expect("create html dir");
  let asset_dir = tmp.path().join("fetches/assets");
  std::fs::create_dir_all(&asset_dir).expect("create asset dir");

  let stem = "example.invalid";
  let page_url = "https://example.invalid/";
  let html_path = html_dir.join(format!("{stem}.html"));
  std::fs::write(
    &html_path,
    "<!doctype html><html><head><link rel=\"stylesheet\" href=\"/a.css\"></head><body></body></html>",
  )
  .expect("write html");
  std::fs::write(
    html_path.with_extension("html.meta"),
    format!("content-type: text/html\nurl: {page_url}\n"),
  )
  .expect("write meta");

  let css_url = "https://example.invalid/a.css".to_string();
  // Extensionless background image URL discovered from `url(...)`. `bundle_page cache` should treat
  // these as images so disk-cache lookups hit the same kind used by the renderer/prefetchers.
  let bg_url = "https://example.invalid/bg".to_string();

  let mut responses: HashMap<String, (Vec<u8>, &'static str)> = HashMap::new();
  responses.insert(
    css_url.clone(),
    (
      b"body { background-image: url(\"bg\"); }".to_vec(),
      "text/css",
    ),
  );
  responses.insert(bg_url.clone(), (b"png-bytes".to_vec(), "image/png"));

  let mut disk_config = DiskCacheConfig::default();
  disk_config.namespace = Some(disk_cache_namespace());
  disk_config.allow_no_store = true;

  let cache_writer = DiskCachingFetcher::with_configs(
    StaticFetcher {
      responses: Arc::new(responses),
    },
    asset_dir.clone(),
    CachingFetcherConfig::default(),
    disk_config,
  );

  cache_writer
    .fetch_with_request(FetchRequest::new(&css_url, FetchDestination::Style))
    .expect("warm css");
  cache_writer
    .fetch_with_request(FetchRequest::new(&bg_url, FetchDestination::Image))
    .expect("warm bg");

  let bundle_dir = tmp.path().join("bundle");
  let status = Command::new(env!("CARGO_BIN_EXE_bundle_page"))
    .current_dir(tmp.path())
    .args(["cache", stem, "--out"])
    .arg(bundle_dir.to_string_lossy().as_ref())
    .args(["--cache-dir"])
    .arg(&asset_dir)
    .status()
    .expect("run bundle_page cache");

  assert!(
    status.success(),
    "expected cache capture to succeed when asset is present under a different cache kind"
  );

  let bundle = Bundle::load(&bundle_dir).expect("load bundle");
  assert!(
    bundle.manifest().resources.contains_key(bg_url.as_str()),
    "bundle should include extensionless image discovered from CSS url()"
  );

  let fetcher = BundledFetcher::new(bundle);
  assert_eq!(
    fetcher.fetch(&bg_url).expect("fetch bg").bytes,
    b"png-bytes".to_vec()
  );
}

#[test]
fn bundle_page_cache_falls_back_between_image_and_image_cors_cache_kinds() {
  let tmp = TempDir::new().expect("tempdir");

  let html_dir = tmp.path().join("fetches/html");
  std::fs::create_dir_all(&html_dir).expect("create html dir");
  let asset_dir = tmp.path().join("fetches/assets");
  std::fs::create_dir_all(&asset_dir).expect("create asset dir");

  let stem = "example.invalid";
  let page_url = "https://example.invalid/";
  let html_path = html_dir.join(format!("{stem}.html"));
  std::fs::write(
    &html_path,
    "<!doctype html><html><body><img src=\"img.png\"></body></html>",
  )
  .expect("write html");
  std::fs::write(
    html_path.with_extension("html.meta"),
    format!("content-type: text/html\nurl: {page_url}\n"),
  )
  .expect("write meta");

  let img_url = "https://example.invalid/img.png".to_string();

  let mut responses: HashMap<String, (Vec<u8>, &'static str)> = HashMap::new();
  responses.insert(img_url.clone(), (b"png-bytes-1".to_vec(), "image/png"));

  let mut disk_config = DiskCacheConfig::default();
  disk_config.namespace = Some(disk_cache_namespace());
  disk_config.allow_no_store = true;

  let cache_writer = DiskCachingFetcher::with_configs(
    StaticFetcher {
      responses: Arc::new(responses),
    },
    asset_dir.clone(),
    CachingFetcherConfig::default(),
    disk_config,
  );

  // Warm the disk cache under `ImageCors` so capture has to fall back from the crawler's inferred
  // `Image` destination.
  cache_writer
    .fetch_with_request(FetchRequest::new(&img_url, FetchDestination::ImageCors))
    .expect("warm img");

  let bundle_dir = tmp.path().join("bundle");
  let status = Command::new(env!("CARGO_BIN_EXE_bundle_page"))
    .current_dir(tmp.path())
    .args(["cache", stem, "--out"])
    .arg(bundle_dir.to_string_lossy().as_ref())
    .args(["--cache-dir"])
    .arg(&asset_dir)
    .status()
    .expect("run bundle_page cache");

  assert!(
    status.success(),
    "expected cache capture to succeed when image is cached under ImageCors kind"
  );

  let bundle = Bundle::load(&bundle_dir).expect("load bundle");
  assert!(
    bundle.manifest().resources.contains_key(img_url.as_str()),
    "bundle should include image referenced from HTML"
  );

  let fetcher = BundledFetcher::new(bundle);
  assert_eq!(
    fetcher.fetch(&img_url).expect("fetch img").bytes,
    b"png-bytes-1".to_vec()
  );
}

#[test]
fn bundle_page_cache_prefers_image_cors_kind_for_img_crossorigin() {
  let tmp = TempDir::new().expect("tempdir");

  let html_dir = tmp.path().join("fetches/html");
  std::fs::create_dir_all(&html_dir).expect("create html dir");
  let asset_dir = tmp.path().join("fetches/assets");
  std::fs::create_dir_all(&asset_dir).expect("create asset dir");

  let stem = "example.invalid";
  let page_url = "https://example.invalid/";
  let html_path = html_dir.join(format!("{stem}.html"));
  std::fs::write(
    &html_path,
    "<!doctype html><html><body><img src=\"img.png\" crossorigin></body></html>",
  )
  .expect("write html");
  std::fs::write(
    html_path.with_extension("html.meta"),
    format!("content-type: text/html\nurl: {page_url}\n"),
  )
  .expect("write meta");

  let img_url = "https://example.invalid/img.png".to_string();

  #[derive(Clone, Default)]
  struct KindAwareFetcher {
    url: String,
  }

  impl ResourceFetcher for KindAwareFetcher {
    fn fetch(&self, url: &str) -> Result<FetchedResource, Error> {
      self.fetch_with_request(FetchRequest::new(url, FetchDestination::Other))
    }

    fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource, Error> {
      if req.url != self.url {
        return Err(Error::Other(format!("unexpected fetch: {}", req.url)));
      }

      let mut resource = match req.destination {
        FetchDestination::Image => FetchedResource::with_final_url(
          b"no-cors".to_vec(),
          Some("image/png".to_string()),
          Some(req.url.to_string()),
        ),
        FetchDestination::ImageCors => {
          let mut res = FetchedResource::with_final_url(
            b"cors".to_vec(),
            Some("image/png".to_string()),
            Some(req.url.to_string()),
          );
          res.access_control_allow_origin = Some("https://example.invalid".to_string());
          res.access_control_allow_credentials = true;
          res
        }
        other => {
          return Err(Error::Other(format!(
            "unexpected destination for {}: {other:?}",
            req.url
          )));
        }
      };
      resource.status = Some(200);
      Ok(resource)
    }
  }

  let mut disk_config = DiskCacheConfig::default();
  disk_config.namespace = Some(disk_cache_namespace());
  disk_config.allow_no_store = true;

  let cache_writer = DiskCachingFetcher::with_configs(
    KindAwareFetcher {
      url: img_url.clone(),
    },
    asset_dir.clone(),
    CachingFetcherConfig::default(),
    disk_config,
  );

  cache_writer
    .fetch_with_request(FetchRequest::new(&img_url, FetchDestination::Image))
    .expect("warm img (no-cors)");
  cache_writer
    .fetch_with_request(FetchRequest::new(&img_url, FetchDestination::ImageCors))
    .expect("warm img (cors)");

  let bundle_dir = tmp.path().join("bundle");
  let status = Command::new(env!("CARGO_BIN_EXE_bundle_page"))
    .current_dir(tmp.path())
    .args(["cache", stem, "--out"])
    .arg(bundle_dir.to_string_lossy().as_ref())
    .status()
    .expect("run bundle_page cache");

  assert!(
    status.success(),
    "bundle_page cache should succeed when both Image and ImageCors variants are cached"
  );

  let bundle = Bundle::load(&bundle_dir).expect("load bundle");
  let fetcher = BundledFetcher::new(bundle);
  let res = fetcher.fetch(&img_url).expect("fetch img");
  assert_eq!(
    res.bytes,
    b"cors".to_vec(),
    "expected bundle to store the ImageCors response when <img crossorigin> is used"
  );
  assert_eq!(
    res.access_control_allow_origin.as_deref(),
    Some("https://example.invalid")
  );
  assert!(
    res.access_control_allow_credentials,
    "expected bundled ImageCors response to preserve Access-Control-Allow-Credentials"
  );
}

#[test]
fn bundle_page_cache_respects_user_agent_for_namespace() {
  let tmp = TempDir::new().expect("tempdir");

  let html_dir = tmp.path().join("fetches/html");
  std::fs::create_dir_all(&html_dir).expect("create html dir");
  let asset_dir = tmp.path().join("fetches/assets");
  std::fs::create_dir_all(&asset_dir).expect("create asset dir");

  let stem = "example.invalid";
  let page_url = "https://example.invalid/";
  let html_path = html_dir.join(format!("{stem}.html"));
  std::fs::write(
    &html_path,
    "<!doctype html><html><head><link rel=\"stylesheet\" href=\"/a.css\"></head><body><img src=\"img.png\"></body></html>",
  )
  .expect("write html");
  std::fs::write(
    html_path.with_extension("html.meta"),
    format!("content-type: text/html\nurl: {page_url}\n"),
  )
  .expect("write meta");

  let css_url = "https://example.invalid/a.css".to_string();
  let img_url = "https://example.invalid/img.png".to_string();
  let bg_url = "https://example.invalid/bg.png".to_string();

  let mut responses: HashMap<String, (Vec<u8>, &'static str)> = HashMap::new();
  responses.insert(
    css_url.clone(),
    (
      b"body { background-image: url(\"bg.png\"); }".to_vec(),
      "text/css",
    ),
  );
  responses.insert(img_url.clone(), (b"png-bytes-1".to_vec(), "image/png"));
  responses.insert(bg_url.clone(), (b"png-bytes-2".to_vec(), "image/png"));

  let custom_user_agent = "custom-test-ua/1.0";
  let mut disk_config = DiskCacheConfig::default();
  disk_config.namespace = Some(disk_cache_namespace_for(
    custom_user_agent,
    DEFAULT_ACCEPT_LANGUAGE,
  ));
  disk_config.allow_no_store = true;

  let cache_writer = DiskCachingFetcher::with_configs(
    StaticFetcher {
      responses: Arc::new(responses),
    },
    asset_dir.clone(),
    CachingFetcherConfig::default(),
    disk_config,
  );

  cache_writer
    .fetch_with_request(FetchRequest::new(&css_url, FetchDestination::Style))
    .expect("warm css");
  cache_writer
    .fetch_with_request(FetchRequest::new(&img_url, FetchDestination::Image))
    .expect("warm img");
  cache_writer
    .fetch_with_request(FetchRequest::new(&bg_url, FetchDestination::Image))
    .expect("warm bg");

  let mismatch_out = tmp.path().join("bundle-mismatch");
  let mismatch_status = Command::new(env!("CARGO_BIN_EXE_bundle_page"))
    .current_dir(tmp.path())
    .args(["cache", stem, "--out"])
    .arg(mismatch_out.to_string_lossy().as_ref())
    .status()
    .expect("run bundle_page cache (mismatch)");
  assert!(
    !mismatch_status.success(),
    "expected cache capture to fail when disk cache namespace (User-Agent) does not match"
  );

  let match_out = tmp.path().join("bundle-match");
  let match_status = Command::new(env!("CARGO_BIN_EXE_bundle_page"))
    .current_dir(tmp.path())
    .args(["cache", stem])
    .args(["--user-agent", custom_user_agent])
    .args(["--out", match_out.to_string_lossy().as_ref()])
    .status()
    .expect("run bundle_page cache (match)");
  assert!(
    match_status.success(),
    "expected cache capture to succeed when --user-agent matches cache namespace"
  );
}
