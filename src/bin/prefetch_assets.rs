//! Prefetch CSS (and optional HTML/CSS subresources) into the disk-backed cache.
//!
//! This is a best-effort helper intended for the pageset workflow:
//! `fetch_pages` caches HTML, then this tool warms `fetches/assets/` so the
//! render step doesn't spend its 5s budget waiting on first-run network fetches.

#[cfg(feature = "disk_cache")]
use fastrender::cli_utils as common;

use serde::Serialize;

#[derive(Serialize)]
struct PrefetchAssetsCapabilities {
  name: &'static str,
  disk_cache_feature: bool,
  flags: PrefetchAssetsCapabilitiesFlags,
}

#[derive(Serialize)]
struct PrefetchAssetsCapabilitiesFlags {
  prefetch_fonts: bool,
  prefetch_images: bool,
  prefetch_media: bool,
  prefetch_scripts: bool,
  prefetch_iframes: bool,
  prefetch_embeds: bool,
  prefetch_icons: bool,
  prefetch_video_posters: bool,
  prefetch_css_url_assets: bool,
  max_discovered_assets_per_page: bool,
  max_images_per_page: bool,
  max_image_urls_per_element: bool,
  max_media_bytes_per_file: bool,
  max_media_bytes_per_page: bool,
  report_json: bool,
  report_per_page_dir: bool,
  max_report_urls_per_kind: bool,
  dry_run: bool,
}

fn capabilities_json(disk_cache_feature: bool) -> String {
  let flags = if disk_cache_feature {
    PrefetchAssetsCapabilitiesFlags {
      prefetch_fonts: true,
      prefetch_images: true,
      prefetch_media: true,
      prefetch_scripts: true,
      prefetch_iframes: true,
      prefetch_embeds: true,
      prefetch_icons: true,
      prefetch_video_posters: true,
      prefetch_css_url_assets: true,
      max_discovered_assets_per_page: true,
      max_images_per_page: true,
      max_image_urls_per_element: true,
      max_media_bytes_per_file: true,
      max_media_bytes_per_page: true,
      report_json: true,
      report_per_page_dir: true,
      max_report_urls_per_kind: true,
      dry_run: true,
    }
  } else {
    PrefetchAssetsCapabilitiesFlags {
      prefetch_fonts: false,
      prefetch_images: false,
      prefetch_media: false,
      prefetch_scripts: false,
      prefetch_iframes: false,
      prefetch_embeds: false,
      prefetch_icons: false,
      prefetch_video_posters: false,
      prefetch_css_url_assets: false,
      max_discovered_assets_per_page: false,
      max_images_per_page: false,
      max_image_urls_per_element: false,
      max_media_bytes_per_file: false,
      max_media_bytes_per_page: false,
      report_json: false,
      report_per_page_dir: false,
      max_report_urls_per_kind: false,
      dry_run: false,
    }
  };

  let caps = PrefetchAssetsCapabilities {
    name: "prefetch_assets",
    disk_cache_feature,
    flags,
  };
  // This should be infallible for a static `Serialize` struct; avoid panicking in CLI binaries.
  serde_json::to_string(&caps).unwrap_or_else(|err| {
    format!(
      "{{\"name\":\"prefetch_assets\",\"disk_cache_feature\":{},\"error\":\"{}\"}}",
      disk_cache_feature,
      err.to_string().replace('\"', "\\\"")
    )
  })
}

fn print_capabilities(disk_cache_feature: bool) {
  println!("{}", capabilities_json(disk_cache_feature));
}

#[cfg(all(test, not(feature = "disk_cache")))]
mod capabilities_tests {
  use super::*;
  use serde_json::Value;

  #[test]
  fn capabilities_json_includes_expected_keys_without_disk_cache() {
    let json = capabilities_json(false);
    let parsed: Value = serde_json::from_str(&json).expect("capabilities JSON should parse");

    assert_eq!(
      parsed.get("name").and_then(Value::as_str),
      Some("prefetch_assets")
    );
    assert_eq!(
      parsed.get("disk_cache_feature").and_then(Value::as_bool),
      Some(false)
    );

    let flags = parsed
      .get("flags")
      .and_then(Value::as_object)
      .expect("capabilities should include flags object");
    for key in [
      "prefetch_fonts",
      "prefetch_images",
      "prefetch_media",
      "prefetch_scripts",
      "prefetch_iframes",
      "prefetch_embeds",
      "prefetch_icons",
      "prefetch_video_posters",
      "prefetch_css_url_assets",
      "max_discovered_assets_per_page",
      "max_images_per_page",
      "max_image_urls_per_element",
      "max_media_bytes_per_file",
      "max_media_bytes_per_page",
      "report_json",
      "report_per_page_dir",
      "max_report_urls_per_kind",
      "dry_run",
    ] {
      assert_eq!(
        flags.get(key).and_then(Value::as_bool),
        Some(false),
        "capabilities should report flags.{key}=false when disk_cache is unavailable"
      );
    }
  }
}

#[cfg(not(feature = "disk_cache"))]
fn main() {
  if std::env::args().any(|arg| arg == "--capabilities" || arg == "--print-capabilities-json") {
    print_capabilities(false);
    return;
  }
  eprintln!(
    "prefetch_assets requires the `disk_cache` feature. Re-run with `--features disk_cache`."
  );
  std::process::exit(2);
}

#[cfg(feature = "disk_cache")]
mod disk_cache_main {
  use clap::{ArgAction, Parser};
  use fastrender::css::encoding::decode_css_bytes;
  use fastrender::css::loader::{
    absolutize_css_urls_cow, extract_css_links_with_meta, extract_embedded_css_urls,
    link_rel_is_stylesheet_candidate, resolve_href, resolve_href_with_base, FetchedStylesheet,
  };
  use fastrender::css::parser::{
    extract_scoped_css_sources, parse_stylesheet, tokenize_rel_list, StylesheetSource,
  };
  use fastrender::css::types::CssImportLoader;
  use fastrender::css::types::{FontFaceSource, FontFaceUrlSource, FontSourceFormat, StyleSheet};
  use fastrender::debug::runtime;
  use fastrender::dom::{parse_html, DomNode};
  use fastrender::geometry::Size;
  use fastrender::html::asset_discovery::discover_html_asset_urls;
  use fastrender::html::image_prefetch::{discover_image_prefetch_requests, ImagePrefetchLimits};
  use fastrender::html::images::ImageSelectionContext;
  use fastrender::image_loader::ImageCache;
  use fastrender::pageset::{cache_html_path, pageset_entries, PagesetEntry, PagesetFilter};
  use fastrender::resource::{
    ensure_font_mime_sane, ensure_http_success, ensure_image_mime_sane, ensure_media_mime_sane,
    ensure_script_mime_sane, ensure_stylesheet_mime_sane, is_data_url, origin_from_url,
    CachingFetcherConfig, CorsMode, DiskCachingFetcher, DocumentOrigin, FetchCredentialsMode,
    FetchDestination, FetchRequest, FetchedResource, ReferrerPolicy, ResourceFetcher,
    DEFAULT_ACCEPT_LANGUAGE, DEFAULT_USER_AGENT,
  };
  use fastrender::style::media::{MediaContext, MediaQuery, MediaQueryCache};
  use fastrender::tree::box_tree::CrossOriginAttribute;
  use rayon::prelude::*;
  use rayon::ThreadPoolBuilder;
  use regex::Regex;
  use serde::Serialize;
  use std::cell::RefCell;
  use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
  use std::fs;
  use std::io;
  use std::path::{Path, PathBuf};
  use std::sync::{Arc, OnceLock};
  use std::time::Duration;

  use crate::common::args::{default_jobs, parse_shard, DiskCacheArgs, TimeoutArgs, ViewportArgs};
  use crate::common::asset_discovery::{discover_css_urls, extract_inline_css_chunks};
  use crate::common::disk_cache_stats::scan_disk_cache_dir;
  use crate::common::render_pipeline::{
    build_http_fetcher, decode_html_resource, disk_cache_namespace, read_cached_document,
  };

  const DEFAULT_ASSET_DIR: &str = "fetches/assets";

  fn trim_ascii_whitespace(value: &str) -> &str {
    value
      .trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
  }

  #[derive(Parser, Debug)]
  #[command(name = "prefetch_assets", version, about)]
  struct Args {
    /// Print machine-readable JSON describing supported CLI flags and exit.
    #[arg(long, alias = "print-capabilities-json")]
    capabilities: bool,

    /// Number of parallel pages to prefetch
    #[arg(long, short, default_value_t = default_jobs())]
    jobs: usize,

    #[command(flatten)]
    timeout: TimeoutArgs,

    /// Override the User-Agent header
    #[arg(long, default_value = DEFAULT_USER_AGENT)]
    user_agent: String,

    /// Override the Accept-Language header
    #[arg(long, default_value = DEFAULT_ACCEPT_LANGUAGE)]
    accept_language: String,

    #[command(flatten)]
    viewport: ViewportArgs,

    #[command(flatten)]
    disk_cache: DiskCacheArgs,

