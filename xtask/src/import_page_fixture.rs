use anyhow::{bail, Context, Result};
use clap::Args;
use fastrender::css::loader::resolve_href;
use fastrender::resource::bundle::{Bundle, BundledResourceInfo};
use fastrender::resource::is_data_url;
use fastrender::resource::FetchedResource;
use regex::Regex;
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use url::Url;

const DEFAULT_FIXTURE_ROOT: &str = "tests/pages/fixtures";
const ASSETS_DIR: &str = "assets";
const HASH_PREFIX_BYTES: usize = 16;
pub(crate) const DEFAULT_MEDIA_MAX_BYTES: u64 = 5 * 1024 * 1024;
pub(crate) const DEFAULT_MEDIA_MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Args, Debug)]
pub struct ImportPageFixtureArgs {
  /// Bundle directory or .tar produced by `bundle_page fetch`
  pub bundle: PathBuf,

  /// Name of the fixture directory to create under `--output-root`
  pub fixture_name: String,

  /// Root directory for fixtures (defaults to tests/pages/fixtures)
  #[arg(long, default_value = DEFAULT_FIXTURE_ROOT)]
  pub output_root: PathBuf,

  /// Allow replacing an existing fixture directory
  #[arg(long)]
  pub overwrite: bool,

  /// Vendor media sources (`<video src>`, `<audio src>`, `<source src>`, `<track src>`) into the
  /// fixture assets.
  ///
  /// By default, media sources are rewritten to deterministic empty placeholder files so imported
  /// fixtures stay small and can be committed safely. Use this flag when you need the offline
  /// fixture to contain **playable** media (for example when testing the browser UI).
  ///
  /// Safety: media vendoring is subject to size budgets (see `--media-max-bytes` and
  /// `--media-max-file-bytes`) to avoid accidentally committing huge blobs.
  #[arg(long)]
  pub include_media: bool,

  /// Maximum total bytes of vendored media assets when `--include-media` is set (0 = unlimited).
  ///
  /// Defaults to 5MiB.
  #[arg(long, value_name = "BYTES", default_value_t = DEFAULT_MEDIA_MAX_BYTES)]
  pub media_max_bytes: u64,

  /// Maximum bytes per vendored media asset when `--include-media` is set (0 = unlimited).
  ///
  /// Defaults to 2MiB.
  #[arg(long, value_name = "BYTES", default_value_t = DEFAULT_MEDIA_MAX_FILE_BYTES)]
  pub media_max_file_bytes: u64,

  /// Replace missing resources with empty placeholder assets instead of failing the import
  #[arg(long)]
  pub allow_missing: bool,

  /// Allow leaving http(s) references in rewritten HTML/CSS (skips validation)
  #[arg(long)]
  pub allow_http_references: bool,

  /// Use the legacy HTML rewrite behavior (rewrites every src/href/poster/data attribute).
  ///
  /// The default behavior only rewrites URLs in places FastRender actually fetches as
  /// subresources (stylesheets, images, etc.).
  #[arg(long)]
  pub legacy_rewrite: bool,

  /// Rewrite script subresources (<script src>, script preloads, modulepreload) so JS-capable
  /// fixtures can be fully offline.
  ///
  /// This is opt-in so existing JS-off fixtures keep their original script references.
  #[arg(long)]
  pub rewrite_scripts: bool,

  /// Validate without writing any files
  #[arg(long)]
  pub dry_run: bool,
}

pub fn run_import_page_fixture(mut args: ImportPageFixtureArgs) -> Result<()> {
  let repo_root = crate::repo_root();
  if !args.bundle.is_absolute() {
    args.bundle = repo_root.join(&args.bundle);
  }
  if !args.output_root.is_absolute() {
    args.output_root = repo_root.join(&args.output_root);
  }

  let fixture_dir = args.output_root.join(&args.fixture_name);
  if fixture_dir.exists() {
    if !args.overwrite {
      bail!(
        "Fixture directory {} already exists; pass --overwrite to replace it",
        fixture_dir.display()
      );
    }
    if !args.dry_run {
      fs::remove_dir_all(&fixture_dir).with_context(|| {
        format!(
          "failed to clear existing fixture at {}",
          fixture_dir.display()
        )
      })?;
    }
  }

  let bundle = Bundle::load(&args.bundle)
    .with_context(|| format!("failed to load bundle at {}", args.bundle.display()))?;
  let manifest = bundle.manifest().clone();
  let (doc_meta, doc_bytes) = bundle.document();
  let document_html = String::from_utf8_lossy(&doc_bytes).to_string();
  let document_base = if doc_meta.final_url.is_empty() {
    manifest.original_url.clone()
  } else {
    doc_meta.final_url.clone()
  };
  let document_base_url =
    Url::parse(&document_base).context("failed to parse document base URL from bundle")?;
  let effective_base = find_base_url(&document_html, &document_base_url);

  let mut catalog = AssetCatalog::new(args.allow_missing);
  let mut media_bytes_total: u64 = 0;
  let mut media_asset_names: HashSet<String> = HashSet::new();
  for (url, info) in &manifest.resources {
    // Bundle manifests can include synthetic keys for internal bookkeeping, such as:
    // - `@@fastr:bundle:vary_v1@@...` for `Vary`-partitioned resources
    // - `@@fastr:bundle:req_v*@@...` for CORS cache partitioning
    //
    // Offline fixtures only need one on-disk file per *real* URL referenced by HTML/CSS, so skip
    // synthetic keys when the base URL entry is present. (If the base entry is missing, fall back
    // to importing the synthetic key.)
    if should_skip_synthetic_bundle_key(url, &manifest.resources) {
      continue;
    }
    let resource = bundle
      .fetch_manifest_entry(url)
      .with_context(|| format!("failed to read bundled resource for {}", url))?;

    if is_media_resource(info, &resource) {
      if !args.include_media {
        // Default: do not import media blobs into offline fixtures. Media elements are rewritten to
        // deterministic placeholder files so fixtures remain small and offline-safe.
        continue;
      }

      // When media vendoring is enabled, apply conservative size budgets to avoid accidentally
      // committing huge files.
      let size_bytes = u64::try_from(resource.bytes.len()).unwrap_or(u64::MAX);
      if args.media_max_file_bytes != 0 && size_bytes > args.media_max_file_bytes {
        bail!(
          "media resource {} is {} bytes, exceeding the per-file limit of {} bytes; \
           pass --media-max-file-bytes to override (or omit --include-media to use placeholders)",
          url,
          size_bytes,
          args.media_max_file_bytes
        );
      }

      // Media assets use a content-hash filename, so dedupe budget accounting by the final filename.
      let ext = extension_from_resource(&info.path, resource.content_type.as_deref());
      let filename = format!("{}.{}", hash_bytes(&resource.bytes), ext);
      if media_asset_names.insert(filename) {
        media_bytes_total = media_bytes_total.saturating_add(size_bytes);
        if args.media_max_bytes != 0 && media_bytes_total > args.media_max_bytes {
          bail!(
            "vendored media exceeds the total budget ({} bytes > {} bytes); \
             pass --media-max-bytes to override (or omit --include-media to use placeholders)",
            media_bytes_total,
            args.media_max_bytes
          );
        }
      }
    }
    catalog.add_resource(url, info, &resource)?;
  }

  // Some sites (notably MDN) ship JS-driven live-sample iframes that have placeholder `src`
  // attributes, but include `data-live-path` + `data-live-id` metadata and embed the HTML/CSS
  // source inline as code blocks. If the bundler didn't capture the derived iframe HTML (common
  // when the authored `src` is `about:blank`), synthesize deterministic HTML assets from the
  // embedded sample code so offline fixtures still exercise the demos in JS-off mode.
  inject_live_sample_iframe_assets(&document_html, &effective_base, &mut catalog)?;

  catalog.rewrite_stylesheets()?;
  catalog.rewrite_html_assets(args.legacy_rewrite, args.rewrite_scripts)?;

  let rewritten_html = rewrite_html(
    &document_html,
    &effective_base,
    ReferenceContext::Html,
    &mut catalog,
    args.legacy_rewrite,
    args.rewrite_scripts,
  )?;

  let rewritten_html = rewrite_mdn_live_sample_iframes(
    &rewritten_html,
    &effective_base,
    ReferenceContext::Html,
    &mut catalog,
    args.legacy_rewrite,
    args.rewrite_scripts,
  )?;

  catalog.fail_if_missing()?;

  if !args.allow_http_references {
    validate_no_remote_fetchable_subresources_in_html(
      "index.html",
      &rewritten_html,
      args.rewrite_scripts,
    )?;
    catalog.validate_no_remote_fetchable_subresources_in_css()?;
    catalog.validate_no_remote_fetchable_subresources_in_html_assets(args.rewrite_scripts)?;
  }

  if args.dry_run {
    println!(
      "✓ Dry run: {} assets would be written to {}",
      catalog.assets.len(),
      fixture_dir.display()
    );
    return Ok(());
  }

  let assets_dir = fixture_dir.join(ASSETS_DIR);
  fs::create_dir_all(&assets_dir)
    .with_context(|| format!("failed to create assets directory {}", assets_dir.display()))?;
  fs::write(fixture_dir.join("index.html"), rewritten_html.as_bytes()).with_context(|| {
    format!(
      "failed to write {}",
      fixture_dir.join("index.html").display()
    )
  })?;

  for asset in catalog.assets.values() {
    fs::write(assets_dir.join(&asset.filename), &asset.bytes).with_context(|| {
      format!(
        "failed to write asset {}",
        assets_dir.join(&asset.filename).display()
      )
    })?;
  }

  println!(
    "✓ Imported bundle {} into {} ({} assets)",
    args.bundle.display(),
    fixture_dir.display(),
    catalog.assets.len()
  );

  Ok(())
}

fn should_skip_synthetic_bundle_key(
  key: &str,
  manifest: &BTreeMap<String, BundledResourceInfo>,
) -> bool {
  const VARY_SENTINEL: &str = "@@fastr:bundle:vary_v1@@";
  const REQ_SENTINEL: &str = "@@fastr:bundle:req_v";

  if let Some((base, _)) = key.split_once(VARY_SENTINEL) {
    return manifest.contains_key(base);
  }
  if let Some((base, _)) = key.split_once(REQ_SENTINEL) {
    return manifest.contains_key(base);
  }
  false
}

fn is_media_resource(info: &BundledResourceInfo, res: &FetchedResource) -> bool {
  if let Some(content_type) = res.content_type.as_deref().or(info.content_type.as_deref()) {
    let mime = content_type
      .split(';')
      .next()
      .unwrap_or(content_type)
      .trim()
      .to_ascii_lowercase();
    if mime.starts_with("video/")
      || mime.starts_with("audio/")
      || mime == "text/vtt"
      || mime == "application/vnd.apple.mpegurl"
      || mime == "application/dash+xml"
    {
      return true;
    }
  }

  // Fall back to extension detection because media content is often served as
  // `application/octet-stream` or with a missing/incorrect Content-Type.
  let ext = extension_from_resource(&info.path, res.content_type.as_deref());
  matches!(
    ext.as_str(),
    "mp4"
      | "m4v"
      | "mov"
      | "webm"
      | "mkv"
      | "mp3"
      | "m4a"
      | "aac"
      | "wav"
      | "ogg"
      | "oga"
      | "ogv"
      | "opus"
      | "flac"
      | "vtt"
      | "srt"
      | "m3u8"
      | "mpd"
  )
}