    /// Prefetch font URLs referenced by fetched CSS (true/false)
    #[arg(
      long,
      default_value_t = true,
      action = ArgAction::Set,
      num_args = 0..=1,
      default_missing_value = "true"
    )]
    prefetch_fonts: bool,

    /// Prefetch image-like URLs referenced directly from HTML (true/false)
    #[arg(
      long,
      default_value_t = false,
      action = ArgAction::Set,
      num_args = 0..=1,
      default_missing_value = "true"
    )]
    prefetch_images: bool,

    /// Prefetch media sources referenced directly from HTML (`<video src>`, `<audio src>`, `<source src>`) (true/false)
    ///
    /// This is opt-in because media files can be large; use `--max-media-bytes-per-file` and
    /// `--max-media-bytes-per-page` as safety valves.
    #[arg(
      long,
      default_value_t = false,
      action = ArgAction::Set,
      num_args = 0..=1,
      default_missing_value = "true"
    )]
    prefetch_media: bool,

    /// Prefetch script resources referenced directly from HTML (`<script src>`, script preloads, modulepreload) (true/false)
    #[arg(
      long,
      default_value_t = false,
      action = ArgAction::Set,
      num_args = 0..=1,
      default_missing_value = "true"
    )]
    prefetch_scripts: bool,

    /// Maximum number of image elements to prefetch per page (bounds cache growth)
    #[arg(long, default_value_t = 150)]
    max_images_per_page: usize,

    /// Maximum number of URLs to prefetch per image element (primary + fallback(s))
    #[arg(long, default_value_t = 2)]
    max_image_urls_per_element: usize,

    /// Maximum bytes to prefetch for a single media file (0 disables the cap).
    #[arg(long, default_value_t = 10_u64 * 1024 * 1024, value_name = "BYTES")]
    max_media_bytes_per_file: u64,

    /// Maximum total bytes to prefetch for all media files discovered in a page (0 disables the cap).
    #[arg(long, default_value_t = 50_u64 * 1024 * 1024, value_name = "BYTES")]
    max_media_bytes_per_page: u64,

    /// Prefetch iframe documents referenced directly from HTML (true/false)
    ///
    /// This also best-effort warms the discovered document's linked stylesheets (and their
    /// `@import` chains/fonts), plus HTML images when `--prefetch-images` is enabled.
    ///
    /// This can explode on pages that embed many third-party iframes, so it defaults to off.
    #[arg(
      long,
      alias = "prefetch-documents",
      default_value_t = false,
      action = ArgAction::Set,
      num_args = 0..=1,
      default_missing_value = "true"
    )]
    prefetch_iframes: bool,

    /// Prefetch subresources referenced by `<embed src>` and `<object data>` (true/false)
    #[arg(
      long,
      default_value_t = false,
      action = ArgAction::Set,
      num_args = 0..=1,
      default_missing_value = "true"
    )]
    prefetch_embeds: bool,

    /// Prefetch icon resources referenced by `<link rel=icon ... href=...>` (true/false)
    #[arg(
      long,
      default_value_t = false,
      action = ArgAction::Set,
      num_args = 0..=1,
      default_missing_value = "true"
    )]
    prefetch_icons: bool,

    /// Prefetch poster images referenced by `<video poster>` (true/false)
    #[arg(
      long,
      default_value_t = false,
      action = ArgAction::Set,
      num_args = 0..=1,
      default_missing_value = "true"
    )]
    prefetch_video_posters: bool,

    /// Prefetch non-CSS assets referenced via `url(...)` in CSS (true/false)
    #[arg(
      long,
      default_value_t = true,
      action = ArgAction::Set,
      num_args = 0..=1,
      default_missing_value = "true"
    )]
    prefetch_css_url_assets: bool,

    /// Safety valve: cap the number of unique discovered assets per page (0 disables the cap)
    #[arg(long, default_value_t = 2000)]
    max_discovered_assets_per_page: usize,

    /// Discovery-only mode: scan HTML/CSS but do not fetch/write any assets to the cache.
    #[arg(long, alias = "discover-only")]
    dry_run: bool,

    /// Write a single deterministic JSON report describing the prefetch run.
    #[arg(long, value_name = "PATH")]
    report_json: Option<PathBuf>,

    /// Write one report JSON per page stem under the given directory.
    #[arg(long, value_name = "DIR")]
    report_per_page_dir: Option<PathBuf>,

    /// Cap the number of sampled URLs per asset kind in the report (0 => counts only).
    #[arg(long, default_value_t = 50)]
    max_report_urls_per_kind: usize,

    /// Override disk cache directory (defaults to fetches/assets)
    #[arg(long, default_value = DEFAULT_ASSET_DIR)]
    cache_dir: PathBuf,

    /// Prefetch only listed pages (comma-separated URLs or stems)
    #[arg(long, value_delimiter = ',')]
    pages: Option<Vec<String>>,

    /// Process only a deterministic shard of the page set (index/total, 0-based)
    #[arg(long, value_parser = parse_shard)]
    shard: Option<(usize, usize)>,
  }

  #[derive(Debug, Default, Clone)]
  struct UrlOutcomeSet {
    discovered: BTreeSet<String>,
    fetched: BTreeSet<String>,
    failed: BTreeSet<String>,
  }

  impl UrlOutcomeSet {
    fn record_discovered(&mut self, url: impl Into<String>) {
      self.discovered.insert(url.into());
    }

    fn record_fetch_result(&mut self, url: impl Into<String>, success: bool) {
      let url = url.into();
      self.discovered.insert(url.clone());
      if success {
        self.fetched.insert(url.clone());
      } else {
        self.failed.insert(url);
      }
    }
  }

  #[derive(Debug, Default, Clone)]
  struct PageSummaryReport {
    css: UrlOutcomeSet,
    imports: UrlOutcomeSet,
    fonts: UrlOutcomeSet,
    images: UrlOutcomeSet,
    media: UrlOutcomeSet,
    scripts: UrlOutcomeSet,
    documents: UrlOutcomeSet,
    css_url_assets: UrlOutcomeSet,
  }

  #[derive(Debug, Default, Clone)]
  struct PageSummary {
    stem: String,
    discovered_css: usize,
    fetched_css: usize,
    failed_css: usize,
    fetched_imports: usize,
    failed_imports: usize,
    fetched_fonts: usize,
    failed_fonts: usize,
    discovered_images: usize,
    fetched_images: usize,
    failed_images: usize,
    discovered_media: usize,
    fetched_media: usize,
    failed_media: usize,
    skipped_media: usize,
    discovered_scripts: usize,
    fetched_scripts: usize,
    failed_scripts: usize,
    discovered_documents: usize,
    fetched_documents: usize,
    failed_documents: usize,
    discovered_css_assets: usize,
    fetched_css_assets: usize,
    failed_css_assets: usize,
    skipped: bool,
    report: PageSummaryReport,
  }

  #[derive(Debug, Clone, Copy)]
  struct PrefetchOptions {
    prefetch_fonts: bool,
    prefetch_images: bool,
    prefetch_media: bool,
    prefetch_scripts: bool,
    prefetch_icons: bool,
    prefetch_video_posters: bool,
    prefetch_iframes: bool,
    prefetch_embeds: bool,
    prefetch_css_url_assets: bool,
    max_discovered_assets_per_page: usize,
    image_limits: ImagePrefetchLimits,
    max_media_bytes_per_file: u64,
    max_media_bytes_per_page: u64,
    dry_run: bool,
  }

  const PREFETCH_ASSETS_REPORT_VERSION: u32 = 1;

  #[derive(Serialize, Debug, Clone)]
  struct PrefetchAssetsReportCountAndSample {
    count: usize,
    urls: Vec<String>,
  }

  #[derive(Serialize, Debug, Clone)]
  struct PrefetchAssetsReportKind {
    discovered: PrefetchAssetsReportCountAndSample,
    fetched: PrefetchAssetsReportCountAndSample,
    failed: PrefetchAssetsReportCountAndSample,
  }

  #[derive(Serialize, Debug, Clone)]
  struct PrefetchAssetsReportPage {
    stem: String,
    skipped: bool,
    css: PrefetchAssetsReportKind,
    imports: PrefetchAssetsReportKind,
    fonts: PrefetchAssetsReportKind,
    images: PrefetchAssetsReportKind,
    media: PrefetchAssetsReportKind,
    scripts: PrefetchAssetsReportKind,
    documents: PrefetchAssetsReportKind,
    css_url_assets: PrefetchAssetsReportKind,
  }

  #[derive(Serialize, Debug, Clone)]
  struct PrefetchAssetsReport {
    version: u32,
    cache_dir: String,
    dry_run: bool,
    max_report_urls_per_kind: usize,
    pages: Vec<PrefetchAssetsReportPage>,
  }

  fn sample_urls(urls: &BTreeSet<String>, max: usize) -> Vec<String> {
    if max == 0 {
      return Vec::new();
    }
    urls.iter().take(max).cloned().collect()
  }

  fn build_kind_report(kind: &UrlOutcomeSet, max: usize) -> PrefetchAssetsReportKind {
    PrefetchAssetsReportKind {
      discovered: PrefetchAssetsReportCountAndSample {
        count: kind.discovered.len(),
        urls: sample_urls(&kind.discovered, max),
      },
      fetched: PrefetchAssetsReportCountAndSample {
        count: kind.fetched.len(),
        urls: sample_urls(&kind.fetched, max),
      },
      failed: PrefetchAssetsReportCountAndSample {
        count: kind.failed.len(),
        urls: sample_urls(&kind.failed, max),
      },
    }
  }

  fn build_prefetch_assets_report(
    pages: &[PageSummary],
    cache_dir: &Path,
    dry_run: bool,
    max_report_urls_per_kind: usize,
  ) -> PrefetchAssetsReport {
    let mut ordered: Vec<&PageSummary> = pages.iter().collect();
    ordered.sort_by(|a, b| a.stem.cmp(&b.stem));

    PrefetchAssetsReport {
      version: PREFETCH_ASSETS_REPORT_VERSION,
      cache_dir: cache_dir.display().to_string(),
      dry_run,
      max_report_urls_per_kind,
      pages: ordered
        .into_iter()
        .map(|page| PrefetchAssetsReportPage {
          stem: page.stem.clone(),
          skipped: page.skipped,
          css: build_kind_report(&page.report.css, max_report_urls_per_kind),
          imports: build_kind_report(&page.report.imports, max_report_urls_per_kind),
          fonts: build_kind_report(&page.report.fonts, max_report_urls_per_kind),
          images: build_kind_report(&page.report.images, max_report_urls_per_kind),
          media: build_kind_report(&page.report.media, max_report_urls_per_kind),
          scripts: build_kind_report(&page.report.scripts, max_report_urls_per_kind),
          documents: build_kind_report(&page.report.documents, max_report_urls_per_kind),
          css_url_assets: build_kind_report(&page.report.css_url_assets, max_report_urls_per_kind),
        })
        .collect(),
    }
  }

  fn prefetch_assets_report_json(report: &PrefetchAssetsReport) -> serde_json::Result<String> {
    let mut json = serde_json::to_string_pretty(report)?;
    json.push('\n');
    Ok(json)
  }

  fn write_prefetch_assets_report(path: &Path, report: &PrefetchAssetsReport) -> io::Result<()> {
    let json = prefetch_assets_report_json(report)
      .map_err(|err| io::Error::new(io::ErrorKind::Other, err))?;
    if let Some(parent) = path.parent() {
      fs::create_dir_all(parent)?;
    }
    fs::write(path, json)
  }

  fn report_per_page_path(dir: &Path, stem: &str) -> PathBuf {
    dir.join(format!("{stem}.json"))
  }

  fn merge_page_summary(into: &mut PageSummary, other: PageSummary) {
    fn merge_outcomes(into: &mut UrlOutcomeSet, other: UrlOutcomeSet) {
      into.discovered.extend(other.discovered);
      into.fetched.extend(other.fetched);
      into.failed.extend(other.failed);
    }

    into.discovered_css += other.discovered_css;
    into.fetched_css += other.fetched_css;
    into.failed_css += other.failed_css;
    into.fetched_imports += other.fetched_imports;
    into.failed_imports += other.failed_imports;
    into.fetched_fonts += other.fetched_fonts;
    into.failed_fonts += other.failed_fonts;
    into.discovered_images += other.discovered_images;
    into.fetched_images += other.fetched_images;
    into.failed_images += other.failed_images;
    into.discovered_media += other.discovered_media;
    into.fetched_media += other.fetched_media;
    into.failed_media += other.failed_media;
    into.skipped_media += other.skipped_media;
    into.discovered_scripts += other.discovered_scripts;
    into.fetched_scripts += other.fetched_scripts;
    into.failed_scripts += other.failed_scripts;
    into.discovered_documents += other.discovered_documents;
    into.fetched_documents += other.fetched_documents;
    into.failed_documents += other.failed_documents;
    into.discovered_css_assets += other.discovered_css_assets;
    into.fetched_css_assets += other.fetched_css_assets;
    into.failed_css_assets += other.failed_css_assets;

    merge_outcomes(&mut into.report.css, other.report.css);
    merge_outcomes(&mut into.report.imports, other.report.imports);
    merge_outcomes(&mut into.report.fonts, other.report.fonts);
    merge_outcomes(&mut into.report.images, other.report.images);
    merge_outcomes(&mut into.report.media, other.report.media);
    merge_outcomes(&mut into.report.scripts, other.report.scripts);
    merge_outcomes(&mut into.report.documents, other.report.documents);
    merge_outcomes(&mut into.report.css_url_assets, other.report.css_url_assets);
  }

  fn selected_pages(
    entries: &[PagesetEntry],
    filter: Option<&PagesetFilter>,
    shard: Option<(usize, usize)>,
  ) -> Vec<PagesetEntry> {
    let filtered: Vec<PagesetEntry> = entries
      .iter()
      .cloned()
      .filter(|entry| match filter {
        Some(filter) => filter.matches_entry(entry),
        None => true,
      })
      .collect();

    if let Some((index, total)) = shard {
      filtered
        .into_iter()
        .enumerate()
        .filter(|(idx, _)| idx % total == index)
        .map(|(_, entry)| entry)
        .collect()
    } else {
      filtered
    }
  }

  fn normalize_prefetch_url(url: &str) -> Option<String> {
    let trimmed = trim_ascii_whitespace(url);
    if trimmed.is_empty() || trimmed.starts_with('#') {
      return None;
    }
    if is_data_url(trimmed) {
      return None;
    }

    // Fetchers treat URLs with different fragments as distinct; strip fragments so we only cache
    // the underlying resource once (e.g. `sprite.svg#icon`).
    let trimmed = trim_ascii_whitespace(trimmed.split('#').next().unwrap_or(""));
    if trimmed.is_empty() {
      return None;
    }

    let parsed = url::Url::parse(trimmed).ok()?;
    match parsed.scheme() {
      "http" | "https" | "file" => Some(parsed.to_string()),
      _ => None,
    }
  }

  fn looks_like_css_url(url: &str) -> bool {
    url::Url::parse(url)
      .ok()
      .map(|parsed| parsed.path().to_ascii_lowercase().ends_with(".css"))
      .unwrap_or(false)
  }

  fn looks_like_font_url(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
      return false;
    };
    let ext = Path::new(parsed.path())
      .extension()
      .and_then(|e| e.to_str())
      .unwrap_or("")
      .to_ascii_lowercase();
    matches!(
      ext.as_str(),
      "woff2" | "woff" | "ttf" | "otf" | "eot" | "ttc"
    )
  }

  fn looks_like_html_document(res: &FetchedResource, requested_url: &str) -> bool {
    let is_html = res
      .content_type
      .as_deref()
      .map(|ct| {
        let ct = ct.to_ascii_lowercase();
        ct.starts_with("text/html")
          || ct.starts_with("application/xhtml+xml")
          || ct.starts_with("application/html")
          || ct.contains("+html")
      })
      .unwrap_or(false);
    if is_html {
      return true;
    }

    let candidate = res.final_url.as_deref().unwrap_or(requested_url);
    let Ok(parsed) = url::Url::parse(candidate) else {
      return false;
    };
    let lower = parsed.path().to_ascii_lowercase();
    lower.ends_with(".html") || lower.ends_with(".htm") || lower.ends_with(".xhtml")
  }

  fn insert_unique_with_cap(
    all: &RefCell<BTreeSet<String>>,
    set: &mut BTreeSet<String>,
    url: String,
    max_total: usize,
    max_set: usize,
  ) -> bool {
    if set.contains(&url) {
      return true;
    }
    if max_set != 0 && set.len() >= max_set {
      return false;
    }

    let mut all = all.borrow_mut();
    if all.contains(&url) {
      // Deduplicate across all discovery sources: do not schedule the same URL for multiple
      // fetch loops (e.g. an image referenced both from HTML and CSS).
      return true;
    }
    if max_total != 0 && all.len() >= max_total {
      return false;
    }
    all.insert(url.clone());
    set.insert(url);
    true
  }

  fn record_image_candidate(
    all: &RefCell<BTreeSet<String>>,
    set: &mut BTreeSet<String>,
    url: &str,
    max_total: usize,
    max_set: usize,
  ) {
    let Some(normalized) = normalize_prefetch_url(url) else {
      return;
    };
    let _ = insert_unique_with_cap(all, set, normalized, max_total, max_set);
  }

  fn merge_crossorigin_attr(
    current: CrossOriginAttribute,
    incoming: CrossOriginAttribute,
  ) -> CrossOriginAttribute {
    match (current, incoming) {
      (CrossOriginAttribute::UseCredentials, _) | (_, CrossOriginAttribute::UseCredentials) => {
        CrossOriginAttribute::UseCredentials
      }
      (CrossOriginAttribute::Anonymous, _) | (_, CrossOriginAttribute::Anonymous) => {
        CrossOriginAttribute::Anonymous
      }
      _ => CrossOriginAttribute::None,
    }
  }

  fn merge_cors_mode(current: CorsMode, incoming: CorsMode) -> CorsMode {
    match (current, incoming) {
      (CorsMode::UseCredentials, _) | (_, CorsMode::UseCredentials) => CorsMode::UseCredentials,
      _ => CorsMode::Anonymous,
    }
  }

  fn record_cors_image_candidate(
    all: &RefCell<BTreeSet<String>>,
    set: &mut BTreeMap<String, CrossOriginAttribute>,
    url: &str,
    crossorigin: CrossOriginAttribute,
    max_total: usize,
    max_set: usize,
  ) {
    let Some(normalized) = normalize_prefetch_url(url) else {
      return;
    };
    if max_set != 0 && set.len() >= max_set && !set.contains_key(&normalized) {
      return;
    }
    if let Some(existing) = set.get_mut(&normalized) {
      *existing = merge_crossorigin_attr(*existing, crossorigin);
      return;
    }
    {
      let mut all_urls = all.borrow_mut();
      if !all_urls.contains(&normalized) {
        if max_total != 0 && all_urls.len() >= max_total {
          return;
        }
        all_urls.insert(normalized.clone());
      }
    }
    set.insert(normalized, crossorigin);
  }

  fn record_script_candidate(
    all: &RefCell<BTreeSet<String>>,
    set: &mut BTreeSet<String>,
    url: &str,
    max_total: usize,
    max_set: usize,
  ) {
    let Some(normalized) = normalize_prefetch_url(url) else {
      return;
    };
    let _ = insert_unique_with_cap(all, set, normalized, max_total, max_set);
  }

  fn record_cors_script_candidate(
    all: &RefCell<BTreeSet<String>>,
    set: &mut BTreeMap<String, CorsMode>,
    url: &str,
    crossorigin: CorsMode,
    max_total: usize,
    max_set: usize,
  ) {
    let Some(normalized) = normalize_prefetch_url(url) else {
      return;
    };
    if max_set != 0 && set.len() >= max_set && !set.contains_key(&normalized) {
      return;
    }
    if let Some(existing) = set.get_mut(&normalized) {
      *existing = merge_cors_mode(*existing, crossorigin);
      return;
    }
    {
      let mut all_urls = all.borrow_mut();
      if !all_urls.contains(&normalized) {
        if max_total != 0 && all_urls.len() >= max_total {
          return;
        }
        all_urls.insert(normalized.clone());
      }
    }
    set.insert(normalized, crossorigin);
  }

  fn record_document_candidate(
    all: &RefCell<BTreeSet<String>>,
    set: &mut BTreeSet<String>,
    url: &str,
    max_total: usize,
    max_set: usize,
  ) {
    let Some(normalized) = normalize_prefetch_url(url) else {
      return;
    };
    let _ = insert_unique_with_cap(all, set, normalized, max_total, max_set);
  }

  fn merge_media_request(
    current: (FetchDestination, CrossOriginAttribute),
    incoming: (FetchDestination, CrossOriginAttribute),
  ) -> (FetchDestination, CrossOriginAttribute) {
    let kind = match (current.0, incoming.0) {
      // If we discover the same URL for both <video> and <audio>, prefer the video request profile
      // (arbitrary but stable).
      (FetchDestination::Video, _) | (_, FetchDestination::Video) => FetchDestination::Video,
      _ => FetchDestination::Audio,
    };
    let crossorigin = merge_crossorigin_attr(current.1, incoming.1);
    (kind, crossorigin)
  }

  fn record_media_candidate(
    all: &RefCell<BTreeSet<String>>,
    set: &mut BTreeMap<String, (FetchDestination, CrossOriginAttribute)>,
    url: &str,
    kind: FetchDestination,
    crossorigin: CrossOriginAttribute,
    max_total: usize,
    max_set: usize,
  ) {
    let Some(normalized) = normalize_prefetch_url(url) else {
      return;
    };
    if max_set != 0 && set.len() >= max_set && !set.contains_key(&normalized) {
      return;
    }
    if let Some(existing) = set.get_mut(&normalized) {
      *existing = merge_media_request(*existing, (kind, crossorigin));
      return;
    }
    {
      let mut all_urls = all.borrow_mut();
      if !all_urls.contains(&normalized) {
        if max_total != 0 && all_urls.len() >= max_total {
          return;
        }
        all_urls.insert(normalized.clone());
      }
    }
    set.insert(normalized, (kind, crossorigin));
  }

  fn link_rel_is_icon_candidate(rel_tokens: &[String]) -> bool {
    if rel_tokens.iter().any(|t| t == "icon") {
      return true;
    }

    if rel_tokens
      .iter()
      .any(|t| t == "apple-touch-icon" || t == "mask-icon")
    {
      return true;
    }

    if rel_tokens
      .iter()
      .any(|t| t == "apple-touch-icon-precomposed")
    {
      return true;
    }

    false
  }

  fn link_rel_is_manifest_candidate(rel_tokens: &[String]) -> bool {
    rel_tokens.iter().any(|t| t == "manifest")
  }

  fn record_manifest_candidates(
    all: &RefCell<BTreeSet<String>>,
    dom: &DomNode,
    base_url: &str,
    out: &mut BTreeSet<String>,
    max_total: usize,
  ) {
    // Keep worst-case work bounded for pages that ship multiple manifests.
    const MAX_MANIFESTS_PER_PAGE: usize = 8;
    let mut stack: Vec<&DomNode> = vec![dom];
    let mut inserted = 0usize;

    while let Some(node) = stack.pop() {
      if max_total != 0 && all.borrow().len() >= max_total {
        break;
      }
      if inserted >= MAX_MANIFESTS_PER_PAGE {
        break;
      }

      if let Some(tag) = node.tag_name() {
        if tag.eq_ignore_ascii_case("link") {
          let href = node.get_attribute_ref("href").unwrap_or("");
          if !trim_ascii_whitespace(href).is_empty() {
            let rel_attr = node.get_attribute_ref("rel").unwrap_or("");
            if !trim_ascii_whitespace(rel_attr).is_empty() {
              let rel_tokens = tokenize_rel_list(rel_attr);
              if !rel_tokens.is_empty() && link_rel_is_manifest_candidate(&rel_tokens) {
                if let Some(resolved) = resolve_href(base_url, href) {
                  let before = out.len();
                  record_image_candidate(all, out, &resolved, max_total, max_total);
                  if out.len() > before {
                    inserted += 1;
                  }
                }
              }
            }
          }
        }
      }

      if node.template_contents_are_inert() {
        continue;
      }
      for child in node.children.iter().rev() {
        stack.push(child);
      }
    }
  }

  fn record_script_candidates(
    all: &RefCell<BTreeSet<String>>,
    dom: &DomNode,
    base_url: &str,
    scripts: &mut BTreeSet<String>,
    cors_scripts: &mut BTreeMap<String, CorsMode>,
    max_total: usize,
  ) {
    let mut stack: Vec<&DomNode> = vec![dom];

    while let Some(node) = stack.pop() {
      if max_total != 0 && all.borrow().len() >= max_total {
        break;
      }

      if let Some(tag) = node.tag_name() {
        if tag.eq_ignore_ascii_case("script") {
          let Some(src) = node.get_attribute_ref("src") else {
            // Inline script.
            continue;
          };
          if trim_ascii_whitespace(src).is_empty() {
            continue;
          }
          let Some(resolved) = resolve_href(base_url, src) else {
            continue;
          };

          let crossorigin = node.get_attribute_ref("crossorigin").map(|value| {
            let value = trim_ascii_whitespace(value);
            if value.eq_ignore_ascii_case("use-credentials") {
              CorsMode::UseCredentials
            } else {
              CorsMode::Anonymous
            }
          });

          match crossorigin {
            Some(mode) => {
              record_cors_script_candidate(all, cors_scripts, &resolved, mode, max_total, max_total)
            }
            None => record_script_candidate(all, scripts, &resolved, max_total, max_total),
          }
        } else if tag.eq_ignore_ascii_case("link") {
          let href = node.get_attribute_ref("href").unwrap_or("");
          if trim_ascii_whitespace(href).is_empty() {
            continue;
          }
          let rel_attr = node.get_attribute_ref("rel").unwrap_or("");
          if trim_ascii_whitespace(rel_attr).is_empty() {
            continue;
          }
          let rel_tokens = tokenize_rel_list(rel_attr);
          if rel_tokens.is_empty() {
            continue;
          }
          let has_preload = rel_tokens.iter().any(|t| t == "preload");
          let has_modulepreload = rel_tokens.iter().any(|t| t == "modulepreload");
          let as_attr = node.get_attribute_ref("as").unwrap_or("");
          let as_trimmed = trim_ascii_whitespace(as_attr);
          let is_preload_script = has_preload
            && (as_trimmed.eq_ignore_ascii_case("script")
              || as_trimmed.eq_ignore_ascii_case("worker")
              || as_trimmed.eq_ignore_ascii_case("sharedworker"));
          if !has_modulepreload && !is_preload_script {
            continue;
          }

          let Some(resolved) = resolve_href(base_url, href) else {
            continue;
          };

          let crossorigin = node.get_attribute_ref("crossorigin").map(|value| {
            let value = trim_ascii_whitespace(value);
            if value.eq_ignore_ascii_case("use-credentials") {
              CorsMode::UseCredentials
            } else {
              CorsMode::Anonymous
            }
          });

          match crossorigin {
            Some(mode) => {
              record_cors_script_candidate(all, cors_scripts, &resolved, mode, max_total, max_total)
            }
            None => record_script_candidate(all, scripts, &resolved, max_total, max_total),
          }
        }
      }

      if node.template_contents_are_inert() {
        continue;
      }
      for child in node.children.iter().rev() {
        stack.push(child);
      }
    }
  }

  fn record_iframe_document_candidates(
    all: &RefCell<BTreeSet<String>>,
    dom: &DomNode,
    base_url: &str,
    out: &mut BTreeSet<String>,
    max_total: usize,
  ) {
    const MAX_IFRAMES_PER_PAGE: usize = 32;

    let mut stack: Vec<&DomNode> = vec![dom];
    let mut inserted = 0usize;

    while let Some(node) = stack.pop() {
      if max_total != 0 && all.borrow().len() >= max_total {
        break;
      }
      if inserted >= MAX_IFRAMES_PER_PAGE {
        break;
      }

      if let Some(tag) = node.tag_name() {
        if tag.eq_ignore_ascii_case("iframe") {
          if let Some(src) = node.get_attribute_ref("src") {
            if let Some(resolved) = resolve_href(base_url, src) {
              let before = out.len();
              record_document_candidate(all, out, &resolved, max_total, max_total);
              if out.len() > before {
                inserted += 1;
              }
            }
          }
        }
      }

      if node.template_contents_are_inert() {
        continue;
      }
      for child in node.children.iter().rev() {
        stack.push(child);
      }
    }
  }

  fn record_iframe_document_candidates_from_html(
    all: &RefCell<BTreeSet<String>>,
    html: &str,
    base_url: &str,
    out: &mut BTreeSet<String>,
    max_total: usize,
  ) {
    const MAX_IFRAMES_PER_PAGE: usize = 32;
    static IFRAME_SRC: OnceLock<Result<Regex, regex::Error>> = OnceLock::new();

    let iframe_src = match IFRAME_SRC.get_or_init(|| {
      Regex::new("(?is)<iframe[^>]*\\ssrc\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
    }) {
      Ok(re) => re,
      Err(_) => return,
    };

    let mut inserted = 0usize;
    for caps in iframe_src.captures_iter(html) {
      if max_total != 0 && all.borrow().len() >= max_total {
        break;
      }
      if inserted >= MAX_IFRAMES_PER_PAGE {
        break;
      }
      let raw = caps
        .get(1)
        .or_else(|| caps.get(2))
        .or_else(|| caps.get(3))
        .map(|m| m.as_str())
        .unwrap_or("");
      if trim_ascii_whitespace(raw).is_empty() {
        continue;
      }
      if let Some(resolved) = resolve_href(base_url, raw) {
        let before = out.len();
        record_document_candidate(all, out, &resolved, max_total, max_total);
        if out.len() > before {
          inserted += 1;
        }
      }
    }
  }

  fn record_icon_candidates_from_html(
    all: &RefCell<BTreeSet<String>>,
    html: &str,
    base_url: &str,
    out: &mut BTreeSet<String>,
    max_total: usize,
  ) {
    const MAX_ICONS_PER_PAGE: usize = 32;
    static LINK_TAG: OnceLock<Result<Regex, regex::Error>> = OnceLock::new();
    static ATTR_REL: OnceLock<Result<Regex, regex::Error>> = OnceLock::new();
    static ATTR_HREF: OnceLock<Result<Regex, regex::Error>> = OnceLock::new();

    let link_tag = match LINK_TAG.get_or_init(|| Regex::new("(?is)<link\\b[^>]*>")) {
      Ok(re) => re,
      Err(_) => return,
    };
    let attr_rel = match ATTR_REL
      .get_or_init(|| Regex::new("(?is)(?:^|\\s)rel\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))"))
    {
      Ok(re) => re,
      Err(_) => return,
    };
    let attr_href = match ATTR_HREF.get_or_init(|| {
      Regex::new("(?is)(?:^|\\s)href\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
    }) {
      Ok(re) => re,
      Err(_) => return,
    };

    let mut inserted = 0usize;
    for caps in link_tag.captures_iter(html) {
      if max_total != 0 && all.borrow().len() >= max_total {
        break;
      }
      if inserted >= MAX_ICONS_PER_PAGE {
        break;
      }
      let tag = caps.get(0).map(|m| m.as_str()).unwrap_or("");
      if tag.is_empty() {
        continue;
      }
      let rel_value = attr_rel
        .captures(tag)
        .and_then(|c| c.get(1).or_else(|| c.get(2)).or_else(|| c.get(3)))
        .map(|m| m.as_str())
        .unwrap_or("");
      if trim_ascii_whitespace(rel_value).is_empty() {
        continue;
      }
      let rel_tokens = tokenize_rel_list(rel_value);
      if rel_tokens.is_empty() || !link_rel_is_icon_candidate(&rel_tokens) {
        continue;
      }

      let href_value = attr_href
        .captures(tag)
        .and_then(|c| c.get(1).or_else(|| c.get(2)).or_else(|| c.get(3)))
        .map(|m| m.as_str())
        .unwrap_or("");
      if trim_ascii_whitespace(href_value).is_empty() {
        continue;
      }
      if let Some(resolved) = resolve_href(base_url, href_value) {
        let before = out.len();
        record_image_candidate(all, out, &resolved, max_total, max_total);
        if out.len() > before {
          inserted += 1;
        }
      }
    }
  }

  fn record_icon_candidates(
    all: &RefCell<BTreeSet<String>>,
    dom: &DomNode,
    base_url: &str,
    out: &mut BTreeSet<String>,
    max_total: usize,
  ) {
    // Keep worst-case work bounded for pages that ship many icon links.
    const MAX_ICONS_PER_PAGE: usize = 32;

    let mut stack: Vec<&DomNode> = vec![dom];
    let mut inserted = 0usize;

    while let Some(node) = stack.pop() {
      if max_total != 0 && all.borrow().len() >= max_total {
        break;
      }
      if inserted >= MAX_ICONS_PER_PAGE {
        break;
      }

      if let Some(tag) = node.tag_name() {
        if tag.eq_ignore_ascii_case("link") {
          let href = node.get_attribute_ref("href").unwrap_or("");
          if !trim_ascii_whitespace(href).is_empty() {
            let rel_attr = node.get_attribute_ref("rel").unwrap_or("");
            if !trim_ascii_whitespace(rel_attr).is_empty() {
              let rel_tokens = tokenize_rel_list(rel_attr);
              if !rel_tokens.is_empty() && link_rel_is_icon_candidate(&rel_tokens) {
                if let Some(resolved) = resolve_href(base_url, href) {
                  let before = out.len();
                  record_image_candidate(all, out, &resolved, max_total, max_total);
                  if out.len() > before {
                    inserted += 1;
                  }
                }
              }
            }
          }
        }
      }

      if node.template_contents_are_inert() {
        continue;
      }
      for child in node.children.iter().rev() {
        stack.push(child);
      }
    }
  }

  fn record_video_poster_candidates_from_html(
    all: &RefCell<BTreeSet<String>>,
    html: &str,
    base_url: &str,
    image_urls: &mut BTreeSet<String>,
    cors_image_urls: &mut BTreeMap<String, CrossOriginAttribute>,
    max_total: usize,
  ) {
    const MAX_VIDEO_POSTERS_PER_PAGE: usize = 32;
    static VIDEO_TAG: OnceLock<Result<Regex, regex::Error>> = OnceLock::new();
    static VIDEO_POSTER_ATTR: OnceLock<Result<Regex, regex::Error>> = OnceLock::new();
    static VIDEO_GNT_GL_PS_ATTR: OnceLock<Result<Regex, regex::Error>> = OnceLock::new();
    static VIDEO_CROSSORIGIN_ATTR: OnceLock<Result<Regex, regex::Error>> = OnceLock::new();

    let video_tag_re = match VIDEO_TAG.get_or_init(|| Regex::new("(?is)<video\\b[^>]*>")) {
      Ok(re) => re,
      Err(_) => return,
    };
    let video_poster_attr = match VIDEO_POSTER_ATTR
      .get_or_init(|| Regex::new("(?is)\\sposter\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))"))
    {
      Ok(re) => re,
      Err(_) => return,
    };
    let video_gnt_gl_ps_attr = match VIDEO_GNT_GL_PS_ATTR
      .get_or_init(|| Regex::new("(?is)\\sgnt-gl-ps\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))"))
    {
      Ok(re) => re,
      Err(_) => return,
    };
    let video_crossorigin_attr = match VIDEO_CROSSORIGIN_ATTR.get_or_init(|| {
      Regex::new("(?is)\\scrossorigin(?:\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+)))?")
    }) {
      Ok(re) => re,
      Err(_) => return,
    };

    let mut inserted = 0usize;
    for tag_match in video_tag_re.find_iter(html) {
      if max_total != 0 && all.borrow().len() >= max_total {
        break;
      }
      if inserted >= MAX_VIDEO_POSTERS_PER_PAGE {
        break;
      }

      let capture_attr = |re: &Regex| -> Option<&str> {
        re.captures(tag_match.as_str())
          .and_then(|caps| {
            caps
              .get(1)
              .or_else(|| caps.get(2))
              .or_else(|| caps.get(3))
              .map(|m| m.as_str())
          })
          .map(trim_ascii_whitespace)
          .filter(|raw| !raw.is_empty())
      };

      let crossorigin = match video_crossorigin_attr.captures(tag_match.as_str()) {
        None => CrossOriginAttribute::None,
        Some(caps) => {
          let value = caps
            .get(1)
            .or_else(|| caps.get(2))
            .or_else(|| caps.get(3))
            .map(|m| m.as_str())
            .unwrap_or("");
          let value = trim_ascii_whitespace(value);
          if value.eq_ignore_ascii_case("use-credentials") {
            CrossOriginAttribute::UseCredentials
          } else {
            // Empty, `anonymous`, and unknown tokens are treated as `anonymous`.
            CrossOriginAttribute::Anonymous
          }
        }
      };

      let Some(raw) =
        capture_attr(video_poster_attr).or_else(|| capture_attr(video_gnt_gl_ps_attr))
      else {
        continue;
      };
      if let Some(resolved) = resolve_href(base_url, raw) {
        match crossorigin {
          CrossOriginAttribute::None => {
            let before = image_urls.len();
            record_image_candidate(all, image_urls, &resolved, max_total, max_total);
            if image_urls.len() > before {
              inserted += 1;
            }
          }
          crossorigin => {
            let before = cors_image_urls.len();
            record_cors_image_candidate(
              all,
              cors_image_urls,
              &resolved,
              crossorigin,
              max_total,
              max_total,
            );
            if cors_image_urls.len() > before {
              inserted += 1;
            }
          }
        }
      }
    }
  }

  fn record_video_poster_candidates(
    all: &RefCell<BTreeSet<String>>,
    dom: &DomNode,
    base_url: &str,
    image_urls: &mut BTreeSet<String>,
    cors_image_urls: &mut BTreeMap<String, CrossOriginAttribute>,
    max_total: usize,
  ) {
    // Keep worst-case work bounded for pages that ship many <video> tags.
    const MAX_VIDEO_POSTERS_PER_PAGE: usize = 32;

    // Some sites (e.g. Webflow background videos) store the poster on an ancestor `data-poster-url`,
    // then rely on JS to populate the descendant `<video poster>`. Carry the wrapper poster through
    // the traversal so we can still prefetch it when normal image discovery is capped.
    let mut stack: Vec<(&DomNode, Option<&str>)> = vec![(dom, None)];
    let mut inserted = 0usize;

    while let Some((node, inherited_wrapper_poster)) = stack.pop() {
      if max_total != 0 && all.borrow().len() >= max_total {
        break;
      }
      if inserted >= MAX_VIDEO_POSTERS_PER_PAGE {
        break;
      }

      let mut wrapper_poster = inherited_wrapper_poster;
      if let Some(tag) = node.tag_name() {
        if !tag.eq_ignore_ascii_case("video") {
          wrapper_poster = node
            .get_attribute_ref("data-poster-url")
            .filter(|value| !trim_ascii_whitespace(value).is_empty())
            .or(wrapper_poster);
        }

        if tag.eq_ignore_ascii_case("video") {
          let poster = node
            .get_attribute_ref("poster")
            .filter(|value| !trim_ascii_whitespace(value).is_empty())
            .or_else(|| {
              node
                .get_attribute_ref("gnt-gl-ps")
                .filter(|value| !trim_ascii_whitespace(value).is_empty())
            })
            .or(wrapper_poster);
          if let Some(poster) = poster {
            if let Some(resolved) = resolve_href(base_url, poster) {
              let crossorigin = match node.get_attribute_ref("crossorigin") {
                None => CrossOriginAttribute::None,
                Some(value) => {
                  let value = trim_ascii_whitespace(value);
                  if value.eq_ignore_ascii_case("use-credentials") {
                    CrossOriginAttribute::UseCredentials
                  } else {
                    // Empty, `anonymous`, and unknown tokens are treated as `anonymous`.
                    CrossOriginAttribute::Anonymous
                  }
                }
              };
              match crossorigin {
                CrossOriginAttribute::None => {
                  let before = image_urls.len();
                  record_image_candidate(all, image_urls, &resolved, max_total, max_total);
                  if image_urls.len() > before {
                    inserted += 1;
                  }
                }
                crossorigin => {
                  let before = cors_image_urls.len();
                  record_cors_image_candidate(
                    all,
                    cors_image_urls,
                    &resolved,
                    crossorigin,
                    max_total,
                    max_total,
                  );
                  if cors_image_urls.len() > before {
                    inserted += 1;
                  }
                }
              }
            }
          }
        }
      }

      if node.template_contents_are_inert() {
        continue;
      }
      for child in node.children.iter().rev() {
        stack.push((child, wrapper_poster));
      }
    }
  }

  fn record_media_source_candidates_from_html(
    all: &RefCell<BTreeSet<String>>,
    html: &str,
    base_url: &str,
    out: &mut BTreeMap<String, (FetchDestination, CrossOriginAttribute)>,
    max_total: usize,
  ) {
    // Keep worst-case work bounded for pages that embed many media tags.
    const MAX_MEDIA_SOURCES_PER_PAGE: usize = 64;
    static MEDIA_TAG: OnceLock<Result<Regex, regex::Error>> = OnceLock::new();

    let media_tag = match MEDIA_TAG.get_or_init(|| {
      Regex::new(concat!(
        "(?is)",
        "<video[^>]*\\ssrc\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))",
        "|",
        "<audio[^>]*\\ssrc\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))",
        "|",
        "<source[^>]*\\ssrc\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))",
      ))
    }) {
      Ok(re) => re,
      Err(_) => return,
    };

    let mut inserted = 0usize;
    for caps in media_tag.captures_iter(html) {
      if max_total != 0 && all.borrow().len() >= max_total {
        break;
      }
      if inserted >= MAX_MEDIA_SOURCES_PER_PAGE {
        break;
      }
      let (raw, destination) = if let Some(raw) = caps
        .get(1)
        .or_else(|| caps.get(2))
        .or_else(|| caps.get(3))
        .map(|m| m.as_str())
      {
        (raw, FetchDestination::Video)
      } else if let Some(raw) = caps
        .get(4)
        .or_else(|| caps.get(5))
        .or_else(|| caps.get(6))
        .map(|m| m.as_str())
      {
        (raw, FetchDestination::Audio)
      } else if let Some(raw) = caps
        .get(7)
        .or_else(|| caps.get(8))
        .or_else(|| caps.get(9))
        .map(|m| m.as_str())
      {
        // `<source>` can also appear inside `<picture>`. When we fall back to regex-based scanning
        // (DOM parse failed), we cannot reliably determine its context, so default to the `<video>`
        // request profile to keep behavior deterministic.
        (raw, FetchDestination::Video)
      } else {
        continue;
      };
      if trim_ascii_whitespace(raw).is_empty() {
        continue;
      }
      if let Some(resolved) = resolve_href(base_url, raw) {
        let before = out.len();
        record_media_candidate(
          all,
          out,
          &resolved,
          destination,
          CrossOriginAttribute::None,
          max_total,
          max_total,
        );
        if out.len() > before {
          inserted += 1;
        }
      }
    }
  }

  fn record_media_source_candidates(
    all: &RefCell<BTreeSet<String>>,
    dom: &DomNode,
    base_url: &str,
    out: &mut BTreeMap<String, (FetchDestination, CrossOriginAttribute)>,
    max_total: usize,
  ) {
    // Keep worst-case work bounded for pages that embed many <video>/<audio> tags with many
    // <source> children.
    const MAX_MEDIA_SOURCES_PER_PAGE: usize = 64;

    let mut stack: Vec<(&DomNode, Option<(FetchDestination, CrossOriginAttribute)>)> =
      vec![(dom, None)];
    let mut inserted = 0usize;

    while let Some((node, in_media)) = stack.pop() {
      if max_total != 0 && all.borrow().len() >= max_total {
        break;
      }
      if inserted >= MAX_MEDIA_SOURCES_PER_PAGE {
        break;
      }

      let mut child_in_media = in_media;
      if let Some(tag) = node.tag_name() {
        if tag.eq_ignore_ascii_case("video") {
          let crossorigin = match node.get_attribute_ref("crossorigin") {
            None => CrossOriginAttribute::None,
            Some(value) => {
              let value = trim_ascii_whitespace(value);
              if value.eq_ignore_ascii_case("use-credentials") {
                CrossOriginAttribute::UseCredentials
              } else {
                // Empty, `anonymous`, and unknown tokens are treated as `anonymous`.
                CrossOriginAttribute::Anonymous
              }
            }
          };
          child_in_media = Some((FetchDestination::Video, crossorigin));
          if let Some(src) = node
            .get_attribute_ref("src")
            .filter(|value| !trim_ascii_whitespace(value).is_empty())
          {
            if let Some(resolved) = resolve_href(base_url, src) {
              let before = out.len();
              record_media_candidate(
                all,
                out,
                &resolved,
                FetchDestination::Video,
                crossorigin,
                max_total,
                max_total,
              );
              if out.len() > before {
                inserted += 1;
              }
            }
          }
        } else if tag.eq_ignore_ascii_case("audio") {
          let crossorigin = match node.get_attribute_ref("crossorigin") {
            None => CrossOriginAttribute::None,
            Some(value) => {
              let value = trim_ascii_whitespace(value);
              if value.eq_ignore_ascii_case("use-credentials") {
                CrossOriginAttribute::UseCredentials
              } else {
                // Empty, `anonymous`, and unknown tokens are treated as `anonymous`.
                CrossOriginAttribute::Anonymous
              }
            }
          };
          child_in_media = Some((FetchDestination::Audio, crossorigin));
          if let Some(src) = node
            .get_attribute_ref("src")
            .filter(|value| !trim_ascii_whitespace(value).is_empty())
          {
            if let Some(resolved) = resolve_href(base_url, src) {
              let before = out.len();
              record_media_candidate(
                all,
                out,
                &resolved,
                FetchDestination::Audio,
                crossorigin,
                max_total,
                max_total,
              );
              if out.len() > before {
                inserted += 1;
              }
            }
          }
        } else if let Some((destination, crossorigin)) = child_in_media {
          if tag.eq_ignore_ascii_case("source") {
            if let Some(src) = node
              .get_attribute_ref("src")
              .filter(|value| !trim_ascii_whitespace(value).is_empty())
            {
              if let Some(resolved) = resolve_href(base_url, src) {
                let before = out.len();
                record_media_candidate(
                  all,
                  out,
                  &resolved,
                  destination,
                  crossorigin,
                  max_total,
                  max_total,
                );
                if out.len() > before {
                  inserted += 1;
                }
              }
            }
          }
        }
      }

      if node.template_contents_are_inert() {
        continue;
      }
      for child in node.children.iter().rev() {
        stack.push((child, child_in_media));
      }
    }
  }

  fn record_embed_document_candidates_from_html(
    all: &RefCell<BTreeSet<String>>,
    html: &str,
    base_url: &str,
    out: &mut BTreeSet<String>,
    max_total: usize,
  ) {
    const MAX_EMBED_DOCS_PER_PAGE: usize = 32;
    static EMBED_DOC: OnceLock<Result<Regex, regex::Error>> = OnceLock::new();

    let embed_doc = match EMBED_DOC.get_or_init(|| {
      Regex::new(concat!(
        "(?is)",
        "<object[^>]*\\sdata\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))",
        "|",
        "<embed[^>]*\\ssrc\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))",
      ))
    }) {
      Ok(re) => re,
      Err(_) => return,
    };

    let mut inserted = 0usize;
    for caps in embed_doc.captures_iter(html) {
      if max_total != 0 && all.borrow().len() >= max_total {
        break;
      }
      if inserted >= MAX_EMBED_DOCS_PER_PAGE {
        break;
      }
      let raw = caps
        .get(1)
        .or_else(|| caps.get(2))
        .or_else(|| caps.get(3))
        .or_else(|| caps.get(4))
        .or_else(|| caps.get(5))
        .or_else(|| caps.get(6))
        .map(|m| m.as_str())
        .unwrap_or("");
      if trim_ascii_whitespace(raw).is_empty() {
        continue;
      }
      if let Some(resolved) = resolve_href(base_url, raw) {
        let before = out.len();
        record_document_candidate(all, out, &resolved, max_total, max_total);
        if out.len() > before {
          inserted += 1;
        }
      }
    }
  }

  fn record_embed_document_candidates(
    all: &RefCell<BTreeSet<String>>,
    dom: &DomNode,
    base_url: &str,
    out: &mut BTreeSet<String>,
    max_total: usize,
  ) {
    // Keep worst-case work bounded for pages that embed many plugin/media elements.
    const MAX_EMBED_DOCS_PER_PAGE: usize = 32;

    let mut stack: Vec<&DomNode> = vec![dom];
    let mut inserted = 0usize;

    while let Some(node) = stack.pop() {
      if max_total != 0 && all.borrow().len() >= max_total {
        break;
      }
      if inserted >= MAX_EMBED_DOCS_PER_PAGE {
        break;
      }

      if let Some(tag) = node.tag_name() {
        if tag.eq_ignore_ascii_case("object") {
          if let Some(data) = node.get_attribute_ref("data") {
            if !trim_ascii_whitespace(data).is_empty() {
              if let Some(resolved) = resolve_href(base_url, data) {
                let before = out.len();
                record_document_candidate(all, out, &resolved, max_total, max_total);
                if out.len() > before {
                  inserted += 1;
                }
              }
            }
          }
        } else if tag.eq_ignore_ascii_case("embed") {
          if let Some(src) = node.get_attribute_ref("src") {
            if !trim_ascii_whitespace(src).is_empty() {
              if let Some(resolved) = resolve_href(base_url, src) {
                let before = out.len();
                record_document_candidate(all, out, &resolved, max_total, max_total);
                if out.len() > before {
                  inserted += 1;
                }
              }
            }
          }
        }
      }

      if node.template_contents_are_inert() {
        continue;
      }
      for child in node.children.iter().rev() {
        stack.push(child);
      }
    }
  }

  fn record_css_url_asset_candidate(
    all: &RefCell<BTreeSet<String>>,
    set: &mut BTreeSet<String>,
    url: &str,
    max_total: usize,
    max_set: usize,
  ) {
    let Some(normalized) = normalize_prefetch_url(url) else {
      return;
    };
    if looks_like_css_url(&normalized) || looks_like_font_url(&normalized) {
      return;
    }
    let _ = insert_unique_with_cap(all, set, normalized, max_total, max_set);
  }

  struct PrefetchImportLoader<'a> {
    fetcher: &'a dyn ResourceFetcher,
    document_referrer: &'a str,
    /// The document base URL used for resolving relative URLs (including `<base href>`).
    ///
    /// The CSS import resolver passes its `base_url` as the `importer_url` when fetching nested
    /// stylesheets. For inline `<style>` blocks this base URL may differ from the document URL,
    /// but the request referrer should still be the document URL. When set, we treat
    /// `importer_url == document_base_url` as a signal to fall back to `document_referrer`.
    document_base_url: Option<&'a str>,
    client_origin: Option<&'a DocumentOrigin>,
    destination: FetchDestination,
    credentials_mode: FetchCredentialsMode,
    referrer_policy: ReferrerPolicy,
    css_cache: RefCell<HashMap<String, FetchedStylesheet>>,
    stylesheet_policies: RefCell<HashMap<String, ReferrerPolicy>>,
    summary: &'a RefCell<PageSummary>,
    all_asset_urls: &'a RefCell<BTreeSet<String>>,
    css_asset_urls: Option<&'a RefCell<BTreeSet<String>>>,
    max_discovered_assets_per_page: usize,
  }

  impl<'a> PrefetchImportLoader<'a> {
    fn new(
      fetcher: &'a dyn ResourceFetcher,
      document_referrer: &'a str,
      document_base_url: Option<&'a str>,
      client_origin: Option<&'a DocumentOrigin>,
      destination: FetchDestination,
      credentials_mode: FetchCredentialsMode,
      referrer_policy: ReferrerPolicy,
      summary: &'a RefCell<PageSummary>,
      all_asset_urls: &'a RefCell<BTreeSet<String>>,
      css_asset_urls: Option<&'a RefCell<BTreeSet<String>>>,
      max_discovered_assets_per_page: usize,
    ) -> Self {
      Self {
        fetcher,
        document_referrer,
        document_base_url,
        client_origin,
        destination,
        credentials_mode,
        referrer_policy,
        css_cache: RefCell::new(HashMap::new()),
        stylesheet_policies: RefCell::new(HashMap::new()),
        summary,
        all_asset_urls,
        css_asset_urls,
        max_discovered_assets_per_page,
      }
    }

    fn referrer_policy_for_importer(&self, importer_url: Option<&str>) -> ReferrerPolicy {
      if let Some(importer_url) = importer_url {
        if let Some(policy) = self.stylesheet_policies.borrow().get(importer_url).copied() {
          return policy;
        }
      }
      self.referrer_policy
    }
  }

  impl CssImportLoader for PrefetchImportLoader<'_> {
    fn load(&self, url: &str) -> fastrender::Result<String> {
      self
        .load_with_importer(url, None)
        .map(|fetched| fetched.css)
    }

    fn referrer_policy_for_stylesheet(&self, url: &str) -> Option<ReferrerPolicy> {
      self.stylesheet_policies.borrow().get(url).copied()
    }

    fn load_with_importer(
      &self,
      url: &str,
      importer_url: Option<&str>,
    ) -> fastrender::Result<FetchedStylesheet> {
      if let Some(cached) = self.css_cache.borrow().get(url).cloned() {
        // `resolve_imports_*` may call us with multiple URL spellings for the same stylesheet
        // (e.g. redirect targets). Report these as successful "fetches" so dry-run and non-dry-run
        // discovery results stay consistent, even when the bytes are already available in-memory.
        self
          .summary
          .borrow_mut()
          .report
          .imports
          .record_fetch_result(url.to_string(), true);
        return Ok(cached);
      }

      let referrer_url = match importer_url {
        Some(importer_url) => {
          if self
            .document_base_url
            .is_some_and(|base_url| base_url == importer_url)
          {
            // `StyleSheet::resolve_imports_*` passes the current resolution base as the importer
            // URL. For inline `<style>` blocks this base may differ from the document URL due to
            // `<base href>`, but the HTTP request referrer should remain the document URL. Only
            // apply this special-case when the importer URL hasn't been fetched as a stylesheet
            // itself (e.g. when `<base href>` points directly at an imported stylesheet URL, nested
            // imports must still use that stylesheet URL as the referrer).
            if !self.stylesheet_policies.borrow().contains_key(importer_url) {
              self.document_referrer
            } else {
              importer_url
            }
          } else {
            importer_url
          }
        }
        None => self.document_referrer,
      };
      let referrer_policy = self.referrer_policy_for_importer(importer_url);
      let mut request = FetchRequest::new(url, self.destination)
        .with_referrer_url(referrer_url)
        .with_referrer_policy(referrer_policy)
        .with_credentials_mode(self.credentials_mode);
      if let Some(origin) = self.client_origin {
        request = request.with_client_origin(origin);
      }

      match self.fetcher.fetch_with_request(request) {
        Ok(res) => {
          if let Err(err) =
            ensure_http_success(&res, url).and_then(|()| ensure_stylesheet_mime_sane(&res, url))
          {
            let mut summary = self.summary.borrow_mut();
            summary.failed_imports += 1;
            summary
              .report
              .imports
              .record_fetch_result(url.to_string(), false);
            return Err(err);
          }
          {
            let mut summary = self.summary.borrow_mut();
            summary.fetched_imports += 1;
            summary
              .report
              .imports
              .record_fetch_result(url.to_string(), true);
          }
          let base = res.final_url.as_deref().unwrap_or(url);
          let effective_policy = res.response_referrer_policy.unwrap_or(referrer_policy);
          {
            let mut policies = self.stylesheet_policies.borrow_mut();
            policies.insert(url.to_string(), effective_policy);
            policies.insert(base.to_string(), effective_policy);
          }
          let decoded = decode_css_bytes(&res.bytes, res.content_type.as_deref());
          let rewritten = match absolutize_css_urls_cow(&decoded, base) {
            Ok(std::borrow::Cow::Owned(css)) => css,
            Ok(std::borrow::Cow::Borrowed(_)) | Err(_) => decoded,
          };

          if let Some(css_asset_urls) = self.css_asset_urls {
            let discovered = discover_css_urls(&rewritten, base);
            let mut set = css_asset_urls.borrow_mut();
            for url in discovered {
              record_css_url_asset_candidate(
                self.all_asset_urls,
                &mut set,
                &url,
                self.max_discovered_assets_per_page,
                self.max_discovered_assets_per_page,
              );
            }
          }

          let fetched = FetchedStylesheet::new(rewritten, res.final_url.clone());
          let mut cache = self.css_cache.borrow_mut();
          cache.insert(url.to_string(), fetched.clone());
          if let Some(final_url) = fetched.final_url.as_deref() {
            cache.insert(final_url.to_string(), fetched.clone());
          }

          Ok(fetched)
        }
        Err(err) => {
          let mut summary = self.summary.borrow_mut();
          summary.failed_imports += 1;
          summary
            .report
            .imports
            .record_fetch_result(url.to_string(), false);
          Err(err)
        }
      }
    }
  }

  struct DryRunImportLoader<'a> {
    summary: &'a RefCell<PageSummary>,
  }

  impl<'a> DryRunImportLoader<'a> {
    fn new(summary: &'a RefCell<PageSummary>) -> Self {
      Self { summary }
    }
  }

  impl CssImportLoader for DryRunImportLoader<'_> {
    fn load(&self, url: &str) -> fastrender::Result<String> {
      self
        .load_with_importer(url, None)
        .map(|fetched| fetched.css)
    }

    fn referrer_policy_for_stylesheet(&self, _url: &str) -> Option<ReferrerPolicy> {
      None
    }

    fn load_with_importer(
      &self,
      url: &str,
      _importer_url: Option<&str>,
    ) -> fastrender::Result<FetchedStylesheet> {
      self
        .summary
        .borrow_mut()
        .report
        .imports
        .record_discovered(url.to_string());
      Ok(FetchedStylesheet::new(String::new(), None))
    }
  }

  #[derive(Debug, Clone)]
  enum StylesheetTask {
    Inline(String),
    External {
      url: String,
      cors_mode: Option<CorsMode>,
      referrer_policy: Option<ReferrerPolicy>,
    },
  }

  fn stylesheet_fetch_profile(
    cors_mode: Option<CorsMode>,
  ) -> (FetchDestination, FetchCredentialsMode) {
    match cors_mode {
      None => (FetchDestination::Style, FetchCredentialsMode::Include),
      Some(CorsMode::Anonymous) => (
        FetchDestination::StyleCors,
        FetchCredentialsMode::SameOrigin,
      ),
      Some(CorsMode::UseCredentials) => {
        (FetchDestination::StyleCors, FetchCredentialsMode::Include)
      }
    }
  }

  fn stylesheet_type_is_css(type_attr: Option<&str>) -> bool {
    match type_attr {
      None => true,
      Some(value) => {
        let mime = value
          .split(';')
          .next()
          .map(trim_ascii_whitespace)
          .unwrap_or("");
        mime.is_empty() || mime.eq_ignore_ascii_case("text/css")
      }
    }
  }

  fn media_attr_allows(
    media_attr: Option<&str>,
    media_ctx: &MediaContext,
    cache: &mut MediaQueryCache,
  ) -> bool {
    match media_attr {
      None => true,
      Some(media) => {
        let trimmed = trim_ascii_whitespace(media);
        if trimmed.is_empty() {
          return true;
        }

        match MediaQuery::parse_list(trimmed) {
          Ok(list) => media_ctx.evaluate_list_with_cache(&list, Some(cache)),
          Err(_) => false,
        }
      }
    }
  }

  fn discover_dom_stylesheet_tasks(
    dom: &DomNode,
    base_url: &str,
    media_ctx: &MediaContext,
    media_query_cache: &mut MediaQueryCache,
  ) -> Vec<StylesheetTask> {
    let scoped_sources = extract_scoped_css_sources(dom);
    let toggles = runtime::runtime_toggles();
    let fetch_link_css = toggles.truthy_with_default("FASTR_FETCH_LINK_CSS", true);
    let preload_stylesheets_enabled =
      toggles.truthy_with_default("FASTR_FETCH_PRELOAD_STYLESHEETS", true);
    let modulepreload_stylesheets_enabled =
      toggles.truthy_with_default("FASTR_FETCH_MODULEPRELOAD_STYLESHEETS", false);
    let alternate_stylesheets_enabled =
      toggles.truthy_with_default("FASTR_FETCH_ALTERNATE_STYLESHEETS", true);

    let mut tasks = Vec::new();
    let mut seen_external: HashSet<(String, Option<CorsMode>)> = HashSet::new();

    let mut consider_source = |source: &StylesheetSource| match source {
      StylesheetSource::Inline(inline) => {
        if inline.disabled || !stylesheet_type_is_css(inline.type_attr.as_deref()) {
          return;
        }
        if !media_attr_allows(inline.media.as_deref(), media_ctx, media_query_cache) {
          return;
        }
        if trim_ascii_whitespace(&inline.css).is_empty() {
          return;
        }
        tasks.push(StylesheetTask::Inline(inline.css.clone()));
      }
      StylesheetSource::External(link) => {
        if !fetch_link_css {
          return;
        }
        if link.disabled
          || !link_rel_is_stylesheet_candidate(
            &link.rel,
            link.as_attr.as_deref(),
            preload_stylesheets_enabled,
            modulepreload_stylesheets_enabled,
            alternate_stylesheets_enabled,
          )
          || !stylesheet_type_is_css(link.type_attr.as_deref())
        {
          return;
        }
        if !media_attr_allows(link.media.as_deref(), media_ctx, media_query_cache) {
          return;
        }
        if trim_ascii_whitespace(&link.href).is_empty() {
          return;
        }

        let Some(stylesheet_url) = resolve_href_with_base(Some(base_url), &link.href) else {
          return;
        };
        let cors_mode = link.crossorigin;
        if seen_external.insert((stylesheet_url.clone(), cors_mode)) {
          tasks.push(StylesheetTask::External {
            url: stylesheet_url,
            cors_mode,
            referrer_policy: link.referrer_policy,
          });
        }
      }
    };

    for source in scoped_sources.document.iter() {
      consider_source(source);
    }

    let mut shadow_hosts: Vec<usize> = scoped_sources.shadows.keys().copied().collect();
    shadow_hosts.sort_unstable();
    for host in shadow_hosts {
      if let Some(sources) = scoped_sources.shadows.get(&host) {
        for source in sources {
          consider_source(source);
        }
      }
    }

    tasks
  }

  fn ordered_remote_font_face_sources(sources: &[FontFaceSource]) -> Vec<&FontFaceUrlSource> {
    let mut ordered = Vec::new();
    let mut idx = 0;
    while idx < sources.len() {
      match &sources[idx] {
        FontFaceSource::Local(_) => idx += 1,
        FontFaceSource::Url(_) => {
          let start = idx;
          while idx < sources.len() && matches!(sources[idx], FontFaceSource::Url(_)) {
            idx += 1;
          }

          // Align with browser semantics (and the renderer): preserve authored ordering and only
          // skip formats we cannot decode (notably legacy EOT and SVG).
          for source in &sources[start..idx] {
            let FontFaceSource::Url(url) = source else {
              continue;
            };
            if format_support_rank(&url.format_hints, &url.url).is_some() {
              ordered.push(url);
            }
          }
        }
      }
    }
    ordered
  }

  fn format_support_rank(hints: &[FontSourceFormat], url: &str) -> Option<usize> {
    let inferred = inferred_format_support_rank_from_url(url);
    if inferred.is_none() {
      return None;
    }
    if hints.is_empty() {
      return inferred;
    }

    let mut best: Option<usize> = None;
    for hint in hints {
      let rank = match hint {
        FontSourceFormat::Woff2 => Some(0),
        FontSourceFormat::Woff => Some(1),
        FontSourceFormat::Opentype | FontSourceFormat::Truetype | FontSourceFormat::Collection => {
          Some(2)
        }
        FontSourceFormat::Unknown(_) => Some(4),
        FontSourceFormat::EmbeddedOpenType | FontSourceFormat::Svg => None,
      };

      if let Some(rank) = rank {
        best = Some(best.map_or(rank, |current| current.min(rank)));
      }
    }

    best
  }

  fn inferred_format_support_rank_from_url(url: &str) -> Option<usize> {
    // `src:` descriptors may omit `format()`. To avoid fetching formats we cannot decode (notably
    // legacy EOT), infer a best-effort rank from the URL suffix / data: MIME.
    //
    // If we can't infer anything, return a low priority "unknown" rank so we still attempt to load
    // it (some endpoints omit extensions).
    if is_data_url(url) {
      let after_prefix = url.get("data:".len()..).unwrap_or("");
      let meta = after_prefix
        .split_once(',')
        .map(|(m, _)| m)
        .unwrap_or(after_prefix);
      let mime = trim_ascii_whitespace(meta.split(';').next().unwrap_or(""));
      if !mime.is_empty() {
        let mime = mime.to_ascii_lowercase();
        if mime.contains("woff2") {
          return Some(0);
        }
        if mime.contains("woff") {
          return Some(1);
        }
        if mime.contains("opentype")
          || mime.contains("otf")
          || mime.contains("truetype")
          || mime.contains("ttf")
          || mime.contains("collection")
          || mime.contains("ttc")
        {
          return Some(2);
        }
        if mime.contains("embedded-opentype") || mime.contains("eot") || mime.contains("svg") {
          return None;
        }
      }
    }

    let lower = url.to_ascii_lowercase();
    let lower = lower
      .split_once('#')
      .map(|(before, _)| before)
      .unwrap_or(&lower);
    let lower = lower
      .split_once('?')
      .map(|(before, _)| before)
      .unwrap_or(lower);

    if lower.ends_with(".woff2") {
      return Some(0);
    }
    if lower.ends_with(".woff") {
      return Some(1);
    }
    if lower.ends_with(".ttf")
      || lower.ends_with(".otf")
      || lower.ends_with(".ttc")
      || lower.ends_with(".opentype")
      || lower.ends_with(".truetype")
      || lower.ends_with(".collection")
      || lower.ends_with(".otc")
    {
      return Some(2);
    }
    if lower.ends_with(".eot") || lower.ends_with(".svg") || lower.ends_with(".svgz") {
      return None;
    }

    Some(3)
  }

  fn prefetch_fonts_from_stylesheet(
    fetcher: &dyn ResourceFetcher,
    referrer_url: &str,
    css_base_url: &str,
    client_origin: Option<&DocumentOrigin>,
    referrer_policy: ReferrerPolicy,
    sheet: &StyleSheet,
    media_ctx: &MediaContext,
    media_query_cache: &mut MediaQueryCache,
    seen_fonts: &mut HashMap<String, bool>,
    summary: &mut PageSummary,
    dry_run: bool,
  ) {
    for face in sheet.collect_font_face_rules_with_cache(media_ctx, Some(media_query_cache)) {
      let stylesheet_url = face
        .source_stylesheet_url
        .as_deref()
        .unwrap_or(referrer_url);
      let base_url = face
        .source_stylesheet_url
        .as_deref()
        .unwrap_or(css_base_url);
      let effective_referrer_policy = face.source_referrer_policy.unwrap_or(referrer_policy);
      let mut candidates: Vec<String> = Vec::new();
      for url_source in ordered_remote_font_face_sources(&face.sources) {
        let Some(resolved) = resolve_href(base_url, &url_source.url) else {
          continue;
        };
        if is_data_url(&resolved) {
          continue;
        }
        summary.report.fonts.record_discovered(resolved.clone());
        candidates.push(resolved);
      }

      if dry_run {
        continue;
      }

      for resolved in candidates {
        match seen_fonts.get(&resolved).copied() {
          Some(true) => break,
          Some(false) => continue,
          None => {}
        }

        let mut request = FetchRequest::new(&resolved, FetchDestination::Font)
          .with_referrer_url(stylesheet_url)
          .with_referrer_policy(effective_referrer_policy)
          .with_credentials_mode(FetchCredentialsMode::SameOrigin);
        if let Some(origin) = client_origin {
          request = request.with_client_origin(origin);
        }
        let success = match fetcher.fetch_with_request(request) {
          Ok(res) => ensure_http_success(&res, &resolved)
            .and_then(|()| ensure_font_mime_sane(&res, &resolved))
            .is_ok(),
          Err(_) => false,
        };

        seen_fonts.insert(resolved.clone(), success);
        summary
          .report
          .fonts
          .record_fetch_result(resolved.clone(), success);
        if success {
          summary.fetched_fonts += 1;
          break;
        }
        summary.failed_fonts += 1;
      }
    }
  }

  fn prefetch_assets_for_html(
    stem: &str,
    _document_url: &str,
    html: &str,
    base_hint: &str,
    base_url: &str,
    referrer_policy: ReferrerPolicy,
    fetcher: &Arc<dyn ResourceFetcher>,
    media_ctx: &MediaContext,
    opts: PrefetchOptions,
  ) -> PageSummary {
    let document_origin = origin_from_url(base_hint);
    let summary = RefCell::new(PageSummary {
      stem: stem.to_string(),
      ..PageSummary::default()
    });

    let dom = parse_html(html).ok();

    let mut media_query_cache = MediaQueryCache::default();

    let mut tasks: Vec<StylesheetTask> = match dom.as_ref() {
      Some(dom) => {
        let tasks = discover_dom_stylesheet_tasks(dom, base_url, media_ctx, &mut media_query_cache);
        if tasks.is_empty() {
          // Pages that load their primary stylesheet dynamically (without emitting `<link rel="stylesheet">`
          // or `<style>`) are still best-effort handled by scanning the raw HTML for `.css`-looking
          // substrings.
          let mut out = Vec::new();
          let mut seen = HashSet::new();
          if let Ok(urls) = extract_embedded_css_urls(html, base_url) {
            for url in urls {
              if seen.insert(url.clone()) {
                out.push(StylesheetTask::External {
                  url,
                  cors_mode: None,
                  referrer_policy: None,
                });
              }
            }
          }
          out
        } else {
          tasks
        }
      }
      None => {
        // DOM parse failed; fall back to the string-based extraction used by older versions.
        let mut out = Vec::new();
        let mut seen_tasks: HashSet<(String, Option<CorsMode>)> = HashSet::new();
        let mut seen_urls: HashSet<String> = HashSet::new();
        if let Ok(candidates) = extract_css_links_with_meta(html, base_url, media_ctx.media_type) {
          for candidate in candidates {
            let cors_mode = candidate.crossorigin;
            let url = candidate.url;
            seen_urls.insert(url.clone());
            if seen_tasks.insert((url.clone(), cors_mode)) {
              out.push(StylesheetTask::External {
                url,
                cors_mode,
                referrer_policy: candidate.referrer_policy,
              });
            }
          }
        }
        if let Ok(urls) = extract_embedded_css_urls(html, base_url) {
          for url in urls {
            if seen_urls.insert(url.clone()) {
              out.push(StylesheetTask::External {
                url,
                cors_mode: None,
                referrer_policy: None,
              });
            }
          }
        }
        out
      }
    };

    {
      let mut summary = summary.borrow_mut();
      summary.discovered_css = tasks.len();
      for task in &tasks {
        if let StylesheetTask::External { url, .. } = task {
          summary.report.css.record_discovered(url.clone());
        }
      }
    }
    if tasks.is_empty()
      && !(opts.prefetch_images
        || opts.prefetch_media
        || opts.prefetch_scripts
        || opts.prefetch_icons
        || opts.prefetch_video_posters
        || opts.prefetch_iframes
        || opts.prefetch_embeds
        || opts.prefetch_css_url_assets)
    {
      return summary.into_inner();
    }

    // Track every unique discovered URL across all enabled discovery classes so
    // `--max-discovered-assets-per-page` caps total work, and so the same URL
    // is not fetched multiple times via different discovery paths.
    let all_asset_urls = RefCell::new(BTreeSet::<String>::new());

    let mut image_urls: BTreeSet<String> = BTreeSet::new();
    let mut cors_image_urls: BTreeMap<String, CrossOriginAttribute> = BTreeMap::new();
    let mut media_urls: BTreeMap<String, (FetchDestination, CrossOriginAttribute)> =
      BTreeMap::new();
    let mut script_urls: BTreeSet<String> = BTreeSet::new();
    let mut cors_script_urls: BTreeMap<String, CorsMode> = BTreeMap::new();
    let mut document_urls: BTreeSet<String> = BTreeSet::new();

    if opts.prefetch_images {
      if let Some(dom) = dom.as_ref() {
        // Prefer renderer-aligned selection so we prefetch the same candidate that paint would
        // request, instead of every `srcset` candidate.
        let selection_ctx = ImageSelectionContext {
          device_pixel_ratio: media_ctx.device_pixel_ratio,
          slot_width: None,
          viewport: Some(Size::new(
            media_ctx.viewport_width,
            media_ctx.viewport_height,
          )),
          media_context: Some(media_ctx),
          font_size: Some(media_ctx.base_font_size),
          root_font_size: Some(media_ctx.base_font_size),
          base_url: Some(base_url),
        };
        let discovery = discover_image_prefetch_requests(dom, selection_ctx, opts.image_limits);

        let max_urls = if opts.max_discovered_assets_per_page == 0 {
          usize::MAX
        } else {
          opts.max_discovered_assets_per_page
        };
        for req in discovery.requests {
          if image_urls.len() + cors_image_urls.len() >= max_urls {
            break;
          }
          match req.crossorigin {
            CrossOriginAttribute::None => record_image_candidate(
              &all_asset_urls,
              &mut image_urls,
              &req.url,
              opts.max_discovered_assets_per_page,
              max_urls,
            ),
            crossorigin => record_cors_image_candidate(
              &all_asset_urls,
              &mut cors_image_urls,
              &req.url,
              crossorigin,
              opts.max_discovered_assets_per_page,
              max_urls,
            ),
          };
        }
      }
    }

    if opts.prefetch_icons || (opts.prefetch_images && dom.is_some()) {
      if let Some(dom) = dom.as_ref() {
        record_icon_candidates(
          &all_asset_urls,
          dom,
          base_url,
          &mut image_urls,
          opts.max_discovered_assets_per_page,
        );
      } else if opts.prefetch_icons {
        record_icon_candidates_from_html(
          &all_asset_urls,
          html,
          base_url,
          &mut image_urls,
          opts.max_discovered_assets_per_page,
        );
      }
    }

    if opts.prefetch_images {
      if let Some(dom) = dom.as_ref() {
        record_manifest_candidates(
          &all_asset_urls,
          dom,
          base_url,
          &mut image_urls,
          opts.max_discovered_assets_per_page,
        );
      }
    }

    if opts.prefetch_scripts {
      if let Some(dom) = dom.as_ref() {
        record_script_candidates(
          &all_asset_urls,
          dom,
          base_url,
          &mut script_urls,
          &mut cors_script_urls,
          opts.max_discovered_assets_per_page,
        );
      }
    }

    if opts.prefetch_images || opts.prefetch_video_posters {
      if let Some(dom) = dom.as_ref() {
        record_video_poster_candidates(
          &all_asset_urls,
          dom,
          base_url,
          &mut image_urls,
          &mut cors_image_urls,
          opts.max_discovered_assets_per_page,
        );
      } else {
        record_video_poster_candidates_from_html(
          &all_asset_urls,
          html,
          base_url,
          &mut image_urls,
          &mut cors_image_urls,
          opts.max_discovered_assets_per_page,
        );
      }
    }

    if opts.prefetch_media {
      if let Some(dom) = dom.as_ref() {
        record_media_source_candidates(
          &all_asset_urls,
          dom,
          base_url,
          &mut media_urls,
          opts.max_discovered_assets_per_page,
        );
      } else {
        record_media_source_candidates_from_html(
          &all_asset_urls,
          html,
          base_url,
          &mut media_urls,
          opts.max_discovered_assets_per_page,
        );
      }
    }

    if opts.prefetch_iframes {
      if let Some(dom) = dom.as_ref() {
        record_iframe_document_candidates(
          &all_asset_urls,
          dom,
          base_url,
          &mut document_urls,
          opts.max_discovered_assets_per_page,
        );
      } else {
        record_iframe_document_candidates_from_html(
          &all_asset_urls,
          html,
          base_url,
          &mut document_urls,
          opts.max_discovered_assets_per_page,
        );
      }
    }

    // Fallback when the DOM parse failed: keep behavior best-effort and bounded using the
    // same overall caps, but do not attempt to prefetch unlimited `srcset` candidates.
    if opts.prefetch_images && dom.is_none() {
      let html_assets = discover_html_asset_urls(html, base_url);
      let mut max_urls = opts
        .image_limits
        .max_image_elements
        .saturating_mul(opts.image_limits.max_urls_per_element);
      if opts.max_discovered_assets_per_page != 0 {
        max_urls = max_urls.min(opts.max_discovered_assets_per_page);
      }
      for url in html_assets.images {
        if image_urls.len() >= max_urls {
          break;
        }
        record_image_candidate(
          &all_asset_urls,
          &mut image_urls,
          &url,
          opts.max_discovered_assets_per_page,
          max_urls,
        );
      }
    }

    if opts.prefetch_embeds {
      if let Some(dom) = dom.as_ref() {
        record_embed_document_candidates(
          &all_asset_urls,
          dom,
          base_url,
          &mut document_urls,
          opts.max_discovered_assets_per_page,
        );
      } else {
        record_embed_document_candidates_from_html(
          &all_asset_urls,
          html,
          base_url,
          &mut document_urls,
          opts.max_discovered_assets_per_page,
        );
      }
    }

    let css_asset_urls = RefCell::new(BTreeSet::<String>::new());
    if opts.prefetch_css_url_assets {
      let mut set = css_asset_urls.borrow_mut();
      for chunk in extract_inline_css_chunks(html) {
        for url in discover_css_urls(&chunk, base_url) {
          record_css_url_asset_candidate(
            &all_asset_urls,
            &mut set,
            &url,
            opts.max_discovered_assets_per_page,
            opts.max_discovered_assets_per_page,
          );
        }
      }
    }

    if !tasks.is_empty() {
      // Process external stylesheets in sorted order for more reproducible network/cache behavior.
      fn cors_mode_sort_key(mode: Option<CorsMode>) -> u8 {
        match mode {
          None => 0,
          Some(CorsMode::Anonymous) => 1,
          Some(CorsMode::UseCredentials) => 2,
        }
      }
      tasks.sort_by(|a, b| match (a, b) {
        (StylesheetTask::Inline(_), StylesheetTask::External { .. }) => std::cmp::Ordering::Less,
        (StylesheetTask::External { .. }, StylesheetTask::Inline(_)) => std::cmp::Ordering::Greater,
        (StylesheetTask::Inline(_), StylesheetTask::Inline(_)) => std::cmp::Ordering::Equal,
        (
          StylesheetTask::External {
            url: a_url,
            cors_mode: a_cors,
            ..
          },
          StylesheetTask::External {
            url: b_url,
            cors_mode: b_cors,
            ..
          },
        ) => {
          let by_url = a_url.cmp(b_url);
          if by_url != std::cmp::Ordering::Equal {
            return by_url;
          }
          cors_mode_sort_key(*a_cors).cmp(&cors_mode_sort_key(*b_cors))
        }
      });

      let css_asset_urls_ref = if opts.prefetch_css_url_assets {
        Some(&css_asset_urls)
      } else {
        None
      };
      let mut seen_fonts: HashMap<String, bool> = HashMap::new();

      for task in tasks {
        match task {
          StylesheetTask::Inline(css) => {
            if opts.prefetch_css_url_assets {
              let scan_css = match absolutize_css_urls_cow(&css, base_url) {
                Ok(css) => css,
                Err(_) => std::borrow::Cow::Borrowed(css.as_str()),
              };
              let discovered = discover_css_urls(scan_css.as_ref(), base_url);
              {
                let mut set = css_asset_urls.borrow_mut();
                for url in discovered {
                  record_css_url_asset_candidate(
                    &all_asset_urls,
                    &mut set,
                    &url,
                    opts.max_discovered_assets_per_page,
                    opts.max_discovered_assets_per_page,
                  );
                }
              }
            }

            let sheet: StyleSheet = match parse_stylesheet(&css) {
              Ok(sheet) => sheet,
              Err(_) => continue,
            };

            let resolved = if sheet.contains_imports() {
              if opts.dry_run {
                let import_loader = DryRunImportLoader::new(&summary);
                let _ = sheet.resolve_imports_with_cache(
                  &import_loader,
                  Some(base_url),
                  media_ctx,
                  Some(&mut media_query_cache),
                );
                sheet
              } else {
                let (destination, credentials_mode) = stylesheet_fetch_profile(None);
                let import_loader = PrefetchImportLoader::new(
                  fetcher.as_ref(),
                  base_hint,
                  Some(base_url),
                  document_origin.as_ref(),
                  destination,
                  credentials_mode,
                  referrer_policy,
                  &summary,
                  &all_asset_urls,
                  css_asset_urls_ref,
                  opts.max_discovered_assets_per_page,
                );
                match sheet.resolve_imports_owned_with_cache(
                  &import_loader,
                  Some(base_url),
                  media_ctx,
                  Some(&mut media_query_cache),
                ) {
                  Ok(sheet) => sheet,
                  Err(_) => match parse_stylesheet(&css) {
                    Ok(sheet) => sheet,
                    Err(_) => continue,
                  },
                }
              }
            } else {
              sheet
            };

            if opts.prefetch_fonts {
              let mut summary = summary.borrow_mut();
              prefetch_fonts_from_stylesheet(
                fetcher.as_ref(),
                base_hint,
                base_url,
                document_origin.as_ref(),
                referrer_policy,
                &resolved,
                media_ctx,
                &mut media_query_cache,
                &mut seen_fonts,
                &mut summary,
                opts.dry_run,
              );
            }
          }
          StylesheetTask::External {
            url: css_url,
            cors_mode,
            referrer_policy: link_referrer_policy,
          } => {
            if opts.dry_run {
              continue;
            }
            let effective_referrer_policy = link_referrer_policy.unwrap_or(referrer_policy);
            let (destination, credentials_mode) = stylesheet_fetch_profile(cors_mode);
            let mut request = FetchRequest::new(css_url.as_str(), destination)
              .with_referrer_url(base_hint)
              .with_referrer_policy(effective_referrer_policy)
              .with_credentials_mode(credentials_mode);
            if let Some(origin) = document_origin.as_ref() {
              request = request.with_client_origin(origin);
            }
            match fetcher.fetch_with_request(request) {
              Ok(res) => {
                if ensure_http_success(&res, &css_url)
                  .and_then(|()| ensure_stylesheet_mime_sane(&res, &css_url))
                  .is_err()
                {
                  let mut summary = summary.borrow_mut();
                  summary.failed_css += 1;
                  summary.report.css.record_fetch_result(css_url, false);
                  continue;
                }

                {
                  let mut summary = summary.borrow_mut();
                  summary.fetched_css += 1;
                  summary
                    .report
                    .css
                    .record_fetch_result(css_url.clone(), true);
                }
                let sheet_base = res.final_url.as_deref().unwrap_or(&css_url);
                let mut css_text = decode_css_bytes(&res.bytes, res.content_type.as_deref());
                if let Ok(std::borrow::Cow::Owned(rewritten)) =
                  absolutize_css_urls_cow(&css_text, sheet_base)
                {
                  css_text = rewritten;
                }

                if opts.prefetch_css_url_assets {
                  let discovered = discover_css_urls(&css_text, sheet_base);
                  {
                    let mut set = css_asset_urls.borrow_mut();
                    for url in discovered {
                      record_css_url_asset_candidate(
                        &all_asset_urls,
                        &mut set,
                        &url,
                        opts.max_discovered_assets_per_page,
                        opts.max_discovered_assets_per_page,
                      );
                    }
                  }
                }

                let stylesheet_referrer_policy = res
                  .response_referrer_policy
                  .unwrap_or(effective_referrer_policy);

                let sheet: StyleSheet = match parse_stylesheet(&css_text) {
                  Ok(sheet) => sheet,
                  Err(_) => continue,
                };

                let resolved = if sheet.contains_imports() {
                  let import_loader = PrefetchImportLoader::new(
                    fetcher.as_ref(),
                    base_hint,
                    None,
                    document_origin.as_ref(),
                    destination,
                    credentials_mode,
                    stylesheet_referrer_policy,
                    &summary,
                    &all_asset_urls,
                    css_asset_urls_ref,
                    opts.max_discovered_assets_per_page,
                  );
                  match sheet.resolve_imports_owned_with_cache(
                    &import_loader,
                    Some(sheet_base),
                    media_ctx,
                    Some(&mut media_query_cache),
                  ) {
                    Ok(sheet) => sheet,
                    Err(_) => match parse_stylesheet(&css_text) {
                      Ok(sheet) => sheet,
                      Err(_) => continue,
                    },
                  }
                } else {
                  sheet
                };

                if opts.prefetch_fonts {
                  let mut summary = summary.borrow_mut();
                  prefetch_fonts_from_stylesheet(
                    fetcher.as_ref(),
                    sheet_base,
                    sheet_base,
                    document_origin.as_ref(),
                    stylesheet_referrer_policy,
                    &resolved,
                    media_ctx,
                    &mut media_query_cache,
                    &mut seen_fonts,
                    &mut summary,
                    false,
                  );
                }
              }
              Err(_) => {
                let mut summary = summary.borrow_mut();
                summary.failed_css += 1;
                summary.report.css.record_fetch_result(css_url, false);
              }
            }
          }
        }
      }
    }

    let css_asset_urls = css_asset_urls.into_inner();

    {
      let mut summary = summary.borrow_mut();
      summary.discovered_images = image_urls.len()
        + cors_image_urls
          .keys()
          .filter(|url| !image_urls.contains(*url))
          .count();
      summary.discovered_media = media_urls.len();
      summary.discovered_scripts = script_urls.len()
        + cors_script_urls
          .keys()
          .filter(|url| !script_urls.contains(*url))
          .count();
      summary.discovered_documents = document_urls.len();
      summary.discovered_css_assets = css_asset_urls.len();

      summary
        .report
        .images
        .discovered
        .extend(image_urls.iter().cloned());
      summary
        .report
        .images
        .discovered
        .extend(cors_image_urls.keys().cloned());
      summary
        .report
        .media
        .discovered
        .extend(media_urls.keys().cloned());
      summary
        .report
        .scripts
        .discovered
        .extend(script_urls.iter().cloned());
      summary
        .report
        .scripts
        .discovered
        .extend(cors_script_urls.keys().cloned());
      summary
        .report
        .documents
        .discovered
        .extend(document_urls.iter().cloned());
      summary
        .report
        .css_url_assets
        .discovered
        .extend(css_asset_urls.iter().cloned());
    }

    if !opts.dry_run && (opts.prefetch_images || opts.prefetch_icons || opts.prefetch_video_posters)
    {
      let image_cache = ImageCache::with_fetcher(Arc::clone(fetcher));
      let mut summary = summary.borrow_mut();
      for url in &image_urls {
        let success = match fetcher.fetch_with_request(
          FetchRequest::new(url.as_str(), FetchDestination::Image)
            .with_referrer_url(base_hint)
            .with_referrer_policy(referrer_policy),
        ) {
          Ok(res) => {
            if ensure_http_success(&res, url)
              .and_then(|()| ensure_image_mime_sane(&res, url))
              .is_ok()
            {
              summary.fetched_images += 1;
              // Best-effort: probe now (outside the 5s render deadline) and persist intrinsic sizing
              // metadata into the disk cache so subsequent renders can avoid repeating image header
              // parsing during box-tree construction.
              let _ = image_cache.probe(url.as_str());
              true
            } else {
              summary.failed_images += 1;
              false
            }
          }
          Err(_) => {
            summary.failed_images += 1;
            false
          }
        };
        summary
          .report
          .images
          .record_fetch_result(url.clone(), success);
      }

      let plain_fetched = image_urls
        .iter()
        .chain(css_asset_urls.iter())
        .cloned()
        .collect::<HashSet<_>>();
      for (url, crossorigin) in &cors_image_urls {
        if !plain_fetched.contains(url) {
          let success = match fetcher.fetch_with_request(
            FetchRequest::new(url.as_str(), FetchDestination::Image)
              .with_referrer_url(base_hint)
              .with_referrer_policy(referrer_policy),
          ) {
            Ok(res) => {
              if ensure_http_success(&res, url)
                .and_then(|()| ensure_image_mime_sane(&res, url))
                .is_ok()
              {
                summary.fetched_images += 1;
                let _ = image_cache.probe(url.as_str());
                true
              } else {
                summary.failed_images += 1;
                false
              }
            }
            Err(_) => {
              summary.failed_images += 1;
              false
            }
          };
          summary
            .report
            .images
            .record_fetch_result(url.clone(), success);
        }

        let credentials_mode = match crossorigin {
          CrossOriginAttribute::None => FetchCredentialsMode::Include,
          CrossOriginAttribute::Anonymous => CorsMode::Anonymous.credentials_mode(),
          CrossOriginAttribute::UseCredentials => CorsMode::UseCredentials.credentials_mode(),
        };
        let success = match fetcher.fetch_with_request(
          FetchRequest::new(url.as_str(), FetchDestination::ImageCors)
            .with_referrer_url(base_hint)
            .with_referrer_policy(referrer_policy)
            .with_credentials_mode(credentials_mode),
        ) {
          Ok(res) => {
            if ensure_http_success(&res, url)
              .and_then(|()| ensure_image_mime_sane(&res, url))
              .is_ok()
            {
              summary.fetched_images += 1;
              let _ = image_cache.probe_with_crossorigin(url.as_str(), *crossorigin);
              true
            } else {
              summary.failed_images += 1;
              false
            }
          }
          Err(_) => {
            summary.failed_images += 1;
            false
          }
        };
        summary
          .report
          .images
          .record_fetch_result(url.clone(), success);
      }
    }

    if !opts.dry_run && opts.prefetch_media {
      let per_file_cap = opts.max_media_bytes_per_file;
      let page_cap = opts.max_media_bytes_per_page;
      let mut remaining_budget = page_cap;

      let mut summary = summary.borrow_mut();
      for (url, profile) in &media_urls {
        let (kind, crossorigin) = *profile;
        let destination = match kind {
          FetchDestination::Video => match crossorigin {
            CrossOriginAttribute::None => FetchDestination::Video,
            _ => FetchDestination::VideoCors,
          },
          FetchDestination::Audio => match crossorigin {
            CrossOriginAttribute::None => FetchDestination::Audio,
            _ => FetchDestination::AudioCors,
          },
          other => other,
        };
        let credentials_mode = match crossorigin {
          CrossOriginAttribute::None => FetchCredentialsMode::Include,
          CrossOriginAttribute::Anonymous => CorsMode::Anonymous.credentials_mode(),
          CrossOriginAttribute::UseCredentials => CorsMode::UseCredentials.credentials_mode(),
        };
        if page_cap != 0 && remaining_budget == 0 {
          summary.skipped_media += 1;
          summary.report.media.record_fetch_result(url.clone(), false);
          eprintln!(
            "Skipping media {} (no remaining page media budget: max_media_bytes_per_page={})",
            url, page_cap
          );
          continue;
        }

        let remaining_before = remaining_budget;
        let mut max_allowed = u64::MAX;
        if per_file_cap != 0 {
          max_allowed = max_allowed.min(per_file_cap);
        }
        if page_cap != 0 {
          max_allowed = max_allowed.min(remaining_budget);
        }

        // When both limits are disabled, do a single fetch just like other asset types.
        if max_allowed == u64::MAX {
          let mut request = FetchRequest::new(url.as_str(), destination)
            .with_referrer_url(base_hint)
            .with_referrer_policy(referrer_policy)
            .with_credentials_mode(credentials_mode);
          if let Some(origin) = document_origin.as_ref() {
            request = request.with_client_origin(origin);
          }
          let success = match fetcher.fetch_with_request(request) {
            Ok(res) => ensure_http_success(&res, url)
              .and_then(|()| ensure_media_mime_sane(&res, url))
              .is_ok(),
            Err(_) => false,
          };
          if success {
            summary.fetched_media += 1;
          } else {
            summary.failed_media += 1;
          }
          summary
            .report
            .media
            .record_fetch_result(url.clone(), success);
          continue;
        }

        // Clamp the probe to `usize::MAX` to keep allocations bounded on 32-bit targets.
        let probe_max_bytes = max_allowed.saturating_add(1).min(usize::MAX as u64) as usize;

        let mut probe_request = FetchRequest::new(url.as_str(), destination)
          .with_referrer_url(base_hint)
          .with_referrer_policy(referrer_policy)
          .with_credentials_mode(credentials_mode);
        if let Some(origin) = document_origin.as_ref() {
          probe_request = probe_request.with_client_origin(origin);
        }

        let probe_size: u64 =
          match fetcher.fetch_partial_with_request(probe_request, probe_max_bytes) {
            Ok(res) => {
              if ensure_http_success(&res, url)
                .and_then(|()| ensure_media_mime_sane(&res, url))
                .is_ok()
              {
                res.bytes.len() as u64
              } else {
                summary.failed_media += 1;
                summary.report.media.record_fetch_result(url.clone(), false);
                continue;
              }
            }
            Err(_) => {
              summary.failed_media += 1;
              summary.report.media.record_fetch_result(url.clone(), false);
              continue;
            }
          };

        if probe_size > max_allowed {
          summary.skipped_media += 1;
          summary.report.media.record_fetch_result(url.clone(), false);

          let mut reasons: Vec<String> = Vec::new();
          if per_file_cap != 0 && probe_size > per_file_cap {
            reasons.push(format!("max_media_bytes_per_file={per_file_cap}"));
          }
          if page_cap != 0 && probe_size > remaining_before {
            reasons.push(format!("remaining_page_budget={remaining_before}"));
          }
          if reasons.is_empty() {
            reasons.push(format!("max_allowed={max_allowed}"));
          }
          eprintln!(
            "Skipping media {} (size exceeds budget; {})",
            url,
            reasons.join(", ")
          );
          continue;
        }

        if page_cap != 0 {
          remaining_budget = remaining_budget.saturating_sub(probe_size);
        }

        let mut request = FetchRequest::new(url.as_str(), destination)
          .with_referrer_url(base_hint)
          .with_referrer_policy(referrer_policy)
          .with_credentials_mode(credentials_mode);
        if let Some(origin) = document_origin.as_ref() {
          request = request.with_client_origin(origin);
        }
        let success = match fetcher.fetch_with_request(request) {
          Ok(res) => ensure_http_success(&res, url)
            .and_then(|()| ensure_media_mime_sane(&res, url))
            .is_ok(),
          Err(_) => false,
        };
        if success {
          summary.fetched_media += 1;
        } else {
          summary.failed_media += 1;
          // Restore budget so subsequent media URLs are still eligible when this fetch failed.
          if page_cap != 0 {
            remaining_budget = remaining_budget.saturating_add(probe_size);
          }
        }
        summary
          .report
          .media
          .record_fetch_result(url.clone(), success);
      }
    }

    if !opts.dry_run && opts.prefetch_scripts {
      let mut summary = summary.borrow_mut();
      for url in &script_urls {
        let mut request = FetchRequest::new(url.as_str(), FetchDestination::Script)
          .with_referrer_url(base_hint)
          .with_referrer_policy(referrer_policy);
        if let Some(origin) = document_origin.as_ref() {
          request = request.with_client_origin(origin);
        }
        let success = match fetcher.fetch_with_request(request) {
          Ok(res) => ensure_http_success(&res, url)
            .and_then(|()| ensure_script_mime_sane(&res, url))
            .is_ok(),
          Err(_) => false,
        };
        if success {
          summary.fetched_scripts += 1;
        } else {
          summary.failed_scripts += 1;
        }
        summary
          .report
          .scripts
          .record_fetch_result(url.clone(), success);
      }

      let plain_fetched = script_urls.iter().cloned().collect::<HashSet<_>>();
      for (url, cors_mode) in &cors_script_urls {
        if !plain_fetched.contains(url) {
          let mut request = FetchRequest::new(url.as_str(), FetchDestination::Script)
            .with_referrer_url(base_hint)
            .with_referrer_policy(referrer_policy);
          if let Some(origin) = document_origin.as_ref() {
            request = request.with_client_origin(origin);
          }
          let success = match fetcher.fetch_with_request(request) {
            Ok(res) => ensure_http_success(&res, url)
              .and_then(|()| ensure_script_mime_sane(&res, url))
              .is_ok(),
            Err(_) => false,
          };
          if success {
            summary.fetched_scripts += 1;
          } else {
            summary.failed_scripts += 1;
          }
          summary
            .report
            .scripts
            .record_fetch_result(url.clone(), success);
        }

        let mut request = FetchRequest::new(url.as_str(), FetchDestination::ScriptCors)
          .with_referrer_url(base_hint)
          .with_referrer_policy(referrer_policy)
          .with_credentials_mode(cors_mode.credentials_mode());
        if let Some(origin) = document_origin.as_ref() {
          request = request.with_client_origin(origin);
        }
        let success = match fetcher.fetch_with_request(request) {
          Ok(res) => ensure_http_success(&res, url)
            .and_then(|()| ensure_script_mime_sane(&res, url))
            .is_ok(),
          Err(_) => false,
        };
        if success {
          summary.fetched_scripts += 1;
        } else {
          summary.failed_scripts += 1;
        }
        summary
          .report
          .scripts
          .record_fetch_result(url.clone(), success);
      }
    }

    if !opts.dry_run && (opts.prefetch_iframes || opts.prefetch_embeds) {
      let mut fetched_docs: Vec<(String, FetchedResource)> = Vec::new();
      // Embedded documents (iframes/embeds/objects) are fetched as subframe navigations, which use
      // a distinct `Sec-Fetch-Dest: iframe` profile (and a distinct cache kind).
      let fetch_destination = FetchDestination::Iframe;
      {
        let mut summary = summary.borrow_mut();
        for url in &document_urls {
          let success = match fetcher.fetch_with_request(
            FetchRequest::new(url.as_str(), fetch_destination)
              .with_referrer_url(base_hint)
              .with_referrer_policy(referrer_policy),
          ) {
            Ok(res) => {
              let is_html = ensure_http_success(&res, url).is_ok()
                && !res.bytes.is_empty()
                && looks_like_html_document(&res, url);
              if is_html {
                summary.fetched_documents += 1;
                fetched_docs.push((url.clone(), res));
                true
              } else {
                summary.failed_documents += 1;
                false
              }
            }
            Err(_) => {
              summary.failed_documents += 1;
              false
            }
          };
          summary
            .report
            .documents
            .record_fetch_result(url.clone(), success);
        }
      }

      // Best-effort: when iframe/embed prefetching is enabled, also warm their subresource
      // dependencies (stylesheets/@imports/fonts, and HTML images when enabled). This reduces
      // render-deadline network fetches because iframes are rendered as nested documents.
      let nested_opts = PrefetchOptions {
        prefetch_iframes: false,
        prefetch_embeds: false,
        ..opts
      };
      for (url, res) in fetched_docs {
        let base_hint = res.final_url.as_deref().unwrap_or(&url);
        let doc = decode_html_resource(&res, base_hint);
        let nested_summary = prefetch_assets_for_html(
          &format!("{stem}::document:{url}"),
          base_hint,
          &doc.html,
          &doc.base_hint,
          &doc.base_url,
          doc.referrer_policy,
          fetcher,
          media_ctx,
          nested_opts,
        );
        merge_page_summary(&mut summary.borrow_mut(), nested_summary);
      }
    }

    if !opts.dry_run && opts.prefetch_css_url_assets {
      let mut summary = summary.borrow_mut();
      for url in &css_asset_urls {
        let success = match fetcher.fetch_with_request(
          FetchRequest::new(url.as_str(), FetchDestination::Image)
            .with_referrer_url(base_hint)
            .with_referrer_policy(referrer_policy),
        ) {
          Ok(res) => {
            if ensure_http_success(&res, url)
              .and_then(|()| ensure_image_mime_sane(&res, url))
              .is_ok()
            {
              summary.fetched_css_assets += 1;
              true
            } else {
              summary.failed_css_assets += 1;
              false
            }
          }
          Err(_) => {
            summary.failed_css_assets += 1;
            false
          }
        };
        summary
          .report
          .css_url_assets
          .record_fetch_result(url.clone(), success);
      }
    }

    summary.into_inner()
  }

  fn prefetch_page(
    entry: &PagesetEntry,
    fetcher: &Arc<dyn ResourceFetcher>,
    media_ctx: &MediaContext,
    opts: PrefetchOptions,
  ) -> PageSummary {
    let cache_path = cache_html_path(&entry.cache_stem);
    if !cache_path.exists() {
      return PageSummary {
        stem: entry.cache_stem.clone(),
        skipped: true,
        ..PageSummary::default()
      };
    }

    let cached = match read_cached_document(&cache_path) {
      Ok(doc) => doc,
      Err(_) => {
        return PageSummary {
          stem: entry.cache_stem.clone(),
          skipped: true,
          ..PageSummary::default()
        };
      }
    };

    prefetch_assets_for_html(
      &entry.cache_stem,
      entry.url.as_str(),
      &cached.document.html,
      &cached.document.base_hint,
      &cached.document.base_url,
      cached.document.referrer_policy,
      fetcher,
      media_ctx,
      opts,
    )
  }

  #[cfg(test)]
  mod tests {
    use super::*;
    use fastrender::resource::{CachingFetcherConfig, DiskCacheConfig, FetchedResource};
    use serde_json::Value;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    #[derive(Clone)]
    struct PanicFetcher;

    impl ResourceFetcher for PanicFetcher {
      fn fetch(&self, _url: &str) -> fastrender::Result<FetchedResource> {
        panic!("network fetch should not be called for disk cache hits");
      }
    }

    #[test]
    fn capabilities_json_includes_expected_keys() {
      let json = crate::capabilities_json(true);
      let parsed: Value = serde_json::from_str(&json).expect("capabilities JSON should parse");

      assert_eq!(
        parsed.get("name").and_then(Value::as_str),
        Some("prefetch_assets")
      );
      assert_eq!(
        parsed.get("disk_cache_feature").and_then(Value::as_bool),
        Some(true)
      );

      let flags = parsed
        .get("flags")
        .and_then(Value::as_object)
        .expect("capabilities should include flags object");
      for key in [
        "prefetch_fonts",
        "prefetch_images",
        "prefetch_scripts",
        "prefetch_iframes",
        "prefetch_embeds",
        "prefetch_icons",
        "prefetch_video_posters",
        "prefetch_css_url_assets",
        "max_discovered_assets_per_page",
        "max_images_per_page",
        "max_image_urls_per_element",
        "report_json",
        "report_per_page_dir",
        "max_report_urls_per_kind",
        "dry_run",
      ] {
        assert!(
          flags.get(key).and_then(Value::as_bool).is_some(),
          "capabilities should include boolean key flags.{key}"
        );
      }
    }

    #[test]
    fn non_ascii_whitespace_prefetch_assets_does_not_trim_nbsp_in_urls() {
      let nbsp = "\u{00A0}";
      let url_with_nbsp = format!("{nbsp}https://example.com/{nbsp}");
      assert!(
        normalize_prefetch_url(&url_with_nbsp).is_none(),
        "NBSP should not be treated as ASCII whitespace when normalizing URLs"
      );

      assert_eq!(
        normalize_prefetch_url(" https://example.com "),
        Some("https://example.com/".to_string())
      );

      let wrapped = format!("{nbsp}x{nbsp}");
      assert_eq!(trim_ascii_whitespace(&wrapped), wrapped.as_str());
    }

    #[test]
    fn report_json_is_deterministic_and_sorted() {
      let mut page_b = PageSummary::default();
      page_b.stem = "b".to_string();
      page_b
        .report
        .images
        .record_discovered("https://example.com/z.png".to_string());
      page_b
        .report
        .images
        .record_discovered("https://example.com/a.png".to_string());
      page_b
        .report
        .images
        .record_discovered("https://example.com/m.png".to_string());

      let mut page_a = PageSummary::default();
      page_a.stem = "a".to_string();
      page_a
        .report
        .css
        .record_fetch_result("https://example.com/style.css".to_string(), true);

      let report1 = build_prefetch_assets_report(
        &[page_b.clone(), page_a.clone()],
        Path::new("cache"),
        false,
        2,
      );
      let report2 = build_prefetch_assets_report(&[page_a, page_b], Path::new("cache"), false, 2);

      let json1 = prefetch_assets_report_json(&report1).expect("serialize report");
      let json2 = prefetch_assets_report_json(&report2).expect("serialize report");
      assert_eq!(json1, json2, "report JSON should be deterministic");

      let parsed: Value = serde_json::from_str(&json1).expect("parse report JSON");
      let pages = parsed
        .get("pages")
        .and_then(Value::as_array)
        .expect("pages array");
      assert_eq!(pages.len(), 2);
      assert_eq!(pages[0].get("stem").and_then(Value::as_str), Some("a"));
      assert_eq!(pages[1].get("stem").and_then(Value::as_str), Some("b"));

      let images = pages[1]
        .get("images")
        .and_then(Value::as_object)
        .expect("images section");
      let discovered = images
        .get("discovered")
        .and_then(Value::as_object)
        .expect("images.discovered");
      assert_eq!(
        discovered.get("count").and_then(Value::as_u64),
        Some(3),
        "report should include full discovered count"
      );
      let urls = discovered
        .get("urls")
        .and_then(Value::as_array)
        .expect("images.discovered.urls");
      let urls: Vec<_> = urls
        .iter()
        .map(|v| v.as_str().expect("url string"))
        .collect();
      assert_eq!(
        urls,
        vec!["https://example.com/a.png", "https://example.com/m.png"],
        "report URL samples should be sorted and capped"
      );
    }

    #[test]
    fn report_per_page_path_preserves_dotted_stems() {
      let dir = Path::new("out");
      assert_eq!(
        report_per_page_path(dir, "example.com_path"),
        PathBuf::from("out").join("example.com_path.json")
      );
    }

    #[test]
    fn report_records_partial_failures_even_if_url_succeeds_elsewhere() {
      let mut set = UrlOutcomeSet::default();
      set.record_fetch_result("https://example.com/a", true);
      set.record_fetch_result("https://example.com/a", false);
      assert!(set.discovered.contains("https://example.com/a"));
      assert!(set.fetched.contains("https://example.com/a"));
      assert!(set.failed.contains("https://example.com/a"));

      let mut set = UrlOutcomeSet::default();
      set.record_fetch_result("https://example.com/a", false);
      set.record_fetch_result("https://example.com/a", true);
      assert!(set.discovered.contains("https://example.com/a"));
      assert!(set.fetched.contains("https://example.com/a"));
      assert!(set.failed.contains("https://example.com/a"));
    }

    #[test]
    fn dry_run_does_not_call_fetcher() {
      #[derive(Clone)]
      struct PanicOnFetch;

      impl ResourceFetcher for PanicOnFetch {
        fn fetch(&self, _url: &str) -> fastrender::Result<FetchedResource> {
          panic!("fetch should not be called in dry-run mode");
        }

        fn fetch_with_request(
          &self,
          _req: FetchRequest<'_>,
        ) -> fastrender::Result<FetchedResource> {
          panic!("fetch_with_request should not be called in dry-run mode");
        }
      }

      let html = r#"<!doctype html><html><head>
        <link rel="stylesheet" href="https://example.com/style.css">
        <style>
          @import "https://example.com/import-dry-run.css";
          @font-face { font-family: X; src: url("https://example.com/font.woff2") format("woff2"); }
          body { background-image: url("https://example.com/bg.png"); }
        </style>
      </head><body>
        <img src="https://example.com/img.png">
        <iframe src="https://example.com/frame.html"></iframe>
      </body></html>"#;
      let document_url = "https://example.com/page";
      let media_ctx = MediaContext::screen(800.0, 600.0);
      let opts = PrefetchOptions {
        prefetch_fonts: true,
        prefetch_images: true,
        prefetch_media: false,
        prefetch_scripts: false,
        prefetch_icons: false,
        prefetch_video_posters: false,
        prefetch_iframes: true,
        prefetch_embeds: false,
        prefetch_css_url_assets: true,
        max_discovered_assets_per_page: 2000,
        image_limits: ImagePrefetchLimits {
          max_image_elements: 150,
          max_urls_per_element: 2,
        },
        max_media_bytes_per_file: 10_u64 * 1024 * 1024,
        max_media_bytes_per_page: 50_u64 * 1024 * 1024,
        dry_run: true,
      };
      let fetcher: Arc<dyn ResourceFetcher> = Arc::new(PanicOnFetch);
      let summary = prefetch_assets_for_html(
        "test",
        document_url,
        html,
        document_url,
        "https://example.com/",
        ReferrerPolicy::default(),
        &fetcher,
        &media_ctx,
        opts,
      );

      assert_eq!(summary.fetched_css, 0);
      assert_eq!(summary.failed_css, 0);
      assert_eq!(summary.fetched_imports, 0);
      assert_eq!(summary.failed_imports, 0);
      assert_eq!(summary.fetched_fonts, 0);
      assert_eq!(summary.failed_fonts, 0);
      assert_eq!(summary.fetched_images, 0);
      assert_eq!(summary.failed_images, 0);
      assert_eq!(summary.fetched_documents, 0);
      assert_eq!(summary.failed_documents, 0);
      assert_eq!(summary.fetched_css_assets, 0);
      assert_eq!(summary.failed_css_assets, 0);

      assert_eq!(summary.discovered_images, 1);
      assert_eq!(summary.discovered_documents, 1);
      assert_eq!(summary.discovered_css_assets, 1);
      assert!(
        summary
          .report
          .css
          .discovered
          .contains("https://example.com/style.css"),
        "expected CSS URL to be recorded in report discovery"
      );
      assert!(
        summary
          .report
          .imports
          .discovered
          .contains("https://example.com/import-dry-run.css"),
        "expected @import URL to be recorded in report discovery"
      );
      assert!(
        summary
          .report
          .fonts
          .discovered
          .contains("https://example.com/font.woff2"),
        "expected @font-face URL to be recorded in report discovery"
      );
      assert!(
        summary
          .report
          .images
          .discovered
          .contains("https://example.com/img.png"),
        "expected image URL to be recorded in report discovery"
      );
    }

    #[test]
    fn report_url_samples_are_truncated_deterministically() {
      let mut summary = PageSummary::default();
      summary.stem = "test".to_string();
      for url in [
        "https://example.com/c.png",
        "https://example.com/a.png",
        "https://example.com/b.png",
      ] {
        summary.report.images.record_discovered(url.to_string());
      }

      let report = build_prefetch_assets_report(&[summary], Path::new("cache"), true, 2);
      let json = prefetch_assets_report_json(&report).expect("serialize report");
      let parsed: Value = serde_json::from_str(&json).expect("parse report JSON");
      let pages = parsed
        .get("pages")
        .and_then(Value::as_array)
        .expect("pages array");
      let discovered = pages[0]["images"]["discovered"]
        .as_object()
        .expect("images.discovered");
      assert_eq!(discovered.get("count").and_then(Value::as_u64), Some(3));
      let urls = discovered
        .get("urls")
        .and_then(Value::as_array)
        .expect("images.discovered.urls");
      let urls: Vec<_> = urls
        .iter()
        .map(|v| v.as_str().expect("url string"))
        .collect();
      assert_eq!(
        urls,
        vec!["https://example.com/a.png", "https://example.com/b.png"]
      );
    }

    #[test]
    fn max_discovered_assets_per_page_caps_total_and_dedupes_across_classes() {
      let all = RefCell::new(BTreeSet::<String>::new());
      let mut image_urls = BTreeSet::<String>::new();
      let mut document_urls = BTreeSet::<String>::new();
      let mut css_assets = BTreeSet::<String>::new();

      record_image_candidate(&all, &mut image_urls, "https://example.com/a.png", 2, 2);
      record_document_candidate(
        &all,
        &mut document_urls,
        "https://example.com/frame.html",
        2,
        2,
      );

      // A duplicate discovered via a different class should not be re-scheduled.
      record_css_url_asset_candidate(&all, &mut css_assets, "https://example.com/a.png", 2, 2);
      assert!(
        css_assets.is_empty(),
        "duplicates across classes should be suppressed"
      );

      // Global cap should prevent inserting additional unique URLs.
      record_image_candidate(&all, &mut image_urls, "https://example.com/b.png", 2, 2);

      assert_eq!(all.borrow().len(), 2);
      assert_eq!(image_urls.len(), 1);
      assert_eq!(document_urls.len(), 1);
    }

    #[test]
    fn inert_templates_are_skipped_during_dom_discovery() {
      let dom = parse_html(
        "<!doctype html><html><body>\
          <template><iframe src=\"/frame.html\"></iframe></template>\
        </body></html>",
      )
      .expect("parse dom");
      let all = RefCell::new(BTreeSet::<String>::new());
      let mut document_urls = BTreeSet::<String>::new();

      record_iframe_document_candidates(
        &all,
        &dom,
        "https://example.com/",
        &mut document_urls,
        100,
      );

      assert!(
        document_urls.is_empty(),
        "iframe inside inert <template> should not be prefetched"
      );
    }

    #[test]
    fn iframe_prefetch_uses_fetch_with_request_with_referrer() {
      use std::sync::Mutex;

      #[derive(Default)]
      struct RecordingFetcher {
        calls: Mutex<Vec<(String, FetchDestination, Option<String>, ReferrerPolicy)>>,
      }

      impl ResourceFetcher for RecordingFetcher {
        fn fetch(&self, _url: &str) -> fastrender::Result<FetchedResource> {
          panic!("iframe prefetch should use fetch_with_request");
        }

        fn fetch_with_request(&self, req: FetchRequest<'_>) -> fastrender::Result<FetchedResource> {
          self.calls.lock().unwrap().push((
            req.url.to_string(),
            req.destination,
            req.referrer_url.map(|r| r.to_string()),
            req.referrer_policy,
          ));
          Ok(FetchedResource::with_final_url(
            b"<html></html>".to_vec(),
            Some("text/html".to_string()),
            Some(req.url.to_string()),
          ))
        }
      }

      let html = r#"<!doctype html><html><head><base href="https://example.com/base/"></head><body><iframe src="frame.html"></iframe></body></html>"#;
      let document_url = "https://example.com/page";
      let base_url = "https://example.com/base/";
      let media_ctx = MediaContext::screen(800.0, 600.0);
      let opts = PrefetchOptions {
        prefetch_fonts: false,
        prefetch_images: false,
        prefetch_media: false,
        prefetch_scripts: false,
        prefetch_icons: false,
        prefetch_video_posters: false,
        prefetch_iframes: true,
        prefetch_embeds: false,
        prefetch_css_url_assets: false,
        max_discovered_assets_per_page: 2000,
        image_limits: ImagePrefetchLimits {
          max_image_elements: 150,
          max_urls_per_element: 2,
        },
        max_media_bytes_per_file: 10_u64 * 1024 * 1024,
        max_media_bytes_per_page: 50_u64 * 1024 * 1024,
        dry_run: false,
      };

      let fetcher_impl = Arc::new(RecordingFetcher::default());
      let fetcher: Arc<dyn ResourceFetcher> = fetcher_impl.clone();
      let summary = prefetch_assets_for_html(
        "test",
        document_url,
        html,
        document_url,
        base_url,
        ReferrerPolicy::NoReferrer,
        &fetcher,
        &media_ctx,
        opts,
      );

      assert_eq!(summary.discovered_documents, 1);
      assert_eq!(summary.fetched_documents, 1);
      assert_eq!(summary.failed_documents, 0);

      let calls = fetcher_impl.calls.lock().unwrap();
      assert_eq!(calls.len(), 1);
      assert_eq!(calls[0].0, "https://example.com/base/frame.html");
      assert_eq!(calls[0].1, FetchDestination::Iframe);
      assert_eq!(calls[0].2.as_deref(), Some(document_url));
      assert_eq!(calls[0].3, ReferrerPolicy::NoReferrer);
    }

    #[test]
    fn meta_referrer_policy_overrides_initial_policy_for_iframe_prefetch() {
      use std::sync::Mutex;

      #[derive(Default)]
      struct RecordingFetcher {
        policies: Mutex<Vec<ReferrerPolicy>>,
      }

      impl ResourceFetcher for RecordingFetcher {
        fn fetch(&self, _url: &str) -> fastrender::Result<FetchedResource> {
          panic!("iframe prefetch should use fetch_with_request");
        }

        fn fetch_with_request(&self, req: FetchRequest<'_>) -> fastrender::Result<FetchedResource> {
          self.policies.lock().unwrap().push(req.referrer_policy);
          Ok(FetchedResource::with_final_url(
            b"<html></html>".to_vec(),
            Some("text/html".to_string()),
            Some(req.url.to_string()),
          ))
        }
      }

      let html = r#"<!doctype html>
<html>
  <head>
    <meta name="referrer" content="no-referrer">
    <base href="https://example.com/base/">
  </head>
  <body>
    <iframe src="frame.html"></iframe>
  </body>
</html>"#;
      let document_url = "https://example.com/page";
      let media_ctx = MediaContext::screen(800.0, 600.0);
      let opts = PrefetchOptions {
        prefetch_fonts: false,
        prefetch_images: false,
        prefetch_media: false,
        prefetch_scripts: false,
        prefetch_icons: false,
        prefetch_video_posters: false,
        prefetch_iframes: true,
        prefetch_embeds: false,
        prefetch_css_url_assets: false,
        max_discovered_assets_per_page: 2000,
        image_limits: ImagePrefetchLimits {
          max_image_elements: 150,
          max_urls_per_element: 2,
        },
        max_media_bytes_per_file: 10_u64 * 1024 * 1024,
        max_media_bytes_per_page: 50_u64 * 1024 * 1024,
        dry_run: false,
      };

      let mut resource = FetchedResource::with_final_url(
        html.as_bytes().to_vec(),
        Some("text/html".to_string()),
        Some(document_url.to_string()),
      );
      resource.response_referrer_policy = Some(ReferrerPolicy::Origin);
      let doc = decode_html_resource(&resource, document_url);

      let fetcher_impl = Arc::new(RecordingFetcher::default());
      let fetcher: Arc<dyn ResourceFetcher> = fetcher_impl.clone();
      let summary = prefetch_assets_for_html(
        "test",
        document_url,
        html,
        &doc.base_hint,
        &doc.base_url,
        doc.referrer_policy,
        &fetcher,
        &media_ctx,
        opts,
      );

      assert_eq!(summary.discovered_documents, 1);
      assert_eq!(summary.fetched_documents, 1);

      let policies = fetcher_impl.policies.lock().unwrap();
      assert_eq!(policies.len(), 1);
      assert_eq!(policies[0], ReferrerPolicy::NoReferrer);
    }

    #[test]
    fn import_referrer_override_only_applies_to_inline_root_document_base_url() {
      use std::sync::Mutex;

      #[derive(Default)]
      struct RecordingFetcher {
        calls: Mutex<Vec<(String, Option<String>)>>,
      }

      impl ResourceFetcher for RecordingFetcher {
        fn fetch(&self, _url: &str) -> fastrender::Result<FetchedResource> {
          panic!("expected prefetch to use fetch_with_request");
        }

        fn fetch_with_request(&self, req: FetchRequest<'_>) -> fastrender::Result<FetchedResource> {
          self
            .calls
            .lock()
            .unwrap()
            .push((req.url.to_string(), req.referrer_url.map(|r| r.to_string())));
          let mut res = FetchedResource::with_final_url(
            b"body {}".to_vec(),
            Some("text/css".to_string()),
            Some(req.url.to_string()),
          );
          res.status = Some(200);
          Ok(res)
        }
      }

      let document_referrer = "https://example.test/page.html";
      let document_base_url = "https://example.test/base.css";
      let nested_import_url = "https://example.test/nested.css";

      let summary = RefCell::new(PageSummary::default());
      let all_asset_urls = RefCell::new(BTreeSet::<String>::new());
      let fetcher = RecordingFetcher::default();
      let loader = PrefetchImportLoader::new(
        &fetcher,
        document_referrer,
        Some(document_base_url),
        None,
        FetchDestination::Style,
        FetchCredentialsMode::Include,
        ReferrerPolicy::Origin,
        &summary,
        &all_asset_urls,
        None,
        2000,
      );

      // First-level import from an inline stylesheet: `importer_url` matches the document base URL
      // (potentially affected by `<base href>`), but the request should still use the document URL
      // as its referrer.
      loader
        .load_with_importer(document_base_url, Some(document_base_url))
        .expect("fetch base stylesheet");

      // Nested import from the fetched stylesheet: if the stylesheet URL happens to equal the
      // document base URL, we must *not* fall back to the document referrer, because the imported
      // stylesheet is now the correct referrer source.
      loader
        .load_with_importer(nested_import_url, Some(document_base_url))
        .expect("fetch nested stylesheet");

      let calls = fetcher.calls.lock().unwrap();
      assert_eq!(calls.len(), 2);
      assert_eq!(calls[0].0, document_base_url);
      assert_eq!(calls[0].1.as_deref(), Some(document_referrer));
      assert_eq!(calls[1].0, nested_import_url);
      assert_eq!(calls[1].1.as_deref(), Some(document_base_url));
    }

    #[test]
    fn stylesheet_referrer_policy_header_applies_to_import_requests() {
      use std::sync::Mutex;

      #[derive(Default)]
      struct RecordingFetcher {
        calls: Mutex<Vec<(String, FetchDestination, ReferrerPolicy)>>,
      }

      impl ResourceFetcher for RecordingFetcher {
        fn fetch(&self, _url: &str) -> fastrender::Result<FetchedResource> {
          panic!("expected prefetch to use fetch_with_request");
        }

        fn fetch_with_request(&self, req: FetchRequest<'_>) -> fastrender::Result<FetchedResource> {
          self.calls.lock().unwrap().push((
            req.url.to_string(),
            req.destination,
            req.referrer_policy,
          ));

          match req.url {
            "https://example.com/style.css" => {
              let mut res = FetchedResource::with_final_url(
                br#"@import url("https://example.com/import.css"); body { color: rgb(1, 2, 3); }"#
                  .to_vec(),
                Some("text/css".to_string()),
                Some(req.url.to_string()),
              );
              res.status = Some(200);
              res.response_referrer_policy = Some(ReferrerPolicy::NoReferrer);
              Ok(res)
            }
            "https://example.com/import.css" => {
              let mut res = FetchedResource::with_final_url(
                b"body { background: rgb(0, 0, 0); }".to_vec(),
                Some("text/css".to_string()),
                Some(req.url.to_string()),
              );
              res.status = Some(200);
              Ok(res)
            }
            other => Err(fastrender::Error::Other(format!(
              "unexpected fetch: {other}"
            ))),
          }
        }
      }

      let html = r#"<!doctype html><html><head>
        <link rel="stylesheet" href="https://example.com/style.css">
      </head><body></body></html>"#;
      let document_url = "https://example.com/page.html";
      let media_ctx = MediaContext::screen(800.0, 600.0);
      let opts = PrefetchOptions {
        prefetch_fonts: false,
        prefetch_images: false,
        prefetch_media: false,
        prefetch_scripts: false,
        prefetch_icons: false,
        prefetch_video_posters: false,
        prefetch_iframes: false,
        prefetch_embeds: false,
        prefetch_css_url_assets: false,
        max_discovered_assets_per_page: 2000,
        image_limits: ImagePrefetchLimits {
          max_image_elements: 150,
          max_urls_per_element: 2,
        },
        max_media_bytes_per_file: 10_u64 * 1024 * 1024,
        max_media_bytes_per_page: 50_u64 * 1024 * 1024,
        dry_run: false,
      };

      let fetcher_impl = Arc::new(RecordingFetcher::default());
      let fetcher: Arc<dyn ResourceFetcher> = fetcher_impl.clone();
      let summary = prefetch_assets_for_html(
        "test",
        document_url,
        html,
        document_url,
        "https://example.com/",
        ReferrerPolicy::Origin,
        &fetcher,
        &media_ctx,
        opts,
      );

      assert_eq!(summary.fetched_css, 1);
      assert_eq!(summary.fetched_imports, 1);

      let calls = fetcher_impl.calls.lock().unwrap();
      assert!(
        calls.iter().any(|(url, dest, policy)| {
          url == "https://example.com/import.css"
            && *dest == FetchDestination::Style
            && *policy == ReferrerPolicy::NoReferrer
        }),
        "expected import request to inherit stylesheet response Referrer-Policy header, got: {calls:?}"
      );
    }

    #[test]
    fn stylesheet_referrer_policy_header_applies_to_nested_import_requests() {
      use std::sync::Mutex;

      #[derive(Default)]
      struct RecordingFetcher {
        calls: Mutex<Vec<(String, FetchDestination, ReferrerPolicy)>>,
      }

      impl ResourceFetcher for RecordingFetcher {
        fn fetch(&self, _url: &str) -> fastrender::Result<FetchedResource> {
          panic!("expected prefetch to use fetch_with_request");
        }

        fn fetch_with_request(&self, req: FetchRequest<'_>) -> fastrender::Result<FetchedResource> {
          self.calls.lock().unwrap().push((
            req.url.to_string(),
            req.destination,
            req.referrer_policy,
          ));

          match req.url {
            "https://example.com/style.css" => {
              let mut res = FetchedResource::with_final_url(
                br#"@import url("import.css"); body { color: rgb(1, 2, 3); }"#.to_vec(),
                Some("text/css".to_string()),
                Some(req.url.to_string()),
              );
              res.status = Some(200);
              res.response_referrer_policy = Some(ReferrerPolicy::Origin);
              Ok(res)
            }
            "https://example.com/import.css" => {
              let mut res = FetchedResource::with_final_url(
                br#"@import url("nested.css"); body { background: rgb(0, 0, 0); }"#.to_vec(),
                Some("text/css".to_string()),
                Some("https://cdn.example.com/import-final-nested-import.css".to_string()),
              );
              res.status = Some(200);
              res.response_referrer_policy = Some(ReferrerPolicy::NoReferrer);
              Ok(res)
            }
            "https://cdn.example.com/nested.css" => {
              let mut res = FetchedResource::with_final_url(
                b"body { background: rgb(4, 5, 6); }".to_vec(),
                Some("text/css".to_string()),
                Some(req.url.to_string()),
              );
              res.status = Some(200);
              Ok(res)
            }
            "https://example.com/nested.css" => {
              let mut res = FetchedResource::with_final_url(
                b"body { background: rgb(255, 0, 0); }".to_vec(),
                Some("text/css".to_string()),
                Some(req.url.to_string()),
              );
              res.status = Some(200);
              Ok(res)
            }
            other => Err(fastrender::Error::Other(format!(
              "unexpected fetch: {other}"
            ))),
          }
        }
      }

      let html = r#"<!doctype html><html><head>
        <link rel="stylesheet" href="https://example.com/style.css">
      </head><body></body></html>"#;
      let document_url = "https://example.com/page.html";
      let media_ctx = MediaContext::screen(800.0, 600.0);
      let opts = PrefetchOptions {
        prefetch_fonts: false,
        prefetch_images: false,
        prefetch_media: false,
        prefetch_scripts: false,
        prefetch_icons: false,
        prefetch_video_posters: false,
        prefetch_iframes: false,
        prefetch_embeds: false,
        prefetch_css_url_assets: false,
        max_discovered_assets_per_page: 2000,
        image_limits: ImagePrefetchLimits {
          max_image_elements: 150,
          max_urls_per_element: 2,
        },
        max_media_bytes_per_file: 10_u64 * 1024 * 1024,
        max_media_bytes_per_page: 50_u64 * 1024 * 1024,
        dry_run: false,
      };

      let fetcher_impl = Arc::new(RecordingFetcher::default());
      let fetcher: Arc<dyn ResourceFetcher> = fetcher_impl.clone();
      let summary = prefetch_assets_for_html(
        "test",
        document_url,
        html,
        document_url,
        "https://example.com/",
        ReferrerPolicy::Origin,
        &fetcher,
        &media_ctx,
        opts,
      );

      assert_eq!(summary.fetched_css, 1);
      assert_eq!(summary.fetched_imports, 2);

      let calls = fetcher_impl.calls.lock().unwrap();
      assert!(
        calls.iter().any(|(url, dest, policy)| {
          url == "https://cdn.example.com/nested.css"
            && *dest == FetchDestination::Style
            && *policy == ReferrerPolicy::NoReferrer
        }),
        "expected nested import request to inherit imported stylesheet response Referrer-Policy header, got: {calls:?}"
      );
      assert!(
        calls
          .iter()
          .all(|(url, _, _)| url != "https://example.com/nested.css"),
        "expected nested import to resolve against imported stylesheet final URL, got: {calls:?}"
      );
    }

    #[test]
    fn stylesheet_referrer_policy_header_applies_to_font_requests() {
      use std::sync::Mutex;

      #[derive(Default)]
      struct RecordingFetcher {
        calls: Mutex<
          Vec<(
            String,
            FetchDestination,
            ReferrerPolicy,
            FetchCredentialsMode,
          )>,
        >,
      }

      impl ResourceFetcher for RecordingFetcher {
        fn fetch(&self, _url: &str) -> fastrender::Result<FetchedResource> {
          panic!("expected prefetch to use fetch_with_request");
        }

        fn fetch_with_request(&self, req: FetchRequest<'_>) -> fastrender::Result<FetchedResource> {
          self.calls.lock().unwrap().push((
            req.url.to_string(),
            req.destination,
            req.referrer_policy,
            req.credentials_mode,
          ));

          match req.url {
            "https://example.com/style.css" => {
              let mut res = FetchedResource::with_final_url(
                br#"@font-face{font-family:X;src:url("https://example.com/font.woff2");}"#.to_vec(),
                Some("text/css".to_string()),
                Some(req.url.to_string()),
              );
              res.status = Some(200);
              res.response_referrer_policy = Some(ReferrerPolicy::NoReferrer);
              Ok(res)
            }
            "https://example.com/font.woff2" => {
              let mut res = FetchedResource::with_final_url(
                b"font".to_vec(),
                Some("font/woff2".to_string()),
                Some(req.url.to_string()),
              );
              res.status = Some(200);
              Ok(res)
            }
            other => Err(fastrender::Error::Other(format!(
              "unexpected fetch: {other}"
            ))),
          }
        }
      }

      let html = r#"<!doctype html><html><head>
        <link rel="stylesheet" href="https://example.com/style.css">
      </head><body></body></html>"#;
      let document_url = "https://example.com/page.html";
      let media_ctx = MediaContext::screen(800.0, 600.0);
      let opts = PrefetchOptions {
        prefetch_fonts: true,
        prefetch_images: false,
        prefetch_media: false,
        prefetch_scripts: false,
        prefetch_icons: false,
        prefetch_video_posters: false,
        prefetch_iframes: false,
        prefetch_embeds: false,
        prefetch_css_url_assets: false,
        max_discovered_assets_per_page: 2000,
        image_limits: ImagePrefetchLimits {
          max_image_elements: 150,
          max_urls_per_element: 2,
        },
        max_media_bytes_per_file: 10_u64 * 1024 * 1024,
        max_media_bytes_per_page: 50_u64 * 1024 * 1024,
        dry_run: false,
      };

      let fetcher_impl = Arc::new(RecordingFetcher::default());
      let fetcher: Arc<dyn ResourceFetcher> = fetcher_impl.clone();
      let summary = prefetch_assets_for_html(
        "test",
        document_url,
        html,
        document_url,
        "https://example.com/",
        ReferrerPolicy::Origin,
        &fetcher,
        &media_ctx,
        opts,
      );

      assert_eq!(summary.fetched_css, 1);
      assert_eq!(summary.fetched_fonts, 1);

      let calls = fetcher_impl.calls.lock().unwrap();
      assert!(
        calls.iter().any(|(url, dest, policy, credentials_mode)| {
          url == "https://example.com/font.woff2"
            && *dest == FetchDestination::Font
            && *policy == ReferrerPolicy::NoReferrer
            && *credentials_mode == FetchCredentialsMode::SameOrigin
        }),
        "expected font request to inherit stylesheet response Referrer-Policy header, got: {calls:?}"
      );
    }

    #[test]
    fn stylesheet_referrer_policy_header_applies_to_nested_font_requests() {
      use std::sync::Mutex;

      #[derive(Default)]
      struct RecordingFetcher {
        calls: Mutex<
          Vec<(
            String,
            FetchDestination,
            ReferrerPolicy,
            FetchCredentialsMode,
          )>,
        >,
      }

      impl ResourceFetcher for RecordingFetcher {
        fn fetch(&self, _url: &str) -> fastrender::Result<FetchedResource> {
          panic!("expected prefetch to use fetch_with_request");
        }

        fn fetch_with_request(&self, req: FetchRequest<'_>) -> fastrender::Result<FetchedResource> {
          self.calls.lock().unwrap().push((
            req.url.to_string(),
            req.destination,
            req.referrer_policy,
            req.credentials_mode,
          ));

          match req.url {
            "https://example.com/style.css" => {
              let mut res = FetchedResource::with_final_url(
                br#"@import url("import.css"); body { color: rgb(1, 2, 3); }"#.to_vec(),
                Some("text/css".to_string()),
                Some(req.url.to_string()),
              );
              res.status = Some(200);
              res.response_referrer_policy = Some(ReferrerPolicy::Origin);
              Ok(res)
            }
            "https://example.com/import.css" => {
              let mut res = FetchedResource::with_final_url(
                br#"@font-face{font-family:X;src:url("nested.woff2");}"#.to_vec(),
                Some("text/css".to_string()),
                Some("https://cdn.example.com/import-final.css".to_string()),
              );
              res.status = Some(200);
              res.response_referrer_policy = Some(ReferrerPolicy::NoReferrer);
              Ok(res)
            }
            "https://cdn.example.com/nested.woff2" | "https://example.com/nested.woff2" => {
              let mut res = FetchedResource::with_final_url(
                b"font".to_vec(),
                Some("font/woff2".to_string()),
                Some(req.url.to_string()),
              );
              res.status = Some(200);
              Ok(res)
            }
            other => Err(fastrender::Error::Other(format!(
              "unexpected fetch: {other}"
            ))),
          }
        }
      }

      let html = r#"<!doctype html><html><head>
        <link rel="stylesheet" href="https://example.com/style.css">
      </head><body></body></html>"#;
      let document_url = "https://example.com/page.html";
      let media_ctx = MediaContext::screen(800.0, 600.0);
      let opts = PrefetchOptions {
        prefetch_fonts: true,
        prefetch_images: false,
        prefetch_media: false,
        prefetch_scripts: false,
        prefetch_icons: false,
        prefetch_video_posters: false,
        prefetch_iframes: false,
        prefetch_embeds: false,
        prefetch_css_url_assets: false,
        max_discovered_assets_per_page: 2000,
        image_limits: ImagePrefetchLimits {
          max_image_elements: 150,
          max_urls_per_element: 2,
        },
        max_media_bytes_per_file: 10_u64 * 1024 * 1024,
        max_media_bytes_per_page: 50_u64 * 1024 * 1024,
        dry_run: false,
      };

      let fetcher_impl = Arc::new(RecordingFetcher::default());
      let fetcher: Arc<dyn ResourceFetcher> = fetcher_impl.clone();
      let summary = prefetch_assets_for_html(
        "test",
        document_url,
        html,
        document_url,
        "https://example.com/",
        ReferrerPolicy::Origin,
        &fetcher,
        &media_ctx,
        opts,
      );

      assert_eq!(summary.fetched_css, 1);
      assert_eq!(summary.fetched_imports, 1);
      assert_eq!(summary.fetched_fonts, 1);

      let calls = fetcher_impl.calls.lock().unwrap();
      assert!(
        calls.iter().any(|(url, dest, policy, credentials_mode)| {
          url == "https://cdn.example.com/nested.woff2"
            && *dest == FetchDestination::Font
            && *policy == ReferrerPolicy::NoReferrer
            && *credentials_mode == FetchCredentialsMode::SameOrigin
        }),
        "expected nested font request to inherit imported stylesheet response Referrer-Policy header, got: {calls:?}"
      );
      assert!(
        calls.iter().all(|(url, dest, _, _)| {
          !(url == "https://example.com/nested.woff2" && *dest == FetchDestination::Font)
        }),
        "expected nested font URL to resolve against imported stylesheet final URL, got: {calls:?}"
      );
    }

    #[test]
    fn poster_is_still_fetched_when_image_cap_is_exceeded() {
      use fastrender::resource::FetchContextKind;
      use std::sync::Mutex;

      #[derive(Default)]
      struct RecordingFetcher {
        calls: Mutex<Vec<String>>,
      }

      impl ResourceFetcher for RecordingFetcher {
        fn fetch(&self, _url: &str) -> fastrender::Result<FetchedResource> {
          panic!("image prefetch should use fetch_with_request");
        }

        fn fetch_with_request(&self, req: FetchRequest<'_>) -> fastrender::Result<FetchedResource> {
          self.calls.lock().unwrap().push(req.url.to_string());
          Ok(FetchedResource::new(
            b"img".to_vec(),
            Some("image/png".to_string()),
          ))
        }

        fn fetch_partial_with_context(
          &self,
          _kind: FetchContextKind,
          _url: &str,
          _max_bytes: usize,
        ) -> fastrender::Result<FetchedResource> {
          // `prefetch_assets` probes image metadata after prefetching; suppress recording so tests
          // only assert the explicit prefetch fetches.
          Ok(FetchedResource::new(
            Vec::new(),
            Some("image/png".to_string()),
          ))
        }
      }

      let n = 1usize;
      let mut html = "<!doctype html><html><body>".to_string();
      for idx in 0..(n + 2) {
        html.push_str(&format!("<img src=\"/img{idx}.png\">"));
      }
      html.push_str("<video poster=\"/poster.png\"></video></body></html>");

      let document_url = "https://example.com/page";
      let base_url = "https://example.com/";
      let media_ctx = MediaContext::screen(800.0, 600.0);
      let opts = PrefetchOptions {
        prefetch_fonts: false,
        prefetch_images: true,
        prefetch_media: false,
        prefetch_scripts: false,
        prefetch_icons: false,
        prefetch_video_posters: false,
        prefetch_iframes: false,
        prefetch_embeds: false,
        prefetch_css_url_assets: false,
        max_discovered_assets_per_page: 2000,
        image_limits: ImagePrefetchLimits {
          max_image_elements: n,
          max_urls_per_element: 2,
        },
        max_media_bytes_per_file: 10_u64 * 1024 * 1024,
        max_media_bytes_per_page: 50_u64 * 1024 * 1024,
        dry_run: false,
      };

      let fetcher_impl = Arc::new(RecordingFetcher::default());
      let fetcher: Arc<dyn ResourceFetcher> = fetcher_impl.clone();
      let summary = prefetch_assets_for_html(
        "test",
        document_url,
        &html,
        document_url,
        base_url,
        ReferrerPolicy::default(),
        &fetcher,
        &media_ctx,
        opts,
      );

      assert_eq!(summary.discovered_images, 2);
      assert!(
        fetcher_impl
          .calls
          .lock()
          .unwrap()
          .contains(&"https://example.com/poster.png".to_string()),
        "poster URL should be requested even when image discovery cap is hit",
      );
    }

    #[test]
    fn gnt_gl_ps_is_recognized() {
      use fastrender::resource::FetchContextKind;
      use std::sync::Mutex;

      #[derive(Default)]
      struct RecordingFetcher {
        calls: Mutex<Vec<String>>,
      }

      impl ResourceFetcher for RecordingFetcher {
        fn fetch(&self, _url: &str) -> fastrender::Result<FetchedResource> {
          panic!("image prefetch should use fetch_with_request");
        }

        fn fetch_with_request(&self, req: FetchRequest<'_>) -> fastrender::Result<FetchedResource> {
          self.calls.lock().unwrap().push(req.url.to_string());
          Ok(FetchedResource::new(
            b"img".to_vec(),
            Some("image/png".to_string()),
          ))
        }

        fn fetch_partial_with_context(
          &self,
          _kind: FetchContextKind,
          _url: &str,
          _max_bytes: usize,
        ) -> fastrender::Result<FetchedResource> {
          Ok(FetchedResource::new(
            Vec::new(),
            Some("image/png".to_string()),
          ))
        }
      }

      let n = 1usize;
      let mut html = "<!doctype html><html><body>".to_string();
      for idx in 0..(n + 2) {
        html.push_str(&format!("<img src=\"/img{idx}.png\">"));
      }
      html.push_str("<video gnt-gl-ps=\"/poster2.png\"></video></body></html>");

      let document_url = "https://example.com/page";
      let base_url = "https://example.com/";
      let media_ctx = MediaContext::screen(800.0, 600.0);
      let opts = PrefetchOptions {
        prefetch_fonts: false,
        prefetch_images: true,
        prefetch_media: false,
        prefetch_scripts: false,
        prefetch_icons: false,
        prefetch_video_posters: false,
        prefetch_iframes: false,
        prefetch_embeds: false,
        prefetch_css_url_assets: false,
        max_discovered_assets_per_page: 2000,
        image_limits: ImagePrefetchLimits {
          max_image_elements: n,
          max_urls_per_element: 2,
        },
        max_media_bytes_per_file: 10_u64 * 1024 * 1024,
        max_media_bytes_per_page: 50_u64 * 1024 * 1024,
        dry_run: false,
      };

      let fetcher_impl = Arc::new(RecordingFetcher::default());
      let fetcher: Arc<dyn ResourceFetcher> = fetcher_impl.clone();
      let summary = prefetch_assets_for_html(
        "test",
        document_url,
        &html,
        document_url,
        base_url,
        ReferrerPolicy::default(),
        &fetcher,
        &media_ctx,
        opts,
      );

      assert_eq!(summary.discovered_images, 2);
      assert!(
        fetcher_impl
          .calls
          .lock()
          .unwrap()
          .contains(&"https://example.com/poster2.png".to_string()),
        "gnt-gl-ps should be recognized as a video poster candidate",
      );
    }

    #[test]
    fn crossorigin_video_poster_is_prefetched_with_image_cors_destination() {
      use fastrender::resource::FetchContextKind;
      use std::sync::Mutex;

      #[derive(Default)]
      struct RecordingFetcher {
        calls: Mutex<Vec<(String, FetchDestination)>>,
      }

      impl ResourceFetcher for RecordingFetcher {
        fn fetch(&self, _url: &str) -> fastrender::Result<FetchedResource> {
          panic!("image prefetch should use fetch_with_request");
        }

        fn fetch_with_request(&self, req: FetchRequest<'_>) -> fastrender::Result<FetchedResource> {
          self
            .calls
            .lock()
            .unwrap()
            .push((req.url.to_string(), req.destination));
          Ok(FetchedResource::new(
            b"img".to_vec(),
            Some("image/png".to_string()),
          ))
        }

        fn fetch_partial_with_context(
          &self,
          _kind: FetchContextKind,
          _url: &str,
          _max_bytes: usize,
        ) -> fastrender::Result<FetchedResource> {
          Ok(FetchedResource::new(
            Vec::new(),
            Some("image/png".to_string()),
          ))
        }
      }

      let html = r#"<!doctype html><html><body>
        <video poster="/poster.png" crossorigin></video>
      </body></html>"#;

      let document_url = "https://example.com/page";
      let base_url = "https://example.com/";
      let media_ctx = MediaContext::screen(800.0, 600.0);
      let opts = PrefetchOptions {
        prefetch_fonts: false,
        prefetch_images: false,
        prefetch_scripts: false,
        prefetch_icons: false,
        prefetch_video_posters: true,
        prefetch_iframes: false,
        prefetch_embeds: false,
        prefetch_css_url_assets: false,
        max_discovered_assets_per_page: 2000,
        image_limits: ImagePrefetchLimits {
          max_image_elements: 150,
          max_urls_per_element: 2,
        },
        dry_run: false,
      };

      let fetcher_impl = Arc::new(RecordingFetcher::default());
      let fetcher: Arc<dyn ResourceFetcher> = fetcher_impl.clone();
      prefetch_assets_for_html(
        "test",
        document_url,
        html,
        document_url,
        base_url,
        ReferrerPolicy::default(),
        &fetcher,
        &media_ctx,
        opts,
      );

      let calls = fetcher_impl.calls.lock().unwrap().clone();
      assert!(
        calls.iter().any(|(url, dest)| {
          url == "https://example.com/poster.png" && *dest == FetchDestination::ImageCors
        }),
        "expected crossorigin video poster to be prefetched via ImageCors, got: {calls:?}"
      );
    }

    #[test]
    fn wrapper_video_poster_is_still_fetched_when_image_cap_is_exceeded() {
      use fastrender::resource::FetchContextKind;
      use std::sync::Mutex;

      #[derive(Default)]
      struct RecordingFetcher {
        calls: Mutex<Vec<(String, FetchDestination)>>,
      }

      impl ResourceFetcher for RecordingFetcher {
        fn fetch(&self, _url: &str) -> fastrender::Result<FetchedResource> {
          panic!("image prefetch should use fetch_with_request");
        }

        fn fetch_with_request(&self, req: FetchRequest<'_>) -> fastrender::Result<FetchedResource> {
          self
            .calls
            .lock()
            .unwrap()
            .push((req.url.to_string(), req.destination));
          Ok(FetchedResource::new(
            b"img".to_vec(),
            Some("image/png".to_string()),
          ))
        }

        fn fetch_partial_with_context(
          &self,
          _kind: FetchContextKind,
          _url: &str,
          _max_bytes: usize,
        ) -> fastrender::Result<FetchedResource> {
          Ok(FetchedResource::new(
            Vec::new(),
            Some("image/png".to_string()),
          ))
        }
      }

      let n = 1usize;
      let mut html = "<!doctype html><html><body>".to_string();
      for idx in 0..(n + 2) {
        html.push_str(&format!("<img src=\"/img{idx}.png\">"));
      }
      html.push_str("<div data-poster-url=\"/wrapper.png\"><video crossorigin></video></div>");
      html.push_str("</body></html>");

      let document_url = "https://example.com/page";
      let base_url = "https://example.com/";
      let media_ctx = MediaContext::screen(800.0, 600.0);
      let opts = PrefetchOptions {
        prefetch_fonts: false,
        prefetch_images: true,
        prefetch_scripts: false,
        prefetch_icons: false,
        prefetch_video_posters: false,
        prefetch_iframes: false,
        prefetch_embeds: false,
        prefetch_css_url_assets: false,
        max_discovered_assets_per_page: 2000,
        image_limits: ImagePrefetchLimits {
          max_image_elements: n,
          max_urls_per_element: 2,
        },
        dry_run: false,
      };

      let fetcher_impl = Arc::new(RecordingFetcher::default());
      let fetcher: Arc<dyn ResourceFetcher> = fetcher_impl.clone();
      prefetch_assets_for_html(
        "test",
        document_url,
        &html,
        document_url,
        base_url,
        ReferrerPolicy::default(),
        &fetcher,
        &media_ctx,
        opts,
      );

      let calls = fetcher_impl.calls.lock().unwrap().clone();
      assert!(
        calls.iter().any(|(url, dest)| {
          url == "https://example.com/wrapper.png" && *dest == FetchDestination::ImageCors
        }),
        "expected wrapper poster URL to be prefetched via ImageCors even when image discovery cap is hit, got: {calls:?}"
      );
    }

    #[test]
    fn font_face_prefetch_skips_unsupported_formats() {
      use std::sync::Mutex;

      #[derive(Default)]
      struct RecordingFetcher {
        calls: Mutex<Vec<String>>,
      }

      impl ResourceFetcher for RecordingFetcher {
        fn fetch(&self, _url: &str) -> fastrender::Result<FetchedResource> {
          panic!("font prefetch should use fetch_with_request");
        }

        fn fetch_with_request(&self, req: FetchRequest<'_>) -> fastrender::Result<FetchedResource> {
          self.calls.lock().unwrap().push(req.url.to_string());
          let mut res = FetchedResource::with_final_url(
            b"font".to_vec(),
            Some("font/woff2".to_string()),
            Some(req.url.to_string()),
          );
          res.status = Some(200);
          Ok(res)
        }
      }

      let html = r#"<!doctype html><html><head><style>
@font-face { font-family: X; src: url(/font.eot) format('embedded-opentype'), url(/font.woff2) format('woff2'), url(/font.woff) format('woff'); }
</style></head><body></body></html>"#;

      let document_url = "https://example.com/page";
      let base_url = "https://example.com/";
      let media_ctx = MediaContext::screen(800.0, 600.0);
      let opts = PrefetchOptions {
        prefetch_fonts: true,
        prefetch_images: false,
        prefetch_media: false,
        prefetch_scripts: false,
        prefetch_icons: false,
        prefetch_video_posters: false,
        prefetch_iframes: false,
        prefetch_embeds: false,
        prefetch_css_url_assets: false,
        max_discovered_assets_per_page: 2000,
        image_limits: ImagePrefetchLimits {
          max_image_elements: 150,
          max_urls_per_element: 2,
        },
        max_media_bytes_per_file: 10_u64 * 1024 * 1024,
        max_media_bytes_per_page: 50_u64 * 1024 * 1024,
        dry_run: false,
      };

      let fetcher_impl = Arc::new(RecordingFetcher::default());
      let fetcher: Arc<dyn ResourceFetcher> = fetcher_impl.clone();
      let summary = prefetch_assets_for_html(
        "test",
        document_url,
        html,
        document_url,
        base_url,
        ReferrerPolicy::default(),
        &fetcher,
        &media_ctx,
        opts,
      );

      let calls = fetcher_impl.calls.lock().unwrap().clone();
      assert_eq!(
        calls,
        vec!["https://example.com/font.woff2"],
        "expected only the first supported font source to be fetched"
      );
      assert_eq!(summary.fetched_fonts, 1);
      assert_eq!(summary.failed_fonts, 0);
    }

    #[test]
    fn font_face_prefetch_sets_stylesheet_referrer_and_client_origin() {
      use std::sync::Mutex;

      #[derive(Default)]
      struct RecordingFetcher {
        calls: Mutex<
          Vec<(
            String,
            FetchDestination,
            FetchCredentialsMode,
            Option<String>,
            Option<DocumentOrigin>,
          )>,
        >,
      }

      impl ResourceFetcher for RecordingFetcher {
        fn fetch(&self, _url: &str) -> fastrender::Result<FetchedResource> {
          panic!("font prefetch should use fetch_with_request");
        }

        fn fetch_with_request(&self, req: FetchRequest<'_>) -> fastrender::Result<FetchedResource> {
          self.calls.lock().unwrap().push((
            req.url.to_string(),
            req.destination,
            req.credentials_mode,
            req.referrer_url.map(|r| r.to_string()),
            req.client_origin.cloned(),
          ));

          const STYLESHEET: &str = "https://cdn.test/style.css";
          const FONT: &str = "https://cdn.test/font.woff2";
          match req.url {
            STYLESHEET => {
              let mut res = FetchedResource::with_final_url(
                br#"@font-face{font-family:X;src:url("https://cdn.test/font.woff2");}"#.to_vec(),
                Some("text/css".to_string()),
                Some(req.url.to_string()),
              );
              res.status = Some(200);
              Ok(res)
            }
            FONT => {
              let mut res = FetchedResource::with_final_url(
                b"font".to_vec(),
                Some("font/woff2".to_string()),
                Some(req.url.to_string()),
              );
              res.status = Some(200);
              Ok(res)
            }
            other => Err(fastrender::Error::Other(format!(
              "unexpected fetch: {other}"
            ))),
          }
        }
      }

      const DOCUMENT_URL: &str = "https://example.com/page";
      const BASE_URL: &str = "https://example.com/";
      const STYLESHEET: &str = "https://cdn.test/style.css";
      const FONT: &str = "https://cdn.test/font.woff2";

      let expected_origin = origin_from_url(DOCUMENT_URL).expect("document origin");

      let html = r#"<!doctype html><html><head><link rel="stylesheet" href="https://cdn.test/style.css"></head><body></body></html>"#;
      let media_ctx = MediaContext::screen(800.0, 600.0);
      let opts = PrefetchOptions {
        prefetch_fonts: true,
        prefetch_images: false,
        prefetch_media: false,
        prefetch_scripts: false,
        prefetch_icons: false,
        prefetch_video_posters: false,
        prefetch_iframes: false,
        prefetch_embeds: false,
        prefetch_css_url_assets: false,
        max_discovered_assets_per_page: 2000,
        image_limits: ImagePrefetchLimits {
          max_image_elements: 150,
          max_urls_per_element: 2,
        },
        max_media_bytes_per_file: 10_u64 * 1024 * 1024,
        max_media_bytes_per_page: 50_u64 * 1024 * 1024,
        dry_run: false,
      };

      let fetcher_impl = Arc::new(RecordingFetcher::default());
      let fetcher: Arc<dyn ResourceFetcher> = fetcher_impl.clone();
      prefetch_assets_for_html(
        "test",
        DOCUMENT_URL,
        html,
        DOCUMENT_URL,
        BASE_URL,
        ReferrerPolicy::default(),
        &fetcher,
        &media_ctx,
        opts,
      );

      let calls = fetcher_impl.calls.lock().unwrap().clone();
      let font_calls: Vec<_> = calls
        .iter()
        .filter(|(url, dest, _, _, _)| url == FONT && *dest == FetchDestination::Font)
        .collect();
      assert_eq!(font_calls.len(), 1, "expected a single font request");
      assert_eq!(font_calls[0].1, FetchDestination::Font);
      assert_eq!(font_calls[0].2, FetchCredentialsMode::SameOrigin);
      assert_eq!(font_calls[0].3.as_deref(), Some(STYLESHEET));
      assert_eq!(font_calls[0].4, Some(expected_origin));
    }

    fn assert_stylesheet_crossorigin_fetches(
      crossorigin_value: &str,
      expected_credentials: FetchCredentialsMode,
    ) {
      use std::sync::Mutex;

      #[derive(Default)]
      struct RecordingFetcher {
        calls: Mutex<
          Vec<(
            String,
            FetchDestination,
            FetchCredentialsMode,
            Option<String>,
            Option<DocumentOrigin>,
          )>,
        >,
      }

      impl ResourceFetcher for RecordingFetcher {
        fn fetch(&self, _url: &str) -> fastrender::Result<FetchedResource> {
          panic!("stylesheet prefetch should use fetch_with_request");
        }

        fn fetch_with_request(&self, req: FetchRequest<'_>) -> fastrender::Result<FetchedResource> {
          self.calls.lock().unwrap().push((
            req.url.to_string(),
            req.destination,
            req.credentials_mode,
            req.referrer_url.map(|r| r.to_string()),
            req.client_origin.cloned(),
          ));

          const STYLESHEET: &str = "https://cdn.test/style.css";
          const IMPORT: &str = "https://cdn.test/import.css";
          match req.url {
            STYLESHEET => {
              let mut res = FetchedResource::with_final_url(
                br#"@import "import.css"; body{color:black;}"#.to_vec(),
                Some("text/css".to_string()),
                Some(req.url.to_string()),
              );
              res.status = Some(200);
              Ok(res)
            }
            IMPORT => {
              let mut res = FetchedResource::with_final_url(
                b"body{color:red;}".to_vec(),
                Some("text/css".to_string()),
                Some(req.url.to_string()),
              );
              res.status = Some(200);
              Ok(res)
            }
            other => Err(fastrender::Error::Other(format!(
              "unexpected fetch: {other}"
            ))),
          }
        }
      }

      const DOCUMENT_URL: &str = "https://example.com/page";
      const BASE_URL: &str = "https://example.com/";
      const STYLESHEET: &str = "https://cdn.test/style.css";
      const IMPORT: &str = "https://cdn.test/import.css";

      let expected_origin = origin_from_url(DOCUMENT_URL).expect("document origin");

      let html = format!(
        r#"<!doctype html><html><head><link rel="stylesheet" crossorigin="{crossorigin_value}" href="{STYLESHEET}"></head><body></body></html>"#
      );
      let media_ctx = MediaContext::screen(800.0, 600.0);
      let opts = PrefetchOptions {
        prefetch_fonts: false,
        prefetch_images: false,
        prefetch_media: false,
        prefetch_scripts: false,
        prefetch_icons: false,
        prefetch_video_posters: false,
        prefetch_iframes: false,
        prefetch_embeds: false,
        prefetch_css_url_assets: false,
        max_discovered_assets_per_page: 2000,
        image_limits: ImagePrefetchLimits {
          max_image_elements: 150,
          max_urls_per_element: 2,
        },
        max_media_bytes_per_file: 10_u64 * 1024 * 1024,
        max_media_bytes_per_page: 50_u64 * 1024 * 1024,
        dry_run: false,
      };

      let fetcher_impl = Arc::new(RecordingFetcher::default());
      let fetcher: Arc<dyn ResourceFetcher> = fetcher_impl.clone();
      prefetch_assets_for_html(
        "test",
        DOCUMENT_URL,
        &html,
        DOCUMENT_URL,
        BASE_URL,
        ReferrerPolicy::default(),
        &fetcher,
        &media_ctx,
        opts,
      );

      let calls = fetcher_impl.calls.lock().unwrap().clone();
      let stylesheet_calls: Vec<_> = calls.iter().filter(|(url, ..)| url == STYLESHEET).collect();
      assert_eq!(
        stylesheet_calls.len(),
        1,
        "expected stylesheet fetch for {STYLESHEET}"
      );
      assert_eq!(stylesheet_calls[0].1, FetchDestination::StyleCors);
      assert_eq!(stylesheet_calls[0].2, expected_credentials);
      assert_eq!(stylesheet_calls[0].3.as_deref(), Some(DOCUMENT_URL));
      assert_eq!(stylesheet_calls[0].4, Some(expected_origin.clone()));

      let import_calls: Vec<_> = calls.iter().filter(|(url, ..)| url == IMPORT).collect();
      assert_eq!(import_calls.len(), 1, "expected import fetch for {IMPORT}");
      assert_eq!(import_calls[0].1, FetchDestination::StyleCors);
      assert_eq!(import_calls[0].2, expected_credentials);
      assert_eq!(import_calls[0].3.as_deref(), Some(STYLESHEET));
      assert_eq!(import_calls[0].4, Some(expected_origin));
    }

    #[test]
    fn stylesheet_crossorigin_anonymous_uses_cors_destination_and_inherits_for_imports() {
      assert_stylesheet_crossorigin_fetches("anonymous", FetchCredentialsMode::SameOrigin);
    }

    #[test]
    fn stylesheet_crossorigin_use_credentials_uses_cors_destination_and_inherits_for_imports() {
      assert_stylesheet_crossorigin_fetches("use-credentials", FetchCredentialsMode::Include);
    }

    #[test]
    fn font_face_prefetch_preserves_authored_order_for_supported_formats() {
      use std::sync::Mutex;

      #[derive(Default)]
      struct RecordingFetcher {
        calls: Mutex<Vec<String>>,
      }

      impl ResourceFetcher for RecordingFetcher {
        fn fetch(&self, _url: &str) -> fastrender::Result<FetchedResource> {
          panic!("font prefetch should use fetch_with_request");
        }

        fn fetch_with_request(&self, req: FetchRequest<'_>) -> fastrender::Result<FetchedResource> {
          self.calls.lock().unwrap().push(req.url.to_string());
          let content_type = if req.url.ends_with(".woff2") {
            "font/woff2"
          } else {
            "font/ttf"
          };
          let mut res = FetchedResource::with_final_url(
            b"font".to_vec(),
            Some(content_type.to_string()),
            Some(req.url.to_string()),
          );
          res.status = Some(200);
          Ok(res)
        }
      }

      let html = r#"<!doctype html><html><head><style>
@font-face { font-family: X; src: url(/font.ttf) format('truetype'), url(/font.woff2) format('woff2'); }
</style></head><body></body></html>"#;

      let document_url = "https://example.com/page";
      let base_url = "https://example.com/";
      let media_ctx = MediaContext::screen(800.0, 600.0);
      let opts = PrefetchOptions {
        prefetch_fonts: true,
        prefetch_images: false,
        prefetch_media: false,
        prefetch_scripts: false,
        prefetch_icons: false,
        prefetch_video_posters: false,
        prefetch_iframes: false,
        prefetch_embeds: false,
        prefetch_css_url_assets: false,
        max_discovered_assets_per_page: 2000,
        image_limits: ImagePrefetchLimits {
          max_image_elements: 150,
          max_urls_per_element: 2,
        },
        max_media_bytes_per_file: 10_u64 * 1024 * 1024,
        max_media_bytes_per_page: 50_u64 * 1024 * 1024,
        dry_run: false,
      };

      let fetcher_impl = Arc::new(RecordingFetcher::default());
      let fetcher: Arc<dyn ResourceFetcher> = fetcher_impl.clone();
      let summary = prefetch_assets_for_html(
        "test",
        document_url,
        html,
        document_url,
        base_url,
        ReferrerPolicy::default(),
        &fetcher,
        &media_ctx,
        opts,
      );

      let calls = fetcher_impl.calls.lock().unwrap().clone();
      assert_eq!(
        calls,
        vec!["https://example.com/font.ttf"],
        "expected authored source order to be preserved (TTF before WOFF2)"
      );
      assert_eq!(summary.fetched_fonts, 1);
      assert_eq!(summary.failed_fonts, 0);
    }

    #[test]
    fn font_face_svg_sources_are_not_fetched_via_css_url_asset_prefetch() {
      use std::sync::Mutex;

      #[derive(Default)]
      struct RecordingFetcher {
        calls: Mutex<Vec<(String, FetchDestination)>>,
      }

      impl ResourceFetcher for RecordingFetcher {
        fn fetch(&self, _url: &str) -> fastrender::Result<FetchedResource> {
          panic!("prefetch should use fetch_with_request");
        }

        fn fetch_with_request(&self, req: FetchRequest<'_>) -> fastrender::Result<FetchedResource> {
          self
            .calls
            .lock()
            .unwrap()
            .push((req.url.to_string(), req.destination));

          let (status, content_type, body) = match req.destination {
            FetchDestination::Font => (200, "font/woff2", b"font".to_vec()),
            FetchDestination::Image => (200, "image/png", b"img".to_vec()),
            _ => (200, "application/octet-stream", b"other".to_vec()),
          };

          let mut res = FetchedResource::with_final_url(
            body,
            Some(content_type.to_string()),
            Some(req.url.to_string()),
          );
          res.status = Some(status);
          Ok(res)
        }
      }

      let html = r#"<!doctype html><html><head><style>
@font-face { font-family: X; src: url(/font.svg) format('svg'), url(/font.woff2) format('woff2'); }
body { background-image: url(/bg.png); }
</style></head><body></body></html>"#;

      let document_url = "https://example.com/page";
      let base_url = "https://example.com/";
      let media_ctx = MediaContext::screen(800.0, 600.0);
      let opts = PrefetchOptions {
        prefetch_fonts: true,
        prefetch_images: false,
        prefetch_media: false,
        prefetch_scripts: false,
        prefetch_icons: false,
        prefetch_video_posters: false,
        prefetch_iframes: false,
        prefetch_embeds: false,
        prefetch_css_url_assets: true,
        max_discovered_assets_per_page: 2000,
        image_limits: ImagePrefetchLimits {
          max_image_elements: 150,
          max_urls_per_element: 2,
        },
        max_media_bytes_per_file: 10_u64 * 1024 * 1024,
        max_media_bytes_per_page: 50_u64 * 1024 * 1024,
        dry_run: false,
      };

      let fetcher_impl = Arc::new(RecordingFetcher::default());
      let fetcher: Arc<dyn ResourceFetcher> = fetcher_impl.clone();
      let summary = prefetch_assets_for_html(
        "test",
        document_url,
        html,
        document_url,
        base_url,
        ReferrerPolicy::default(),
        &fetcher,
        &media_ctx,
        opts,
      );

      let calls = fetcher_impl.calls.lock().unwrap().clone();
      assert_eq!(
        calls,
        vec![
          ("https://example.com/font.woff2".to_string(), FetchDestination::Font),
          ("https://example.com/bg.png".to_string(), FetchDestination::Image),
        ],
        "expected SVG font source to be ignored (handled neither as a supported font nor as a generic CSS url asset)"
      );

      assert_eq!(summary.fetched_fonts, 1);
      assert_eq!(summary.failed_fonts, 0);
      assert_eq!(summary.fetched_css_assets, 1);
      assert_eq!(summary.failed_css_assets, 0);
    }

    #[test]
    fn font_face_prefetch_falls_back_on_failure() {
      use std::sync::Mutex;

      #[derive(Default)]
      struct RecordingFetcher {
        calls: Mutex<Vec<String>>,
      }

      impl ResourceFetcher for RecordingFetcher {
        fn fetch(&self, _url: &str) -> fastrender::Result<FetchedResource> {
          panic!("font prefetch should use fetch_with_request");
        }

        fn fetch_with_request(&self, req: FetchRequest<'_>) -> fastrender::Result<FetchedResource> {
          self.calls.lock().unwrap().push(req.url.to_string());
          let (status, content_type) = if req.url.ends_with("/font.woff2") {
            (404, "font/woff2")
          } else {
            (200, "font/woff")
          };
          let mut res = FetchedResource::with_final_url(
            b"font".to_vec(),
            Some(content_type.to_string()),
            Some(req.url.to_string()),
          );
          res.status = Some(status);
          Ok(res)
        }
      }

      let html = r#"<!doctype html><html><head><style>
@font-face { font-family: X; src: url(/font.eot) format('embedded-opentype'), url(/font.woff2) format('woff2'), url(/font.woff) format('woff'); }
</style></head><body></body></html>"#;

      let document_url = "https://example.com/page";
      let base_url = "https://example.com/";
      let media_ctx = MediaContext::screen(800.0, 600.0);
      let opts = PrefetchOptions {
        prefetch_fonts: true,
        prefetch_images: false,
        prefetch_media: false,
        prefetch_scripts: false,
        prefetch_icons: false,
        prefetch_video_posters: false,
        prefetch_iframes: false,
        prefetch_embeds: false,
        prefetch_css_url_assets: false,
        max_discovered_assets_per_page: 2000,
        image_limits: ImagePrefetchLimits {
          max_image_elements: 150,
          max_urls_per_element: 2,
        },
        max_media_bytes_per_file: 10_u64 * 1024 * 1024,
        max_media_bytes_per_page: 50_u64 * 1024 * 1024,
        dry_run: false,
      };

      let fetcher_impl = Arc::new(RecordingFetcher::default());
      let fetcher: Arc<dyn ResourceFetcher> = fetcher_impl.clone();
      let summary = prefetch_assets_for_html(
        "test",
        document_url,
        html,
        document_url,
        base_url,
        ReferrerPolicy::default(),
        &fetcher,
        &media_ctx,
        opts,
      );

      let calls = fetcher_impl.calls.lock().unwrap().clone();
      assert_eq!(
        calls,
        vec![
          "https://example.com/font.woff2",
          "https://example.com/font.woff"
        ],
        "expected fallback to the next supported font when the preferred source fails"
      );
      assert_eq!(summary.failed_fonts, 1);
      assert_eq!(summary.fetched_fonts, 1);
    }

    #[test]
    fn font_face_prefetch_dedupes_without_hiding_failure() {
      use std::sync::Mutex;

      #[derive(Default)]
      struct RecordingFetcher {
        calls: Mutex<Vec<String>>,
      }

      impl ResourceFetcher for RecordingFetcher {
        fn fetch(&self, _url: &str) -> fastrender::Result<FetchedResource> {
          panic!("font prefetch should use fetch_with_request");
        }

        fn fetch_with_request(&self, req: FetchRequest<'_>) -> fastrender::Result<FetchedResource> {
          self.calls.lock().unwrap().push(req.url.to_string());
          let (status, content_type) = if req.url.ends_with("/font.woff2") {
            (404, "font/woff2")
          } else if req.url.ends_with("/font.woff") {
            (200, "font/woff")
          } else {
            (200, "font/ttf")
          };
          let mut res = FetchedResource::with_final_url(
            b"font".to_vec(),
            Some(content_type.to_string()),
            Some(req.url.to_string()),
          );
          res.status = Some(status);
          Ok(res)
        }
      }

      let html = r#"<!doctype html><html><head><style>
@font-face { font-family: X; src: url(/font.woff2) format('woff2'), url(/font.woff) format('woff'); }
@font-face { font-family: Y; src: url(/font.woff2) format('woff2'), url(/font.woff) format('woff'), url(/font.ttf) format('truetype'); }
</style></head><body></body></html>"#;

      let document_url = "https://example.com/page";
      let base_url = "https://example.com/";
      let media_ctx = MediaContext::screen(800.0, 600.0);
      let opts = PrefetchOptions {
        prefetch_fonts: true,
        prefetch_images: false,
        prefetch_media: false,
        prefetch_scripts: false,
        prefetch_icons: false,
        prefetch_video_posters: false,
        prefetch_iframes: false,
        prefetch_embeds: false,
        prefetch_css_url_assets: false,
        max_discovered_assets_per_page: 2000,
        image_limits: ImagePrefetchLimits {
          max_image_elements: 150,
          max_urls_per_element: 2,
        },
        max_media_bytes_per_file: 10_u64 * 1024 * 1024,
        max_media_bytes_per_page: 50_u64 * 1024 * 1024,
        dry_run: false,
      };

      let fetcher_impl = Arc::new(RecordingFetcher::default());
      let fetcher: Arc<dyn ResourceFetcher> = fetcher_impl.clone();
      let summary = prefetch_assets_for_html(
        "test",
        document_url,
        html,
        document_url,
        base_url,
        ReferrerPolicy::default(),
        &fetcher,
        &media_ctx,
        opts,
      );

      let calls = fetcher_impl.calls.lock().unwrap().clone();
      assert_eq!(
        calls,
        vec!["https://example.com/font.woff2", "https://example.com/font.woff"],
        "expected shared URLs to be fetched at most once, without fetching later fallbacks when a previous success is known"
      );
      assert_eq!(summary.failed_fonts, 1);
      assert_eq!(summary.fetched_fonts, 1);
    }

    #[test]
    fn stylesheet_http_error_is_not_parsed_for_css_url_discovery() {
      let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
      listener.set_nonblocking(true).expect("set_nonblocking");
      let addr = listener.local_addr().expect("addr");

      let done = Arc::new(AtomicBool::new(false));
      let poison_hits = Arc::new(AtomicUsize::new(0));
      let server_done = Arc::clone(&done);
      let server_poison_hits = Arc::clone(&poison_hits);
      let handle = std::thread::spawn(move || {
        while !server_done.load(Ordering::SeqCst) {
          match listener.accept() {
            Ok((mut stream, _)) => {
              let mut buf = [0u8; 4096];
              let n = stream.read(&mut buf).unwrap_or(0);
              let req = String::from_utf8_lossy(&buf[..n]);
              let path = req
                .lines()
                .next()
                .and_then(|line| line.split_ascii_whitespace().nth(1))
                .unwrap_or("/");
              let path = path.split('?').next().unwrap_or(path);

              let (status, content_type, body): (&str, &str, &[u8]) = match path {
                "/style.css" => (
                  "403 Forbidden",
                  "text/html",
                  b"<!doctype html><html><body>forbidden url(/poison.png)</body></html>",
                ),
                "/poison.png" => {
                  server_poison_hits.fetch_add(1, Ordering::SeqCst);
                  ("200 OK", "image/png", b"poison")
                }
                _ => ("404 Not Found", "text/plain", b"not found"),
              };

              let response = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: {content_type}\r\nConnection: close\r\n\r\n",
                body.len()
              );
              let _ = stream.write_all(response.as_bytes());
              let _ = stream.write_all(body);
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
              std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => break,
          }
        }
      });

      let tmp = tempfile::tempdir().expect("tempdir");
      let cache_dir = tmp.path().join("cache");

      let base = format!("http://{addr}");
      let document_url = format!("{base}/index.html");
      let html = r#"<!doctype html><html><head><link rel="stylesheet" href="/style.css"></head><body></body></html>"#;

      let http = build_http_fetcher(
        DEFAULT_USER_AGENT,
        DEFAULT_ACCEPT_LANGUAGE,
        Some(Duration::from_secs(2)),
      );
      let mut disk_config = DiskCacheConfig {
        max_bytes: 0,
        ..DiskCacheConfig::default()
      };
      disk_config.namespace = Some(disk_cache_namespace(
        DEFAULT_USER_AGENT,
        DEFAULT_ACCEPT_LANGUAGE,
      ));

      let fetcher = DiskCachingFetcher::with_configs(
        http,
        &cache_dir,
        CachingFetcherConfig {
          honor_http_cache_freshness: true,
          ..CachingFetcherConfig::default()
        },
        disk_config,
      );
      let fetcher: Arc<dyn ResourceFetcher> = Arc::new(fetcher);

      let media_ctx = MediaContext::screen(800.0, 600.0);
      let opts = PrefetchOptions {
        prefetch_fonts: false,
        prefetch_images: false,
        prefetch_media: false,
        prefetch_scripts: false,
        prefetch_icons: false,
        prefetch_video_posters: false,
        prefetch_iframes: false,
        prefetch_embeds: false,
        prefetch_css_url_assets: true,
        max_discovered_assets_per_page: 2000,
        image_limits: ImagePrefetchLimits {
          max_image_elements: 150,
          max_urls_per_element: 2,
        },
        max_media_bytes_per_file: 10_u64 * 1024 * 1024,
        max_media_bytes_per_page: 50_u64 * 1024 * 1024,
        dry_run: false,
      };

      let summary = prefetch_assets_for_html(
        "test",
        &document_url,
        html,
        &document_url,
        &document_url,
        ReferrerPolicy::default(),
        &fetcher,
        &media_ctx,
        opts,
      );

      done.store(true, Ordering::SeqCst);
      handle.join().expect("server thread");

      assert_eq!(summary.discovered_css, 1);
      assert_eq!(summary.fetched_css, 0);
      assert_eq!(summary.failed_css, 1);
      assert_eq!(summary.discovered_css_assets, 0);
      assert_eq!(summary.fetched_css_assets, 0);
      assert_eq!(summary.failed_css_assets, 0);
      assert_eq!(poison_hits.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn prefetch_warms_disk_cache_for_html_images_and_css_url_assets() {
      let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
      listener.set_nonblocking(true).expect("set_nonblocking");
      let addr = listener.local_addr().expect("addr");

      const CSS: &str =
        "@import \"/imported.css\";\nbody { background-image: url(\"/bg.png#hash\"); }\n";
      const IMPORTED_CSS: &str = "div { background-image: url(/import.png); }\n";
      const IMG_BYTES: &[u8] = b"img";
      const BG_BYTES: &[u8] = b"bg";
      const INLINE_BYTES: &[u8] = b"inline";
      const IMPORT_BYTES: &[u8] = b"import";

      let done = Arc::new(AtomicBool::new(false));
      let server_done = Arc::clone(&done);
      let handle = std::thread::spawn(move || {
        while !server_done.load(Ordering::SeqCst) {
          match listener.accept() {
            Ok((mut stream, _)) => {
              let mut buf = [0u8; 4096];
              let n = stream.read(&mut buf).unwrap_or(0);
              let req = String::from_utf8_lossy(&buf[..n]);
              let path = req
                .lines()
                .next()
                .and_then(|line| line.split_ascii_whitespace().nth(1))
                .unwrap_or("/");
              let path = path.split('?').next().unwrap_or(path);

              let (status, content_type, body): (&str, &str, &[u8]) = match path {
                "/style.css" => ("200 OK", "text/css", CSS.as_bytes()),
                "/imported.css" => ("200 OK", "text/css", IMPORTED_CSS.as_bytes()),
                "/img.png" => ("200 OK", "image/png", IMG_BYTES),
                "/bg.png" => ("200 OK", "image/png", BG_BYTES),
                "/inline.png" => ("200 OK", "image/png", INLINE_BYTES),
                "/import.png" => ("200 OK", "image/png", IMPORT_BYTES),
                _ => ("404 Not Found", "text/plain", b"not found"),
              };

              let response = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: {content_type}\r\nCache-Control: max-age=86400\r\nConnection: close\r\n\r\n",
                body.len()
              );
              let _ = stream.write_all(response.as_bytes());
              let _ = stream.write_all(body);
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
              std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => break,
          }
        }
      });

      let tmp = tempfile::tempdir().expect("tempdir");
      let cache_dir = tmp.path().join("cache");

      let base = format!("http://{addr}");
      let document_url = format!("{base}/index.html");
      let html = format!(
        r#"<!doctype html>
<html>
  <head>
    <link rel="stylesheet" href="/style.css">
  </head>
  <body style="background-image: url(/inline.png#frag)">
    <img src="/img.png#fragment">
  </body>
</html>"#
      );

      let html_path = tmp.path().join("cached.html");
      std::fs::write(&html_path, html).expect("write html");
      let mut meta_path = html_path.clone();
      meta_path.set_extension("html.meta");
      std::fs::write(
        &meta_path,
        format!("content-type: text/html\nurl: {document_url}\n"),
      )
      .expect("write meta");

      let cached = read_cached_document(&html_path).expect("read cached document");

      let http = build_http_fetcher(
        DEFAULT_USER_AGENT,
        DEFAULT_ACCEPT_LANGUAGE,
        Some(Duration::from_secs(2)),
      );
      let mut disk_config = DiskCacheConfig {
        max_bytes: 0,
        ..DiskCacheConfig::default()
      };
      disk_config.namespace = Some(disk_cache_namespace(
        DEFAULT_USER_AGENT,
        DEFAULT_ACCEPT_LANGUAGE,
      ));

      let fetcher = DiskCachingFetcher::with_configs(
        http,
        &cache_dir,
        CachingFetcherConfig {
          honor_http_cache_freshness: true,
          ..CachingFetcherConfig::default()
        },
        disk_config.clone(),
      );
      let fetcher: Arc<dyn ResourceFetcher> = Arc::new(fetcher);

      let media_ctx = MediaContext::screen(800.0, 600.0);
      let opts = PrefetchOptions {
        prefetch_fonts: false,
        prefetch_images: true,
        prefetch_media: false,
        prefetch_scripts: false,
        prefetch_icons: false,
        prefetch_video_posters: false,
        prefetch_iframes: false,
        prefetch_embeds: false,
        prefetch_css_url_assets: true,
        max_discovered_assets_per_page: 2000,
        image_limits: ImagePrefetchLimits {
          max_image_elements: 150,
          max_urls_per_element: 2,
        },
        max_media_bytes_per_file: 10_u64 * 1024 * 1024,
        max_media_bytes_per_page: 50_u64 * 1024 * 1024,
        dry_run: false,
      };

      let summary = prefetch_assets_for_html(
        "test",
        &cached.document.base_hint,
        &cached.document.html,
        &cached.document.base_hint,
        &cached.document.base_url,
        cached.document.referrer_policy,
        &fetcher,
        &media_ctx,
        opts,
      );

      done.store(true, Ordering::SeqCst);
      handle.join().expect("server thread");

      assert_eq!(summary.fetched_css, 1);
      assert_eq!(summary.fetched_imports, 1);
      assert_eq!(summary.discovered_images, 1);
      assert_eq!(summary.fetched_images, 1);
      assert_eq!(summary.failed_images, 0);
      assert_eq!(summary.discovered_css_assets, 3);
      assert_eq!(summary.fetched_css_assets, 3);
      assert_eq!(summary.failed_css_assets, 0);

      let entries: Vec<_> = std::fs::read_dir(&cache_dir)
        .expect("read cache dir")
        .map(|e| e.expect("dir entry").path())
        .collect();
      let bin_count = entries
        .iter()
        .filter(|p| p.to_string_lossy().ends_with(".bin"))
        .count();
      let meta_count = entries
        .iter()
        .filter(|p| p.to_string_lossy().ends_with(".bin.meta"))
        .count();
      assert!(
        bin_count >= 4,
        "expected at least 4 cached .bin entries, got {bin_count} (entries={entries:?})"
      );
      assert!(
        meta_count >= 4,
        "expected at least 4 cached .bin.meta entries, got {meta_count} (entries={entries:?})"
      );

      // Ensure the persisted resources are actually usable from disk without network access.
      let offline = DiskCachingFetcher::with_configs(
        PanicFetcher,
        &cache_dir,
        CachingFetcherConfig {
          honor_http_cache_freshness: true,
          ..CachingFetcherConfig::default()
        },
        disk_config,
      );
      let res = offline
        .fetch_with_request(FetchRequest::new(
          &format!("{base}/img.png"),
          FetchDestination::Image,
        ))
        .expect("disk hit");
      assert_eq!(res.bytes, IMG_BYTES);
    }

    #[test]
    fn prefetch_warms_disk_cache_for_crossorigin_images() {
      let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
      listener.set_nonblocking(true).expect("set_nonblocking");
      let addr = listener.local_addr().expect("addr");

      const IMG_BYTES: &[u8] = b"cors-img";

      let done = Arc::new(AtomicBool::new(false));
      let server_done = Arc::clone(&done);
      let handle = std::thread::spawn(move || {
        while !server_done.load(Ordering::SeqCst) {
          match listener.accept() {
            Ok((mut stream, _)) => {
              let mut buf = [0u8; 4096];
              let n = stream.read(&mut buf).unwrap_or(0);
              let req = String::from_utf8_lossy(&buf[..n]);
              let path = req
                .lines()
                .next()
                .and_then(|line| line.split_ascii_whitespace().nth(1))
                .unwrap_or("/");
              let path = path.split('?').next().unwrap_or(path);

              let (status, content_type, body): (&str, &str, &[u8]) = match path {
                "/img.png" => ("200 OK", "image/png", IMG_BYTES),
                _ => ("404 Not Found", "text/plain", b"not found"),
              };

              let response = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: {content_type}\r\nCache-Control: max-age=86400\r\nConnection: close\r\n\r\n",
                body.len()
              );
              let _ = stream.write_all(response.as_bytes());
              let _ = stream.write_all(body);
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
              std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => break,
          }
        }
      });

      let tmp = tempfile::tempdir().expect("tempdir");
      let cache_dir = tmp.path().join("cache");

      let base = format!("http://{addr}");
      let document_url = format!("{base}/index.html");
      let html = r#"<!doctype html><html><body><img crossorigin src="/img.png"></body></html>"#;

      let html_path = tmp.path().join("cached.html");
      std::fs::write(&html_path, html).expect("write html");
      let mut meta_path = html_path.clone();
      meta_path.set_extension("html.meta");
      std::fs::write(
        &meta_path,
        format!("content-type: text/html\nurl: {document_url}\n"),
      )
      .expect("write meta");

      let cached = read_cached_document(&html_path).expect("read cached document");

      let http = build_http_fetcher(
        DEFAULT_USER_AGENT,
        DEFAULT_ACCEPT_LANGUAGE,
        Some(Duration::from_secs(2)),
      );
      let mut disk_config = DiskCacheConfig {
        max_bytes: 0,
        ..DiskCacheConfig::default()
      };
      disk_config.namespace = Some(disk_cache_namespace(
        DEFAULT_USER_AGENT,
        DEFAULT_ACCEPT_LANGUAGE,
      ));

      let fetcher = DiskCachingFetcher::with_configs(
        http,
        &cache_dir,
        CachingFetcherConfig {
          honor_http_cache_freshness: true,
          ..CachingFetcherConfig::default()
        },
        disk_config.clone(),
      );
      let fetcher: Arc<dyn ResourceFetcher> = Arc::new(fetcher);

      let media_ctx = MediaContext::screen(800.0, 600.0);
      let opts = PrefetchOptions {
        prefetch_fonts: false,
        prefetch_images: true,
        prefetch_media: false,
        prefetch_scripts: false,
        prefetch_icons: false,
        prefetch_video_posters: false,
        prefetch_iframes: false,
        prefetch_embeds: false,
        prefetch_css_url_assets: false,
        max_discovered_assets_per_page: 2000,
        image_limits: ImagePrefetchLimits {
          max_image_elements: 150,
          max_urls_per_element: 2,
        },
        max_media_bytes_per_file: 10_u64 * 1024 * 1024,
        max_media_bytes_per_page: 50_u64 * 1024 * 1024,
        dry_run: false,
      };

      let summary = prefetch_assets_for_html(
        "test",
        &cached.document.base_hint,
        &cached.document.html,
        &cached.document.base_hint,
        &cached.document.base_url,
        cached.document.referrer_policy,
        &fetcher,
        &media_ctx,
        opts,
      );

      done.store(true, Ordering::SeqCst);
      handle.join().expect("server thread");

      assert_eq!(summary.discovered_images, 1);
      assert_eq!(summary.failed_images, 0);

      // Ensure the persisted resource is actually usable from disk without network access.
      let offline = DiskCachingFetcher::with_configs(
        PanicFetcher,
        &cache_dir,
        CachingFetcherConfig {
          honor_http_cache_freshness: true,
          ..CachingFetcherConfig::default()
        },
        disk_config,
      );
      let res = offline
        .fetch_with_request(FetchRequest::new(
          &format!("{base}/img.png"),
          FetchDestination::ImageCors,
        ))
        .expect("disk hit");
      assert_eq!(res.bytes, IMG_BYTES);
    }
  }

  fn log_disk_cache_stats(
    phase: &str,
    cache_dir: &Path,
    lock_stale_after: Duration,
    max_bytes: u64,
    lock_stale_secs: u64,
  ) {
    let stats = match scan_disk_cache_dir(cache_dir, lock_stale_after) {
      Ok(stats) => stats,
      Err(err) => {
        println!("Disk cache stats ({phase}): unavailable ({err})");
        return;
      }
    };

    println!(
      "Disk cache stats ({phase}): bin_count={} meta_count={} alias_count={} bin_bytes={} locks={} stale_locks={} tmp={} journal={}",
      stats.bin_count,
      stats.meta_count,
      stats.alias_count,
      stats.bin_bytes,
      stats.lock_count,
      stats.stale_lock_count,
      stats.tmp_count,
      stats.journal_bytes
    );
    println!("{}", stats.usage_summary(max_bytes));

    if max_bytes != 0 && stats.bin_bytes > max_bytes {
      println!(
        "Warning: disk cache usage exceeds max_bytes (bin_bytes={} > max_bytes={}). Consider increasing --disk-cache-max-bytes or setting FASTR_DISK_CACHE_MAX_BYTES=0 to disable eviction.",
        stats.bin_bytes, max_bytes
      );
    }
    if stats.stale_lock_count > 0 {
      println!(
        "Warning: disk cache contains {} stale .lock file(s). Consider tuning FASTR_DISK_CACHE_LOCK_STALE_SECS (currently {}).",
        stats.stale_lock_count, lock_stale_secs
      );
    }
    println!();
  }

  #[derive(Clone)]
  struct DryRunFetcher;

  impl ResourceFetcher for DryRunFetcher {
    fn fetch(&self, url: &str) -> fastrender::Result<FetchedResource> {
      Err(fastrender::Error::Other(format!(
        "dry-run: network/disk fetch disabled ({url})"
      )))
    }
  }

  pub fn main() {
    let args = Args::parse();
    if args.capabilities {
      crate::print_capabilities(true);
      return;
    }

    if args.jobs == 0 {
      eprintln!("jobs must be > 0");
      std::process::exit(2);
    }

    if args.prefetch_images {
      if args.max_images_per_page == 0 {
        eprintln!("max-images-per-page must be > 0 when --prefetch-images is enabled");
        std::process::exit(2);
      }
      if args.max_image_urls_per_element == 0 {
        eprintln!("max-image-urls-per-element must be > 0 when --prefetch-images is enabled");
        std::process::exit(2);
      }
    }

    let timeout_secs = args.timeout.seconds(Some(30));
    let per_request_timeout_label = timeout_secs.unwrap_or(0);

    let page_filter = args
      .pages
      .as_ref()
      .and_then(|pages| PagesetFilter::from_inputs(pages));

    let entries = pageset_entries();
    let selected = selected_pages(&entries, page_filter.as_ref(), args.shard);
    if selected.is_empty() {
      if page_filter.is_some() {
        println!("No pages matched the provided filter");
      } else {
        println!("No pages to prefetch");
      }
      std::process::exit(1);
    }

    if let Some(filter) = &page_filter {
      let missing = filter.unmatched(&selected);
      if !missing.is_empty() {
        println!("Warning: unknown pages in filter: {}", missing.join(", "));
      }
    }

    let media_ctx = MediaContext::screen(
      args.viewport.viewport.0 as f32,
      args.viewport.viewport.1 as f32,
    )
    .with_device_pixel_ratio(args.viewport.dpr)
    .with_env_overrides();
    let mut disk_config = args.disk_cache.to_config();
    let lock_stale_after = disk_config.lock_stale_after;
    let fetcher: Arc<dyn ResourceFetcher> = if args.dry_run {
      Arc::new(DryRunFetcher)
    } else {
      let http = build_http_fetcher(
        &args.user_agent,
        &args.accept_language,
        timeout_secs.map(Duration::from_secs),
      );
      disk_config.namespace = Some(disk_cache_namespace(
        &args.user_agent,
        &args.accept_language,
      ));
      Arc::new(DiskCachingFetcher::with_configs(
        http,
        args.cache_dir.clone(),
        CachingFetcherConfig {
          honor_http_cache_freshness: true,
          ..CachingFetcherConfig::default()
        },
        disk_config,
      ))
    };

    let image_limits = ImagePrefetchLimits {
      max_image_elements: args.max_images_per_page,
      max_urls_per_element: args.max_image_urls_per_element,
    };
    let opts = PrefetchOptions {
      prefetch_fonts: args.prefetch_fonts,
      prefetch_images: args.prefetch_images,
      prefetch_media: args.prefetch_media,
      prefetch_scripts: args.prefetch_scripts,
      prefetch_icons: args.prefetch_icons,
      prefetch_video_posters: args.prefetch_video_posters,
      prefetch_iframes: args.prefetch_iframes,
      prefetch_embeds: args.prefetch_embeds,
      prefetch_css_url_assets: args.prefetch_css_url_assets,
      max_discovered_assets_per_page: args.max_discovered_assets_per_page,
      image_limits,
      max_media_bytes_per_file: args.max_media_bytes_per_file,
      max_media_bytes_per_page: args.max_media_bytes_per_page,
      dry_run: args.dry_run,
    };
    let prefetch_any_images =
      args.prefetch_images || args.prefetch_icons || args.prefetch_video_posters;
    let prefetch_any_media = args.prefetch_media;
    let prefetch_any_documents = args.prefetch_iframes || args.prefetch_embeds;

    println!(
      "Prefetching assets for {} page(s) ({} parallel, {}s timeout, fonts={} images={} media={} scripts={} iframes={} embeds={} icons={} video_posters={} css_url_assets={} max_assets_per_page={} dry_run={})...",
      selected.len(),
      args.jobs,
      per_request_timeout_label,
      args.prefetch_fonts,
      args.prefetch_images,
      args.prefetch_media,
      args.prefetch_scripts,
      args.prefetch_iframes,
      args.prefetch_embeds,
      args.prefetch_icons,
      args.prefetch_video_posters,
      args.prefetch_css_url_assets,
      args.max_discovered_assets_per_page,
      args.dry_run
    );
    if args.prefetch_images
      || args.prefetch_media
      || args.prefetch_scripts
      || args.prefetch_iframes
      || args.prefetch_embeds
      || args.prefetch_icons
      || args.prefetch_video_posters
    {
      println!(
        "HTML assets: images={} media={} scripts={} documents={} embeds={} icons={} video_posters={}",
        args.prefetch_images,
        args.prefetch_media,
        args.prefetch_scripts,
        args.prefetch_iframes,
        args.prefetch_embeds,
        args.prefetch_icons,
        args.prefetch_video_posters
      );
    }
    if args.prefetch_images {
      println!(
        "Image prefetch limits: max_images_per_page={} max_urls_per_element={}",
        args.max_images_per_page, args.max_image_urls_per_element
      );
    }
    if args.prefetch_media {
      println!(
        "Media prefetch limits: max_media_bytes_per_file={} max_media_bytes_per_page={}",
        args.max_media_bytes_per_file, args.max_media_bytes_per_page
      );
    }
    if let Some((index, total)) = args.shard {
      println!("Shard: {}/{}", index, total);
    }
    println!("Cache dir: {}", args.cache_dir.display());
    let max_age = if args.disk_cache.max_age_secs == 0 {
      "none".to_string()
    } else {
      format!("{}s", args.disk_cache.max_age_secs)
    };
    println!(
      "Disk cache: max_bytes={} max_age={}",
      args.disk_cache.max_bytes, max_age
    );
    log_disk_cache_stats(
      "start",
      &args.cache_dir,
      lock_stale_after,
      args.disk_cache.max_bytes,
      args.disk_cache.lock_stale_secs,
    );
    println!();

    let pool = match ThreadPoolBuilder::new().num_threads(args.jobs).build() {
      Ok(pool) => pool,
      Err(err) => {
        eprintln!(
          "Failed to create thread pool with {} job(s): {err}",
          args.jobs
        );
        std::process::exit(2);
      }
    };

    let mut results: Vec<PageSummary> = pool.install(|| {
      selected
        .par_iter()
        .map(|entry| prefetch_page(entry, &fetcher, &media_ctx, opts))
        .collect()
    });

    results.sort_by(|a, b| a.stem.cmp(&b.stem));

    let report_requested = args.report_json.is_some() || args.report_per_page_dir.is_some();
    let report = report_requested.then(|| {
      build_prefetch_assets_report(
        &results,
        &args.cache_dir,
        args.dry_run,
        args.max_report_urls_per_kind,
      )
    });
    if let Some(report) = &report {
      if let Some(path) = &args.report_json {
        if let Err(err) = write_prefetch_assets_report(path, report) {
          eprintln!("Failed to write --report-json {}: {err}", path.display());
          std::process::exit(2);
        }
      }
      if let Some(dir) = &args.report_per_page_dir {
        for page in &report.pages {
          let path = report_per_page_path(dir, &page.stem);
          let single = PrefetchAssetsReport {
            version: report.version,
            cache_dir: report.cache_dir.clone(),
            dry_run: report.dry_run,
            max_report_urls_per_kind: report.max_report_urls_per_kind,
            pages: vec![page.clone()],
          };
          if let Err(err) = write_prefetch_assets_report(&path, &single) {
            eprintln!(
              "Failed to write --report-per-page-dir file {}: {err}",
              path.display()
            );
            std::process::exit(2);
          }
        }
      }
    }

    let mut total_discovered = 0usize;
    let mut total_fetched = 0usize;
    let mut total_failed = 0usize;
    let mut total_skipped = 0usize;
    let mut total_imports_fetched = 0usize;
    let mut total_imports_failed = 0usize;
    let mut total_fonts_fetched = 0usize;
    let mut total_fonts_failed = 0usize;
    let mut total_images_discovered = 0usize;
    let mut total_images_fetched = 0usize;
    let mut total_images_failed = 0usize;
    let mut total_media_discovered = 0usize;
    let mut total_media_fetched = 0usize;
    let mut total_media_failed = 0usize;
    let mut total_media_skipped = 0usize;
    let mut total_scripts_discovered = 0usize;
    let mut total_scripts_fetched = 0usize;
    let mut total_scripts_failed = 0usize;
    let mut total_documents_discovered = 0usize;
    let mut total_documents_fetched = 0usize;
    let mut total_documents_failed = 0usize;
    let mut total_css_assets_discovered = 0usize;
    let mut total_css_assets_fetched = 0usize;
    let mut total_css_assets_failed = 0usize;

    for r in &results {
      if r.skipped {
        total_skipped += 1;
        println!("• {} (no cached HTML, skipped)", r.stem);
        continue;
      }
      total_discovered += r.discovered_css;
      total_fetched += r.fetched_css;
      total_failed += r.failed_css;
      total_imports_fetched += r.fetched_imports;
      total_imports_failed += r.failed_imports;
      total_fonts_fetched += r.fetched_fonts;
      total_fonts_failed += r.failed_fonts;
      total_images_discovered += r.discovered_images;
      total_images_fetched += r.fetched_images;
      total_images_failed += r.failed_images;
      total_media_discovered += r.discovered_media;
      total_media_fetched += r.fetched_media;
      total_media_failed += r.failed_media;
      total_media_skipped += r.skipped_media;
      total_scripts_discovered += r.discovered_scripts;
      total_scripts_fetched += r.fetched_scripts;
      total_scripts_failed += r.failed_scripts;
      total_documents_discovered += r.discovered_documents;
      total_documents_fetched += r.fetched_documents;
      total_documents_failed += r.failed_documents;
      total_css_assets_discovered += r.discovered_css_assets;
      total_css_assets_fetched += r.fetched_css_assets;
      total_css_assets_failed += r.failed_css_assets;

      let mut line = format!(
        "• {} css={} fetched={} failed={} imports_fetched={} imports_failed={} fonts_fetched={} fonts_failed={}",
        r.stem,
        r.discovered_css,
        r.fetched_css,
        r.failed_css,
        r.fetched_imports,
        r.failed_imports,
        r.fetched_fonts,
        r.failed_fonts
      );
      if prefetch_any_images {
        line.push_str(&format!(
          " images={} img_fetched={} img_failed={}",
          r.discovered_images, r.fetched_images, r.failed_images
        ));
      }
      if prefetch_any_media {
        line.push_str(&format!(
          " media={} media_fetched={} media_failed={} media_skipped={}",
          r.discovered_media, r.fetched_media, r.failed_media, r.skipped_media
        ));
      }
      if args.prefetch_scripts {
        line.push_str(&format!(
          " scripts={} scripts_fetched={} scripts_failed={}",
          r.discovered_scripts, r.fetched_scripts, r.failed_scripts
        ));
      }
      if prefetch_any_documents {
        line.push_str(&format!(
          " docs={} docs_fetched={} docs_failed={}",
          r.discovered_documents, r.fetched_documents, r.failed_documents
        ));
      }
      if args.prefetch_css_url_assets {
        line.push_str(&format!(
          " css_assets={} css_assets_fetched={} css_assets_failed={}",
          r.discovered_css_assets, r.fetched_css_assets, r.failed_css_assets
        ));
      }
      println!("{line}");
    }

    println!();
    let mut done = format!(
      "Done: css_discovered={} css_fetched={} css_failed={} pages_skipped={} imports_fetched={} imports_failed={} fonts_fetched={} fonts_failed={}",
      total_discovered,
      total_fetched,
      total_failed,
      total_skipped,
      total_imports_fetched,
      total_imports_failed,
      total_fonts_fetched,
      total_fonts_failed
    );
    if prefetch_any_images {
      done.push_str(&format!(
        " images_discovered={} images_fetched={} images_failed={}",
        total_images_discovered, total_images_fetched, total_images_failed
      ));
    }
    if prefetch_any_media {
      done.push_str(&format!(
        " media_discovered={} media_fetched={} media_failed={} media_skipped={}",
        total_media_discovered, total_media_fetched, total_media_failed, total_media_skipped
      ));
    }
    if args.prefetch_scripts {
      done.push_str(&format!(
        " scripts_discovered={} scripts_fetched={} scripts_failed={}",
        total_scripts_discovered, total_scripts_fetched, total_scripts_failed
      ));
    }
    if prefetch_any_documents {
      done.push_str(&format!(
        " docs_discovered={} docs_fetched={} docs_failed={}",
        total_documents_discovered, total_documents_fetched, total_documents_failed
      ));
    }
    if args.prefetch_css_url_assets {
      done.push_str(&format!(
        " css_assets_discovered={} css_assets_fetched={} css_assets_failed={}",
        total_css_assets_discovered, total_css_assets_fetched, total_css_assets_failed
      ));
    }
    println!("{done}");
    println!();
    log_disk_cache_stats(
      "end",
      &args.cache_dir,
      lock_stale_after,
      args.disk_cache.max_bytes,
      args.disk_cache.lock_stale_secs,
    );

    // Best-effort tool: do not fail the process on fetch errors.
  }
}

#[cfg(feature = "disk_cache")]
fn main() {
  disk_cache_main::main();
}