fn inject_live_sample_iframe_assets(
  document_html: &str,
  base_url: &Url,
  catalog: &mut AssetCatalog,
) -> Result<()> {
  #[derive(Default)]
  struct LiveSampleBlocks {
    html: Vec<String>,
    css: Vec<String>,
    js: Vec<String>,
  }

  fn extract_blocks(html: &str, live_id: &str) -> LiveSampleBlocks {
    fn capture_first_match<'t>(
      caps: &regex::Captures<'t>,
      groups: &[usize],
    ) -> Option<regex::Match<'t>> {
      groups.iter().find_map(|idx| caps.get(*idx))
    }

    // MDN uses `live-sample---<id>` today, but older dumps have used `live-sample___<id>`.
    let pre_regex = Regex::new(&format!(
      "(?is)<pre\\b[^>]*\\b(?:live-sample---{}|live-sample___{})\\b[^>]*>.*?</pre>",
      regex::escape(live_id),
      regex::escape(live_id),
    ))
    .expect("live sample <pre> regex must compile");
    let class_attr =
      Regex::new("(?is)(?:^|\\s)class\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
        .expect("class attr regex must compile");
    let brush_lang =
      Regex::new("(?i)\\bbrush:\\s*([a-z0-9_-]+)").expect("brush lang regex must compile");
    let code_inner =
      Regex::new("(?is)<code\\b[^>]*>(?P<body>.*?)</code>").expect("code inner regex must compile");
    let strip_tags =
      Regex::new("(?is)<[^>]+>").expect("strip tags regex must compile for code blocks");

    let mut blocks = LiveSampleBlocks::default();
    for pre_match in pre_regex.find_iter(html) {
      let pre = &html[pre_match.start()..pre_match.end()];

      let class_value = class_attr
        .captures(pre)
        .and_then(|caps| capture_first_match(&caps, &[1, 2, 3]))
        .map(|m| m.as_str().to_string())
        .unwrap_or_default();

      let lang = brush_lang
        .captures(&class_value)
        .and_then(|caps| caps.get(1).map(|m| m.as_str().to_ascii_lowercase()))
        .or_else(|| {
          if class_value.to_ascii_lowercase().contains("language-html") {
            Some("html".to_string())
          } else if class_value.to_ascii_lowercase().contains("language-css") {
            Some("css".to_string())
          } else if class_value.to_ascii_lowercase().contains("language-js")
            || class_value
              .to_ascii_lowercase()
              .contains("language-javascript")
          {
            Some("js".to_string())
          } else {
            None
          }
        });

      let Some(lang) = lang else {
        continue;
      };

      let raw_code = code_inner
        .captures(pre)
        .and_then(|caps| caps.name("body").map(|m| m.as_str().to_string()))
        .unwrap_or_default();
      let without_markup = strip_tags.replace_all(&raw_code, "");
      let decoded = decode_html_entities_if_needed(without_markup.trim())
        .trim()
        .to_string();
      if decoded.is_empty() {
        continue;
      }

      match lang.as_str() {
        "html" => blocks.html.push(decoded),
        "css" => blocks.css.push(decoded),
        "js" | "javascript" => blocks.js.push(decoded),
        _ => {}
      }
    }
    blocks
  }

  fn build_live_sample_document(blocks: LiveSampleBlocks) -> Option<String> {
    if blocks.html.is_empty() && blocks.css.is_empty() && blocks.js.is_empty() {
      return None;
    }

    let html_body = blocks.html.join("\n");
    let css = blocks.css.join("\n\n");
    let js = blocks.js.join("\n\n");

    let mut out = String::new();
    out.push_str("<!doctype html>\n<html>\n<head>\n<meta charset=\"utf-8\">\n");
    if !css.is_empty() {
      out.push_str("<style>\n");
      out.push_str(&css);
      out.push_str("\n</style>\n");
    }
    out.push_str("</head>\n<body>\n");
    out.push_str(&html_body);
    if !js.is_empty() {
      out.push_str("\n<script>\n");
      out.push_str(&js);
      out.push_str("\n</script>\n");
    }
    out.push_str("\n</body>\n</html>\n");
    Some(out)
  }

  fn capture_first_match<'t>(
    caps: &regex::Captures<'t>,
    groups: &[usize],
  ) -> Option<regex::Match<'t>> {
    groups.iter().find_map(|idx| caps.get(*idx))
  }

  let iframe_tag = Regex::new("(?is)<iframe\\b[^>]*>").expect("iframe tag regex must compile");
  let attr_src = Regex::new("(?is)(?:^|\\s)src\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
    .expect("iframe src attr regex must compile");
  let attr_data_live_path =
    Regex::new("(?is)(?:^|\\s)data-live-path\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
      .expect("data-live-path attr regex must compile");
  let attr_data_live_id =
    Regex::new("(?is)(?:^|\\s)data-live-id\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
      .expect("data-live-id attr regex must compile");

  let mut seen: HashSet<String> = HashSet::new();
  for tag_match in iframe_tag.find_iter(document_html) {
    let tag = &document_html[tag_match.start()..tag_match.end()];

    let src_match = attr_src
      .captures(tag)
      .and_then(|caps| capture_first_match(&caps, &[1, 2, 3]));
    let src_is_placeholder = src_match
      .as_ref()
      .map(|m| {
        let value = decode_html_entities_if_needed(m.as_str());
        fastrender::dom::img_src_is_placeholder(value.trim())
      })
      .unwrap_or(true);

    if !src_is_placeholder {
      continue;
    }

    let live_path = attr_data_live_path
      .captures(tag)
      .and_then(|caps| capture_first_match(&caps, &[1, 2, 3]))
      .map(|m| {
        decode_html_entities_if_needed(m.as_str())
          .trim()
          .to_string()
      })
      .filter(|s| !s.is_empty());
    let live_id = attr_data_live_id
      .captures(tag)
      .and_then(|caps| capture_first_match(&caps, &[1, 2, 3]))
      .map(|m| {
        decode_html_entities_if_needed(m.as_str())
          .trim()
          .to_string()
      })
      .filter(|s| !s.is_empty());

    let (Some(path), Some(id)) = (live_path, live_id) else {
      continue;
    };

    let synthesized = format!("{path}{id}.html");
    let Some(resolved) = resolve_href(base_url.as_str(), &synthesized) else {
      continue;
    };

    if !seen.insert(resolved.clone()) {
      continue;
    }
    if catalog.url_to_filename.contains_key(&resolved) {
      continue;
    }

    // Attempt to synthesize the iframe HTML from embedded live-sample code blocks.
    let blocks = extract_blocks(document_html, &id);
    let Some(live_html) = build_live_sample_document(blocks) else {
      continue;
    };

    let info = BundledResourceInfo {
      path: format!("synthetic/{}.html", id),
      content_type: Some("text/html; charset=utf-8".to_string()),
      nosniff: false,
      status: Some(200),
      final_url: Some(resolved.clone()),
      etag: None,
      last_modified: None,
      response_referrer_policy: None,
      response_headers: None,
      vary: None,
      access_control_allow_origin: None,
      timing_allow_origin: None,
      access_control_allow_credentials: false,
    };
    let res = FetchedResource::new(live_html.into_bytes(), info.content_type.clone());
    catalog.add_resource(&resolved, &info, &res)?;
  }

  Ok(())
}

#[derive(Clone, Copy, Debug)]
enum ReferenceContext {
  Html,
  Css,
}

#[derive(Clone, Debug)]
struct AssetData {
  filename: String,
  bytes: Vec<u8>,
  content_type: Option<String>,
  source_url: String,
}

#[derive(Default, Debug)]
struct AssetCatalog {
  assets: BTreeMap<String, AssetData>,
  url_to_filename: HashMap<String, String>,
  allow_missing: bool,
  missing_urls: BTreeSet<String>,
}

impl AssetCatalog {
  fn new(allow_missing: bool) -> Self {
    Self {
      assets: BTreeMap::new(),
      url_to_filename: HashMap::new(),
      allow_missing,
      missing_urls: BTreeSet::new(),
    }
  }

  fn add_resource(
    &mut self,
    url: &str,
    info: &BundledResourceInfo,
    res: &FetchedResource,
  ) -> Result<()> {
    let source_url = info.final_url.clone().unwrap_or_else(|| url.to_string());
    let sniffed_font_ext = sniff_font_extension(&res.bytes);
    let mut ext = extension_from_resource(&info.path, res.content_type.as_deref());
    if let Some(font_ext) = sniffed_font_ext {
      ext = font_ext.to_string();
    }
    // Some CDNs (notably `fonts.gstatic.com/l/font?...`) mislabel fonts as `text/html`. These
    // resources are still valid WOFF2 binaries and must not be treated as HTML during fixture
    // rewriting (otherwise they get corrupted by UTF-8 lossy decoding).
    let content_type = sniffed_font_ext
      .map(|ext| format!("font/{ext}"))
      .or_else(|| res.content_type.clone());
    // HTML/CSS rewriting depends on the document's base URL, so byte-based deduplication can
    // incorrectly collapse multiple distinct source URLs into one rewritten output.
    let base_sensitive = sniffed_font_ext.is_none()
      && (is_html_extension(&ext)
        || ext == "css"
        || content_type
          .as_deref()
          .map(|ct| {
            let lower = ct.to_ascii_lowercase();
            lower.contains("html") || lower.contains("css")
          })
          .unwrap_or(false));
    let filename_hash = if base_sensitive {
      hash_bytes(source_url.as_bytes())
    } else {
      hash_bytes(&res.bytes)
    };
    let filename = format!("{filename_hash}.{ext}");
    if let Some(existing) = self.assets.get(&filename) {
      if existing.bytes != res.bytes {
        bail!(
          "hash collision while importing {} ({}); existing asset has different contents",
          url,
          filename
        );
      }
    }

    let data = AssetData {
      filename: filename.clone(),
      bytes: res.bytes.clone(),
      content_type,
      source_url,
    };

    self.assets.insert(filename.clone(), data);
    self
      .url_to_filename
      .insert(url.to_string(), filename.clone());
    if let Some((without_fragment, _)) = url.split_once('#') {
      self
        .url_to_filename
        .entry(without_fragment.to_string())
        .or_insert_with(|| filename.clone());
    }
    let decoded_url = decode_html_entities_if_needed(url);
    if decoded_url.as_ref() != url {
      self
        .url_to_filename
        .entry(decoded_url.to_string())
        .or_insert_with(|| filename.clone());
      if let Some((without_fragment, _)) = decoded_url.as_ref().split_once('#') {
        self
          .url_to_filename
          .entry(without_fragment.to_string())
          .or_insert_with(|| filename.clone());
      }
    }
    if let Some(final_url) = &info.final_url {
      self
        .url_to_filename
        .entry(final_url.clone())
        .or_insert_with(|| filename.clone());
      if let Some((without_fragment, _)) = final_url.split_once('#') {
        self
          .url_to_filename
          .entry(without_fragment.to_string())
          .or_insert_with(|| filename.clone());
      }
      let decoded_final = decode_html_entities_if_needed(final_url);
      if decoded_final.as_ref() != final_url {
        self
          .url_to_filename
          .entry(decoded_final.to_string())
          .or_insert_with(|| filename.clone());
        if let Some((without_fragment, _)) = decoded_final.as_ref().split_once('#') {
          self
            .url_to_filename
            .entry(without_fragment.to_string())
            .or_insert_with(|| filename.clone());
        }
      }
    }
    Ok(())
  }

  fn rewrite_stylesheets(&mut self) -> Result<()> {
    let names: Vec<String> = self.assets.keys().cloned().collect();
    for name in names {
      let Some(asset) = self.assets.get(&name).cloned() else {
        continue;
      };
      let is_css = asset
        .content_type
        .as_deref()
        .map(|ct| ct.to_ascii_lowercase().contains("css"))
        .unwrap_or_else(|| name.to_ascii_lowercase().ends_with(".css"));

      if !is_css {
        continue;
      }

      let base = Url::parse(&asset.source_url).with_context(|| {
        format!(
          "stylesheet {} has unparsable URL {}",
          name, asset.source_url
        )
      })?;
      let css = String::from_utf8_lossy(&asset.bytes).to_string();
      let rewritten = rewrite_css(&css, &base, self, ReferenceContext::Css)
        .with_context(|| format!("failed to rewrite stylesheet {}", name))?;
      self.assets.insert(
        name.clone(),
        AssetData {
          filename: asset.filename.clone(),
          bytes: rewritten.into_bytes(),
          content_type: asset.content_type.clone(),
          source_url: asset.source_url.clone(),
        },
      );
    }

    Ok(())
  }

  fn rewrite_html_assets(&mut self, legacy_rewrite: bool, rewrite_scripts: bool) -> Result<()> {
    let names: Vec<String> = self.assets.keys().cloned().collect();
    for name in names {
      let Some(asset) = self.assets.get(&name).cloned() else {
        continue;
      };
      if !is_html_asset(&asset) {
        continue;
      }

      let base = Url::parse(&asset.source_url).with_context(|| {
        format!(
          "HTML asset {} has unparsable URL {}",
          name, asset.source_url
        )
      })?;
      let html = String::from_utf8_lossy(&asset.bytes).to_string();
      let effective_base = find_base_url(&html, &base);
      // HTML assets are written under `assets/`, so their rewritten subresource URLs must be
      // relative to the assets directory (same as CSS).
      let rewritten = rewrite_html(
        &html,
        &effective_base,
        ReferenceContext::Css,
        self,
        legacy_rewrite,
        rewrite_scripts,
      )
      .with_context(|| format!("failed to rewrite HTML asset {}", name))?;

      self.assets.insert(
        name.clone(),
        AssetData {
          filename: asset.filename.clone(),
          bytes: rewritten.into_bytes(),
          content_type: asset.content_type.clone(),
          source_url: asset.source_url.clone(),
        },
      );
    }
    Ok(())
  }

  fn path_for(&mut self, url: &str, ctx: ReferenceContext) -> Option<String> {
    if let Some(filename) = self.url_to_filename.get(url) {
      return match ctx {
        ReferenceContext::Html => Some(format!("{ASSETS_DIR}/{filename}")),
        ReferenceContext::Css => Some(filename.clone()),
      };
    }

    if !self.allow_missing {
      self.missing_urls.insert(url.to_string());
      return None;
    }

    let ext = extension_from_path(url);
    let filename = format!("missing_{}.{}", hash_bytes(url.as_bytes()), ext);
    if !self.assets.contains_key(&filename) {
      self.assets.insert(
        filename.clone(),
        AssetData {
          filename: filename.clone(),
          bytes: Vec::new(),
          content_type: None,
          source_url: url.to_string(),
        },
      );
    }
    self
      .url_to_filename
      .entry(url.to_string())
      .or_insert_with(|| filename.clone());

    match ctx {
      ReferenceContext::Html => Some(format!("{ASSETS_DIR}/{filename}")),
      ReferenceContext::Css => Some(filename),
    }
  }

  /// Like `path_for`, but always returns a deterministic placeholder path for missing resources.
  ///
  /// This is intended for URLs that are fetchable in browsers (e.g. preloads, media sources) but
  /// are not required for FastRender output correctness.
  fn path_for_optional(&mut self, url: &str, ctx: ReferenceContext) -> String {
    if let Some(filename) = self.url_to_filename.get(url) {
      return match ctx {
        ReferenceContext::Html => format!("{ASSETS_DIR}/{filename}"),
        ReferenceContext::Css => filename.clone(),
      };
    }

    let ext = extension_from_path(url);
    let filename = format!("missing_{}.{}", hash_bytes(url.as_bytes()), ext);
    if !self.assets.contains_key(&filename) {
      self.assets.insert(
        filename.clone(),
        AssetData {
          filename: filename.clone(),
          bytes: Vec::new(),
          content_type: None,
          source_url: url.to_string(),
        },
      );
    }
    self
      .url_to_filename
      .entry(url.to_string())
      .or_insert_with(|| filename.clone());

    match ctx {
      ReferenceContext::Html => format!("{ASSETS_DIR}/{filename}"),
      ReferenceContext::Css => filename,
    }
  }

  fn fail_if_missing(&self) -> Result<()> {
    if self.allow_missing || self.missing_urls.is_empty() {
      return Ok(());
    }

    let mut msg = String::from("bundle is missing required subresources:\n");
    for url in &self.missing_urls {
      msg.push_str("  - ");
      msg.push_str(url);
      msg.push('\n');
    }
    msg.push_str(
      "\nRe-run with --allow-missing to create deterministic empty placeholder assets.\n",
    );
    bail!(msg)
  }

  fn validate_no_remote_fetchable_subresources_in_css(&self) -> Result<()> {
    let mut remote: BTreeSet<String> = BTreeSet::new();

    for asset in self.assets.values() {
      let is_css = asset
        .content_type
        .as_deref()
        .map(|ct| ct.to_ascii_lowercase().contains("css"))
        .unwrap_or_else(|| asset.filename.to_ascii_lowercase().ends_with(".css"));
      if !is_css {
        continue;
      }

      let css = String::from_utf8_lossy(&asset.bytes);
      for url in extract_fetchable_css_urls(&css) {
        if is_remote_fetch_url(&url) {
          remote.insert(url);
        }
      }
    }

    if remote.is_empty() {
      return Ok(());
    }

    let mut msg = String::from("rewritten CSS still contains remote fetchable subresources:\n");
    for url in &remote {
      msg.push_str("  - ");
      msg.push_str(url);
      msg.push('\n');
    }
    bail!(msg)
  }

  fn validate_no_remote_fetchable_subresources_in_html_assets(
    &self,
    validate_scripts: bool,
  ) -> Result<()> {
    for asset in self.assets.values() {
      if !is_html_asset(asset) {
        continue;
      }
      let html = String::from_utf8_lossy(&asset.bytes);
      validate_no_remote_fetchable_subresources_in_html(&asset.filename, &html, validate_scripts)?;
    }
    Ok(())
  }
}

fn rewrite_html(
  input: &str,
  base_url: &Url,
  ctx: ReferenceContext,
  catalog: &mut AssetCatalog,
  legacy_rewrite: bool,
  rewrite_scripts: bool,
) -> Result<String> {
  let mut rewritten = input.to_string();

  // FastRender parses HTML with `scripting_enabled=true` by default (see `DomParseOptions`), which
  // means `<noscript>` contents are not parsed as markup and therefore cannot fetch subresources.
  //
  // Strip these blocks so:
  // - we don't rewrite/require assets that will never be fetched, and
  // - offline fixture validation doesn't flag remote URLs embedded in `<noscript>`.
  let noscript_regex =
    Regex::new("(?is)<noscript\\b[^>]*>.*?</noscript>").expect("noscript regex must compile");
  rewritten = noscript_regex.replace_all(&rewritten, "").to_string();

  // Normalize <base href> to keep the output local.
  let base_tag_regex = Regex::new(
    "(?is)(?P<prefix><base[^>]*href\\s*=\\s*[\"'])(?P<url>[^\"'>]+)(?P<suffix>[\"'][^>]*>)",
  )
  .expect("base regex must compile");
  rewritten = apply_rewrite(
    &base_tag_regex,
    &rewritten,
    base_url,
    ctx,
    catalog,
    Some("."),
  )?;

  if legacy_rewrite {
    let attr_regex = Regex::new(
      "(?is)(?P<prefix>(?:src|href|poster|data)\\s*=\\s*[\"'])(?P<url>[^\"'>]+)(?P<suffix>[\"'])",
    )
    .expect("attr regex must compile");
    rewritten = apply_rewrite(&attr_regex, &rewritten, base_url, ctx, catalog, None)?;

    let content_url_regex = Regex::new(
      "(?is)(?P<prefix>content\\s*=\\s*[\"'])(?P<url>https?://[^\"'>]+)(?P<suffix>[\"'])",
    )
    .expect("content url regex must compile");
    rewritten = apply_rewrite(
      &content_url_regex,
      &rewritten,
      base_url,
      ctx,
      catalog,
      Some("."),
    )?;

    let srcset_regex =
      Regex::new("(?is)(?P<prefix>srcset\\s*=\\s*[\"'])(?P<value>[^\"']+)(?P<suffix>[\"'])")
        .expect("srcset regex must compile");
    let mut srcset_error: Option<anyhow::Error> = None;
    rewritten = srcset_regex
      .replace_all(
        &rewritten,
        |caps: &regex::Captures<'_>| match rewrite_srcset(&caps["value"], base_url, ctx, catalog) {
          Ok(value) => format!("{}{}{}", &caps["prefix"], value, &caps["suffix"]),
          Err(err) => {
            srcset_error = Some(err);
            caps[0].to_string()
          }
        },
      )
      .to_string();
    if let Some(err) = srcset_error {
      return Err(err);
    }
  } else {
    rewritten = rewrite_html_resource_attrs(&rewritten, base_url, ctx, catalog, rewrite_scripts)?;
  }

  let mut style_attr_error: Option<anyhow::Error> = None;
  let style_attr_double =
    Regex::new("(?is)(?P<prefix>\\sstyle\\s*=\\s*\")(?P<css>[^\"]*)(?P<suffix>\")")
      .expect("style attr regex must compile");
  rewritten = style_attr_double
    .replace_all(&rewritten, |caps: &regex::Captures<'_>| {
      let css = &caps["css"];
      match rewrite_css(css, base_url, catalog, ctx) {
        Ok(css) => format!("{}{}{}", &caps["prefix"], css, &caps["suffix"]),
        Err(err) => {
          style_attr_error = Some(err);
          caps[0].to_string()
        }
      }
    })
    .to_string();

  let style_attr_single =
    Regex::new("(?is)(?P<prefix>\\sstyle\\s*=\\s*')(?P<css>[^']*)(?P<suffix>')")
      .expect("style attr regex must compile");
  rewritten = style_attr_single
    .replace_all(&rewritten, |caps: &regex::Captures<'_>| {
      let css = &caps["css"];
      match rewrite_css(css, base_url, catalog, ctx) {
        Ok(css) => format!("{}{}{}", &caps["prefix"], css, &caps["suffix"]),
        Err(err) => {
          style_attr_error = Some(err);
          caps[0].to_string()
        }
      }
    })
    .to_string();

  if let Some(err) = style_attr_error {
    return Err(err);
  }

  let style_tag_regex =
    Regex::new("(?is)(?P<prefix><style[^>]*>)(?P<body>.*?)(?P<suffix></style>)")
      .expect("style tag regex must compile");
  let mut style_tag_error: Option<anyhow::Error> = None;
  rewritten = style_tag_regex
    .replace_all(&rewritten, |caps: &regex::Captures<'_>| {
      match rewrite_css(&caps["body"], base_url, catalog, ctx) {
        Ok(css) => format!("{}{}{}", &caps["prefix"], css, &caps["suffix"]),
        Err(err) => {
          style_tag_error = Some(err);
          caps[0].to_string()
        }
      }
    })
    .to_string();
  if let Some(err) = style_tag_error {
    return Err(err);
  }

  Ok(rewritten)
}

#[derive(Default, Debug)]
struct MdnLiveSampleBlocks {
  html: Vec<String>,
  css: Vec<String>,
  js: Vec<String>,
}

fn mdn_parse_live_sample_id(class_value: &str) -> Option<String> {
  class_value
    .split_ascii_whitespace()
    .find_map(|token| token.strip_prefix("live-sample---"))
    .map(|id| id.to_string())
}

fn mdn_parse_brush_language(class_value: &str) -> Option<String> {
  let tokens: Vec<&str> = class_value.split_ascii_whitespace().collect();
  for (idx, token) in tokens.iter().enumerate() {
    let lower = token.to_ascii_lowercase();
    if lower == "brush:" {
      if let Some(next) = tokens.get(idx + 1) {
        return Some(next.trim_end_matches(';').to_ascii_lowercase());
      }
    } else if let Some(after) = lower.strip_prefix("brush:") {
      let after = after.trim().trim_end_matches(';');
      if !after.is_empty() {
        return Some(after.to_ascii_lowercase());
      }
      if let Some(next) = tokens.get(idx + 1) {
        return Some(next.trim_end_matches(';').to_ascii_lowercase());
      }
    }
  }
  None
}

fn mdn_build_live_sample_document(blocks: &MdnLiveSampleBlocks) -> String {
  let html = blocks.html.join("\n");
  let css = blocks.css.join("\n");
  let js = blocks.js.join("\n");

  fn push_with_trailing_newline(out: &mut String, s: &str) {
    if s.is_empty() {
      return;
    }
    out.push_str(s);
    if !s.ends_with('\n') {
      out.push('\n');
    }
  }

  let mut out = String::new();
  out.push_str("<!doctype html>\n<html>\n<head>\n<meta charset=\"utf-8\">\n");
  if !css.trim().is_empty() {
    out.push_str("<style>\n");
    push_with_trailing_newline(&mut out, &css);
    out.push_str("</style>\n");
  }
  out.push_str("</head>\n<body>\n");
  push_with_trailing_newline(&mut out, &html);
  if !js.trim().is_empty() {
    out.push_str("<script>\n");
    push_with_trailing_newline(&mut out, &js);
    out.push_str("</script>\n");
  }
  out.push_str("</body>\n</html>\n");
  out
}

/// MDN "live sample" pages embed the iframe source in `<pre class="... live-sample---<id>">`
/// blocks, and use `about:blank` iframe placeholders that are normally populated by client-side JS.
///
/// When importing offline fixtures, generate a minimal HTML document for each referenced sample id
/// (CSS + HTML + JS blocks) and rewrite the iframe to point at a deterministic local asset.
fn rewrite_mdn_live_sample_iframes(
  input: &str,
  base_url: &Url,
  ctx: ReferenceContext,
  catalog: &mut AssetCatalog,
  legacy_rewrite: bool,
  rewrite_scripts: bool,
) -> Result<String> {
  fn capture_first_match<'t>(
    caps: &'t regex::Captures<'t>,
    groups: &[usize],
  ) -> Option<regex::Match<'t>> {
    groups.iter().find_map(|idx| caps.get(*idx))
  }

  let pre_block = Regex::new(
    r#"(?is)<pre\b[^>]*\bclass\s*=\s*(?:"(?P<class_d>[^"]*)"|'(?P<class_s>[^']*)'|(?P<class_u>[^\s>]+))[^>]*>\s*<code\b[^>]*>(?P<code>.*?)</code>\s*</pre>"#,
  )
  .expect("mdn live sample pre regex must compile");

  let mut samples: HashMap<String, MdnLiveSampleBlocks> = HashMap::new();
  for caps in pre_block.captures_iter(input) {
    let class = caps
      .name("class_d")
      .or_else(|| caps.name("class_s"))
      .or_else(|| caps.name("class_u"))
      .map(|m| m.as_str())
      .unwrap_or_default();

    if !class.contains("live-sample---") {
      continue;
    }

    let Some(id) = mdn_parse_live_sample_id(class) else {
      continue;
    };
    let Some(lang) = mdn_parse_brush_language(class) else {
      continue;
    };

    let code_raw = caps
      .name("code")
      .map(|m| m.as_str())
      .unwrap_or_default();
    let code = decode_html_entities(code_raw);

    let entry = samples.entry(id).or_default();
    match lang.as_str() {
      "html" => entry.html.push(code),
      "css" => entry.css.push(code),
      "js" | "javascript" => entry.js.push(code),
      _ => {}
    }
  }

  if samples.is_empty() {
    return Ok(input.to_string());
  }

  let iframe_tag = Regex::new("(?is)<iframe\\b[^>]*>").expect("mdn iframe tag regex");
  let attr_data_live_id =
    Regex::new("(?is)(?:^|\\s)data-live-id\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
      .expect("data-live-id regex");
  let attr_src =
    Regex::new("(?is)(?:^|\\s)src\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
      .expect("src attr regex");

  let mut generated: HashMap<String, String> = HashMap::new();

  let mut out = String::with_capacity(input.len());
  let mut last = 0usize;
  for tag_match in iframe_tag.find_iter(input) {
    let tag = tag_match.as_str();
    let rewritten_tag = if let Some(id_caps) = attr_data_live_id.captures(tag) {
      let live_id_raw = capture_first_match(&id_caps, &[1, 2, 3])
        .map(|m| m.as_str())
        .unwrap_or_default();
      let live_id = decode_html_entities_if_needed(live_id_raw).trim().to_string();

      let src_is_about_blank = attr_src
        .captures(tag)
        .and_then(|caps| capture_first_match(&caps, &[1, 2, 3]).map(|m| m.as_str().to_string()))
        .map(|raw| {
          let decoded = decode_html_entities_if_needed(&raw);
          let trimmed = strip_wrapping_quotes(decoded.trim());
          trimmed.eq_ignore_ascii_case("about:blank") || trimmed.is_empty()
        })
        .unwrap_or(true);

      if src_is_about_blank {
        if let Some(blocks) = samples.get(&live_id) {
          // Only generate an asset if we have at least one block (MDN samples always have HTML/CSS,
          // but keep the importer resilient to unexpected pages).
          if !(blocks.html.is_empty() && blocks.css.is_empty() && blocks.js.is_empty()) {
            let filename = if let Some(existing) = generated.get(&live_id) {
              existing.clone()
            } else {
              let doc = mdn_build_live_sample_document(blocks);
              let rewritten_doc = rewrite_html(
                &doc,
                base_url,
                ReferenceContext::Css,
                catalog,
                legacy_rewrite,
                rewrite_scripts,
              )?;
              let bytes = rewritten_doc.into_bytes();
              let filename = format!("{}.html", hash_bytes(&bytes));

              if let Some(existing_asset) = catalog.assets.get(&filename) {
                if existing_asset.bytes != bytes {
                  bail!(
                    "hash collision while generating MDN live sample {} ({}); existing asset has different contents",
                    live_id,
                    filename
                  );
                }
              } else {
                catalog.assets.insert(
                  filename.clone(),
                  AssetData {
                    filename: filename.clone(),
                    bytes,
                    content_type: Some("text/html".to_string()),
                    source_url: format!("mdn-live-sample:{live_id}"),
                  },
                );
              }

              generated.insert(live_id.clone(), filename.clone());
              filename
            };

            let new_src = match ctx {
              ReferenceContext::Html => format!("{ASSETS_DIR}/{filename}"),
              ReferenceContext::Css => filename,
            };

            // Rewrite src= if present, otherwise insert one.
            if let Some(src_caps) = attr_src.captures(tag) {
              if let Some(src_match) = capture_first_match(&src_caps, &[1, 2, 3]) {
                let start = src_match.start();
                let end = src_match.end();
                format!("{}{}{}", &tag[..start], new_src, &tag[end..])
              } else {
                tag.to_string()
              }
            } else {
              let mut tag = tag.to_string();
              if let Some(close_idx) = tag.rfind('>') {
                let mut insert_pos = close_idx;
                let mut cursor = close_idx;
                while cursor > 0 && tag.as_bytes()[cursor - 1].is_ascii_whitespace() {
                  cursor -= 1;
                }
                if cursor > 0 && tag.as_bytes()[cursor - 1] == b'/' {
                  insert_pos = cursor - 1;
                }
                tag.insert_str(insert_pos, &format!(" src=\"{new_src}\""));
              }
              tag
            }
          } else {
            tag.to_string()
          }
        } else {
          // Missing `<pre class="... live-sample---{id}">` blocks for this iframe; leave it alone.
          tag.to_string()
        }
      } else {
        tag.to_string()
      }
    } else {
      tag.to_string()
    };

    out.push_str(&input[last..tag_match.start()]);
    out.push_str(&rewritten_tag);
    last = tag_match.end();
  }
  out.push_str(&input[last..]);
  Ok(out)
}

fn apply_rewrite(
  regex: &Regex,
  input: &str,
  base_url: &Url,
  ctx: ReferenceContext,
  catalog: &mut AssetCatalog,
  force_value: Option<&str>,
) -> Result<String> {
  let mut error: Option<anyhow::Error> = None;
  let rewritten = regex
    .replace_all(input, |caps: &regex::Captures<'_>| {
      let new_value = if let Some(forced) = force_value {
        Some(forced.to_string())
      } else {
        match rewrite_reference(&caps["url"], base_url, ctx, catalog) {
          Ok(value) => value,
          Err(err) => {
            error = Some(err);
            None
          }
        }
      };

      match new_value {
        Some(value) => format!("{}{}{}", &caps["prefix"], value, &caps["suffix"]),
        None => caps[0].to_string(),
      }
    })
    .to_string();

  if let Some(err) = error {
    return Err(err);
  }

  Ok(rewritten)
}

fn rewrite_srcset(
  input: &str,
  base_url: &Url,
  ctx: ReferenceContext,
  catalog: &mut AssetCatalog,
) -> Result<String> {
  rewrite_srcset_with_limit(input, base_url, ctx, catalog, usize::MAX)
}

fn rewrite_css(
  input: &str,
  base_url: &Url,
  catalog: &mut AssetCatalog,
  ctx: ReferenceContext,
) -> Result<String> {
  let pruned = prune_css_font_face_sources(input, base_url);
  let url_regex =
    // Some real pages ship malformed url() values (e.g. missing the closing ')'). We still want to
    // rewrite the URL so the imported fixture is fully offline/deterministic.
    Regex::new("(?i)(?P<prefix>url\\(\\s*[\"']?)(?P<url>[^\"')]+)(?P<suffix>[\"']?\\s*\\)?)")
      .expect("url regex must compile");
  let import_regex =
    Regex::new("(?i)(?P<prefix>@import\\s*['\"])(?P<url>[^\"']+)(?P<suffix>['\"])")
      .expect("import regex must compile");

  let mut rewritten = apply_rewrite(&url_regex, &pruned, base_url, ctx, catalog, None)?;
  rewritten = apply_rewrite(&import_regex, &rewritten, base_url, ctx, catalog, None)?;
  Ok(rewritten)
}

fn prune_css_font_face_sources(input: &str, base_url: &Url) -> String {
  use cssparser::{Parser, ParserInput, ToCss, Token};

  fn font_face_url_is_decodable(url: &str) -> bool {
    let lower_owned = url.to_ascii_lowercase();
    let lower = lower_owned.as_str();
    let mut end = lower.len();
    if let Some(idx) = lower.find('#') {
      end = end.min(idx);
    }
    if let Some(idx) = lower.find('?') {
      end = end.min(idx);
    }
    let lower = &lower[..end];
    !(lower.ends_with(".eot") || lower.ends_with(".svg") || lower.ends_with(".svgz"))
  }

  fn push_token_to_css(out: &mut String, token: &Token) {
    match token {
      Token::WhiteSpace(ws) => out.push_str(ws.as_ref()),
      Token::Comment(text) => {
        out.push_str("/*");
        out.push_str(text.as_ref());
        out.push_str("*/");
      }
      Token::QuotedString(text) => {
        let raw = text.as_ref();
        if !raw.contains('\'') {
          out.push('\'');
          out.push_str(raw);
          out.push('\'');
        } else if !raw.contains('"') {
          out.push('"');
          out.push_str(raw);
          out.push('"');
        } else {
          token
            .to_css(out)
            .expect("writing to String should be infallible");
        }
      }
      Token::UnquotedUrl(url) => {
        out.push_str("url(");
        out.push_str(url.as_ref());
        out.push(')');
      }
      other => other
        .to_css(out)
        .expect("writing to String should be infallible"),
    }
  }

  fn skip_tokens<'i, 't>(parser: &mut Parser<'i, 't>) {
    while !parser.is_exhausted() {
      let token = match parser.next_including_whitespace_and_comments() {
        Ok(token) => token,
        Err(_) => break,
      };
      match token {
        Token::Function(_)
        | Token::ParenthesisBlock
        | Token::SquareBracketBlock
        | Token::CurlyBracketBlock => {
          let _ = parser.parse_nested_block(|nested| {
            skip_tokens(nested);
            Ok::<_, cssparser::ParseError<'i, ()>>(())
          });
        }
        _ => {}
      }
    }
  }

  fn scan_plain<'i, 't>(parser: &mut Parser<'i, 't>, base_url: &Url, out: &mut String) {
    while !parser.is_exhausted() {
      let token = match parser.next_including_whitespace_and_comments() {
        Ok(token) => token,
        Err(_) => break,
      };
      match token {
        Token::Function(name) => {
          out.push_str(name.as_ref());
          out.push('(');
          let _ = parser.parse_nested_block(|nested| {
            scan_plain(nested, base_url, out);
            Ok::<_, cssparser::ParseError<'i, ()>>(())
          });
          out.push(')');
        }
        Token::ParenthesisBlock => {
          out.push('(');
          let _ = parser.parse_nested_block(|nested| {
            scan_plain(nested, base_url, out);
            Ok::<_, cssparser::ParseError<'i, ()>>(())
          });
          out.push(')');
        }
        Token::SquareBracketBlock => {
          out.push('[');
          let _ = parser.parse_nested_block(|nested| {
            scan_plain(nested, base_url, out);
            Ok::<_, cssparser::ParseError<'i, ()>>(())
          });
          out.push(']');
        }
        Token::CurlyBracketBlock => {
          out.push('{');
          let _ = parser.parse_nested_block(|nested| {
            scan_plain(nested, base_url, out);
            Ok::<_, cssparser::ParseError<'i, ()>>(())
          });
          out.push('}');
        }
        other => push_token_to_css(out, other),
      }
    }
  }

  fn consume_font_face_src_declaration<'i, 't>(
    parser: &mut Parser<'i, 't>,
    base_url: &Url,
  ) -> Option<String> {
    let mut selected: Option<String> = None;
    let mut current = String::new();
    let mut current_has_decodable_url = false;
    let mut skipping = false;

    while !parser.is_exhausted() {
      let token = match parser.next_including_whitespace_and_comments() {
        Ok(token) => token,
        Err(_) => break,
      };

      match token {
        Token::Semicolon => break,
        Token::Comma if !skipping => {
          if current_has_decodable_url && selected.is_none() {
            selected = Some(current);
            skipping = true;
            current = String::new();
            current_has_decodable_url = false;
          } else {
            current.clear();
            current_has_decodable_url = false;
          }
        }
        Token::UnquotedUrl(url) => {
          if skipping {
            continue;
          }
          let url_str = url.as_ref();
          if !current_has_decodable_url {
            let decoded = decode_html_entities_if_needed(url_str);
            let resolved = resolve_href(base_url.as_str(), decoded.as_ref())
              .unwrap_or_else(|| decoded.as_ref().to_string());
            if font_face_url_is_decodable(&resolved) {
              current_has_decodable_url = true;
            }
          }
          current.push_str("url(");
          current.push_str(url_str);
          current.push(')');
        }
        Token::Function(name) if name.eq_ignore_ascii_case("url") => {
          if skipping {
            let _ = parser.parse_nested_block(|nested| {
              skip_tokens(nested);
              Ok::<_, cssparser::ParseError<'i, ()>>(())
            });
            continue;
          }

          current.push_str("url(");
          let mut arg: Option<String> = None;
          let _ = parser.parse_nested_block(|nested| {
            while !nested.is_exhausted() {
              let inner = match nested.next_including_whitespace_and_comments() {
                Ok(token) => token,
                Err(_) => break,
              };

              match inner {
                Token::WhiteSpace(_) | Token::Comment(_) => {}
                Token::QuotedString(s) | Token::UnquotedUrl(s) => {
                  if arg.is_none() {
                    arg = Some(s.as_ref().to_string());
                  }
                }
                Token::Ident(s) => {
                  if arg.is_none() {
                    arg = Some(s.as_ref().to_string());
                  }
                }
                _ => {}
              }

              match inner {
                Token::Function(inner_name) => {
                  current.push_str(inner_name.as_ref());
                  current.push('(');
                  let _ = nested.parse_nested_block(|nested2| {
                    scan_plain(nested2, base_url, &mut current);
                    Ok::<_, cssparser::ParseError<'i, ()>>(())
                  });
                  current.push(')');
                }
                Token::ParenthesisBlock => {
                  current.push('(');
                  let _ = nested.parse_nested_block(|nested2| {
                    scan_plain(nested2, base_url, &mut current);
                    Ok::<_, cssparser::ParseError<'i, ()>>(())
                  });
                  current.push(')');
                }
                Token::SquareBracketBlock => {
                  current.push('[');
                  let _ = nested.parse_nested_block(|nested2| {
                    scan_plain(nested2, base_url, &mut current);
                    Ok::<_, cssparser::ParseError<'i, ()>>(())
                  });
                  current.push(']');
                }
                Token::CurlyBracketBlock => {
                  current.push('{');
                  let _ = nested.parse_nested_block(|nested2| {
                    scan_plain(nested2, base_url, &mut current);
                    Ok::<_, cssparser::ParseError<'i, ()>>(())
                  });
                  current.push('}');
                }
                other => push_token_to_css(&mut current, other),
              }
            }
            Ok::<_, cssparser::ParseError<'i, ()>>(())
          });
          current.push(')');

          if !current_has_decodable_url {
            if let Some(arg) = arg {
              let decoded = decode_html_entities_if_needed(arg.trim());
              let resolved = resolve_href(base_url.as_str(), decoded.as_ref())
                .unwrap_or_else(|| decoded.as_ref().to_string());
              if font_face_url_is_decodable(&resolved) {
                current_has_decodable_url = true;
              }
            }
          }
        }
        Token::Function(name) => {
          if skipping {
            let _ = parser.parse_nested_block(|nested| {
              skip_tokens(nested);
              Ok::<_, cssparser::ParseError<'i, ()>>(())
            });
            continue;
          }

          current.push_str(name.as_ref());
          current.push('(');
          let _ = parser.parse_nested_block(|nested| {
            scan_plain(nested, base_url, &mut current);
            Ok::<_, cssparser::ParseError<'i, ()>>(())
          });
          current.push(')');
        }
        Token::ParenthesisBlock => {
          if skipping {
            let _ = parser.parse_nested_block(|nested| {
              skip_tokens(nested);
              Ok::<_, cssparser::ParseError<'i, ()>>(())
            });
            continue;
          }
          current.push('(');
          let _ = parser.parse_nested_block(|nested| {
            scan_plain(nested, base_url, &mut current);
            Ok::<_, cssparser::ParseError<'i, ()>>(())
          });
          current.push(')');
        }
        Token::SquareBracketBlock => {
          if skipping {
            let _ = parser.parse_nested_block(|nested| {
              skip_tokens(nested);
              Ok::<_, cssparser::ParseError<'i, ()>>(())
            });
            continue;
          }
          current.push('[');
          let _ = parser.parse_nested_block(|nested| {
            scan_plain(nested, base_url, &mut current);
            Ok::<_, cssparser::ParseError<'i, ()>>(())
          });
          current.push(']');
        }
        Token::CurlyBracketBlock => {
          if skipping {
            let _ = parser.parse_nested_block(|nested| {
              skip_tokens(nested);
              Ok::<_, cssparser::ParseError<'i, ()>>(())
            });
            continue;
          }
          current.push('{');
          let _ = parser.parse_nested_block(|nested| {
            scan_plain(nested, base_url, &mut current);
            Ok::<_, cssparser::ParseError<'i, ()>>(())
          });
          current.push('}');
        }
        other => {
          if skipping {
            continue;
          }
          push_token_to_css(&mut current, other);
        }
      }
    }

    if selected.is_none() && current_has_decodable_url && !skipping {
      selected = Some(current);
    }

    selected
  }

  fn scan_font_face_block<'i, 't>(parser: &mut Parser<'i, 't>, base_url: &Url, out: &mut String) {
    let mut at_decl_start = true;
    let mut src_emitted = false;

    while !parser.is_exhausted() {
      let token = match parser.next_including_whitespace_and_comments() {
        Ok(token) => token,
        Err(_) => break,
      };

      match token {
        Token::Semicolon => {
          push_token_to_css(out, token);
          at_decl_start = true;
        }
        Token::WhiteSpace(_) | Token::Comment(_) => {
          push_token_to_css(out, token);
        }
        Token::Ident(ident) if at_decl_start && ident.eq_ignore_ascii_case("src") => {
          let mut saw_colon = false;
          while !parser.is_exhausted() {
            let token = match parser.next_including_whitespace_and_comments() {
              Ok(token) => token,
              Err(_) => break,
            };
            match token {
              Token::Colon => {
                saw_colon = true;
                break;
              }
              Token::Semicolon => break,
              Token::Function(_)
              | Token::ParenthesisBlock
              | Token::SquareBracketBlock
              | Token::CurlyBracketBlock => {
                let _ = parser.parse_nested_block(|nested| {
                  skip_tokens(nested);
                  Ok::<_, cssparser::ParseError<'i, ()>>(())
                });
              }
              _ => {}
            }
          }

          if !saw_colon {
            at_decl_start = true;
            continue;
          }

          let selected = consume_font_face_src_declaration(parser, base_url);
          at_decl_start = true;

          if src_emitted {
            continue;
          }
          if let Some(selected) = selected {
            out.push_str("src:");
            out.push_str(&selected);
            out.push(';');
            src_emitted = true;
          }
        }
        Token::Function(name) => {
          at_decl_start = false;
          out.push_str(name.as_ref());
          out.push('(');
          let _ = parser.parse_nested_block(|nested| {
            scan_plain(nested, base_url, out);
            Ok::<_, cssparser::ParseError<'i, ()>>(())
          });
          out.push(')');
        }
        Token::ParenthesisBlock => {
          at_decl_start = false;
          out.push('(');
          let _ = parser.parse_nested_block(|nested| {
            scan_plain(nested, base_url, out);
            Ok::<_, cssparser::ParseError<'i, ()>>(())
          });
          out.push(')');
        }
        Token::SquareBracketBlock => {
          at_decl_start = false;
          out.push('[');
          let _ = parser.parse_nested_block(|nested| {
            scan_plain(nested, base_url, out);
            Ok::<_, cssparser::ParseError<'i, ()>>(())
          });
          out.push(']');
        }
        Token::CurlyBracketBlock => {
          at_decl_start = false;
          out.push('{');
          let _ = parser.parse_nested_block(|nested| {
            scan_plain(nested, base_url, out);
            Ok::<_, cssparser::ParseError<'i, ()>>(())
          });
          out.push('}');
        }
        other => {
          at_decl_start = false;
          push_token_to_css(out, other);
        }
      }
    }
  }

  fn scan<'i, 't>(parser: &mut Parser<'i, 't>, base_url: &Url, out: &mut String) {
    let mut next_font_face_block = false;
    while !parser.is_exhausted() {
      let token = match parser.next_including_whitespace_and_comments() {
        Ok(token) => token,
        Err(_) => break,
      };

      match token {
        Token::AtKeyword(name) if name.eq_ignore_ascii_case("font-face") => {
          next_font_face_block = true;
          push_token_to_css(out, token);
        }
        Token::Semicolon => {
          next_font_face_block = false;
          push_token_to_css(out, token);
        }
        Token::CurlyBracketBlock => {
          out.push('{');
          let is_font_face = next_font_face_block;
          next_font_face_block = false;
          let _ = parser.parse_nested_block(|nested| {
            if is_font_face {
              scan_font_face_block(nested, base_url, out);
            } else {
              scan(nested, base_url, out);
            }
            Ok::<_, cssparser::ParseError<'i, ()>>(())
          });
          out.push('}');
        }
        Token::Function(name) => {
          out.push_str(name.as_ref());
          out.push('(');
          let _ = parser.parse_nested_block(|nested| {
            scan(nested, base_url, out);
            Ok::<_, cssparser::ParseError<'i, ()>>(())
          });
          out.push(')');
        }
        Token::ParenthesisBlock => {
          out.push('(');
          let _ = parser.parse_nested_block(|nested| {
            scan(nested, base_url, out);
            Ok::<_, cssparser::ParseError<'i, ()>>(())
          });
          out.push(')');
        }
        Token::SquareBracketBlock => {
          out.push('[');
          let _ = parser.parse_nested_block(|nested| {
            scan(nested, base_url, out);
            Ok::<_, cssparser::ParseError<'i, ()>>(())
          });
          out.push(']');
        }
        other => push_token_to_css(out, other),
      }
    }
  }

  let mut out = String::with_capacity(input.len());
  let mut parser_input = ParserInput::new(input);
  let mut parser = Parser::new(&mut parser_input);
  scan(&mut parser, base_url, &mut out);
  out
}

fn rewrite_reference(
  raw: &str,
  base_url: &Url,
  ctx: ReferenceContext,
  catalog: &mut AssetCatalog,
) -> Result<Option<String>> {
  let decoded = decode_html_entities_if_needed(raw.trim());
  let mut trimmed = decoded.trim();
  trimmed = strip_wrapping_quotes(trimmed);
  if trimmed.is_empty()
    || trimmed.starts_with('#')
    || trimmed.to_ascii_lowercase().starts_with("javascript:")
    || is_data_url(trimmed)
    || trimmed.to_ascii_lowercase().starts_with("about:")
    || trimmed.to_ascii_lowercase().starts_with("mailto:")
    || trimmed.to_ascii_lowercase().starts_with("tel:")
    || trimmed.contains("data:")
  {
    return Ok(None);
  }

  let Some(resolved) = resolve_href(base_url.as_str(), trimmed) else {
    if catalog.allow_missing {
      return Ok(None);
    }
    bail!("could not resolve URL '{trimmed}' against base {base_url}");
  };

  if is_data_url(&resolved) {
    return Ok(None);
  }

  let (without_fragment, fragment) = split_fragment(&resolved);
  let mut path = match catalog.path_for(&without_fragment, ctx) {
    Some(path) => path,
    None => return Ok(None),
  };

  if let Some(fragment) = fragment {
    path.push('#');
    path.push_str(&fragment);
  }

  Ok(Some(path))
}

fn rewrite_reference_optional(
  raw: &str,
  base_url: &Url,
  ctx: ReferenceContext,
  catalog: &mut AssetCatalog,
) -> Result<Option<String>> {
  let decoded = decode_html_entities_if_needed(raw.trim());
  let mut trimmed = decoded.trim();
  trimmed = strip_wrapping_quotes(trimmed);
  if trimmed.is_empty()
    || trimmed.starts_with('#')
    || trimmed.to_ascii_lowercase().starts_with("javascript:")
    || is_data_url(trimmed)
    || trimmed.to_ascii_lowercase().starts_with("about:")
    || trimmed.to_ascii_lowercase().starts_with("mailto:")
    || trimmed.to_ascii_lowercase().starts_with("tel:")
    || trimmed.contains("data:")
  {
    return Ok(None);
  }

  let Some(resolved) = resolve_href(base_url.as_str(), trimmed) else {
    return Ok(None);
  };

  if is_data_url(&resolved) {
    return Ok(None);
  }

  let (without_fragment, fragment) = split_fragment(&resolved);
  let mut path = catalog.path_for_optional(&without_fragment, ctx);

  if let Some(fragment) = fragment {
    path.push('#');
    path.push_str(&fragment);
  }

  Ok(Some(path))
}

fn split_fragment(url: &str) -> (String, Option<String>) {
  match url.find('#') {
    Some(idx) => (url[..idx].to_string(), Some(url[idx + 1..].to_string())),
    None => (url.to_string(), None),
  }
}

fn strip_wrapping_quotes(value: &str) -> &str {
  let mut value = value;
  loop {
    if value.len() >= 2 {
      let first = value.as_bytes()[0];
      let last = *value.as_bytes().last().unwrap_or(&0);
      if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
        value = &value[1..value.len() - 1];
        continue;
      }
    }
    break;
  }
  value
}

fn find_base_url(html: &str, document_base: &Url) -> Url {
  let regex = Regex::new("(?is)<base[^>]*href\\s*=\\s*[\"']([^\"'>]+)[\"']")
    .expect("base regex must compile");
  if let Some(caps) = regex.captures(html) {
    if let Some(href) = caps.get(1) {
      if let Some(resolved) = resolve_href(document_base.as_str(), href.as_str()) {
        if let Ok(parsed) = Url::parse(&resolved) {
          return parsed;
        }
      }
    }
  }
  document_base.clone()
}

fn extension_from_path(path: &str) -> String {
  if let Ok(url) = Url::parse(path) {
    if let Some(ext) = Path::new(url.path()).extension().and_then(|e| e.to_str()) {
      let ext = ext.to_ascii_lowercase();
      if ext.is_empty() {
        return "bin".to_string();
      }
      return ext;
    }
    return "bin".to_string();
  }

  Path::new(path)
    .extension()
    .and_then(|e| e.to_str())
    .unwrap_or("bin")
    .to_ascii_lowercase()
}

fn extension_from_resource(path: &str, content_type: Option<&str>) -> String {
  let ext = extension_from_path(path);
  if ext != "bin" {
    return ext;
  }

  let Some(ct) = content_type else {
    return ext;
  };
  let lower = ct.to_ascii_lowercase();
  if lower.contains("html") {
    return "html".to_string();
  }
  if lower.contains("css") {
    return "css".to_string();
  }
  if lower.contains("javascript") || lower.contains("ecmascript") {
    return "js".to_string();
  }
  ext
}

fn is_html_extension(ext: &str) -> bool {
  matches!(ext, "html" | "htm" | "xhtml")
}

fn is_html_asset(asset: &AssetData) -> bool {
  if sniff_font_extension(&asset.bytes).is_some() {
    return false;
  }
  let is_html = asset
    .content_type
    .as_deref()
    .map(|ct| ct.to_ascii_lowercase().contains("html"))
    .unwrap_or(false)
    || {
      let lower = asset.filename.to_ascii_lowercase();
      lower.ends_with(".html") || lower.ends_with(".htm") || lower.ends_with(".xhtml")
    };

  if !is_html {
    return false;
  }

  // Avoid rewriting/validating resources that were fetched for non-document contexts (images, fonts,
  // etc.) but were served as HTML error pages. The renderer will still treat them as the original
  // destination (e.g. <img>), so any nested <link>/<script> inside the HTML bytes is not fetchable
  // subresource content.
  if let Ok(parsed) = Url::parse(&asset.source_url) {
    if let Some(ext) = Path::new(parsed.path())
      .extension()
      .and_then(|ext| ext.to_str())
      .map(|ext| ext.to_ascii_lowercase())
    {
      if matches!(
        ext.as_str(),
        "css"
          | "js"
          | "mjs"
          | "png"
          | "jpg"
          | "jpeg"
          | "gif"
          | "webp"
          | "avif"
          | "svg"
          | "svgz"
          | "ico"
          | "woff"
          | "woff2"
          | "ttf"
          | "otf"
          | "eot"
          | "mp4"
          | "webm"
          | "mp3"
          | "wav"
          | "ogg"
          | "pdf"
          | "json"
      ) {
        return false;
      }
    }
  }

  true
}

fn sniff_font_extension(bytes: &[u8]) -> Option<&'static str> {
  bytes.get(..4).and_then(|prefix| match prefix {
    b"wOF2" => Some("woff2"),
    b"wOFF" => Some("woff"),
    _ => None,
  })
}

fn hash_bytes(bytes: &[u8]) -> String {
  let digest = Sha256::digest(bytes);
  digest
    .iter()
    .take(HASH_PREFIX_BYTES)
    .map(|b| format!("{b:02x}"))
    .collect::<String>()
}

fn rewrite_and_strip_link_tags(
  input: &str,
  base_url: &Url,
  ctx: ReferenceContext,
  catalog: &mut AssetCatalog,
  screen_stylesheets: &HashSet<String>,
  rewrite_scripts: bool,
) -> Result<String> {
  fn capture_first_match<'t>(
    caps: &'t regex::Captures<'t>,
    groups: &[usize],
  ) -> Option<regex::Match<'t>> {
    groups.iter().find_map(|idx| caps.get(*idx))
  }

  let link_tag = Regex::new("(?is)<link\\b[^>]*>").expect("link tag regex must compile");
  let attr_rel = Regex::new("(?is)(?:^|\\s)rel\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
    .expect("link rel regex must compile");
  let attr_href = Regex::new("(?is)(?:^|\\s)href\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
    .expect("link href regex must compile");
  let attr_as = Regex::new("(?is)(?:^|\\s)as\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
    .expect("link as regex must compile");

  let mut out = String::with_capacity(input.len());
  let mut last = 0usize;
  for tag_match in link_tag.find_iter(input) {
    let tag = &input[tag_match.start()..tag_match.end()];

    let rel_value = attr_rel
      .captures(tag)
      .and_then(|c| capture_first_match(&c, &[1, 2, 3]).map(|m| m.as_str().to_string()))
      .unwrap_or_default();
    let as_value = attr_as
      .captures(tag)
      .and_then(|c| capture_first_match(&c, &[1, 2, 3]).map(|m| m.as_str().to_string()))
      .unwrap_or_default();

    let mut has_stylesheet = false;
    let mut is_strippable_fetchable = false;
    let mut is_script_fetchable = false;
    for token in rel_value.split_ascii_whitespace() {
      if token.eq_ignore_ascii_case("stylesheet") {
        has_stylesheet = true;
      } else if token.eq_ignore_ascii_case("modulepreload") {
        if rewrite_scripts {
          is_script_fetchable = true;
        }
      } else if token.eq_ignore_ascii_case("preload")
        || token.eq_ignore_ascii_case("prefetch")
        || token.eq_ignore_ascii_case("icon")
        || token.eq_ignore_ascii_case("apple-touch-icon")
        || token.eq_ignore_ascii_case("apple-touch-icon-precomposed")
        || token.eq_ignore_ascii_case("manifest")
        || token.eq_ignore_ascii_case("mask-icon")
        || token.eq_ignore_ascii_case("preconnect")
        || token.eq_ignore_ascii_case("dns-prefetch")
      {
        is_strippable_fetchable = true;
        if rewrite_scripts
          && token.eq_ignore_ascii_case("preload")
          && (as_value.eq_ignore_ascii_case("script")
            || as_value.eq_ignore_ascii_case("worker")
            || as_value.eq_ignore_ascii_case("sharedworker"))
        {
          is_script_fetchable = true;
        }
      }
    }

    // Strip link tags that would trigger network loads in browsers but are not needed for
    // FastRender output correctness (preloads, icons, preconnect/dns-prefetch, etc).
    if !has_stylesheet && is_strippable_fetchable && !is_script_fetchable {
      out.push_str(&input[last..tag_match.start()]);
      last = tag_match.end();
      continue;
    }

    let mut rewritten_tag = tag.to_string();
    if has_stylesheet || is_script_fetchable {
      if let Some(href_caps) = attr_href.captures(tag) {
        if let Some(href_match) = capture_first_match(&href_caps, &[1, 2, 3]) {
          let rewritten_href = if has_stylesheet {
            let decoded = decode_html_entities_if_needed(href_match.as_str());
            let resolved =
              resolve_href(base_url.as_str(), decoded.trim()).unwrap_or_else(|| "".to_string());
            let required = !resolved.is_empty() && screen_stylesheets.contains(&resolved);
            if required {
              rewrite_reference(href_match.as_str(), base_url, ctx, catalog)?
            } else {
              rewrite_reference_optional(href_match.as_str(), base_url, ctx, catalog)?
            }
          } else {
            rewrite_reference(href_match.as_str(), base_url, ctx, catalog)?
          };
          if let Some(new_value) = rewritten_href {
            let start = href_match.start();
            let end = href_match.end();
            rewritten_tag = format!(
              "{}{}{}",
              &rewritten_tag[..start],
              new_value,
              &rewritten_tag[end..]
            );
          }
        }
      }
    }

    out.push_str(&input[last..tag_match.start()]);
    out.push_str(&rewritten_tag);
    last = tag_match.end();
  }
  out.push_str(&input[last..]);
  Ok(out)
}

fn rewrite_html_resource_attrs(
  input: &str,
  base_url: &Url,
  ctx: ReferenceContext,
  catalog: &mut AssetCatalog,
  rewrite_scripts: bool,
) -> Result<String> {
  fn rewrite_iframe_live_sample_src_placeholders(input: &str) -> String {
    fn capture_first_match<'t>(
      caps: &regex::Captures<'t>,
      groups: &[usize],
    ) -> Option<regex::Match<'t>> {
      groups.iter().find_map(|idx| caps.get(*idx))
    }
    fn insert_attr(tag: &mut String, key: &str, value: &str) {
      let value = value.replace('\"', "&quot;");
      let Some(close_idx) = tag.rfind('>') else {
        return;
      };

      let mut insert_pos = close_idx;
      let mut cursor = close_idx;
      while cursor > 0 && tag.as_bytes()[cursor - 1].is_ascii_whitespace() {
        cursor -= 1;
      }
      if cursor > 0 && tag.as_bytes()[cursor - 1] == b'/' {
        insert_pos = cursor - 1;
      }
      tag.insert_str(insert_pos, &format!(" {key}=\"{value}\""));
    }

    let iframe_tag = Regex::new("(?is)<iframe\\b[^>]*>").expect("iframe tag regex must compile");
    let attr_src = Regex::new("(?is)(?:^|\\s)src\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
      .expect("iframe src attr regex must compile");
    let attr_data_live_path =
      Regex::new("(?is)(?:^|\\s)data-live-path\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
        .expect("data-live-path attr regex must compile");
    let attr_data_live_id =
      Regex::new("(?is)(?:^|\\s)data-live-id\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
        .expect("data-live-id attr regex must compile");

    let mut out = String::with_capacity(input.len());
    let mut last = 0usize;
    for tag_match in iframe_tag.find_iter(input) {
      let tag = &input[tag_match.start()..tag_match.end()];
      let mut rewritten_tag = tag.to_string();

      let src_match = attr_src
        .captures(&rewritten_tag)
        .and_then(|caps| capture_first_match(&caps, &[1, 2, 3]));
      let src_is_placeholder = src_match
        .as_ref()
        .map(|m| {
          let value = decode_html_entities_if_needed(m.as_str());
          fastrender::dom::img_src_is_placeholder(value.trim())
        })
        .unwrap_or(true);

      if src_is_placeholder {
        let live_path = attr_data_live_path
          .captures(&rewritten_tag)
          .and_then(|caps| capture_first_match(&caps, &[1, 2, 3]))
          .map(|m| {
            decode_html_entities_if_needed(m.as_str())
              .trim()
              .to_string()
          })
          .filter(|s| !s.is_empty());
        let live_id = attr_data_live_id
          .captures(&rewritten_tag)
          .and_then(|caps| capture_first_match(&caps, &[1, 2, 3]))
          .map(|m| {
            decode_html_entities_if_needed(m.as_str())
              .trim()
              .to_string()
          })
          .filter(|s| !s.is_empty());
        if let (Some(path), Some(id)) = (live_path, live_id) {
          let synthesized = format!("{path}{id}.html");
          if let Some(m) = src_match {
            let start = m.start();
            let end = m.end();
            rewritten_tag = format!(
              "{}{}{}",
              &rewritten_tag[..start],
              synthesized,
              &rewritten_tag[end..]
            );
          } else {
            insert_attr(&mut rewritten_tag, "src", &synthesized);
          }
        }
      }

      out.push_str(&input[last..tag_match.start()]);
      out.push_str(&rewritten_tag);
      last = tag_match.end();
    }
    out.push_str(&input[last..]);
    out
  }

  let stylesheet_urls: HashSet<String> = fastrender::css::loader::extract_css_links(
    input,
    base_url.as_str(),
    fastrender::style::media::MediaType::Screen,
  )
  .unwrap_or_default()
  .into_iter()
  .collect();

  let mut rewritten = rewrite_and_strip_link_tags(
    input,
    base_url,
    ctx,
    catalog,
    &stylesheet_urls,
    rewrite_scripts,
  )?;

  if rewrite_scripts {
    let script_src =
      Regex::new("(?is)<script[^>]*\\ssrc\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
        .expect("script src regex must compile");
    rewritten = replace_attr_values_with(&script_src, &rewritten, &[1, 2, 3], |raw| {
      rewrite_reference(raw, base_url, ctx, catalog)
    })?;
  }

  let img_src = Regex::new("(?is)<img[^>]*\\ssrc\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
    .expect("img src regex must compile");
  rewritten = replace_attr_values_with(&img_src, &rewritten, &[1, 2, 3], |raw| {
    rewrite_reference(raw, base_url, ctx, catalog)
  })?;

  rewritten = rewrite_iframe_live_sample_src_placeholders(&rewritten);

  let iframe_src =
    Regex::new("(?is)<iframe[^>]*\\ssrc\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
      .expect("iframe src regex must compile");
  rewritten = replace_attr_values_with(&iframe_src, &rewritten, &[1, 2, 3], |raw| {
    rewrite_reference(raw, base_url, ctx, catalog)
  })?;

  let embed_src =
    Regex::new("(?is)<embed[^>]*\\ssrc\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
      .expect("embed src regex must compile");
  rewritten = replace_attr_values_with(&embed_src, &rewritten, &[1, 2, 3], |raw| {
    rewrite_reference(raw, base_url, ctx, catalog)
  })?;

  let object_data =
    Regex::new("(?is)<object[^>]*\\sdata\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
      .expect("object data regex must compile");
  rewritten = replace_attr_values_with(&object_data, &rewritten, &[1, 2, 3], |raw| {
    rewrite_reference(raw, base_url, ctx, catalog)
  })?;

  let video_poster =
    Regex::new("(?is)<video[^>]*\\sposter\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
      .expect("video poster regex must compile");
  rewritten = replace_attr_values_with(&video_poster, &rewritten, &[1, 2, 3], |raw| {
    rewrite_reference(raw, base_url, ctx, catalog)
  })?;

  // Media sources are fetchable in browsers but generally not required for FastRender's output. Use
  // deterministic placeholders so imported fixtures remain offline even if the bundle didn't
  // capture the media.
  let video_src =
    Regex::new("(?is)<video[^>]*\\ssrc\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
      .expect("video src regex must compile");
  rewritten = replace_attr_values_with(&video_src, &rewritten, &[1, 2, 3], |raw| {
    rewrite_reference_optional(raw, base_url, ctx, catalog)
  })?;

  let audio_src =
    Regex::new("(?is)<audio[^>]*\\ssrc\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
      .expect("audio src regex must compile");
  rewritten = replace_attr_values_with(&audio_src, &rewritten, &[1, 2, 3], |raw| {
    rewrite_reference_optional(raw, base_url, ctx, catalog)
  })?;

  let track_src =
    Regex::new("(?is)<track[^>]*\\ssrc\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
      .expect("track src regex must compile");
  rewritten = replace_attr_values_with(&track_src, &rewritten, &[1, 2, 3], |raw| {
    rewrite_reference_optional(raw, base_url, ctx, catalog)
  })?;

  let source_src =
    Regex::new("(?is)<source[^>]*\\ssrc\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
      .expect("source src regex must compile");
  rewritten = replace_attr_values_with(&source_src, &rewritten, &[1, 2, 3], |raw| {
    rewrite_reference_optional(raw, base_url, ctx, catalog)
  })?;

  // The bundler only captures a limited number of srcset candidates (to avoid pathological pages
  // exploding bundle size). Keep the rewritten fixture aligned with that by truncating srcsets so
  // FastRender won't pick a missing candidate at render time.
  const IMG_SRCSET_MAX_CANDIDATES: usize = 1;
  const SRCSET_MAX_CANDIDATES: usize = 32;
  let img_srcset = Regex::new("(?is)<img[^>]*\\ssrcset\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)')")
    .expect("img srcset regex must compile");
  rewritten = replace_attr_values_with(&img_srcset, &rewritten, &[1, 2], |raw| {
    rewrite_srcset_with_limit(raw, base_url, ctx, catalog, IMG_SRCSET_MAX_CANDIDATES)
      .map(Some)
      .or_else(|err| Err(err))
  })?;

  let source_srcset = Regex::new("(?is)<source[^>]*\\ssrcset\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)')")
    .expect("source srcset regex must compile");
  rewritten = replace_attr_values_with(&source_srcset, &rewritten, &[1, 2], |raw| {
    // Unlike the legacy srcset rewrite, keep the candidate cap low to avoid pathological inputs.
    rewrite_srcset_with_limit(raw, base_url, ctx, catalog, SRCSET_MAX_CANDIDATES)
      .map(Some)
      .or_else(|err| Err(err))
  })?;

  rewritten = rewrite_lazy_load_image_attrs(&rewritten, base_url, ctx, catalog)?;

  Ok(rewritten)
}

fn rewrite_lazy_load_image_attrs(
  input: &str,
  base_url: &Url,
  ctx: ReferenceContext,
  catalog: &mut AssetCatalog,
) -> Result<String> {
  fn capture_first_match<'t>(
    caps: &'t regex::Captures<'t>,
    groups: &[usize],
  ) -> Option<regex::Match<'t>> {
    groups.iter().find_map(|idx| caps.get(*idx))
  }

  fn src_or_srcset_placeholder(value: &str) -> bool {
    let value = decode_html_entities_if_needed(value);
    let trimmed = value.trim();
    trimmed.is_empty() || fastrender::dom::img_src_is_placeholder(trimmed)
  }

  fn srcset_is_placeholder(value: &str) -> bool {
    let value = decode_html_entities_if_needed(value);
    let trimmed = value.trim();
    if trimmed.is_empty() {
      return true;
    }
    let candidates = fastrender::html::image_attrs::parse_srcset_with_limit(trimmed, 32);
    if candidates.is_empty() {
      return false;
    }
    candidates
      .iter()
      .all(|candidate| fastrender::dom::img_src_is_placeholder(&candidate.url))
  }

  fn insert_attr(tag: &mut String, key: &str, value: &str) {
    let value = value.replace('\"', "&quot;");
    let Some(close_idx) = tag.rfind('>') else {
      return;
    };

    let mut insert_pos = close_idx;
    let mut cursor = close_idx;
    while cursor > 0 && tag.as_bytes()[cursor - 1].is_ascii_whitespace() {
      cursor -= 1;
    }
    if cursor > 0 && tag.as_bytes()[cursor - 1] == b'/' {
      insert_pos = cursor - 1;
    }
    tag.insert_str(insert_pos, &format!(" {key}=\"{value}\""));
  }

  let img_tag = Regex::new("(?is)<img\\b[^>]*>").expect("img tag regex must compile");
  let source_tag = Regex::new("(?is)<source\\b[^>]*>").expect("source tag regex must compile");
  let attr_src = Regex::new("(?is)(?:^|\\s)src\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
    .expect("img src attr regex must compile");
  let attr_srcset = Regex::new("(?is)(?:^|\\s)srcset\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)')")
    .expect("srcset attr regex must compile");
  let attr_sizes = Regex::new("(?is)(?:^|\\s)sizes\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
    .expect("sizes attr regex must compile");

  let attr_data_src =
    Regex::new("(?is)(?:^|\\s)data-src\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
      .expect("data-src attr regex must compile");
  let attr_data_srcset = Regex::new("(?is)(?:^|\\s)data-srcset\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)')")
    .expect("data-srcset attr regex must compile");
  let attr_data_sizes =
    Regex::new("(?is)(?:^|\\s)data-sizes\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
      .expect("data-sizes attr regex must compile");

  // Keep these limits aligned with the main rewrite pass.
  const IMG_SRCSET_MAX_CANDIDATES: usize = 1;
  const SRCSET_MAX_CANDIDATES: usize = 32;

  let mut out = String::with_capacity(input.len());
  let mut last = 0usize;
  for tag_match in img_tag.find_iter(input) {
    let tag = &input[tag_match.start()..tag_match.end()];
    let mut rewritten_tag = tag.to_string();

    // Only consult `data-src` / `data-srcset` when we're going to *use* them to backfill a
    // placeholder/missing `src`/`srcset`. Many pages include `data-src` for JS-driven lazy loading
    // even when `src` already points at the real image; in that case, rewriting `data-src` would
    // incorrectly require the bundle to contain it.
    let src_needs_backfill = attr_src
      .captures(&rewritten_tag)
      .and_then(|caps| {
        capture_first_match(&caps, &[1, 2, 3]).map(|m| src_or_srcset_placeholder(m.as_str()))
      })
      .unwrap_or(true);

    if src_needs_backfill {
      if let Some(caps) = attr_data_src.captures(&rewritten_tag) {
        if let Some(m) = capture_first_match(&caps, &[1, 2, 3]) {
          if let Some(new_src) = rewrite_reference(m.as_str(), base_url, ctx, catalog)? {
            if let Some(src_caps) = attr_src.captures(&rewritten_tag) {
              if let Some(src_match) = capture_first_match(&src_caps, &[1, 2, 3]) {
                if src_or_srcset_placeholder(src_match.as_str()) {
                  let start = src_match.start();
                  let end = src_match.end();
                  rewritten_tag = format!(
                    "{}{}{}",
                    &rewritten_tag[..start],
                    &new_src,
                    &rewritten_tag[end..]
                  );
                }
              }
            } else {
              insert_attr(&mut rewritten_tag, "src", &new_src);
            }

            // If we actually used `data-src` to populate `src`, keep `data-src` pointing at the
            // rewritten local asset path so JS-capable fixtures remain offline-friendly.
            if let Some(data_caps) = attr_data_src.captures(&rewritten_tag) {
              if let Some(data_match) = capture_first_match(&data_caps, &[1, 2, 3]) {
                let start = data_match.start();
                let end = data_match.end();
                rewritten_tag = format!(
                  "{}{}{}",
                  &rewritten_tag[..start],
                  &new_src,
                  &rewritten_tag[end..]
                );
              }
            }
          }
        }
      }
    }

    let data_srcset_value = attr_data_srcset
      .captures(&rewritten_tag)
      .and_then(|caps| capture_first_match(&caps, &[1, 2]).map(|m| m.as_str().to_string()));
    let srcset_needs_backfill = attr_srcset
      .captures(&rewritten_tag)
      .and_then(|caps| {
        capture_first_match(&caps, &[1, 2]).map(|m| srcset_is_placeholder(m.as_str()))
      })
      .unwrap_or(false)
      || (!attr_srcset.is_match(&rewritten_tag) && data_srcset_value.is_some());

    let mut inserted_srcset = false;
    if srcset_needs_backfill {
      if let Some(raw) = data_srcset_value.as_deref() {
        let new_srcset =
          rewrite_srcset_with_limit(raw, base_url, ctx, catalog, IMG_SRCSET_MAX_CANDIDATES)?;
        if let Some(srcset_caps) = attr_srcset.captures(&rewritten_tag) {
          if let Some(srcset_match) = capture_first_match(&srcset_caps, &[1, 2]) {
            if srcset_is_placeholder(srcset_match.as_str()) {
              let start = srcset_match.start();
              let end = srcset_match.end();
              rewritten_tag = format!(
                "{}{}{}",
                &rewritten_tag[..start],
                &new_srcset,
                &rewritten_tag[end..]
              );
              inserted_srcset = true;
            }
          }
        } else {
          insert_attr(&mut rewritten_tag, "srcset", &new_srcset);
          inserted_srcset = true;
        }

        if inserted_srcset {
          if let Some(data_caps) = attr_data_srcset.captures(&rewritten_tag) {
            if let Some(data_match) = capture_first_match(&data_caps, &[1, 2]) {
              let start = data_match.start();
              let end = data_match.end();
              rewritten_tag = format!(
                "{}{}{}",
                &rewritten_tag[..start],
                &new_srcset,
                &rewritten_tag[end..]
              );
            }
          }
        }
      }
    }

    if inserted_srcset && !attr_sizes.is_match(&rewritten_tag) {
      let data_sizes_value = attr_data_sizes.captures(&rewritten_tag).and_then(|caps| {
        capture_first_match(&caps, &[1, 2, 3]).map(|m| m.as_str().trim().to_string())
      });
      if let Some(value) = data_sizes_value {
        if !value.is_empty() {
          insert_attr(&mut rewritten_tag, "sizes", &value);
        }
      }
    }

    out.push_str(&input[last..tag_match.start()]);
    out.push_str(&rewritten_tag);
    last = tag_match.end();
  }
  out.push_str(&input[last..]);
  let input = out;

  let mut out = String::with_capacity(input.len());
  let mut last = 0usize;
  for tag_match in source_tag.find_iter(&input) {
    let tag = &input[tag_match.start()..tag_match.end()];
    let mut rewritten_tag = tag.to_string();

    let data_srcset_value = attr_data_srcset
      .captures(&rewritten_tag)
      .and_then(|caps| capture_first_match(&caps, &[1, 2]).map(|m| m.as_str().to_string()));
    let srcset_needs_backfill = attr_srcset
      .captures(&rewritten_tag)
      .and_then(|caps| {
        capture_first_match(&caps, &[1, 2]).map(|m| srcset_is_placeholder(m.as_str()))
      })
      .unwrap_or(false)
      || (!attr_srcset.is_match(&rewritten_tag) && data_srcset_value.is_some());

    let mut inserted_srcset = false;
    if srcset_needs_backfill {
      if let Some(raw) = data_srcset_value.as_deref() {
        let new_srcset =
          rewrite_srcset_with_limit(raw, base_url, ctx, catalog, SRCSET_MAX_CANDIDATES)?;
        if let Some(srcset_caps) = attr_srcset.captures(&rewritten_tag) {
          if let Some(srcset_match) = capture_first_match(&srcset_caps, &[1, 2]) {
            if srcset_is_placeholder(srcset_match.as_str()) {
              let start = srcset_match.start();
              let end = srcset_match.end();
              rewritten_tag = format!(
                "{}{}{}",
                &rewritten_tag[..start],
                &new_srcset,
                &rewritten_tag[end..]
              );
              inserted_srcset = true;
            }
          }
        } else {
          insert_attr(&mut rewritten_tag, "srcset", &new_srcset);
          inserted_srcset = true;
        }

        if inserted_srcset {
          if let Some(data_caps) = attr_data_srcset.captures(&rewritten_tag) {
            if let Some(data_match) = capture_first_match(&data_caps, &[1, 2]) {
              let start = data_match.start();
              let end = data_match.end();
              rewritten_tag = format!(
                "{}{}{}",
                &rewritten_tag[..start],
                &new_srcset,
                &rewritten_tag[end..]
              );
            }
          }
        }
      }
    }

    if inserted_srcset && !attr_sizes.is_match(&rewritten_tag) {
      let data_sizes_value = attr_data_sizes.captures(&rewritten_tag).and_then(|caps| {
        capture_first_match(&caps, &[1, 2, 3]).map(|m| m.as_str().trim().to_string())
      });
      if let Some(value) = data_sizes_value {
        if !value.is_empty() {
          insert_attr(&mut rewritten_tag, "sizes", &value);
        }
      }
    }

    out.push_str(&input[last..tag_match.start()]);
    out.push_str(&rewritten_tag);
    last = tag_match.end();
  }
  out.push_str(&input[last..]);

  Ok(out)
}

fn replace_attr_values_with<F>(
  regex: &Regex,
  input: &str,
  groups: &[usize],
  mut rewrite: F,
) -> Result<String>
where
  F: FnMut(&str) -> Result<Option<String>>,
{
  if !regex.is_match(input) {
    return Ok(input.to_string());
  }

  let mut out = String::with_capacity(input.len());
  let mut last = 0;
  for caps in regex.captures_iter(input) {
    let Some(m) = groups.iter().find_map(|&idx| caps.get(idx)) else {
      continue;
    };
    out.push_str(&input[last..m.start()]);
    match rewrite(m.as_str())? {
      Some(new_value) => out.push_str(&new_value),
      None => out.push_str(m.as_str()),
    }
    last = m.end();
  }
  out.push_str(&input[last..]);
  Ok(out)
}

fn validate_no_remote_fetchable_subresources_in_html(
  label: &str,
  html: &str,
  validate_scripts: bool,
) -> Result<()> {
  let mut remote: BTreeSet<String> = BTreeSet::new();

  fn capture_first_match<'t>(
    caps: &'t regex::Captures<'t>,
    groups: &[usize],
  ) -> Option<regex::Match<'t>> {
    groups.iter().find_map(|idx| caps.get(*idx))
  }

  // Fetchable <link> resources (stylesheets, icons, preloads, prefetch, preconnect, etc.).
  // We intentionally validate the authored href/srcset strings to catch scheme-relative URLs.
  let link_tag = Regex::new("(?is)<link\\b[^>]*>").expect("link tag validation regex");
  let attr_rel = Regex::new("(?is)(?:^|\\s)rel\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
    .expect("link rel validation regex");
  let attr_href = Regex::new("(?is)(?:^|\\s)href\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
    .expect("link href validation regex");
  let attr_imagesrcset = Regex::new("(?is)(?:^|\\s)imagesrcset\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)')")
    .expect("link imagesrcset validation regex");

  for tag_match in link_tag.find_iter(html) {
    let tag = tag_match.as_str();
    let rel_value = attr_rel
      .captures(tag)
      .and_then(|c| capture_first_match(&c, &[1, 2, 3]).map(|m| m.as_str().to_string()))
      .unwrap_or_default();

    let mut is_fetchable = false;
    for token in rel_value.split_ascii_whitespace() {
      if token.eq_ignore_ascii_case("stylesheet")
        || token.eq_ignore_ascii_case("preload")
        || token.eq_ignore_ascii_case("prefetch")
        || token.eq_ignore_ascii_case("icon")
        || token.eq_ignore_ascii_case("apple-touch-icon")
        || token.eq_ignore_ascii_case("apple-touch-icon-precomposed")
        || token.eq_ignore_ascii_case("manifest")
        || token.eq_ignore_ascii_case("mask-icon")
        || token.eq_ignore_ascii_case("preconnect")
        || token.eq_ignore_ascii_case("dns-prefetch")
        || (validate_scripts && token.eq_ignore_ascii_case("modulepreload"))
      {
        is_fetchable = true;
        break;
      }
    }
    if !is_fetchable {
      continue;
    }

    if let Some(href_caps) = attr_href.captures(tag) {
      if let Some(m) = capture_first_match(&href_caps, &[1, 2, 3]) {
        let decoded = decode_html_entities_if_needed(m.as_str());
        let trimmed = decoded.trim();
        if is_remote_fetch_url(trimmed) {
          remote.insert(trimmed.to_string());
        }
      }
    }

    if let Some(srcset_caps) = attr_imagesrcset.captures(tag) {
      if let Some(m) = capture_first_match(&srcset_caps, &[1, 2]) {
        for candidate in parse_srcset_urls(m.as_str(), 32) {
          let decoded = decode_html_entities_if_needed(candidate.trim());
          let trimmed = decoded.trim();
          if is_remote_fetch_url(trimmed) {
            remote.insert(trimmed.to_string());
          }
        }
      }
    }
  }

  let img_src = Regex::new("(?is)<img[^>]*\\ssrc\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
    .expect("img src validation regex");
  collect_remote_attr_values(&img_src, html, &[1, 2, 3], &mut remote);

  let iframe_src =
    Regex::new("(?is)<iframe[^>]*\\ssrc\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
      .expect("iframe src validation regex");
  collect_remote_attr_values(&iframe_src, html, &[1, 2, 3], &mut remote);

  let embed_src =
    Regex::new("(?is)<embed[^>]*\\ssrc\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
      .expect("embed src validation regex");
  collect_remote_attr_values(&embed_src, html, &[1, 2, 3], &mut remote);

  let object_data =
    Regex::new("(?is)<object[^>]*\\sdata\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
      .expect("object data validation regex");
  collect_remote_attr_values(&object_data, html, &[1, 2, 3], &mut remote);

  let video_poster =
    Regex::new("(?is)<video[^>]*\\sposter\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
      .expect("video poster validation regex");
  collect_remote_attr_values(&video_poster, html, &[1, 2, 3], &mut remote);

  let video_src =
    Regex::new("(?is)<video[^>]*\\ssrc\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
      .expect("video src validation regex");
  collect_remote_attr_values(&video_src, html, &[1, 2, 3], &mut remote);

  let audio_src =
    Regex::new("(?is)<audio[^>]*\\ssrc\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
      .expect("audio src validation regex");
  collect_remote_attr_values(&audio_src, html, &[1, 2, 3], &mut remote);

  let track_src =
    Regex::new("(?is)<track[^>]*\\ssrc\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
      .expect("track src validation regex");
  collect_remote_attr_values(&track_src, html, &[1, 2, 3], &mut remote);

  let source_src =
    Regex::new("(?is)<source[^>]*\\ssrc\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
      .expect("source src validation regex");
  collect_remote_attr_values(&source_src, html, &[1, 2, 3], &mut remote);

  if validate_scripts {
    let script_src =
      Regex::new("(?is)<script[^>]*\\ssrc\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)'|([^\\s>]+))")
        .expect("script src validation regex");
    collect_remote_attr_values(&script_src, html, &[1, 2, 3], &mut remote);
  }

  let img_srcset = Regex::new("(?is)<img[^>]*\\ssrcset\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)')")
    .expect("img srcset validation regex");
  collect_remote_srcset_candidates(&img_srcset, html, &[1, 2], &mut remote);

  let source_srcset = Regex::new("(?is)<source[^>]*\\ssrcset\\s*=\\s*(?:\"([^\"]*)\"|'([^']*)')")
    .expect("source srcset validation regex");
  collect_remote_srcset_candidates(&source_srcset, html, &[1, 2], &mut remote);

  // Inline CSS inside <style> and style="".
  let style_tag = Regex::new("(?is)<style[^>]*>(.*?)</style>").expect("style tag validation regex");
  for caps in style_tag.captures_iter(html) {
    if let Some(css) = caps.get(1).map(|m| m.as_str()) {
      for url in extract_fetchable_css_urls(css) {
        if is_remote_fetch_url(&url) {
          remote.insert(url);
        }
      }
    }
  }

  let style_attr_double =
    Regex::new("(?is)\\sstyle\\s*=\\s*\"([^\"]*)\"").expect("style attr validation regex");
  for caps in style_attr_double.captures_iter(html) {
    if let Some(css) = caps.get(1).map(|m| m.as_str()) {
      for url in extract_fetchable_css_urls(css) {
        if is_remote_fetch_url(&url) {
          remote.insert(url);
        }
      }
    }
  }
  let style_attr_single =
    Regex::new("(?is)\\sstyle\\s*=\\s*'([^']*)'").expect("style attr validation regex");
  for caps in style_attr_single.captures_iter(html) {
    if let Some(css) = caps.get(1).map(|m| m.as_str()) {
      for url in extract_fetchable_css_urls(css) {
        if is_remote_fetch_url(&url) {
          remote.insert(url);
        }
      }
    }
  }

  if remote.is_empty() {
    return Ok(());
  }

  let mut msg = format!("{label} still contains remote fetchable subresources:\n");
  for url in &remote {
    msg.push_str("  - ");
    msg.push_str(url);
    msg.push('\n');
  }
  bail!(msg)
}

fn collect_remote_attr_values(
  regex: &Regex,
  input: &str,
  groups: &[usize],
  out: &mut BTreeSet<String>,
) {
  for caps in regex.captures_iter(input) {
    let Some(m) = groups.iter().find_map(|&idx| caps.get(idx)) else {
      continue;
    };
    let decoded = decode_html_entities_if_needed(m.as_str());
    let trimmed = decoded.trim();
    if is_remote_fetch_url(trimmed) {
      out.insert(trimmed.to_string());
    }
  }
}

fn collect_remote_srcset_candidates(
  regex: &Regex,
  input: &str,
  groups: &[usize],
  out: &mut BTreeSet<String>,
) {
  for caps in regex.captures_iter(input) {
    let Some(m) = groups.iter().find_map(|&idx| caps.get(idx)) else {
      continue;
    };
    for candidate in parse_srcset_urls(m.as_str(), 32) {
      let decoded = decode_html_entities_if_needed(candidate.trim());
      let trimmed = decoded.trim();
      if is_remote_fetch_url(trimmed) {
        out.insert(trimmed.to_string());
      }
    }
  }
}

fn parse_srcset_urls(srcset: &str, max_candidates: usize) -> Vec<String> {
  fastrender::html::image_attrs::parse_srcset_with_limit(srcset, max_candidates)
    .into_iter()
    .map(|candidate| candidate.url)
    .collect()
}

fn rewrite_srcset_with_limit(
  input: &str,
  base_url: &Url,
  ctx: ReferenceContext,
  catalog: &mut AssetCatalog,
  max_candidates: usize,
) -> Result<String> {
  use fastrender::tree::box_tree::SrcsetDescriptor;

  let mut rewritten = Vec::new();
  for candidate in fastrender::html::image_attrs::parse_srcset_with_limit(input, max_candidates) {
    let rewritten_url =
      rewrite_reference(&candidate.url, base_url, ctx, catalog)?.unwrap_or_else(|| candidate.url);
    let entry = match candidate.descriptor {
      SrcsetDescriptor::Density(d) if d == 1.0 => rewritten_url,
      descriptor => format!("{rewritten_url} {descriptor}"),
    };
    rewritten.push(entry);
  }
  Ok(rewritten.join(", "))
}

fn is_remote_fetch_url(url: &str) -> bool {
  let lower = url.trim_start().to_ascii_lowercase();
  lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("//")
}

fn decode_html_entities_if_needed(input: &str) -> Cow<'_, str> {
  if !input.contains('&') {
    return Cow::Borrowed(input);
  }
  Cow::Owned(decode_html_entities(input))
}

fn decode_html_entities(input: &str) -> String {
  let mut out = String::with_capacity(input.len());
  let mut chars = input.chars().peekable();
  while let Some(c) = chars.next() {
    if c != '&' {
      out.push(c);
      continue;
    }

    let mut entity = String::new();
    while let Some(&next) = chars.peek() {
      entity.push(next);
      chars.next();
      if next == ';' {
        break;
      }
    }

    if entity.is_empty() {
      out.push('&');
      continue;
    }

    let mut ent = entity.as_str();
    if let Some(stripped) = ent.strip_prefix('/') {
      ent = stripped;
    }

    let decoded = match ent {
      "amp;" => Some('&'),
      "quot;" => Some('"'),
      "apos;" => Some('\''),
      "lt;" => Some('<'),
      "gt;" => Some('>'),
      _ => {
        if let Some(num) = ent.strip_prefix('#') {
          let trimmed = num.trim_end_matches(';');
          if let Some(hex) = trimmed.strip_prefix(['x', 'X']) {
            u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
          } else {
            trimmed.parse::<u32>().ok().and_then(char::from_u32)
          }
        } else {
          None
        }
      }
    };

    if let Some(ch) = decoded {
      out.push(ch);
    } else {
      out.push('&');
      out.push_str(&entity);
    }
  }
  out
}

fn extract_fetchable_css_urls(css: &str) -> Vec<String> {
  use cssparser::{Parser, ParserInput, Token};

  fn record(out: &mut Vec<String>, raw: &str) {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
      return;
    }
    out.push(trimmed.to_string());
  }

  fn scan<'i, 't>(parser: &mut Parser<'i, 't>, out: &mut Vec<String>) {
    while !parser.is_exhausted() {
      let token = match parser.next_including_whitespace_and_comments() {
        Ok(t) => t,
        Err(_) => break,
      };

      match token {
        Token::UnquotedUrl(url) => record(out, url.as_ref()),
        Token::Function(name) if name.eq_ignore_ascii_case("url") => {
          let parse_result = parser.parse_nested_block(|nested| {
            let mut arg: Option<String> = None;
            while !nested.is_exhausted() {
              match nested.next_including_whitespace_and_comments() {
                Ok(Token::WhiteSpace(_)) | Ok(Token::Comment(_)) => {}
                Ok(Token::QuotedString(s)) | Ok(Token::UnquotedUrl(s)) => {
                  arg = Some(s.as_ref().to_string());
                  break;
                }
                Ok(Token::Ident(s)) => {
                  arg = Some(s.as_ref().to_string());
                  break;
                }
                Ok(Token::BadUrl(_)) | Err(_) => break,
                Ok(_) => {}
              }
            }
            Ok::<_, cssparser::ParseError<'i, ()>>(arg)
          });

          if let Ok(Some(arg)) = parse_result {
            record(out, &arg);
          }
        }
        Token::AtKeyword(name) if name.eq_ignore_ascii_case("import") => {
          let mut target: Option<String> = None;
          while !parser.is_exhausted() {
            let next = match parser.next_including_whitespace_and_comments() {
              Ok(t) => t,
              Err(_) => break,
            };
            match next {
              Token::WhiteSpace(_) | Token::Comment(_) => continue,
              Token::QuotedString(s) | Token::UnquotedUrl(s) => {
                target = Some(s.as_ref().to_string());
                break;
              }
              Token::Function(fname) if fname.eq_ignore_ascii_case("url") => {
                let parse_result = parser.parse_nested_block(|nested| {
                  let mut arg: Option<String> = None;
                  while !nested.is_exhausted() {
                    match nested.next_including_whitespace_and_comments() {
                      Ok(Token::WhiteSpace(_)) | Ok(Token::Comment(_)) => {}
                      Ok(Token::QuotedString(s)) | Ok(Token::UnquotedUrl(s)) => {
                        arg = Some(s.as_ref().to_string());
                        break;
                      }
                      Ok(Token::Ident(s)) => {
                        arg = Some(s.as_ref().to_string());
                        break;
                      }
                      Ok(Token::BadUrl(_)) | Err(_) => break,
                      Ok(_) => {}
                    }
                  }
                  Ok::<_, cssparser::ParseError<'i, ()>>(arg)
                });
                target = parse_result.ok().flatten();
                break;
              }
              Token::Ident(s) => {
                target = Some(s.as_ref().to_string());
                break;
              }
              Token::Semicolon => break,
              _ => break,
            }
          }
          if let Some(target) = target {
            record(out, &target);
          }
        }
        Token::Function(_)
        | Token::ParenthesisBlock
        | Token::SquareBracketBlock
        | Token::CurlyBracketBlock => {
          let _ = parser.parse_nested_block(|nested| {
            scan(nested, out);
            Ok::<_, cssparser::ParseError<'i, ()>>(())
          });
        }
        _ => {}
      }
    }
  }

  let mut out = Vec::new();
  let mut input = ParserInput::new(css);
  let mut parser = Parser::new(&mut input);
  scan(&mut parser, &mut out);
  out
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;
  use std::fs;
  use tempfile::tempdir;

  fn write_synthetic_bundle(dir: &Path, include_font: bool) -> Result<()> {
    let resources_dir = dir.join("resources");
    fs::create_dir_all(&resources_dir)?;

    let document_html = r#"<!doctype html>
<html>
  <head>
    <link rel="preconnect" href="https://cdn.example.test">
    <link rel="preload" href="https://example.test/preload.png" as="image">
    <link rel="icon" href="https://example.test/favicon.ico">
    <link rel="stylesheet" href="https://example.test/print.css" media="print">
  </head>
  <body>
    <iframe src="https://example.test/frame.html"></iframe>
  </body>
</html>
"#;
    fs::write(dir.join("document.html"), document_html)?;

    let frame_html = r#"<!doctype html>
<html>
  <head>
    <style>@font-face{src:url(//cdn.example.test/font.woff2)}</style>
  </head>
  <body>
    <img src="https://example.test/img.png">
    <video controls><source src="https://example.test/movie.mp4" type="video/mp4"></video>
  </body>
</html>
"#;
    fs::write(resources_dir.join("00000_frame.html"), frame_html)?;

    fs::write(resources_dir.join("00001_img.png"), b"dummy png")?;
    if include_font {
      fs::write(resources_dir.join("00002_font.woff2"), b"dummy font")?;
    }

    let mut resources = serde_json::Map::new();
    resources.insert(
      "https://example.test/frame.html".to_string(),
      json!({
        "path": "resources/00000_frame.html",
        "content_type": "text/html; charset=utf-8",
        "status": 200,
        "final_url": "https://example.test/frame.html",
        "etag": null,
        "last_modified": null
      }),
    );
    resources.insert(
      "https://example.test/img.png".to_string(),
      json!({
        "path": "resources/00001_img.png",
        "content_type": "image/png",
        "status": 200,
        "final_url": "https://example.test/img.png",
        "etag": null,
        "last_modified": null
      }),
    );
    if include_font {
      resources.insert(
        "https://cdn.example.test/font.woff2".to_string(),
        json!({
          "path": "resources/00002_font.woff2",
          "content_type": "font/woff2",
          "status": 200,
          "final_url": "https://cdn.example.test/font.woff2",
          "etag": null,
          "last_modified": null
        }),
      );
    }

    let manifest = json!({
      "version": 1,
      "original_url": "https://example.test/",
      "document": {
        "path": "document.html",
        "content_type": "text/html; charset=utf-8",
        "final_url": "https://example.test/",
        "status": 200,
        "etag": null,
        "last_modified": null
      },
      "render": {
        "viewport": [800, 600],
        "device_pixel_ratio": 1.0,
        "scroll_x": 0.0,
        "scroll_y": 0.0,
        "full_page": false
      },
      "resources": resources
    });
    fs::write(
      dir.join("bundle.json"),
      serde_json::to_vec_pretty(&manifest).expect("manifest json"),
    )?;

    Ok(())
  }

  fn write_synthetic_bundle_with_noscript_missing_image(dir: &Path) -> Result<()> {
    let resources_dir = dir.join("resources");
    fs::create_dir_all(&resources_dir)?;

    let document_html = r#"<!doctype html>
<html>
  <body>
    <noscript><img src="https://example.test/missing.png"></noscript>
    <img src="https://example.test/img.png">
  </body>
</html>
"#;
    fs::write(dir.join("document.html"), document_html)?;
    fs::write(resources_dir.join("00000_img.png"), b"dummy png")?;

    let manifest = json!({
      "version": 1,
      "original_url": "https://example.test/",
      "document": {
        "path": "document.html",
        "content_type": "text/html; charset=utf-8",
        "final_url": "https://example.test/",
        "status": 200,
        "etag": null,
        "last_modified": null
      },
      "render": {
        "viewport": [800, 600],
        "device_pixel_ratio": 1.0,
        "scroll_x": 0.0,
        "scroll_y": 0.0,
        "full_page": false
      },
      "resources": {
        "https://example.test/img.png": {
          "path": "resources/00000_img.png",
          "content_type": "image/png",
          "status": 200,
          "final_url": "https://example.test/img.png",
          "etag": null,
          "last_modified": null
        }
      }
    });
    fs::write(
      dir.join("bundle.json"),
      serde_json::to_vec_pretty(&manifest).expect("manifest json"),
    )?;

    Ok(())
  }

  fn write_synthetic_bundle_with_lazy_loaded_images(dir: &Path) -> Result<()> {
    let resources_dir = dir.join("resources");
    fs::create_dir_all(&resources_dir)?;

    // Use a well-known 1×1 transparent GIF placeholder (common in lazy-loading implementations).
    let document_html = r#"<!doctype html>
<html>
  <body>
    <picture>
      <source data-srcset="https://example.test/img.webp" type="image/webp">
      <img
        src="data:image/gif;base64,R0lGODlhAQABAIAAAAAAAP///ywAAAAAAQABAAACAUwAOw=="
        data-src="https://example.test/img.png"
        alt="lazy image"
      >
    </picture>
  </body>
</html>
"#;
    fs::write(dir.join("document.html"), document_html)?;

    fs::write(resources_dir.join("00000_img.webp"), b"dummy webp")?;
    fs::write(resources_dir.join("00001_img.png"), b"dummy png")?;

    let manifest = json!({
      "version": 1,
      "original_url": "https://example.test/",
      "document": {
        "path": "document.html",
        "content_type": "text/html; charset=utf-8",
        "final_url": "https://example.test/",
        "status": 200,
        "etag": null,
        "last_modified": null
      },
      "render": {
        "viewport": [800, 600],
        "device_pixel_ratio": 1.0,
        "scroll_x": 0.0,
        "scroll_y": 0.0,
        "full_page": false
      },
      "resources": {
        "https://example.test/img.webp": {
          "path": "resources/00000_img.webp",
          "content_type": "image/webp",
          "status": 200,
          "final_url": "https://example.test/img.webp",
          "etag": null,
          "last_modified": null
        },
        "https://example.test/img.png": {
          "path": "resources/00001_img.png",
          "content_type": "image/png",
          "status": 200,
          "final_url": "https://example.test/img.png",
          "etag": null,
          "last_modified": null
        }
      }
    });
    fs::write(
      dir.join("bundle.json"),
      serde_json::to_vec_pretty(&manifest).expect("manifest json"),
    )?;

    Ok(())
  }

  fn write_synthetic_bundle_with_live_sample_iframe(dir: &Path) -> Result<()> {
    let resources_dir = dir.join("resources");
    fs::create_dir_all(&resources_dir)?;

    // Simulate MDN "live sample" iframes that ship with placeholder `src` but include the
    // live-sample metadata attributes.
    let document_html = r#"<!doctype html>
<html>
  <body>
    <iframe src="about:blank" data-live-path="/live/" data-live-id="frame"></iframe>
  </body>
</html>
"#;
    fs::write(dir.join("document.html"), document_html)?;

    let frame_html = r#"<!doctype html>
<html>
  <head>
    <style>@font-face{src:url(//cdn.example.test/font.woff2)}</style>
  </head>
  <body>
    <img src="https://example.test/img.png">
  </body>
</html>
"#;
    fs::write(resources_dir.join("00000_frame.html"), frame_html)?;
    fs::write(resources_dir.join("00001_img.png"), b"dummy png")?;
    fs::write(resources_dir.join("00002_font.woff2"), b"dummy font")?;

    let manifest = json!({
      "version": 1,
      "original_url": "https://example.test/",
      "document": {
        "path": "document.html",
        "content_type": "text/html; charset=utf-8",
        "final_url": "https://example.test/",
        "status": 200,
        "etag": null,
        "last_modified": null
      },
      "render": {
        "viewport": [800, 600],
        "device_pixel_ratio": 1.0,
        "scroll_x": 0.0,
        "scroll_y": 0.0,
        "full_page": false
      },
      "resources": {
        "https://example.test/live/frame.html": {
          "path": "resources/00000_frame.html",
          "content_type": "text/html; charset=utf-8",
          "status": 200,
          "final_url": "https://example.test/live/frame.html",
          "etag": null,
          "last_modified": null
        },
        "https://example.test/img.png": {
          "path": "resources/00001_img.png",
          "content_type": "image/png",
          "status": 200,
          "final_url": "https://example.test/img.png",
          "etag": null,
          "last_modified": null
        },
        "https://cdn.example.test/font.woff2": {
          "path": "resources/00002_font.woff2",
          "content_type": "font/woff2",
          "status": 200,
          "final_url": "https://cdn.example.test/font.woff2",
          "etag": null,
          "last_modified": null
        }
      }
    });
    fs::write(
      dir.join("bundle.json"),
      serde_json::to_vec_pretty(&manifest).expect("manifest json"),
    )?;

    Ok(())
  }

  fn write_synthetic_bundle_with_embedded_live_sample_code_blocks(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir.join("resources"))?;

    // Synthetic MDN-style code blocks that the importer can stitch into an iframe document when
    // the derived `data-live-path + data-live-id + ".html"` resource is absent from the bundle.
    let document_html = r#"<!doctype html>
<html>
  <body>
    <pre class="brush: html notranslate live-sample---demo"><code>&lt;p&gt;Hello&lt;/p&gt;</code></pre>
    <pre class="brush: css notranslate live-sample---demo"><code>p{color:red}</code></pre>
    <iframe src="about:blank" data-live-path="/live/" data-live-id="demo"></iframe>
  </body>
</html>
"#;
    fs::write(dir.join("document.html"), document_html)?;

    let manifest = json!({
      "version": 1,
      "original_url": "https://example.test/",
      "document": {
        "path": "document.html",
        "content_type": "text/html; charset=utf-8",
        "final_url": "https://example.test/",
        "status": 200,
        "etag": null,
        "last_modified": null
      },
      "render": {
        "viewport": [800, 600],
        "device_pixel_ratio": 1.0,
        "scroll_x": 0.0,
        "scroll_y": 0.0,
        "full_page": false
      },
      "resources": {}
    });
    fs::write(
      dir.join("bundle.json"),
      serde_json::to_vec_pretty(&manifest).expect("manifest json"),
    )?;
    Ok(())
  }

  fn write_synthetic_bundle_with_font_face_fallbacks(dir: &Path) -> Result<()> {
    let resources_dir = dir.join("resources");
    fs::create_dir_all(&resources_dir)?;

    let document_html = r#"<!doctype html>
<html>
  <head>
    <link rel="stylesheet" href="https://example.test/style.css">
  </head>
  <body>test</body>
</html>
"#;
    fs::write(dir.join("document.html"), document_html)?;

    let css = br#"@font-face{
  font-family:"X";
  src:url("/font.eot");
  src:url("/font.eot?#iefix") format("embedded-opentype"),url("/font.woff2") format("woff2"),url("/font.woff") format("woff");
}
body{background:url("/bg.png");}
"#;
    fs::write(resources_dir.join("00000_style.css"), css)?;
    fs::write(resources_dir.join("00001_font.woff2"), b"dummy font")?;
    fs::write(resources_dir.join("00002_bg.png"), b"dummy png")?;

    let manifest = json!({
      "version": 1,
      "original_url": "https://example.test/",
      "document": {
        "path": "document.html",
        "content_type": "text/html; charset=utf-8",
        "final_url": "https://example.test/",
        "status": 200,
        "etag": null,
        "last_modified": null
      },
      "render": {
        "viewport": [800, 600],
        "device_pixel_ratio": 1.0,
        "scroll_x": 0.0,
        "scroll_y": 0.0,
        "full_page": false
      },
      "resources": {
        "https://example.test/style.css": {
          "path": "resources/00000_style.css",
          "content_type": "text/css; charset=utf-8",
          "status": 200,
          "final_url": "https://example.test/style.css",
          "etag": null,
          "last_modified": null
        },
        "https://example.test/font.woff2": {
          "path": "resources/00001_font.woff2",
          "content_type": "font/woff2",
          "status": 200,
          "final_url": "https://example.test/font.woff2",
          "etag": null,
          "last_modified": null
        },
        "https://example.test/bg.png": {
          "path": "resources/00002_bg.png",
          "content_type": "image/png",
          "status": 200,
          "final_url": "https://example.test/bg.png",
          "etag": null,
          "last_modified": null
        }
      }
    });

    fs::write(
      dir.join("bundle.json"),
      serde_json::to_vec_pretty(&manifest).expect("manifest json"),
    )?;
    Ok(())
  }

  fn assert_no_remote_url_strings(content: &str) {
    let lower = content.to_ascii_lowercase();
    assert!(
      !lower.contains("http://"),
      "unexpected remote http:// reference: {content}"
    );
    assert!(
      !lower.contains("https://"),
      "unexpected remote https:// reference: {content}"
    );
    assert!(
      !lower.contains("url(//"),
      "unexpected scheme-relative url(): {content}"
    );
    assert!(
      !lower.contains("src=\"//") && !lower.contains("src='//"),
      "unexpected scheme-relative src= reference: {content}"
    );
    assert!(
      !lower.contains("href=\"//") && !lower.contains("href='//"),
      "unexpected scheme-relative href= reference: {content}"
    );
    assert!(
      !lower.contains("srcset=\"//") && !lower.contains("srcset='//"),
      "unexpected scheme-relative srcset= reference: {content}"
    );
  }

  #[test]
  fn imports_and_rewrites_nested_html_assets() -> Result<()> {
    let bundle_dir = tempdir()?;
    write_synthetic_bundle(bundle_dir.path(), true)?;

    let output = tempdir()?;
    let output_root = output.path().join("fixtures");
    let fixture_name = "example_test";

    run_import_page_fixture(ImportPageFixtureArgs {
      bundle: bundle_dir.path().to_path_buf(),
      fixture_name: fixture_name.to_string(),
      output_root: output_root.clone(),
      overwrite: true,
      include_media: false,
      media_max_bytes: DEFAULT_MEDIA_MAX_BYTES,
      media_max_file_bytes: DEFAULT_MEDIA_MAX_FILE_BYTES,
      allow_missing: false,
      allow_http_references: false,
      legacy_rewrite: false,
      rewrite_scripts: false,
      dry_run: false,
    })?;

    let fixture_dir = output_root.join(fixture_name);
    let index_html = fs::read_to_string(fixture_dir.join("index.html"))?;
    assert_no_remote_url_strings(&index_html);
    assert!(
      !index_html.contains("rel=\"preconnect\"")
        && !index_html.contains("rel=\"preload\"")
        && !index_html.contains("rel=\"icon\""),
      "expected preconnect/preload/icon link tags to be stripped from output HTML"
    );

    let assets_dir = fixture_dir.join(ASSETS_DIR);
    let mut html_assets = Vec::new();
    let mut png_asset = None;
    let mut woff2_asset = None;
    for entry in fs::read_dir(&assets_dir)? {
      let entry = entry?;
      if !entry.file_type()?.is_file() {
        continue;
      }
      let filename = entry.file_name().to_string_lossy().to_string();
      if filename.ends_with(".html") {
        html_assets.push(filename);
      } else if filename.ends_with(".png") {
        png_asset = Some(filename);
      } else if filename.ends_with(".woff2") {
        woff2_asset = Some(filename);
      }
    }
    assert_eq!(html_assets.len(), 1, "expected exactly one HTML asset");

    let frame_asset = &html_assets[0];
    assert!(
      index_html.contains(&format!("{ASSETS_DIR}/{frame_asset}")),
      "index.html should rewrite iframe src to point at the local HTML asset"
    );

    let frame_html = fs::read_to_string(assets_dir.join(frame_asset))?;
    assert_no_remote_url_strings(&frame_html);

    let optional_css_url = "https://example.test/print.css";
    let optional_css_asset = format!("missing_{}.css", hash_bytes(optional_css_url.as_bytes()));
    assert!(
      index_html.contains(&format!("{ASSETS_DIR}/{optional_css_asset}")),
      "expected optional stylesheet href to be rewritten"
    );
    assert!(
      assets_dir.join(&optional_css_asset).exists(),
      "expected optional stylesheet placeholder asset to be created"
    );

    let optional_video_url = "https://example.test/movie.mp4";
    let optional_video_asset = format!("missing_{}.mp4", hash_bytes(optional_video_url.as_bytes()));
    assert!(
      frame_html.contains(&optional_video_asset),
      "expected nested HTML asset to reference optional media placeholder {optional_video_asset}"
    );
    assert!(
      assets_dir.join(&optional_video_asset).exists(),
      "expected optional media placeholder asset to be created"
    );

    let png_asset = png_asset.expect("missing png asset");
    let woff2_asset = woff2_asset.expect("missing woff2 asset");
    assert!(
      frame_html.contains(&png_asset),
      "iframe HTML should reference local image asset {png_asset}"
    );
    assert!(
      frame_html.contains(&woff2_asset),
      "iframe HTML should reference local font asset {woff2_asset}"
    );

    Ok(())
  }

  #[test]
  fn imports_and_rewrites_iframe_data_live_path_id_placeholders() -> Result<()> {
    let bundle_dir = tempdir()?;
    write_synthetic_bundle_with_live_sample_iframe(bundle_dir.path())?;

    let output = tempdir()?;
    let output_root = output.path().join("fixtures");
    let fixture_name = "example_live_sample";

    run_import_page_fixture(ImportPageFixtureArgs {
      bundle: bundle_dir.path().to_path_buf(),
      fixture_name: fixture_name.to_string(),
      output_root: output_root.clone(),
      overwrite: true,
      include_media: false,
      media_max_bytes: DEFAULT_MEDIA_MAX_BYTES,
      media_max_file_bytes: DEFAULT_MEDIA_MAX_FILE_BYTES,
      allow_missing: false,
      allow_http_references: false,
      legacy_rewrite: false,
      rewrite_scripts: false,
      dry_run: false,
    })?;

    let fixture_dir = output_root.join(fixture_name);
    let index_html = fs::read_to_string(fixture_dir.join("index.html"))?;
    assert_no_remote_url_strings(&index_html);
    assert!(
      !index_html.contains("about:blank"),
      "expected placeholder iframe src to be replaced with a live-sample URL and rewritten"
    );

    let assets_dir = fixture_dir.join(ASSETS_DIR);
    let mut html_assets = Vec::new();
    let mut png_asset = None;
    let mut woff2_asset = None;
    for entry in fs::read_dir(&assets_dir)? {
      let entry = entry?;
      if !entry.file_type()?.is_file() {
        continue;
      }
      let filename = entry.file_name().to_string_lossy().to_string();
      if filename.ends_with(".html") {
        html_assets.push(filename);
      } else if filename.ends_with(".png") {
        png_asset = Some(filename);
      } else if filename.ends_with(".woff2") {
        woff2_asset = Some(filename);
      }
    }
    assert_eq!(html_assets.len(), 1, "expected exactly one HTML asset");
    let frame_asset = &html_assets[0];
    assert!(
      index_html.contains(&format!("{ASSETS_DIR}/{frame_asset}")),
      "index.html should rewrite iframe src to point at the local HTML asset"
    );

    let frame_html = fs::read_to_string(assets_dir.join(frame_asset))?;
    assert_no_remote_url_strings(&frame_html);

    let png_asset = png_asset.expect("missing png asset");
    let woff2_asset = woff2_asset.expect("missing woff2 asset");
    assert!(
      frame_html.contains(&png_asset),
      "iframe HTML should reference local image asset {png_asset}"
    );
    assert!(
      frame_html.contains(&woff2_asset),
      "iframe HTML should reference local font asset {woff2_asset}"
    );

    Ok(())
  }

  #[test]
  fn imports_and_synthesizes_live_sample_iframe_html_from_code_blocks() -> Result<()> {
    let bundle_dir = tempdir()?;
    write_synthetic_bundle_with_embedded_live_sample_code_blocks(bundle_dir.path())?;

    let output = tempdir()?;
    let output_root = output.path().join("fixtures");
    let fixture_name = "example_live_sample_synth";

    run_import_page_fixture(ImportPageFixtureArgs {
      bundle: bundle_dir.path().to_path_buf(),
      fixture_name: fixture_name.to_string(),
      output_root: output_root.clone(),
      overwrite: true,
      include_media: false,
      media_max_bytes: DEFAULT_MEDIA_MAX_BYTES,
      media_max_file_bytes: DEFAULT_MEDIA_MAX_FILE_BYTES,
      allow_missing: false,
      allow_http_references: false,
      legacy_rewrite: false,
      rewrite_scripts: false,
      dry_run: false,
    })?;

    let fixture_dir = output_root.join(fixture_name);
    let index_html = fs::read_to_string(fixture_dir.join("index.html"))?;
    assert_no_remote_url_strings(&index_html);
    assert!(
      !index_html.contains("about:blank"),
      "expected placeholder iframe src to be replaced with synthesized live-sample HTML"
    );

    let assets_dir = fixture_dir.join(ASSETS_DIR);
    let html_assets: Vec<String> = fs::read_dir(&assets_dir)?
      .filter_map(|entry| entry.ok())
      .filter_map(|entry| {
        let filename = entry.file_name().to_string_lossy().to_string();
        if filename.ends_with(".html") {
          Some(filename)
        } else {
          None
        }
      })
      .collect();
    assert_eq!(html_assets.len(), 1, "expected one synthesized HTML asset");

    let frame_asset = &html_assets[0];
    assert!(
      index_html.contains(&format!("{ASSETS_DIR}/{frame_asset}")),
      "index.html should reference synthesized HTML asset {frame_asset}, got: {index_html}"
    );

    let frame_html = fs::read_to_string(assets_dir.join(frame_asset))?;
    assert_no_remote_url_strings(&frame_html);
    assert!(
      frame_html.contains("<p>Hello</p>"),
      "expected synthesized iframe HTML to contain decoded HTML snippet, got: {frame_html}"
    );
    assert!(
      frame_html.contains("color:red"),
      "expected synthesized iframe HTML to include decoded CSS snippet, got: {frame_html}"
    );

    Ok(())
  }

  #[test]
  fn import_prunes_font_face_fallback_sources() -> Result<()> {
    let bundle_dir = tempdir()?;
    write_synthetic_bundle_with_font_face_fallbacks(bundle_dir.path())?;

    let output = tempdir()?;
    let output_root = output.path().join("fixtures");
    let fixture_name = "font_face_fallbacks";

    run_import_page_fixture(ImportPageFixtureArgs {
      bundle: bundle_dir.path().to_path_buf(),
      fixture_name: fixture_name.to_string(),
      output_root: output_root.clone(),
      overwrite: true,
      include_media: false,
      media_max_bytes: DEFAULT_MEDIA_MAX_BYTES,
      media_max_file_bytes: DEFAULT_MEDIA_MAX_FILE_BYTES,
      allow_missing: false,
      allow_http_references: false,
      legacy_rewrite: false,
      rewrite_scripts: false,
      dry_run: false,
    })?;

    let fixture_dir = output_root.join(fixture_name);
    let index_html = fs::read_to_string(fixture_dir.join("index.html"))?;
    assert_no_remote_url_strings(&index_html);

    let assets_dir = fixture_dir.join(ASSETS_DIR);
    let css_asset = fs::read_dir(&assets_dir)?
      .filter_map(|entry| entry.ok())
      .find_map(|entry| {
        let filename = entry.file_name().to_string_lossy().to_string();
        if filename.ends_with(".css") {
          Some(filename)
        } else {
          None
        }
      })
      .expect("missing css asset");
    let rewritten_css = fs::read_to_string(assets_dir.join(&css_asset))?;
    assert_no_remote_url_strings(&rewritten_css);

    assert!(
      rewritten_css.contains(".woff2"),
      "expected rewritten css to reference bundled woff2 font: {rewritten_css}"
    );
    assert!(
      !rewritten_css.contains(".eot"),
      "expected rewritten css to drop non-decodable eot sources: {rewritten_css}"
    );
    assert!(
      !rewritten_css.contains(".woff)")
        && !rewritten_css.contains(".woff\"")
        && !rewritten_css.contains(".woff'"),
      "expected rewritten css to drop missing woff fallback sources: {rewritten_css}"
    );
    assert!(
      rewritten_css.contains(".png"),
      "expected rewritten css to rewrite background url() to local asset: {rewritten_css}"
    );

    Ok(())
  }

  #[test]
  fn import_strips_noscript_blocks() -> Result<()> {
    let bundle_dir = tempdir()?;
    write_synthetic_bundle_with_noscript_missing_image(bundle_dir.path())?;

    let output = tempdir()?;
    let output_root = output.path().join("fixtures");
    let fixture_name = "example_noscript";

    run_import_page_fixture(ImportPageFixtureArgs {
      bundle: bundle_dir.path().to_path_buf(),
      fixture_name: fixture_name.to_string(),
      output_root: output_root.clone(),
      overwrite: true,
      include_media: false,
      media_max_bytes: DEFAULT_MEDIA_MAX_BYTES,
      media_max_file_bytes: DEFAULT_MEDIA_MAX_FILE_BYTES,
      allow_missing: false,
      allow_http_references: false,
      legacy_rewrite: false,
      rewrite_scripts: false,
      dry_run: false,
    })?;

    let fixture_dir = output_root.join(fixture_name);
    let index_html = fs::read_to_string(fixture_dir.join("index.html"))?;
    assert_no_remote_url_strings(&index_html);
    assert!(
      !index_html.to_ascii_lowercase().contains("<noscript"),
      "expected noscript blocks to be stripped from output HTML: {index_html}"
    );
    Ok(())
  }

  #[test]
  fn import_promotes_lazy_loaded_image_attrs_to_src_and_srcset() -> Result<()> {
    let bundle_dir = tempdir()?;
    write_synthetic_bundle_with_lazy_loaded_images(bundle_dir.path())?;

    let output = tempdir()?;
    let output_root = output.path().join("fixtures");
    let fixture_name = "lazy_loading";

    run_import_page_fixture(ImportPageFixtureArgs {
      bundle: bundle_dir.path().to_path_buf(),
      fixture_name: fixture_name.to_string(),
      output_root: output_root.clone(),
      overwrite: true,
      include_media: false,
      media_max_bytes: DEFAULT_MEDIA_MAX_BYTES,
      media_max_file_bytes: DEFAULT_MEDIA_MAX_FILE_BYTES,
      allow_missing: false,
      allow_http_references: false,
      legacy_rewrite: false,
      rewrite_scripts: false,
      dry_run: false,
    })?;

    let fixture_dir = output_root.join(fixture_name);
    let index_html = fs::read_to_string(fixture_dir.join("index.html"))?;
    assert_no_remote_url_strings(&index_html);
    assert!(
      !index_html.contains("data:image/gif"),
      "expected placeholder src to be replaced with local asset: {index_html}"
    );

    let assets_dir = fixture_dir.join(ASSETS_DIR);
    let mut png_asset = None;
    let mut webp_asset = None;
    for entry in fs::read_dir(&assets_dir)? {
      let entry = entry?;
      if !entry.file_type()?.is_file() {
        continue;
      }
      let filename = entry.file_name().to_string_lossy().to_string();
      if filename.ends_with(".png") {
        png_asset = Some(filename);
      } else if filename.ends_with(".webp") {
        webp_asset = Some(filename);
      }
    }

    let png_asset = png_asset.expect("missing png asset");
    let webp_asset = webp_asset.expect("missing webp asset");
    let png_path = format!("{ASSETS_DIR}/{png_asset}");
    let webp_path = format!("{ASSETS_DIR}/{webp_asset}");

    assert!(
      index_html.contains(&format!("src=\"{png_path}\"")),
      "expected lazy-loaded img src= to be promoted and rewritten: {index_html}"
    );
    assert!(
      index_html.contains(&format!("data-src=\"{png_path}\"")),
      "expected data-src= to be rewritten to local asset: {index_html}"
    );
    assert!(
      index_html.contains(&format!("srcset=\"{webp_path}\"")),
      "expected lazy-loaded source srcset= to be promoted and rewritten: {index_html}"
    );
    assert!(
      index_html.contains(&format!("data-srcset=\"{webp_path}\"")),
      "expected data-srcset= to be rewritten to local asset: {index_html}"
    );

    Ok(())
  }

  #[test]
  fn import_fails_when_nested_html_references_missing_asset() -> Result<()> {
    let bundle_dir = tempdir()?;
    write_synthetic_bundle(bundle_dir.path(), false)?;

    let output = tempdir()?;
    let output_root = output.path().join("fixtures");

    let res = run_import_page_fixture(ImportPageFixtureArgs {
      bundle: bundle_dir.path().to_path_buf(),
      fixture_name: "example_test".to_string(),
      output_root,
      overwrite: true,
      include_media: false,
      media_max_bytes: DEFAULT_MEDIA_MAX_BYTES,
      media_max_file_bytes: DEFAULT_MEDIA_MAX_FILE_BYTES,
      allow_missing: false,
      allow_http_references: false,
      legacy_rewrite: false,
      rewrite_scripts: false,
      dry_run: false,
    });
    assert!(
      res.is_err(),
      "import should fail when a fetchable asset is missing"
    );
    Ok(())
  }

  #[test]
  fn rewrite_css_rewrites_unclosed_url_functions() -> Result<()> {
    let base = Url::parse("https://example.test/")?;
    let mut catalog = AssetCatalog::new(true);
    let css = "background-image:url(https://example.test/img";
    let rewritten = rewrite_css(css, &base, &mut catalog, ReferenceContext::Html)?;
    assert!(
      !rewritten.contains("https://example.test/img"),
      "expected url() to be rewritten, got: {rewritten}"
    );
    assert!(
      rewritten.contains("assets/missing_"),
      "expected placeholder asset to be inserted, got: {rewritten}"
    );
    Ok(())
  }

  #[test]
  fn rewrite_css_rewrites_import_without_whitespace() -> Result<()> {
    let base = Url::parse("https://example.test/")?;
    let mut catalog = AssetCatalog::new(true);
    let css = "@import\"https://example.test/style.css\";";
    let rewritten = rewrite_css(css, &base, &mut catalog, ReferenceContext::Html)?;
    assert!(
      !rewritten.contains("https://example.test/style.css"),
      "expected @import to be rewritten, got: {rewritten}"
    );
    assert!(
      rewritten.contains("assets/missing_"),
      "expected placeholder asset to be inserted, got: {rewritten}"
    );
    Ok(())
  }

  #[test]
  fn rewrite_html_rewrites_script_src_only_when_enabled() -> Result<()> {
    let base = Url::parse("https://example.test/")?;
    let script_url = "https://example.test/app.js";
    let script_bytes = b"console.log('hi');".to_vec();

    let info = BundledResourceInfo {
      path: "resources/00000_app.bin".to_string(),
      content_type: Some("application/javascript".to_string()),
      nosniff: false,
      status: Some(200),
      final_url: Some(script_url.to_string()),
      etag: None,
      last_modified: None,
      response_referrer_policy: None,
      response_headers: None,
      vary: None,
      access_control_allow_origin: None,
      timing_allow_origin: None,
      access_control_allow_credentials: false,
    };
    let res = FetchedResource::new(
      script_bytes.clone(),
      Some("application/javascript".to_string()),
    );

    let html = format!("<!doctype html><script src=\"{script_url}\"></script>");
    let expected_filename = format!("{}.js", hash_bytes(&script_bytes));

    let mut catalog = AssetCatalog::new(false);
    catalog.add_resource(script_url, &info, &res)?;
    let rewritten = rewrite_html(
      &html,
      &base,
      ReferenceContext::Html,
      &mut catalog,
      false,
      true,
    )?;
    assert!(
      rewritten.contains(&format!("src=\"assets/{expected_filename}\"")),
      "expected script src to be rewritten, got: {rewritten}"
    );

    let mut catalog = AssetCatalog::new(false);
    catalog.add_resource(script_url, &info, &res)?;
    let not_rewritten = rewrite_html(
      &html,
      &base,
      ReferenceContext::Html,
      &mut catalog,
      false,
      false,
    )?;
    assert!(
      not_rewritten.contains(script_url),
      "expected script src to remain untouched, got: {not_rewritten}"
    );
    Ok(())
  }

  #[test]
  fn rewrite_html_rewrites_script_preload_link_only_when_enabled() -> Result<()> {
    let base = Url::parse("https://example.test/")?;
    let script_url = "https://example.test/app.js";
    let script_bytes = b"console.log('hi');".to_vec();

    let info = BundledResourceInfo {
      path: "resources/00000_app.bin".to_string(),
      content_type: Some("application/javascript".to_string()),
      nosniff: false,
      status: Some(200),
      final_url: Some(script_url.to_string()),
      etag: None,
      last_modified: None,
      response_referrer_policy: None,
      response_headers: None,
      vary: None,
      access_control_allow_origin: None,
      timing_allow_origin: None,
      access_control_allow_credentials: false,
    };
    let res = FetchedResource::new(
      script_bytes.clone(),
      Some("application/javascript".to_string()),
    );
    let expected_filename = format!("{}.js", hash_bytes(&script_bytes));

    let html = format!("<!doctype html><link rel=\"preload\" as=\"script\" href=\"{script_url}\">");

    let mut catalog = AssetCatalog::new(false);
    catalog.add_resource(script_url, &info, &res)?;
    let rewritten = rewrite_html(
      &html,
      &base,
      ReferenceContext::Html,
      &mut catalog,
      false,
      true,
    )?;
    assert!(
      rewritten.contains("rel=\"preload\""),
      "expected preload link tag to remain in output, got: {rewritten}"
    );
    assert!(
      rewritten.contains(&format!("href=\"assets/{expected_filename}\"")),
      "expected preload href to be rewritten, got: {rewritten}"
    );

    let mut catalog = AssetCatalog::new(false);
    catalog.add_resource(script_url, &info, &res)?;
    let not_rewritten = rewrite_html(
      &html,
      &base,
      ReferenceContext::Html,
      &mut catalog,
      false,
      false,
    )?;
    assert!(
      !not_rewritten.contains("rel=\"preload\""),
      "expected preload link tag to be stripped, got: {not_rewritten}"
    );
    Ok(())
  }

  #[test]
  fn rewrite_reference_strips_wrapping_quotes_after_entity_decode() -> Result<()> {
    let base = Url::parse("https://example.test/")?;
    let resource_url = "https://cdn.example.test/bg.png";
    let info = BundledResourceInfo {
      path: "resources/00000_bg.png".to_string(),
      content_type: Some("image/png".to_string()),
      nosniff: false,
      status: Some(200),
      final_url: Some(resource_url.to_string()),
      etag: None,
      last_modified: None,
      response_referrer_policy: None,
      response_headers: None,
      vary: None,
      access_control_allow_origin: None,
      timing_allow_origin: None,
      access_control_allow_credentials: false,
    };
    let res = FetchedResource::new(vec![0u8, 1, 2, 3], Some("image/png".to_string()));

    let mut catalog = AssetCatalog::new(false);
    catalog.add_resource(resource_url, &info, &res)?;

    let rewritten = rewrite_reference(
      "&quot;//cdn.example.test/bg.png&quot;",
      &base,
      ReferenceContext::Html,
      &mut catalog,
    )?;
    assert!(
      rewritten.is_some(),
      "expected wrapped URL to rewrite to a local asset"
    );
    Ok(())
  }

  #[test]
  fn validate_html_rejects_remote_script_src_only_when_enabled() -> Result<()> {
    let html = r#"<!doctype html><script src="https://example.test/app.js"></script>"#;
    validate_no_remote_fetchable_subresources_in_html("index.html", html, false)?;
    let err = validate_no_remote_fetchable_subresources_in_html("index.html", html, true)
      .expect_err("validation should fail when script validation is enabled");
    assert!(
      err.to_string().contains("https://example.test/app.js"),
      "error should mention the remote script URL, got: {err}"
    );
    Ok(())
  }

  #[test]
  fn mdn_live_sample_iframe_generation_rewrites_about_blank_iframe() -> Result<()> {
    let base = Url::parse("https://example.test/")?;
    let input = r#"<!doctype html>
<pre class="brush: html notranslate live-sample---demo"><code>&lt;div id=&quot;box&quot;&gt;Hello&lt;/div&gt;</code></pre>
<pre class="brush: css notranslate live-sample---demo"><code>#box { color: red; }</code></pre>
<iframe class="sample-code-frame" src="about:blank" data-live-id="demo"></iframe>
"#;

    let mut catalog = AssetCatalog::new(false);
    let rewritten = rewrite_mdn_live_sample_iframes(
      input,
      &base,
      ReferenceContext::Html,
      &mut catalog,
      false,
      false,
    )?;

    assert_eq!(
      catalog.assets.len(),
      1,
      "expected exactly one generated iframe asset, got keys: {:?}",
      catalog.assets.keys().collect::<Vec<_>>()
    );
    let filename = catalog
      .assets
      .keys()
      .next()
      .expect("missing generated asset filename")
      .clone();
    assert!(
      rewritten.contains(&format!("src=\"assets/{filename}\"")),
      "expected iframe src to be rewritten to local asset, got: {rewritten}"
    );
    let asset = catalog
      .assets
      .get(&filename)
      .unwrap_or_else(|| panic!("missing generated asset {filename}"));
    let asset_html = String::from_utf8_lossy(&asset.bytes);
    assert!(
      asset_html.contains("<div id=\"box\">Hello</div>"),
      "expected decoded HTML snippet in generated asset, got: {asset_html}"
    );
    assert!(
      asset_html.contains("#box {") && asset_html.contains("color: red"),
      "expected CSS snippet in generated asset, got: {asset_html}"
    );
    Ok(())
  }

  #[test]
  fn mdn_live_sample_iframe_generation_concatenates_multiple_css_blocks() -> Result<()> {
    let base = Url::parse("https://example.test/")?;
    let input = r#"<!doctype html>
<pre class="brush: html notranslate live-sample---demo"><code>&lt;div id=&quot;a&quot;&gt;Hi&lt;/div&gt;</code></pre>
<pre class="brush: css notranslate live-sample---demo"><code>#a { color: red; }</code></pre>
<pre class="brush: css hidden notranslate live-sample---demo"><code>#b { color: blue; }</code></pre>
<iframe class="sample-code-frame" src="about:blank" data-live-id="demo"></iframe>
"#;

    let mut catalog = AssetCatalog::new(false);
    let rewritten = rewrite_mdn_live_sample_iframes(
      input,
      &base,
      ReferenceContext::Html,
      &mut catalog,
      false,
      false,
    )?;

    assert_eq!(
      catalog.assets.len(),
      1,
      "expected exactly one generated iframe asset, got keys: {:?}",
      catalog.assets.keys().collect::<Vec<_>>()
    );
    let filename = catalog
      .assets
      .keys()
      .next()
      .expect("missing generated asset filename")
      .clone();
    assert!(
      rewritten.contains(&format!("src=\"assets/{filename}\"")),
      "expected iframe src to be rewritten to local asset, got: {rewritten}"
    );
    let asset = catalog
      .assets
      .get(&filename)
      .unwrap_or_else(|| panic!("missing generated asset {filename}"));
    let asset_html = String::from_utf8_lossy(&asset.bytes);
    assert!(
      asset_html.contains("<div id=\"a\">Hi</div>"),
      "expected decoded HTML snippet in generated asset, got: {asset_html}"
    );
    let pos_a = asset_html
      .find("#a")
      .unwrap_or_else(|| panic!("missing first CSS block in generated asset, got: {asset_html}"));
    let pos_b = asset_html
      .find("#b")
      .unwrap_or_else(|| panic!("missing second CSS block in generated asset, got: {asset_html}"));
    assert!(
      pos_a < pos_b,
      "expected CSS blocks to appear in document order, got: {asset_html}"
    );
    Ok(())
  }
} 
