use crate::compat::CompatProfile;
use crate::dom::DomCompatibilityMode;
use crate::error::{Error, ResourceError, Result};
use crate::fallible_vec_writer::FallibleVecWriter;
use crate::resource::{
  origin_from_url, DocumentOrigin, FetchContextKind, FetchCredentialsMode, FetchRequest,
  FetchedResource, HttpRequest, ReferrerPolicy, ResourceFetcher,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{self, Read, Write as _};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

/// File name of the bundle manifest inside directories and archives.
pub const BUNDLE_MANIFEST: &str = "bundle.json";

/// Schema version for bundle manifests.
pub const BUNDLE_VERSION: u32 = 1;

/// Synthetic manifest key used for request-partitioned resources.
///
/// This is primarily used for CORS-mode resources (`Font` / `ImageCors` / `StylesheetCors`) when
/// CORS cache partitioning is enabled (`FASTR_FETCH_PARTITION_CORS_CACHE=1`, default). Some servers
/// vary `Access-Control-Allow-Origin` by the initiating origin; bundles need to preserve the
/// per-origin metadata so offline renders can replay the same behavior.
///
/// The returned key is **not** a real URL. It is only used as a stable lookup key inside
/// `bundle.json`.
pub fn request_partitioned_resource_key(
  kind: FetchContextKind,
  url: &str,
  origin: &DocumentOrigin,
) -> String {
  request_partitioned_resource_key_with_credentials(kind, url, origin, FetchCredentialsMode::Omit)
}

/// Synthetic manifest key used for request-partitioned resources keyed by the cache partition key.
///
/// This encoding partitions by the CORS cache partition key (initiating origin + cookie inclusion)
/// but **does not** distinguish all [`FetchCredentialsMode`] variants.
///
/// New bundles should prefer [`request_partitioned_resource_key_v3`], which additionally encodes
/// `FetchCredentialsMode` so manifest keying matches the in-memory/disk cache partitioning used by
/// [`crate::resource::CachingFetcher`] / [`crate::resource::disk_cache::DiskCachingFetcher`].
pub fn request_partitioned_resource_key_v2(
  kind: FetchContextKind,
  url: &str,
  partition_key: &str,
) -> String {
  let kind_tag = match kind {
    FetchContextKind::Document => "document",
    FetchContextKind::Iframe => "iframe",
    FetchContextKind::Stylesheet => "stylesheet",
    FetchContextKind::StylesheetCors => "stylesheet-cors",
    FetchContextKind::Image => "image",
    FetchContextKind::ImageCors => "image-cors",
    FetchContextKind::Font => "font",
    FetchContextKind::Script => "script",
    FetchContextKind::ScriptCors => "script-cors",
    FetchContextKind::Other => "other",
  };
  format!("{url}@@fastr:bundle:req_v2@@kind={kind_tag}@@partition={partition_key}")
}

/// Synthetic manifest key used for request-partitioned resources keyed by the cache partition key
/// and request [`FetchCredentialsMode`].
///
/// This is the preferred encoding for new bundles because it matches the cache partitioning logic
/// used by [`crate::resource::CachingFetcher`] / [`crate::resource::disk_cache::DiskCachingFetcher`].
pub fn request_partitioned_resource_key_v3(
  kind: FetchContextKind,
  url: &str,
  partition_key: &str,
  credentials_mode: FetchCredentialsMode,
) -> String {
  let kind_tag = match kind {
    FetchContextKind::Document => "document",
    FetchContextKind::Iframe => "iframe",
    FetchContextKind::Stylesheet => "stylesheet",
    FetchContextKind::StylesheetCors => "stylesheet-cors",
    FetchContextKind::Image => "image",
    FetchContextKind::ImageCors => "image-cors",
    FetchContextKind::Font => "font",
    FetchContextKind::Script => "script",
    FetchContextKind::ScriptCors => "script-cors",
    FetchContextKind::Other => "other",
  };
  format!(
    "{url}@@fastr:bundle:req_v3@@kind={kind_tag}@@creds={}@@partition={partition_key}",
    credentials_mode.as_cache_tag()
  )
}

/// Compute the request-partitioned manifest key for a fetch request.
///
/// Returns `None` when the request is not in CORS mode or when CORS cache partitioning is
/// disabled.
pub fn request_partitioned_resource_key_for_request(req: &FetchRequest<'_>) -> Option<String> {
  let kind: FetchContextKind = req.destination.into();
  let partition_key = super::cors_cache_partition_key(req)?;
  Some(request_partitioned_resource_key_v3(
    kind,
    req.url,
    &partition_key,
    req.credentials_mode,
  ))
}

/// Like [`request_partitioned_resource_key`] but also partitions by request credentials mode.
///
/// For compatibility, anonymous (`omit`) requests produce the same key as the legacy helper. Other
/// credential modes append a `creds=...` tag.
pub fn request_partitioned_resource_key_with_credentials(
  kind: FetchContextKind,
  url: &str,
  origin: &DocumentOrigin,
  credentials_mode: FetchCredentialsMode,
) -> String {
  let kind_tag = match kind {
    FetchContextKind::Document => "document",
    FetchContextKind::Iframe => "iframe",
    FetchContextKind::Stylesheet => "stylesheet",
    FetchContextKind::StylesheetCors => "stylesheet-cors",
    FetchContextKind::Image => "image",
    FetchContextKind::ImageCors => "image-cors",
    FetchContextKind::Font => "font",
    FetchContextKind::Script => "script",
    FetchContextKind::ScriptCors => "script-cors",
    FetchContextKind::Other => "other",
  };
  let mut key = format!("{url}@@fastr:bundle:req_v1@@kind={kind_tag}@@origin={origin}");
  match credentials_mode {
    FetchCredentialsMode::Omit => {}
    FetchCredentialsMode::SameOrigin => key.push_str("@@creds=same-origin"),
    FetchCredentialsMode::Include => key.push_str("@@creds=include"),
  }
  key
}

/// Synthetic manifest key suffix used for `Vary`-partitioned resources.
///
/// Bundles can contain multiple variants for the same URL when the upstream server includes a
/// `Vary` header. Each variant is stored under a synthetic manifest key that appends
/// `@@fastr:bundle:vary_v1@@<vary_key>`.
///
/// The returned key is **not** a real URL. It is only used as a stable lookup key inside
/// `bundle.json`.
const BUNDLE_VARY_KEY_SENTINEL: &str = "@@fastr:bundle:vary_v1@@";

/// Create a synthetic manifest key for a specific `Vary` variant.
pub fn vary_partitioned_resource_key(url: &str, vary_key: &str) -> String {
  if vary_key.is_empty() {
    url.to_string()
  } else {
    format!("{url}{BUNDLE_VARY_KEY_SENTINEL}{vary_key}")
  }
}

fn parse_vary_partitioned_resource_key(key: &str) -> Option<(&str, &str)> {
  let (base, rest) = key.split_once(BUNDLE_VARY_KEY_SENTINEL)?;
  let vary_key = super::trim_http_whitespace(rest);
  if base.is_empty() || vary_key.is_empty() {
    return None;
  }
  Some((base, vary_key))
}

fn bool_is_false(value: &bool) -> bool {
  !*value
}

fn default_fetch_profile_user_agent() -> String {
  super::DEFAULT_USER_AGENT.to_string()
}

fn default_fetch_profile_accept_language() -> String {
  super::DEFAULT_ACCEPT_LANGUAGE.to_string()
}

/// Request header profile captured with the bundle.
///
/// Bundles can contain multiple variants for a single URL when the upstream server responds with a
/// `Vary` header (e.g. `Vary: User-Agent`). During offline replay we must compute the same
/// request-side `Vary` key that was used at capture-time; otherwise the bundled resource variants
/// may not be found deterministically.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleFetchProfile {
  #[serde(default = "default_fetch_profile_user_agent")]
  pub user_agent: String,
  #[serde(default = "default_fetch_profile_accept_language")]
  pub accept_language: String,
}

impl Default for BundleFetchProfile {
  fn default() -> Self {
    Self {
      user_agent: default_fetch_profile_user_agent(),
      accept_language: default_fetch_profile_accept_language(),
    }
  }
}

/// Render settings captured with the bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleRenderConfig {
  pub viewport: (u32, u32),
  pub device_pixel_ratio: f32,
  pub scroll_x: f32,
  pub scroll_y: f32,
  pub full_page: bool,
  /// When true, restrict subresource loads (CSS/images/fonts/etc.) to the document origin unless
  /// allowlisted.
  ///
  /// Note: this does not block cross-origin iframe/embed document navigation.
  #[serde(default)]
  pub same_origin_subresources: bool,
  /// Additional origins allowed when `same_origin_subresources` is enabled.
  #[serde(default)]
  pub allowed_subresource_origins: Vec<String>,
  #[serde(default)]
  pub compat_profile: CompatProfile,
  #[serde(default)]
  pub dom_compat_mode: DomCompatibilityMode,
}

/// Metadata describing the bundled document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundledDocument {
  pub path: String,
  pub content_type: Option<String>,
  #[serde(default, skip_serializing_if = "bool_is_false")]
  pub nosniff: bool,
  pub final_url: String,
  pub status: Option<u16>,
  pub etag: Option<String>,
  pub last_modified: Option<String>,
  /// Stored `Referrer-Policy` response header value, when present.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub response_referrer_policy: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub response_headers: Option<Vec<(String, String)>>,
  #[serde(default)]
  pub access_control_allow_origin: Option<String>,
  #[serde(default)]
  pub timing_allow_origin: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub vary: Option<String>,
}

/// Metadata describing a bundled resource.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundledResourceInfo {
  pub path: String,
  pub content_type: Option<String>,
  #[serde(default, skip_serializing_if = "bool_is_false")]
  pub nosniff: bool,
  pub status: Option<u16>,
  pub final_url: Option<String>,
  pub etag: Option<String>,
  pub last_modified: Option<String>,
  /// Stored `Referrer-Policy` response header value, when present.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub response_referrer_policy: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub response_headers: Option<Vec<(String, String)>>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub vary: Option<String>,
  #[serde(default)]
  pub access_control_allow_origin: Option<String>,
  #[serde(default)]
  pub timing_allow_origin: Option<String>,
  #[serde(default, skip_serializing_if = "bool_is_false")]
  pub access_control_allow_credentials: bool,
}

/// Manifest describing all resources contained in a bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleManifest {
  pub version: u32,
  pub original_url: String,
  pub document: BundledDocument,
  pub render: BundleRenderConfig,
  #[serde(default)]
  pub fetch_profile: BundleFetchProfile,
  pub resources: BTreeMap<String, BundledResourceInfo>,
}

#[derive(Clone)]
struct BundledResource {
  manifest_kind: Option<FetchContextKind>,
  info: BundledResourceInfo,
  bytes: Arc<Vec<u8>>,
}

impl BundledResource {
  fn from_parts(manifest_key: &str, info: BundledResourceInfo, bytes: Arc<Vec<u8>>) -> Self {
    let manifest_kind = parse_request_partitioned_resource_kind(manifest_key);
    Self {
      manifest_kind,
      info,
      bytes,
    }
  }

  fn as_fetched(&self) -> Result<FetchedResource> {
    self.as_fetched_prefix(usize::MAX)
  }

  fn as_fetched_prefix(&self, max_bytes: usize) -> Result<FetchedResource> {
    let prefix_len = max_bytes.min(self.bytes.len());
    let bytes = clone_bytes_fallible(&self.bytes[..prefix_len], "bundle resource bytes")?;
    let mut res = FetchedResource::with_final_url(
      bytes,
      self.info.content_type.clone(),
      self.info.final_url.clone(),
    );
    res.nosniff = self.info.nosniff;
    res.status = self.info.status;
    res.etag = self.info.etag.clone();
    res.last_modified = self.info.last_modified.clone();
    res.vary = self.info.vary.clone();
    res.access_control_allow_origin = self.info.access_control_allow_origin.clone();
    res.timing_allow_origin = self.info.timing_allow_origin.clone();
    res.response_referrer_policy = self
      .info
      .response_referrer_policy
      .as_deref()
      .and_then(ReferrerPolicy::parse_value_list);
    res.response_headers = self.info.response_headers.clone();
    res.access_control_allow_credentials = self.info.access_control_allow_credentials;
    Ok(res)
  }

  fn as_fetched_range(
    &self,
    url: &str,
    start: u64,
    end: u64,
    max_bytes: usize,
  ) -> Result<FetchedResource> {
    let bytes =
      clone_bytes_range_fallible(url, &self.bytes, start, end, max_bytes, "bundle resource bytes")?;
    let mut res = FetchedResource::with_final_url(
      bytes,
      self.info.content_type.clone(),
      self.info.final_url.clone(),
    );
    res.nosniff = self.info.nosniff;
    res.status = self.info.status;
    res.etag = self.info.etag.clone();
    res.last_modified = self.info.last_modified.clone();
    res.vary = self.info.vary.clone();
    res.access_control_allow_origin = self.info.access_control_allow_origin.clone();
    res.timing_allow_origin = self.info.timing_allow_origin.clone();
    res.response_referrer_policy = self
      .info
      .response_referrer_policy
      .as_deref()
      .and_then(ReferrerPolicy::parse_value_list);
    res.response_headers = self.info.response_headers.clone();
    res.access_control_allow_credentials = self.info.access_control_allow_credentials;
    Ok(res)
  }
}

fn bundle_key_is_request_partitioned(key: &str) -> bool {
  key.contains("@@fastr:bundle:req_v1@@")
    || key.contains("@@fastr:bundle:req_v2@@")
    || key.contains("@@fastr:bundle:req_v3@@")
}

fn parse_request_partitioned_resource_kind(key: &str) -> Option<FetchContextKind> {
  let rest = if let Some((_, rest)) = key.split_once("@@fastr:bundle:req_v3@@") {
    rest
  } else if let Some((_, rest)) = key.split_once("@@fastr:bundle:req_v2@@") {
    rest
  } else {
    key.split_once("@@fastr:bundle:req_v1@@")?.1
  };

  for part in rest.split("@@") {
    let Some(kind) = part.strip_prefix("kind=") else {
      continue;
    };
    return match kind {
      "document" => Some(FetchContextKind::Document),
      "iframe" => Some(FetchContextKind::Iframe),
      "stylesheet" => Some(FetchContextKind::Stylesheet),
      "stylesheet-cors" => Some(FetchContextKind::StylesheetCors),
      "image" => Some(FetchContextKind::Image),
      "image-cors" => Some(FetchContextKind::ImageCors),
      "font" => Some(FetchContextKind::Font),
      "other" => Some(FetchContextKind::Other),
      _ => None,
    };
  }
  None
}

fn vary_contains_header(vary: &str, header_name: &str) -> bool {
  let header_name = super::trim_http_whitespace(header_name);
  if header_name.is_empty() {
    return false;
  }
  for part in vary.split(',') {
    let part = super::trim_http_whitespace(part);
    if part.is_empty() {
      continue;
    }
    if part.eq_ignore_ascii_case(header_name) {
      return true;
    }
  }
  false
}

#[derive(Clone)]
struct BundledVaryBucket {
  canonical_url: String,
  vary: Option<String>,
  variants: HashMap<String, BundledResource>,
}

impl BundledVaryBucket {
  fn new(canonical_url: String, vary: Option<String>) -> Self {
    Self {
      canonical_url,
      vary,
      variants: HashMap::new(),
    }
  }
}

/// In-memory representation of a bundle.
pub struct Bundle {
  manifest: BundleManifest,
  document_bytes: Arc<Vec<u8>>,
  resources: HashMap<String, BundledResource>,
  vary_resources: HashMap<String, Arc<BundledVaryBucket>>,
}

fn read_all_with_limit<R: Read>(
  reader: &mut R,
  max_bytes: usize,
  context: &'static str,
) -> io::Result<Vec<u8>> {
  let mut bytes = FallibleVecWriter::new(max_bytes, context);
  let mut buf = [0u8; 8 * 1024];
  loop {
    let n = match reader.read(&mut buf) {
      Ok(0) => break,
      Ok(n) => n,
      Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
      Err(err) => return Err(err),
    };
    bytes.write_all(&buf[..n])?;
  }
  Ok(bytes.into_inner())
}

fn read_file_fallible(path: &Path) -> io::Result<Vec<u8>> {
  let mut file = fs::File::open(path)?;
  let max_bytes = match file.metadata() {
    Ok(meta) => usize::try_from(meta.len()).map_err(|_| {
      io::Error::new(
        io::ErrorKind::Other,
        format!(
          "bundle file is too large to read on this platform: {} ({} bytes)",
          path.display(),
          meta.len()
        ),
      )
    })?,
    Err(_) => usize::MAX,
  };
  read_all_with_limit(&mut file, max_bytes, "bundle file")
}

fn clone_bytes_fallible(bytes: &[u8], context: &'static str) -> Result<Vec<u8>> {
  let mut writer = FallibleVecWriter::new(bytes.len(), context);
  writer.write_all(bytes).map_err(Error::Io)?;
  Ok(writer.into_inner())
}

fn clone_bytes_range_fallible(
  url: &str,
  bytes: &[u8],
  start: u64,
  mut end: u64,
  max_bytes: usize,
  context: &'static str,
) -> Result<Vec<u8>> {
  if max_bytes == 0 {
    return Ok(Vec::new());
  }

  let cap_end = start.saturating_add((max_bytes as u64).saturating_sub(1));
  end = end.min(cap_end);

  let start_idx = usize::try_from(start).map_err(|_| {
    Error::Resource(ResourceError::new(
      url,
      format!("byte range start {start} is too large to slice in memory"),
    ))
  })?;
  if start_idx >= bytes.len() {
    return Err(Error::Resource(ResourceError::new(
      url,
      format!(
        "byte range start {start} is beyond end of response body (len={})",
        bytes.len()
      ),
    )));
  }

  let end_idx = usize::try_from(end).map_err(|_| {
    Error::Resource(ResourceError::new(
      url,
      format!("byte range end {end} is too large to slice in memory"),
    ))
  })?;
  let available_end = bytes.len().saturating_sub(1);
  let end_idx = end_idx.min(available_end);

  let bytes = clone_bytes_fallible(&bytes[start_idx..=end_idx], context)?;
  Ok(bytes)
}

impl Bundle {
  /// Load a bundle from a directory or `.tar` archive path.
  pub fn load(path: impl AsRef<Path>) -> Result<Self> {
    let path = path.as_ref();
    if path.is_dir() {
      Self::load_directory(path)
    } else {
      Self::load_archive(path)
    }
  }

  /// Returns the parsed manifest.
  pub fn manifest(&self) -> &BundleManifest {
    &self.manifest
  }

  /// Returns the bundled document metadata and bytes.
  pub fn document(&self) -> (&BundledDocument, Arc<Vec<u8>>) {
    (&self.manifest.document, Arc::clone(&self.document_bytes))
  }

  /// Fetch a resource by its manifest key.
  ///
  /// Bundles may store synthetic manifest keys (e.g. request-partitioned resources or
  /// `Vary`-partitioned variants) that are not real URLs. This helper accepts those keys and
  /// returns the exact bytes stored in the bundle.
  ///
  /// Unlike [`BundledFetcher`], this does **not** enforce replay-safety checks for unhandled
  /// `Vary` headers. It is intended for offline tooling (such as fixture importers) that needs to
  /// read captured bytes verbatim.
  pub fn fetch_manifest_entry(&self, key: &str) -> Result<FetchedResource> {
    let doc_final_url = if self.manifest.document.final_url.is_empty() {
      self.manifest.original_url.clone()
    } else {
      self.manifest.document.final_url.clone()
    };
    if key == self.manifest.original_url || key == doc_final_url {
      let (doc_meta, bytes) = self.document();
      let bytes = clone_bytes_fallible(&bytes, "bundle document bytes")?;
      let mut res =
        FetchedResource::with_final_url(bytes, doc_meta.content_type.clone(), Some(doc_final_url));
      res.nosniff = doc_meta.nosniff;
      res.status = doc_meta.status;
      res.etag = doc_meta.etag.clone();
      res.last_modified = doc_meta.last_modified.clone();
      res.vary = doc_meta.vary.clone();
      res.access_control_allow_origin = doc_meta.access_control_allow_origin.clone();
      res.timing_allow_origin = doc_meta.timing_allow_origin.clone();
      res.response_referrer_policy = doc_meta
        .response_referrer_policy
        .as_deref()
        .and_then(ReferrerPolicy::parse_value_list);
      res.response_headers = doc_meta.response_headers.clone();
      return Ok(res);
    }

    if let Some((base_url, vary_key)) = parse_vary_partitioned_resource_key(key) {
      if let Some(bucket) = self.vary_bucket_for_url(base_url) {
        if let Some(resource) = bucket.variants.get(vary_key) {
          return resource.as_fetched();
        }
        return Err(Error::Other(format!(
          "Resource not found in bundle (no matching Vary variant): {}",
          key
        )));
      }
    }

    if let Some(resource) = self.resource_for_url(key) {
      return resource.as_fetched();
    }

    // Bundles are meant to be replayable without network access, but data: URLs encode their
    // payload in the URL itself. Decode them directly so bundles don't need to persist huge
    // `data:` strings in the manifest for correctness.
    if key
      .get(..5)
      .map(|prefix| prefix.eq_ignore_ascii_case("data:"))
      .unwrap_or(false)
    {
      return super::data_url::decode_data_url(key);
    }

    Err(Error::Other(format!(
      "Resource not found in bundle: {}",
      key
    )))
  }

  fn load_directory(dir: &Path) -> Result<Self> {
    let manifest_path = dir.join(BUNDLE_MANIFEST);
    let manifest_bytes = read_file_fallible(&manifest_path).map_err(|e| {
      Error::Io(io::Error::new(
        e.kind(),
        format!(
          "Failed to read manifest {path:?}: {e}",
          path = manifest_path
        ),
      ))
    })?;
    let manifest: BundleManifest = serde_json::from_slice(&manifest_bytes)
      .map_err(|e| Error::Other(format!("Invalid bundle manifest: {e}")))?;
    Self::build_from_manifest(dir, manifest, None)
  }

  fn load_archive(path: &Path) -> Result<Self> {
    let file = fs::File::open(path).map_err(|e| {
      Error::Io(std::io::Error::new(
        e.kind(),
        format!("Failed to open bundle archive {path:?}: {e}"),
      ))
    })?;
    let mut archive = tar::Archive::new(file);
    let mut files: HashMap<String, Vec<u8>> = HashMap::new();
    for entry in archive.entries().map_err(Error::Io)? {
      let mut entry = entry.map_err(Error::Io)?;
      if !entry.header().entry_type().is_file() {
        continue;
      }
      let path = entry
        .path()
        .map_err(Error::Io)?
        .to_string_lossy()
        .trim_start_matches("./")
        .to_string();
      let size = entry.header().size().map_err(Error::Io)?;
      let max_bytes = usize::try_from(size).map_err(|_| {
        Error::Other(format!(
          "Bundle entry is too large to load on this platform: {path} ({size} bytes)"
        ))
      })?;
      let data =
        read_all_with_limit(&mut entry, max_bytes, "bundle archive entry").map_err(|err| {
          Error::Io(io::Error::new(
            err.kind(),
            format!("Failed to read {path}: {err}"),
          ))
        })?;
      files.insert(path, data);
    }

    let manifest_bytes = files.get(BUNDLE_MANIFEST).ok_or_else(|| {
      Error::Other(format!(
        "Bundle archive {} missing manifest",
        path.display()
      ))
    })?;
    let manifest: BundleManifest = serde_json::from_slice(manifest_bytes)
      .map_err(|e| Error::Other(format!("Invalid bundle manifest: {e}")))?;
    Self::build_from_manifest(path, manifest, Some(files))
  }

  fn build_from_manifest(
    base_path: &Path,
    manifest: BundleManifest,
    mut archive_files: Option<HashMap<String, Vec<u8>>>,
  ) -> Result<Self> {
    if manifest.version != BUNDLE_VERSION {
      return Err(Error::Other(format!(
        "Unsupported bundle version {} (expected {})",
        manifest.version, BUNDLE_VERSION
      )));
    }

    let mut file_cache: HashMap<String, Arc<Vec<u8>>> = HashMap::new();
    let mut fetch_file = |relative: &str| -> Result<Arc<Vec<u8>>> {
      let relative_path = validate_relative_path(relative)?;
      let relative_str = relative_path.to_string_lossy();
      let relative_str = relative_str.trim_start_matches("./").to_string();

      if let Some(cached) = file_cache.get(&relative_str) {
        return Ok(Arc::clone(cached));
      }

      let data = if let Some(files) = archive_files.as_mut() {
        files.remove(&relative_str).ok_or_else(|| {
          Error::Other(format!(
            "Bundle missing resource file referenced in manifest: {}",
            relative
          ))
        })?
      } else {
        let target = base_path.join(&relative_path);
        if target.is_dir() {
          return Err(Error::Other(format!(
            "Bundle entry {} resolves to directory",
            relative
          )));
        }
        read_file_fallible(&target).map_err(Error::Io)?
      };

      let data = Arc::new(data);
      file_cache.insert(relative_str, Arc::clone(&data));
      Ok(data)
    };

    let document_bytes = fetch_file(&manifest.document.path)?;

    let mut resources: HashMap<String, BundledResource> = HashMap::new();
    let mut vary_canonical: HashMap<String, BundledVaryBucket> = HashMap::new();
    let mut vary_aliases: Vec<(String, String)> = Vec::new();
    for (original_url, info) in &manifest.resources {
      let data = fetch_file(&info.path)?;
      let resource = BundledResource::from_parts(original_url, info.clone(), data);

      if let Some((base_url, vary_key)) = parse_vary_partitioned_resource_key(original_url) {
        let canonical = info.final_url.as_deref().unwrap_or(base_url).to_string();
        let bucket = vary_canonical
          .entry(canonical.clone())
          .or_insert_with(|| BundledVaryBucket::new(canonical.clone(), info.vary.clone()));
        if bucket.vary.is_none() {
          bucket.vary = info.vary.clone();
        }
        bucket
          .variants
          .insert(vary_key.to_string(), resource.clone());
        vary_aliases.push((base_url.to_string(), canonical));
        continue;
      }

      resources.insert(original_url.clone(), resource.clone());
      if let Some(final_url) = &info.final_url {
        resources
          .entry(final_url.clone())
          .or_insert_with(|| resource.clone());
      }
    }

    let mut vary_resources: HashMap<String, Arc<BundledVaryBucket>> = HashMap::new();
    let mut canonical_arcs: HashMap<String, Arc<BundledVaryBucket>> = HashMap::new();
    for (canonical, bucket) in vary_canonical {
      let bucket = Arc::new(bucket);
      canonical_arcs.insert(canonical.clone(), Arc::clone(&bucket));
      vary_resources.insert(canonical, bucket);
    }
    for (alias, canonical) in vary_aliases {
      if let Some(bucket) = canonical_arcs.get(&canonical) {
        vary_resources.insert(alias, Arc::clone(bucket));
      }
    }

    Ok(Self {
      manifest,
      document_bytes,
      resources,
      vary_resources,
    })
  }

  fn resource_for_url(&self, url: &str) -> Option<&BundledResource> {
    if let Some(resource) = self.resources.get(url) {
      return Some(resource);
    }

    // `bundle_page` can record multiple `Vary` variants for a URL. These are stored in the
    // manifest under a synthetic key of the form:
    //
    //   <url>@@fastr:bundle:vary_v1@@<vary_key>
    //
    // That key is not a real URL and is not used during normal bundle replay. However, tooling
    // (e.g. `xtask import-page-fixture`) may need to enumerate `manifest.resources` and retrieve the
    // raw bytes for each manifest key. Support direct lookup of these synthetic keys by mapping
    // them back to the appropriate `Vary` bucket + variant.
    if let Some((base_url, vary_key)) = parse_vary_partitioned_resource_key(url) {
      if let Some(bucket) = self.vary_bucket_for_url(base_url) {
        return bucket.variants.get(vary_key);
      }
    }

    None
  }

  fn vary_bucket_for_url(&self, url: &str) -> Option<&Arc<BundledVaryBucket>> {
    self.vary_resources.get(url)
  }
}

fn validate_relative_path(path: &str) -> Result<PathBuf> {
  let candidate = Path::new(path);
  if candidate.is_absolute()
    || candidate
      .components()
      .any(|c| matches!(c, Component::ParentDir | Component::RootDir))
  {
    return Err(Error::Other(format!(
      "Bundle entry path must be relative: {}",
      path
    )));
  }
  Ok(candidate.to_path_buf())
}

/// [`ResourceFetcher`] implementation that serves resources from a bundle without network access.
#[derive(Clone)]
pub struct BundledFetcher {
  bundle: Arc<Bundle>,
}

impl BundledFetcher {
  /// Construct a bundled fetcher from a loaded [`Bundle`].
  pub fn new(bundle: Bundle) -> Self {
    Self {
      bundle: Arc::new(bundle),
    }
  }
}

impl ResourceFetcher for BundledFetcher {
  fn fetch(&self, url: &str) -> Result<FetchedResource> {
    let lookup_is_request_partitioned = bundle_key_is_request_partitioned(url);
    let doc_matches =
      url == self.bundle.manifest.original_url || url == self.bundle.manifest.document.final_url;

    if doc_matches {
      let (doc_meta, bytes) = self.bundle.document();
      let bytes = clone_bytes_fallible(&bytes, "bundle document bytes")?;
      let mut res = FetchedResource::with_final_url(
        bytes,
        doc_meta.content_type.clone(),
        Some(doc_meta.final_url.clone()),
      );
      res.nosniff = doc_meta.nosniff;
      res.status = doc_meta.status;
      res.etag = doc_meta.etag.clone();
      res.last_modified = doc_meta.last_modified.clone();
      res.vary = doc_meta.vary.clone();
      res.access_control_allow_origin = doc_meta.access_control_allow_origin.clone();
      res.timing_allow_origin = doc_meta.timing_allow_origin.clone();
      res.response_referrer_policy = doc_meta
        .response_referrer_policy
        .as_deref()
        .and_then(ReferrerPolicy::parse_value_list);
      res.response_headers = doc_meta.response_headers.clone();
      if let Some(vary) = res.vary.as_deref() {
        if super::vary_contains_star(vary)
          || (!super::allow_unhandled_vary_env()
            && !super::vary_is_cacheable(vary, FetchContextKind::Document, None))
        {
          return Err(Error::Other(format!(
            "Bundle document has unhandled Vary and cannot be replayed safely: {}",
            doc_meta.final_url
          )));
        }
      }
      return Ok(res);
    }

    // Bundles store Vary-partitioned variants under synthetic manifest keys:
    //   "<url>@@fastr:bundle:vary_v1@@<vary_key>"
    //
    // Most callers should fetch the canonical URL (without the suffix) and let BundledFetcher
    // select the correct variant based on request headers. But tooling (e.g. fixture import) may
    // iterate `bundle.json` keys directly and expects `fetch()` to succeed for every manifest key.
    if let Some((base_url, vary_key)) = parse_vary_partitioned_resource_key(url) {
      let Some(bucket) = self.bundle.vary_bucket_for_url(base_url) else {
        return Err(Error::Other(format!(
          "Resource not found in bundle (missing Vary bucket): {}",
          url
        )));
      };
      if bucket.vary.as_deref() == Some("*") {
        return Err(Error::Other(format!(
          "Resource is not cacheable in bundle (Vary: *): {}",
          url
        )));
      }
      if let Some(resource) = bucket.variants.get(vary_key) {
        return resource.as_fetched();
      }
      // Back-compat: older bundles may include a single Vary entry without the `vary` field.
      if bucket.vary.is_none() && bucket.variants.len() == 1 {
        if let Some(resource) = bucket.variants.values().next() {
          return resource.as_fetched();
        }
      }
      return Err(Error::Other(format!(
        "Resource not found in bundle (unknown Vary variant): {}",
        url
      )));
    }

    if let Some(bucket) = self.bundle.vary_bucket_for_url(url) {
      if bucket.vary.as_deref() == Some("*") {
        return Err(Error::Other(format!(
          "Resource is not cacheable in bundle (Vary: *): {}",
          url
        )));
      }
      let destination = parse_request_partitioned_resource_kind(url)
        .or_else(|| {
          bucket
            .variants
            .values()
            .find_map(|resource| resource.manifest_kind)
        })
        .map(super::FetchDestination::from)
        .unwrap_or_else(|| super::http_browser_request_profile_for_url(url));
      if !lookup_is_request_partitioned
        && destination.sec_fetch_mode() == "cors"
        && bucket
          .vary
          .as_deref()
          .is_some_and(|vary| vary_contains_header(vary, "origin"))
      {
        return Err(Error::Other(format!(
          "Bundle entry has unhandled Vary and cannot be replayed safely: {url}"
        )));
      }
      let request = FetchRequest::new(bucket.canonical_url.as_str(), destination);
      let Some(vary_key) =
        super::compute_vary_key_for_request(self, request, bucket.vary.as_deref())
      else {
        return Err(Error::Other(format!(
          "Resource not cacheable in bundle (unknown Vary headers): {}",
          url
        )));
      };
      if let Some(resource) = bucket.variants.get(&vary_key) {
        return resource.as_fetched();
      }
      if let Some(legacy_key) =
        super::compute_vary_key_for_request_legacy(self, request, bucket.vary.as_deref())
      {
        if legacy_key != vary_key {
          if let Some(resource) = bucket.variants.get(&legacy_key) {
            return resource.as_fetched();
          }
        }
      }
      // Back-compat: older bundles may include a single Vary entry without the `vary` field.
      if bucket.vary.is_none() && bucket.variants.len() == 1 {
        if let Some(resource) = bucket.variants.values().next() {
          return resource.as_fetched();
        }
      }

      // `fetch(url)` does not carry enough context to reproduce request headers like `Origin`
      // (the most common cause of `Vary: Origin`). When this happens, we still want best-effort
      // access to the captured bytes (e.g. for `xtask import-page-fixture`, which iterates all
      // manifest entries by key). If the bundle manifest includes a direct entry for this URL,
      // return it deterministically instead of failing.
      if let Some(resource) = self.bundle.resource_for_url(url) {
        return resource.as_fetched();
      }
      return Err(Error::Other(format!(
        "Resource not found in bundle (no matching Vary variant): {}",
        url
      )));
    }

    if let Some(resource) = self.bundle.resource_for_url(url) {
      let res = resource.as_fetched()?;
      let destination = resource
        .manifest_kind
        .map(super::FetchDestination::from)
        .unwrap_or_else(|| super::http_browser_request_profile_for_url(url));
      if !lookup_is_request_partitioned
        && destination.sec_fetch_mode() == "cors"
        && res
          .vary
          .as_deref()
          .is_some_and(|vary| vary_contains_header(vary, "origin"))
      {
        return Err(Error::Other(format!(
          "Bundle entry has unhandled Vary and cannot be replayed safely: {url}"
        )));
      }
      let kind: FetchContextKind = destination.into();
      let origin_key = lookup_is_request_partitioned.then_some("bundled");
      if let Some(vary) = res.vary.as_deref() {
        if super::vary_contains_star(vary)
          || (!super::allow_unhandled_vary_env()
            && !super::vary_is_cacheable(vary, kind, origin_key))
        {
          return Err(Error::Other(format!(
            "Bundle entry has unhandled Vary and cannot be replayed safely: {url}"
          )));
        }
      }
      return Ok(res);
    }

    // Bundles are meant to be replayable without network access, but data: URLs encode their
    // payload in the URL itself. Decode them directly so bundles don't need to persist huge
    // `data:` strings in the manifest for correctness.
    if url
      .get(..5)
      .map(|prefix| prefix.eq_ignore_ascii_case("data:"))
      .unwrap_or(false)
    {
      return super::data_url::decode_data_url(url);
    }

    Err(Error::Other(format!(
      "Resource not found in bundle: {}",
      url
    )))
  }

  fn fetch_http_request(&self, req: HttpRequest<'_>) -> Result<FetchedResource> {
    let method_is_get = req.method.eq_ignore_ascii_case("GET");
    let method_is_head = req.method.eq_ignore_ascii_case("HEAD");
    if !method_is_get && !method_is_head {
      return Err(Error::Other(format!(
        "Bundle cannot replay non-GET/HEAD request ({} {}): bundles only store GET responses",
        req.method, req.fetch.url
      )));
    }
    if !req.headers.is_empty() || req.body.is_some() {
      return Err(Error::Other(format!(
        "Bundle cannot replay request with custom headers/body ({} {}): bundles only store browser-profiled GET responses",
        req.method, req.fetch.url
      )));
    }

    let mut res = self.fetch_with_request(req.fetch)?;
    if method_is_head {
      res.bytes.clear();
    }
    Ok(res)
  }

  fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
    let kind: FetchContextKind = req.destination.into();
    let cors_partition_key = super::cors_cache_partition_key(&req);
    if let Some(partition_key) = cors_partition_key.as_deref() {
      let validate_vary = |res: &FetchedResource| {
        if let Some(vary) = res.vary.as_deref() {
          if super::vary_contains_star(vary)
            || (!super::allow_unhandled_vary_env()
              && !super::vary_is_cacheable(vary, kind, Some("bundled")))
          {
            return Err(Error::Other(format!(
              "Bundle entry has unhandled Vary and cannot be replayed safely: {}",
              req.url
            )));
          }
        }
        Ok(())
      };

      // Preferred encoding: matches cache partitioning (origin key + credentials mode).
      let key =
        request_partitioned_resource_key_v3(kind, req.url, partition_key, req.credentials_mode);
      if let Some(resource) = self.bundle.resource_for_url(&key) {
        let res = resource.as_fetched()?;
        validate_vary(&res)?;
        return Ok(res);
      }

      // Back-compat: v2 bundles partition by the origin key but not `FetchCredentialsMode`.
      let key = request_partitioned_resource_key_v2(kind, req.url, partition_key);
      if let Some(resource) = self.bundle.resource_for_url(&key) {
        let res = resource.as_fetched()?;
        validate_vary(&res)?;
        return Ok(res);
      }

      // Backward compatibility: older bundles used the v1 origin-based key.
      let origin_from_referrer = req.referrer_url.and_then(origin_from_url);
      let origin_from_target = origin_from_url(req.url);
      let origin = req
        .client_origin
        .or(origin_from_referrer.as_ref())
        .or(origin_from_target.as_ref());
      if let Some(origin) = origin {
        let key = request_partitioned_resource_key_with_credentials(
          kind,
          req.url,
          origin,
          req.credentials_mode,
        );
        if let Some(resource) = self.bundle.resource_for_url(&key) {
          let res = resource.as_fetched()?;
          validate_vary(&res)?;
          return Ok(res);
        }

        if req.credentials_mode != FetchCredentialsMode::Omit {
          let key = request_partitioned_resource_key(kind, req.url, origin);
          if let Some(resource) = self.bundle.resource_for_url(&key) {
            let res = resource.as_fetched()?;
            validate_vary(&res)?;
            return Ok(res);
          }
        }
      }
    }

    if let Some(bucket) = self.bundle.vary_bucket_for_url(req.url) {
      if bucket.vary.as_deref() == Some("*") {
        return Err(Error::Other(format!(
          "Resource is not cacheable in bundle (Vary: *): {}",
          req.url
        )));
      }
      let canonical = FetchRequest {
        url: bucket.canonical_url.as_str(),
        destination: req.destination,
        referrer_url: req.referrer_url,
        client_origin: req.client_origin,
        referrer_policy: req.referrer_policy,
        credentials_mode: req.credentials_mode,
      };
      let Some(vary_key) =
        super::compute_vary_key_for_request(self, canonical, bucket.vary.as_deref())
      else {
        return Err(Error::Other(format!(
          "Resource not cacheable in bundle (unknown Vary headers): {}",
          req.url
        )));
      };
      if let Some(resource) = bucket.variants.get(&vary_key) {
        return resource.as_fetched();
      }
      if let Some(legacy_key) =
        super::compute_vary_key_for_request_legacy(self, canonical, bucket.vary.as_deref())
      {
        if legacy_key != vary_key {
          if let Some(resource) = bucket.variants.get(&legacy_key) {
            return resource.as_fetched();
          }
        }
      }
      if bucket.vary.is_none() && bucket.variants.len() == 1 {
        if let Some(resource) = bucket.variants.values().next() {
          return resource.as_fetched();
        }
      }
      return Err(Error::Other(format!(
        "Resource not found in bundle (no matching Vary variant): {}",
        req.url
      )));
    }

    if cors_partition_key.is_some() {
      if let Some(resource) = self.bundle.resource_for_url(req.url) {
        if resource
          .info
          .vary
          .as_deref()
          .is_some_and(|vary| vary_contains_header(vary, "origin"))
        {
          return Err(Error::Other(format!(
            "Bundle entry has unhandled Vary and cannot be replayed safely: {}",
            req.url
          )));
        }
      }
    }

    self.fetch(req.url)
  }

  fn fetch_partial(&self, url: &str, max_bytes: usize) -> Result<FetchedResource> {
    let lookup_is_request_partitioned = bundle_key_is_request_partitioned(url);
    let doc_matches =
      url == self.bundle.manifest.original_url || url == self.bundle.manifest.document.final_url;

    if doc_matches {
      let (doc_meta, bytes) = self.bundle.document();
      let prefix_len = max_bytes.min(bytes.len());
      let bytes = clone_bytes_fallible(&bytes[..prefix_len], "bundle document bytes")?;
      let mut res = FetchedResource::with_final_url(
        bytes,
        doc_meta.content_type.clone(),
        Some(doc_meta.final_url.clone()),
      );
      res.nosniff = doc_meta.nosniff;
      res.status = doc_meta.status;
      res.etag = doc_meta.etag.clone();
      res.last_modified = doc_meta.last_modified.clone();
      res.vary = doc_meta.vary.clone();
      res.access_control_allow_origin = doc_meta.access_control_allow_origin.clone();
      res.timing_allow_origin = doc_meta.timing_allow_origin.clone();
      res.response_referrer_policy = doc_meta
        .response_referrer_policy
        .as_deref()
        .and_then(ReferrerPolicy::parse_value_list);
      res.response_headers = doc_meta.response_headers.clone();
      if let Some(vary) = res.vary.as_deref() {
        if super::vary_contains_star(vary)
          || (!super::allow_unhandled_vary_env()
            && !super::vary_is_cacheable(vary, FetchContextKind::Document, None))
        {
          return Err(Error::Other(format!(
            "Bundle document has unhandled Vary and cannot be replayed safely: {}",
            doc_meta.final_url
          )));
        }
      }
      return Ok(res);
    }

    if let Some(bucket) = self.bundle.vary_bucket_for_url(url) {
      if bucket.vary.as_deref() == Some("*") {
        return Err(Error::Other(format!(
          "Resource is not cacheable in bundle (Vary: *): {}",
          url
        )));
      }
      let destination = parse_request_partitioned_resource_kind(url)
        .or_else(|| {
          bucket
            .variants
            .values()
            .find_map(|resource| resource.manifest_kind)
        })
        .map(super::FetchDestination::from)
        .unwrap_or_else(|| super::http_browser_request_profile_for_url(url));
      if !lookup_is_request_partitioned
        && destination.sec_fetch_mode() == "cors"
        && bucket
          .vary
          .as_deref()
          .is_some_and(|vary| vary_contains_header(vary, "origin"))
      {
        return Err(Error::Other(format!(
          "Bundle entry has unhandled Vary and cannot be replayed safely: {url}"
        )));
      }
      let request = FetchRequest::new(bucket.canonical_url.as_str(), destination);
      let Some(vary_key) =
        super::compute_vary_key_for_request(self, request, bucket.vary.as_deref())
      else {
        return Err(Error::Other(format!(
          "Resource not cacheable in bundle (unknown Vary headers): {}",
          url
        )));
      };
      if let Some(resource) = bucket.variants.get(&vary_key) {
        return resource.as_fetched_prefix(max_bytes);
      }
      if let Some(legacy_key) =
        super::compute_vary_key_for_request_legacy(self, request, bucket.vary.as_deref())
      {
        if legacy_key != vary_key {
          if let Some(resource) = bucket.variants.get(&legacy_key) {
            return resource.as_fetched_prefix(max_bytes);
          }
        }
      }
      // Back-compat: older bundles may include a single Vary entry without the `vary` field.
      if bucket.vary.is_none() && bucket.variants.len() == 1 {
        if let Some(resource) = bucket.variants.values().next() {
          return resource.as_fetched_prefix(max_bytes);
        }
      }
      return Err(Error::Other(format!(
        "Resource not found in bundle (no matching Vary variant): {}",
        url
      )));
    }

    if let Some(resource) = self.bundle.resource_for_url(url) {
      let res = resource.as_fetched_prefix(max_bytes)?;
      let destination = resource
        .manifest_kind
        .map(super::FetchDestination::from)
        .unwrap_or_else(|| super::http_browser_request_profile_for_url(url));
      if !lookup_is_request_partitioned
        && destination.sec_fetch_mode() == "cors"
        && res
          .vary
          .as_deref()
          .is_some_and(|vary| vary_contains_header(vary, "origin"))
      {
        return Err(Error::Other(format!(
          "Bundle entry has unhandled Vary and cannot be replayed safely: {url}"
        )));
      }
      let kind: FetchContextKind = destination.into();
      let origin_key = lookup_is_request_partitioned.then_some("bundled");
      if let Some(vary) = res.vary.as_deref() {
        if super::vary_contains_star(vary)
          || (!super::allow_unhandled_vary_env()
            && !super::vary_is_cacheable(vary, kind, origin_key))
        {
          return Err(Error::Other(format!(
            "Bundle entry has unhandled Vary and cannot be replayed safely: {url}"
          )));
        }
      }
      return Ok(res);
    }

    // Bundles are meant to be replayable without network access, but data: URLs encode their
    // payload in the URL itself. Decode them directly so bundles don't need to persist huge
    // `data:` strings in the manifest for correctness.
    if url
      .get(..5)
      .map(|prefix| prefix.eq_ignore_ascii_case("data:"))
      .unwrap_or(false)
    {
      return super::data_url::decode_data_url_prefix(url, max_bytes);
    }

    Err(Error::Other(format!(
      "Resource not found in bundle: {}",
      url
    )))
  }

  fn fetch_partial_with_context(
    &self,
    kind: FetchContextKind,
    url: &str,
    max_bytes: usize,
  ) -> Result<FetchedResource> {
    self.fetch_partial_with_request(FetchRequest::new(url, kind.into()), max_bytes)
  }

  fn fetch_partial_with_request(
    &self,
    req: FetchRequest<'_>,
    max_bytes: usize,
  ) -> Result<FetchedResource> {
    let kind: FetchContextKind = req.destination.into();
    let cors_partition_key = super::cors_cache_partition_key(&req);
    if let Some(partition_key) = cors_partition_key.as_deref() {
      let validate_vary = |vary: Option<&str>| {
        if let Some(vary) = vary {
          if super::vary_contains_star(vary)
            || (!super::allow_unhandled_vary_env()
              && !super::vary_is_cacheable(vary, kind, Some("bundled")))
          {
            return Err(Error::Other(format!(
              "Bundle entry has unhandled Vary and cannot be replayed safely: {}",
              req.url
            )));
          }
        }
        Ok(())
      };

      // Preferred encoding: matches cache partitioning (origin key + credentials mode).
      let key =
        request_partitioned_resource_key_v3(kind, req.url, partition_key, req.credentials_mode);
      if let Some(resource) = self.bundle.resource_for_url(&key) {
        validate_vary(resource.info.vary.as_deref())?;
        return resource.as_fetched_prefix(max_bytes);
      }

      // Back-compat: v2 bundles partition by the origin key but not `FetchCredentialsMode`.
      let key = request_partitioned_resource_key_v2(kind, req.url, partition_key);
      if let Some(resource) = self.bundle.resource_for_url(&key) {
        validate_vary(resource.info.vary.as_deref())?;
        return resource.as_fetched_prefix(max_bytes);
      }

      // Backward compatibility: older bundles used the v1 origin-based key.
      let origin_from_referrer = req.referrer_url.and_then(origin_from_url);
      let origin_from_target = origin_from_url(req.url);
      let origin = req
        .client_origin
        .or(origin_from_referrer.as_ref())
        .or(origin_from_target.as_ref());
      if let Some(origin) = origin {
        let key = request_partitioned_resource_key_with_credentials(
          kind,
          req.url,
          origin,
          req.credentials_mode,
        );
        if let Some(resource) = self.bundle.resource_for_url(&key) {
          validate_vary(resource.info.vary.as_deref())?;
          return resource.as_fetched_prefix(max_bytes);
        }

        if req.credentials_mode != FetchCredentialsMode::Omit {
          let key = request_partitioned_resource_key(kind, req.url, origin);
          if let Some(resource) = self.bundle.resource_for_url(&key) {
            validate_vary(resource.info.vary.as_deref())?;
            return resource.as_fetched_prefix(max_bytes);
          }
        }
      }
    }

    if let Some(bucket) = self.bundle.vary_bucket_for_url(req.url) {
      if bucket.vary.as_deref() == Some("*") {
        return Err(Error::Other(format!(
          "Resource is not cacheable in bundle (Vary: *): {}",
          req.url
        )));
      }
      let canonical = FetchRequest {
        url: bucket.canonical_url.as_str(),
        destination: req.destination,
        referrer_url: req.referrer_url,
        client_origin: req.client_origin,
        referrer_policy: req.referrer_policy,
        credentials_mode: req.credentials_mode,
      };
      let Some(vary_key) =
        super::compute_vary_key_for_request(self, canonical, bucket.vary.as_deref())
      else {
        return Err(Error::Other(format!(
          "Resource not cacheable in bundle (unknown Vary headers): {}",
          req.url
        )));
      };
      if let Some(resource) = bucket.variants.get(&vary_key) {
        return resource.as_fetched_prefix(max_bytes);
      }
      if let Some(legacy_key) =
        super::compute_vary_key_for_request_legacy(self, canonical, bucket.vary.as_deref())
      {
        if legacy_key != vary_key {
          if let Some(resource) = bucket.variants.get(&legacy_key) {
            return resource.as_fetched_prefix(max_bytes);
          }
        }
      }
      if bucket.vary.is_none() && bucket.variants.len() == 1 {
        if let Some(resource) = bucket.variants.values().next() {
          return resource.as_fetched_prefix(max_bytes);
        }
      }
      return Err(Error::Other(format!(
        "Resource not found in bundle (no matching Vary variant): {}",
        req.url
      )));
    }

    if cors_partition_key.is_some() {
      if let Some(resource) = self.bundle.resource_for_url(req.url) {
        if resource
          .info
          .vary
          .as_deref()
          .is_some_and(|vary| vary_contains_header(vary, "origin"))
        {
          return Err(Error::Other(format!(
            "Bundle entry has unhandled Vary and cannot be replayed safely: {}",
            req.url
          )));
        }
      }
    }

    self.fetch_partial(req.url, max_bytes)
  }

  fn fetch_range_with_request(
    &self,
    req: FetchRequest<'_>,
    range: std::ops::RangeInclusive<u64>,
    max_bytes: usize,
  ) -> Result<FetchedResource> {
    let kind: FetchContextKind = req.destination.into();
    let start = *range.start();
    let end = *range.end();
    if start > end {
      return Err(Error::Resource(ResourceError::new(
        req.url,
        format!("invalid byte range: start {start} is greater than end {end}"),
      )));
    }

    let capped_end = if max_bytes == 0 {
      start
    } else {
      let cap_end = start.saturating_add((max_bytes as u64).saturating_sub(1));
      end.min(cap_end)
    };

    let lookup_is_request_partitioned = bundle_key_is_request_partitioned(req.url);
    let doc_matches = req.url == self.bundle.manifest.original_url
      || req.url == self.bundle.manifest.document.final_url;

    if doc_matches {
      let (doc_meta, bytes) = self.bundle.document();
      let bytes = clone_bytes_range_fallible(
        req.url,
        &bytes,
        start,
        capped_end,
        max_bytes,
        "bundle document bytes",
      )?;
      let mut res = FetchedResource::with_final_url(
        bytes,
        doc_meta.content_type.clone(),
        Some(doc_meta.final_url.clone()),
      );
      res.nosniff = doc_meta.nosniff;
      res.status = doc_meta.status;
      res.etag = doc_meta.etag.clone();
      res.last_modified = doc_meta.last_modified.clone();
      res.vary = doc_meta.vary.clone();
      res.access_control_allow_origin = doc_meta.access_control_allow_origin.clone();
      res.timing_allow_origin = doc_meta.timing_allow_origin.clone();
      res.response_referrer_policy = doc_meta
        .response_referrer_policy
        .as_deref()
        .and_then(ReferrerPolicy::parse_value_list);
      res.response_headers = doc_meta.response_headers.clone();
      if let Some(vary) = res.vary.as_deref() {
        if super::vary_contains_star(vary)
          || (!super::allow_unhandled_vary_env()
            && !super::vary_is_cacheable(vary, FetchContextKind::Document, None))
        {
          return Err(Error::Other(format!(
            "Bundle document has unhandled Vary and cannot be replayed safely: {}",
            doc_meta.final_url
          )));
        }
      }
      return Ok(res);
    }

    let cors_partition_key = super::cors_cache_partition_key(&req);
    if let Some(partition_key) = cors_partition_key.as_deref() {
      let validate_vary = |vary: Option<&str>| {
        if let Some(vary) = vary {
          if super::vary_contains_star(vary)
            || (!super::allow_unhandled_vary_env()
              && !super::vary_is_cacheable(vary, kind, Some("bundled")))
          {
            return Err(Error::Other(format!(
              "Bundle entry has unhandled Vary and cannot be replayed safely: {}",
              req.url
            )));
          }
        }
        Ok(())
      };

      let key =
        request_partitioned_resource_key_v3(kind, req.url, partition_key, req.credentials_mode);
      if let Some(resource) = self.bundle.resource_for_url(&key) {
        validate_vary(resource.info.vary.as_deref())?;
        return resource.as_fetched_range(req.url, start, capped_end, max_bytes);
      }

      let key = request_partitioned_resource_key_v2(kind, req.url, partition_key);
      if let Some(resource) = self.bundle.resource_for_url(&key) {
        validate_vary(resource.info.vary.as_deref())?;
        return resource.as_fetched_range(req.url, start, capped_end, max_bytes);
      }

      let origin_from_referrer = req.referrer_url.and_then(origin_from_url);
      let origin_from_target = origin_from_url(req.url);
      let origin = req
        .client_origin
        .or(origin_from_referrer.as_ref())
        .or(origin_from_target.as_ref());
      if let Some(origin) = origin {
        let key = request_partitioned_resource_key_with_credentials(
          kind,
          req.url,
          origin,
          req.credentials_mode,
        );
        if let Some(resource) = self.bundle.resource_for_url(&key) {
          validate_vary(resource.info.vary.as_deref())?;
          return resource.as_fetched_range(req.url, start, capped_end, max_bytes);
        }

        if req.credentials_mode != FetchCredentialsMode::Omit {
          let key = request_partitioned_resource_key(kind, req.url, origin);
          if let Some(resource) = self.bundle.resource_for_url(&key) {
            validate_vary(resource.info.vary.as_deref())?;
            return resource.as_fetched_range(req.url, start, capped_end, max_bytes);
          }
        }
      }
    }

    if let Some(bucket) = self.bundle.vary_bucket_for_url(req.url) {
      if bucket.vary.as_deref() == Some("*") {
        return Err(Error::Other(format!(
          "Resource is not cacheable in bundle (Vary: *): {}",
          req.url
        )));
      }
      let canonical = FetchRequest {
        url: bucket.canonical_url.as_str(),
        destination: req.destination,
        referrer_url: req.referrer_url,
        client_origin: req.client_origin,
        referrer_policy: req.referrer_policy,
        credentials_mode: req.credentials_mode,
      };
      let Some(vary_key) =
        super::compute_vary_key_for_request(self, canonical, bucket.vary.as_deref())
      else {
        return Err(Error::Other(format!(
          "Resource not cacheable in bundle (unknown Vary headers): {}",
          req.url
        )));
      };
      if let Some(resource) = bucket.variants.get(&vary_key) {
        return resource.as_fetched_range(req.url, start, capped_end, max_bytes);
      }
      if let Some(legacy_key) =
        super::compute_vary_key_for_request_legacy(self, canonical, bucket.vary.as_deref())
      {
        if legacy_key != vary_key {
          if let Some(resource) = bucket.variants.get(&legacy_key) {
            return resource.as_fetched_range(req.url, start, capped_end, max_bytes);
          }
        }
      }
      if bucket.vary.is_none() && bucket.variants.len() == 1 {
        if let Some(resource) = bucket.variants.values().next() {
          return resource.as_fetched_range(req.url, start, capped_end, max_bytes);
        }
      }
      return Err(Error::Other(format!(
        "Resource not found in bundle (no matching Vary variant): {}",
        req.url
      )));
    }

    if cors_partition_key.is_some() {
      if let Some(resource) = self.bundle.resource_for_url(req.url) {
        if resource
          .info
          .vary
          .as_deref()
          .is_some_and(|vary| vary_contains_header(vary, "origin"))
        {
          return Err(Error::Other(format!(
            "Bundle entry has unhandled Vary and cannot be replayed safely: {}",
            req.url
          )));
        }
      }
    }

    if let Some(resource) = self.bundle.resource_for_url(req.url) {
      let res = resource.as_fetched_range(req.url, start, capped_end, max_bytes)?;
      let destination = resource
        .manifest_kind
        .map(super::FetchDestination::from)
        .unwrap_or(req.destination);
      if !lookup_is_request_partitioned
        && destination.sec_fetch_mode() == "cors"
        && res
          .vary
          .as_deref()
          .is_some_and(|vary| vary_contains_header(vary, "origin"))
      {
        return Err(Error::Other(format!(
          "Bundle entry has unhandled Vary and cannot be replayed safely: {}",
          req.url
        )));
      }
      let kind: FetchContextKind = destination.into();
      let origin_key = lookup_is_request_partitioned.then_some("bundled");
      if let Some(vary) = res.vary.as_deref() {
        if super::vary_contains_star(vary)
          || (!super::allow_unhandled_vary_env()
            && !super::vary_is_cacheable(vary, kind, origin_key))
        {
          return Err(Error::Other(format!(
            "Bundle entry has unhandled Vary and cannot be replayed safely: {}",
            req.url
          )));
        }
      }
      return Ok(res);
    }

    if req
      .url
      .get(..5)
      .map(|prefix| prefix.eq_ignore_ascii_case("data:"))
      .unwrap_or(false)
    {
      if max_bytes == 0 {
        return super::data_url::decode_data_url_prefix(req.url, 0);
      }

      let decode_len: usize = capped_end
        .saturating_add(1)
        .try_into()
        .map_err(|_| {
          Error::Resource(ResourceError::new(
            req.url,
            format!("byte range end {capped_end} is too large to decode"),
          ))
        })?;
      let mut res = super::data_url::decode_data_url_prefix(req.url, decode_len)?;
      let start_idx = usize::try_from(start).map_err(|_| {
        Error::Resource(ResourceError::new(
          req.url,
          format!("byte range start {start} is too large to slice in memory"),
        ))
      })?;
      if start_idx >= res.bytes.len() {
        return Err(Error::Resource(ResourceError::new(
          req.url,
          format!(
            "byte range start {start} is beyond end of decoded data URL (len={})",
            res.bytes.len()
          ),
        )));
      }
      let end_idx = usize::try_from(capped_end).map_err(|_| {
        Error::Resource(ResourceError::new(
          req.url,
          format!("byte range end {capped_end} is too large to slice in memory"),
        ))
      })?;
      let available_end = res.bytes.len().saturating_sub(1);
      let end_idx = end_idx.min(available_end);
      res.bytes = res.bytes[start_idx..=end_idx].to_vec();
      if res.bytes.len() > max_bytes {
        res.bytes.truncate(max_bytes);
      }
      return Ok(res);
    }

    Err(Error::Other(format!(
      "Resource not found in bundle: {}",
      req.url
    )))
  }

  fn request_header_value(&self, req: FetchRequest<'_>, header_name: &str) -> Option<String> {
    let headers = super::build_http_header_pairs(
      req.url,
      &self.bundle.manifest.fetch_profile.user_agent,
      &self.bundle.manifest.fetch_profile.accept_language,
      super::SUPPORTED_ACCEPT_ENCODING,
      None,
      req.destination,
      req.client_origin,
      req.referrer_url,
      req.referrer_policy,
    );
    for (name, value) in headers {
      if name.eq_ignore_ascii_case(header_name) {
        return Some(value);
      }
    }
    // Some commonly varied headers (notably `Origin`/`Referer` and browser-ish `Sec-Fetch-*`) are
    // only emitted for certain destinations (or when `FASTR_HTTP_BROWSER_HEADERS=0`). Treat their
    // absence deterministically as an empty string so callers can still compute a stable variant
    // key.
    if header_name.eq_ignore_ascii_case("origin")
      || header_name.eq_ignore_ascii_case("accept-encoding")
      || header_name.eq_ignore_ascii_case("accept-language")
      || header_name.eq_ignore_ascii_case("user-agent")
      || header_name.eq_ignore_ascii_case("referer")
      || header_name.eq_ignore_ascii_case("x-subdomain")
      || header_name.eq_ignore_ascii_case("sec-fetch-dest")
      || header_name.eq_ignore_ascii_case("sec-fetch-mode")
      || header_name.eq_ignore_ascii_case("sec-fetch-site")
      || header_name.eq_ignore_ascii_case("sec-fetch-user")
      || header_name.eq_ignore_ascii_case("upgrade-insecure-requests")
    {
      return Some(String::new());
    }
    None
  }

  fn fetch_with_validation(
    &self,
    url: &str,
    _etag: Option<&str>,
    _last_modified: Option<&str>,
  ) -> Result<FetchedResource> {
    self.fetch(url)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::debug::runtime::{with_thread_runtime_toggles, RuntimeToggles};
  use crate::html::content_security_policy::CspPolicy;
  use crate::resource::{compute_vary_key_for_request, FetchDestination, FetchRequest};
  use std::io::Cursor;

  fn write_minimal_bundle(dir: &Path) {
    std::fs::write(dir.join("document.html"), "<!doctype html><html></html>").expect("write doc");
    let manifest = BundleManifest {
      version: BUNDLE_VERSION,
      original_url: "https://example.com/".to_string(),
      document: BundledDocument {
        path: "document.html".to_string(),
        content_type: Some("text/html".to_string()),
        nosniff: false,
        final_url: "https://example.com/".to_string(),
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
        viewport: (1200, 800),
        device_pixel_ratio: 1.0,
        scroll_x: 0.0,
        scroll_y: 0.0,
        full_page: false,
        same_origin_subresources: false,
        allowed_subresource_origins: Vec::new(),
        compat_profile: CompatProfile::default(),
        dom_compat_mode: DomCompatibilityMode::default(),
      },
      fetch_profile: BundleFetchProfile::default(),
      resources: BTreeMap::new(),
    };
    std::fs::write(
      dir.join(BUNDLE_MANIFEST),
      serde_json::to_vec_pretty(&manifest).expect("serialize manifest"),
    )
    .expect("write manifest");
  }

  fn create_minimal_bundle_with_vary_manifest_key() -> (tempfile::TempDir, String, Vec<u8>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();

    std::fs::write(root.join("doc.html"), b"<!doctype html><html></html>").expect("write doc");

    let resource_bytes = vec![0x00, 0x01, 0x02, 0x03, 0x7f, 0xfe, 0xff];
    std::fs::write(root.join("res.bin"), &resource_bytes).expect("write resource");

    let base_url = "https://example.invalid/res.bin";
    let synthetic_key = vary_partitioned_resource_key(base_url, "test-key");

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
      fetch_profile: BundleFetchProfile::default(),
      resources,
    };

    std::fs::write(
      root.join(BUNDLE_MANIFEST),
      serde_json::to_vec_pretty(&manifest).expect("serialize manifest"),
    )
    .expect("write manifest");

    (dir, synthetic_key, resource_bytes)
  }

  #[test]
  fn bundled_fetcher_request_header_value_treats_missing_upgrade_insecure_requests_as_empty() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write_minimal_bundle(tmp.path());
    let bundle = Bundle::load(tmp.path()).expect("load bundle");
    let fetcher = BundledFetcher::new(bundle);

    let req = FetchRequest::new("https://example.com/image.png", FetchDestination::Image);
    assert_eq!(
      fetcher
        .request_header_value(req, "upgrade-insecure-requests")
        .as_deref(),
      Some("")
    );

    let req = FetchRequest::new("https://example.com/image.png", FetchDestination::Image);
    assert!(
      compute_vary_key_for_request(&fetcher, req, Some("upgrade-insecure-requests")).is_some(),
      "expected Vary key to be computed deterministically when header is omitted"
    );
  }

  #[test]
  fn bundled_fetcher_request_header_value_treats_missing_sec_fetch_user_as_empty() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write_minimal_bundle(tmp.path());
    let bundle = Bundle::load(tmp.path()).expect("load bundle");
    let fetcher = BundledFetcher::new(bundle);

    let req = FetchRequest::new("https://example.com/", FetchDestination::DocumentNoUser);
    assert_eq!(
      fetcher
        .request_header_value(req, "sec-fetch-user")
        .as_deref(),
      Some("")
    );

    let req = FetchRequest::new("https://example.com/", FetchDestination::DocumentNoUser);
    assert!(
      compute_vary_key_for_request(&fetcher, req, Some("sec-fetch-user")).is_some(),
      "expected Vary key to be computed deterministically when header is omitted"
    );
  }

  #[test]
  fn bundled_fetcher_decodes_data_urls_without_manifest_entries() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let doc_path = tmp.path().join("document.html");
    std::fs::write(&doc_path, "<!doctype html><html><body>hi</body></html>").expect("write doc");

    let manifest = BundleManifest {
      version: BUNDLE_VERSION,
      original_url: "https://example.com/".to_string(),
      document: BundledDocument {
        path: "document.html".to_string(),
        content_type: Some("text/html".to_string()),
        nosniff: false,
        final_url: "https://example.com/".to_string(),
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
        viewport: (1200, 800),
        device_pixel_ratio: 1.0,
        scroll_x: 0.0,
        scroll_y: 0.0,
        full_page: false,
        same_origin_subresources: false,
        allowed_subresource_origins: Vec::new(),
        compat_profile: CompatProfile::default(),
        dom_compat_mode: DomCompatibilityMode::default(),
      },
      fetch_profile: BundleFetchProfile::default(),
      resources: BTreeMap::new(),
    };
    std::fs::write(
      tmp.path().join(BUNDLE_MANIFEST),
      serde_json::to_vec_pretty(&manifest).expect("serialize manifest"),
    )
    .expect("write manifest");

    let bundle = Bundle::load(tmp.path()).expect("load bundle");
    let fetcher = BundledFetcher::new(bundle);

    let res = fetcher
      .fetch("DATA:text/plain;base64,aGk=")
      .expect("fetch data url");
    assert_eq!(res.bytes, b"hi");
    assert_eq!(res.content_type.as_deref(), Some("text/plain"));
  }

  #[test]
  fn bundled_fetcher_decodes_data_urls_with_prefix() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let doc_path = tmp.path().join("document.html");
    std::fs::write(&doc_path, "<!doctype html><html><body>hi</body></html>").expect("write doc");

    let manifest = BundleManifest {
      version: BUNDLE_VERSION,
      original_url: "https://example.com/".to_string(),
      document: BundledDocument {
        path: "document.html".to_string(),
        content_type: Some("text/html".to_string()),
        nosniff: false,
        final_url: "https://example.com/".to_string(),
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
        viewport: (1200, 800),
        device_pixel_ratio: 1.0,
        scroll_x: 0.0,
        scroll_y: 0.0,
        full_page: false,
        same_origin_subresources: false,
        allowed_subresource_origins: Vec::new(),
        compat_profile: CompatProfile::default(),
        dom_compat_mode: DomCompatibilityMode::default(),
      },
      fetch_profile: BundleFetchProfile::default(),
      resources: BTreeMap::new(),
    };
    std::fs::write(
      tmp.path().join(BUNDLE_MANIFEST),
      serde_json::to_vec_pretty(&manifest).expect("serialize manifest"),
    )
    .expect("write manifest");

    let bundle = Bundle::load(tmp.path()).expect("load bundle");
    let fetcher = BundledFetcher::new(bundle);

    let url = "DATA:text/plain;base64,aGVsbG8gd29ybGQ=";
    let res = fetcher
      .fetch_partial(url, 5)
      .expect("fetch data url prefix");
    assert_eq!(res.bytes, b"hello");
    assert_eq!(res.content_type.as_deref(), Some("text/plain"));

    let res_empty = fetcher
      .fetch_partial(url, 0)
      .expect("fetch empty data url prefix");
    assert!(res_empty.bytes.is_empty());
    assert_eq!(res_empty.content_type.as_deref(), Some("text/plain"));
  }

  #[test]
  fn bundled_fetcher_fetch_falls_back_to_base_url_entry_when_vary_variant_missing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
      tmp.path().join("document.html"),
      "<!doctype html><html></html>",
    )
    .expect("write doc");
    std::fs::write(tmp.path().join("base.bin"), b"base").expect("write base");
    std::fs::write(tmp.path().join("variant.bin"), b"variant").expect("write variant");

    let url = "https://cdn.example/font";
    let manifest_key = vary_partitioned_resource_key(url, "deadbeef");

    let base_info = BundledResourceInfo {
      path: "base.bin".to_string(),
      content_type: Some("application/octet-stream".to_string()),
      nosniff: false,
      status: Some(200),
      final_url: Some(url.to_string()),
      etag: None,
      last_modified: None,
      response_referrer_policy: None,
      response_headers: None,
      vary: Some("accept-language".to_string()),
      access_control_allow_origin: None,
      timing_allow_origin: None,
      access_control_allow_credentials: false,
    };

    let manifest = BundleManifest {
      version: BUNDLE_VERSION,
      original_url: "https://example.com/".to_string(),
      document: BundledDocument {
        path: "document.html".to_string(),
        content_type: Some("text/html".to_string()),
        nosniff: false,
        final_url: "https://example.com/".to_string(),
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
        viewport: (1200, 800),
        device_pixel_ratio: 1.0,
        scroll_x: 0.0,
        scroll_y: 0.0,
        full_page: false,
        same_origin_subresources: false,
        allowed_subresource_origins: Vec::new(),
        compat_profile: CompatProfile::default(),
        dom_compat_mode: DomCompatibilityMode::default(),
      },
      fetch_profile: BundleFetchProfile::default(),
      resources: BTreeMap::from([
        (url.to_string(), base_info.clone()),
        (
          manifest_key.clone(),
          BundledResourceInfo {
            path: "variant.bin".to_string(),
            ..base_info.clone()
          },
        ),
      ]),
    };

    std::fs::write(
      tmp.path().join(BUNDLE_MANIFEST),
      serde_json::to_vec_pretty(&manifest).expect("serialize manifest"),
    )
    .expect("write manifest");

    let bundle = Bundle::load(tmp.path()).expect("load bundle");
    let fetcher = BundledFetcher::new(bundle);

    let res = fetcher.fetch(url).expect("fetch base URL");
    assert_eq!(
      res.bytes, b"base",
      "expected fetch() to fall back to the base URL entry when no Vary variant matches"
    );
    let res = fetcher.fetch(&manifest_key).expect("fetch vary key");
    assert_eq!(res.bytes, b"variant");
  }

  #[test]
  fn bundled_fetcher_fetch_partial_clones_only_requested_prefix() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
      tmp.path().join("document.html"),
      "<!doctype html><html></html>",
    )
    .expect("write doc");
    let url = "https://example.com/large.bin";

    let size = 1024 * 1024;
    let mut data = vec![0u8; size];
    for (i, byte) in data.iter_mut().enumerate() {
      *byte = (i % 256) as u8;
    }
    std::fs::write(tmp.path().join("large.bin"), &data).expect("write large resource");

    let expected_headers = vec![("x-test".to_string(), "ok".to_string())];

    let manifest = BundleManifest {
      version: BUNDLE_VERSION,
      original_url: "https://example.com/".to_string(),
      document: BundledDocument {
        path: "document.html".to_string(),
        content_type: Some("text/html".to_string()),
        nosniff: false,
        final_url: "https://example.com/".to_string(),
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
        viewport: (1200, 800),
        device_pixel_ratio: 1.0,
        scroll_x: 0.0,
        scroll_y: 0.0,
        full_page: false,
        same_origin_subresources: false,
        allowed_subresource_origins: Vec::new(),
        compat_profile: CompatProfile::default(),
        dom_compat_mode: DomCompatibilityMode::default(),
      },
      fetch_profile: BundleFetchProfile::default(),
      resources: BTreeMap::from([(
        url.to_string(),
        BundledResourceInfo {
          path: "large.bin".to_string(),
          content_type: Some("application/octet-stream".to_string()),
          nosniff: true,
          status: Some(200),
          final_url: Some(url.to_string()),
          etag: Some("\"etag\"".to_string()),
          last_modified: Some("Mon, 01 Jan 2024 00:00:00 GMT".to_string()),
          response_referrer_policy: Some("no-referrer".to_string()),
          response_headers: Some(expected_headers.clone()),
          vary: Some("accept-encoding".to_string()),
          access_control_allow_origin: Some("*".to_string()),
          timing_allow_origin: Some("*".to_string()),
          access_control_allow_credentials: true,
        },
      )]),
    };
    std::fs::write(
      tmp.path().join(BUNDLE_MANIFEST),
      serde_json::to_vec_pretty(&manifest).expect("serialize manifest"),
    )
    .expect("write manifest");

    let bundle = Bundle::load(tmp.path()).expect("load bundle");
    let fetcher = BundledFetcher::new(bundle);

    let prefix_res = fetcher.fetch_partial(url, 16).expect("fetch prefix");
    assert_eq!(&prefix_res.bytes, &data[..16]);
    assert_eq!(
      prefix_res.content_type.as_deref(),
      Some("application/octet-stream")
    );
    assert!(prefix_res.nosniff);
    assert_eq!(prefix_res.status, Some(200));
    assert_eq!(prefix_res.final_url.as_deref(), Some(url));
    assert_eq!(prefix_res.etag.as_deref(), Some("\"etag\""));
    assert_eq!(
      prefix_res.last_modified.as_deref(),
      Some("Mon, 01 Jan 2024 00:00:00 GMT")
    );
    assert_eq!(prefix_res.vary.as_deref(), Some("accept-encoding"));
    assert_eq!(prefix_res.access_control_allow_origin.as_deref(), Some("*"));
    assert_eq!(prefix_res.timing_allow_origin.as_deref(), Some("*"));
    assert_eq!(
      prefix_res.response_referrer_policy,
      Some(ReferrerPolicy::NoReferrer)
    );
    assert_eq!(prefix_res.response_headers, Some(expected_headers.clone()));
    assert!(prefix_res.access_control_allow_credentials);

    let empty_res = fetcher.fetch_partial(url, 0).expect("fetch empty prefix");
    assert!(empty_res.bytes.is_empty());
    assert_eq!(
      empty_res.content_type.as_deref(),
      Some("application/octet-stream")
    );
    assert!(empty_res.nosniff);
    assert_eq!(empty_res.status, Some(200));
    assert_eq!(empty_res.final_url.as_deref(), Some(url));
    assert_eq!(empty_res.etag.as_deref(), Some("\"etag\""));
    assert_eq!(empty_res.vary.as_deref(), Some("accept-encoding"));
    assert_eq!(empty_res.access_control_allow_origin.as_deref(), Some("*"));
    assert_eq!(
      empty_res.response_referrer_policy,
      Some(ReferrerPolicy::NoReferrer)
    );
    assert_eq!(empty_res.response_headers, Some(expected_headers));
    assert!(empty_res.access_control_allow_credentials);
  }

  #[test]
  fn bundled_fetcher_fetch_range_clones_only_requested_slice() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
      tmp.path().join("document.html"),
      "<!doctype html><html></html>",
    )
    .expect("write doc");
    let url = "https://example.com/large.bin";

    let size = 1024 * 1024;
    let mut data = vec![0u8; size];
    for (i, byte) in data.iter_mut().enumerate() {
      *byte = (i % 256) as u8;
    }
    std::fs::write(tmp.path().join("large.bin"), &data).expect("write large resource");

    let manifest = BundleManifest {
      version: BUNDLE_VERSION,
      original_url: "https://example.com/".to_string(),
      document: BundledDocument {
        path: "document.html".to_string(),
        content_type: Some("text/html".to_string()),
        nosniff: false,
        final_url: "https://example.com/".to_string(),
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
        viewport: (1200, 800),
        device_pixel_ratio: 1.0,
        scroll_x: 0.0,
        scroll_y: 0.0,
        full_page: false,
        same_origin_subresources: false,
        allowed_subresource_origins: Vec::new(),
        compat_profile: CompatProfile::default(),
        dom_compat_mode: DomCompatibilityMode::default(),
      },
      fetch_profile: BundleFetchProfile::default(),
      resources: BTreeMap::from([(
        url.to_string(),
        BundledResourceInfo {
          path: "large.bin".to_string(),
          content_type: Some("application/octet-stream".to_string()),
          nosniff: false,
          status: Some(200),
          final_url: Some(url.to_string()),
          etag: None,
          last_modified: None,
          response_referrer_policy: None,
          response_headers: None,
          vary: None,
          access_control_allow_origin: None,
          timing_allow_origin: None,
          access_control_allow_credentials: false,
        },
      )]),
    };

    std::fs::write(
      tmp.path().join(BUNDLE_MANIFEST),
      serde_json::to_vec_pretty(&manifest).expect("serialize manifest"),
    )
    .expect("write manifest");

    let bundle = Bundle::load(tmp.path()).expect("load bundle");
    let fetcher = BundledFetcher::new(bundle);

    let range_res = fetcher
      .fetch_range_with_request(
        FetchRequest::new(url, FetchDestination::Other),
        100..=200,
        1024,
      )
      .expect("fetch range");
    assert_eq!(&range_res.bytes, &data[100..=200]);
    assert!(
      range_res.bytes.capacity() < 8 * 1024,
      "expected range fetch to only allocate the requested slice, got capacity={}",
      range_res.bytes.capacity()
    );

    let capped_res = fetcher
      .fetch_range_with_request(
        FetchRequest::new(url, FetchDestination::Other),
        100..=200,
        10,
      )
      .expect("fetch capped range");
    assert_eq!(&capped_res.bytes, &data[100..=109]);
    assert_eq!(capped_res.bytes.len(), 10);
  }

  #[test]
  fn bundled_fetcher_fetch_range_selects_vary_variant() {
    struct FixedHeaderFetcher {
      header: &'static str,
      value: String,
    }

    impl ResourceFetcher for FixedHeaderFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        Ok(FetchedResource::new(Vec::new(), None))
      }

      fn request_header_value(&self, _req: FetchRequest<'_>, header_name: &str) -> Option<String> {
        if header_name.eq_ignore_ascii_case(self.header) {
          return Some(self.value.clone());
        }
        Some(String::new())
      }
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
      tmp.path().join("document.html"),
      "<!doctype html><html></html>",
    )
    .expect("write doc");

    let url = "https://example.com/asset.bin";
    let variant_default = b"abcdef".to_vec();
    let variant_other = b"UVWXYZ".to_vec();
    std::fs::write(tmp.path().join("default.bin"), &variant_default).expect("write default");
    std::fs::write(tmp.path().join("other.bin"), &variant_other).expect("write other");

    let vary = "user-agent";

    let default_key = compute_vary_key_for_request(
      &FixedHeaderFetcher {
        header: "user-agent",
        value: crate::resource::DEFAULT_USER_AGENT.to_string(),
      },
      FetchRequest::new(url, FetchDestination::Other),
      Some(vary),
    )
    .expect("compute vary key");
    let other_key = compute_vary_key_for_request(
      &FixedHeaderFetcher {
        header: "user-agent",
        value: "Other-UA".to_string(),
      },
      FetchRequest::new(url, FetchDestination::Other),
      Some(vary),
    )
    .expect("compute vary key");

    let default_manifest_key = vary_partitioned_resource_key(url, &default_key);
    let other_manifest_key = vary_partitioned_resource_key(url, &other_key);

    let info = BundledResourceInfo {
      path: String::new(),
      content_type: Some("application/octet-stream".to_string()),
      nosniff: false,
      status: Some(200),
      final_url: Some(url.to_string()),
      etag: None,
      last_modified: None,
      response_referrer_policy: None,
      response_headers: None,
      vary: Some(vary.to_string()),
      access_control_allow_origin: None,
      timing_allow_origin: None,
      access_control_allow_credentials: false,
    };

    let manifest = BundleManifest {
      version: BUNDLE_VERSION,
      original_url: "https://example.com/".to_string(),
      document: BundledDocument {
        path: "document.html".to_string(),
        content_type: Some("text/html".to_string()),
        nosniff: false,
        final_url: "https://example.com/".to_string(),
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
        viewport: (1200, 800),
        device_pixel_ratio: 1.0,
        scroll_x: 0.0,
        scroll_y: 0.0,
        full_page: false,
        same_origin_subresources: false,
        allowed_subresource_origins: Vec::new(),
        compat_profile: CompatProfile::default(),
        dom_compat_mode: DomCompatibilityMode::default(),
      },
      fetch_profile: BundleFetchProfile::default(),
      resources: BTreeMap::from([
        (
          default_manifest_key.clone(),
          BundledResourceInfo {
            path: "default.bin".to_string(),
            ..info.clone()
          },
        ),
        (
          other_manifest_key.clone(),
          BundledResourceInfo {
            path: "other.bin".to_string(),
            ..info.clone()
          },
        ),
      ]),
    };

    std::fs::write(
      tmp.path().join(BUNDLE_MANIFEST),
      serde_json::to_vec_pretty(&manifest).expect("serialize manifest"),
    )
    .expect("write manifest");

    let bundle = Bundle::load(tmp.path()).expect("load bundle");
    let fetcher = BundledFetcher::new(bundle);

    let res = fetcher
      .fetch_range_with_request(
        FetchRequest::new(url, FetchDestination::Other),
        1..=3,
        32,
      )
      .expect("fetch vary range");
    assert_eq!(&res.bytes, &variant_default[1..=3]);
  }

  #[test]
  fn synthetic_vary_manifest_key_can_be_fetched() {
    let (tmp, synthetic_key, expected_bytes) = create_minimal_bundle_with_vary_manifest_key();

    let bundle = Bundle::load(tmp.path()).expect("load bundle");

    let fetched = bundle
      .fetch_manifest_entry(&synthetic_key)
      .expect("Bundle::fetch_manifest_entry resolves synthetic Vary key");
    assert_eq!(
      fetched.content_type.as_deref(),
      Some("application/octet-stream")
    );
    assert_eq!(fetched.bytes, expected_bytes);

    let fetcher = BundledFetcher::new(bundle);
    let fetched = fetcher
      .fetch(&synthetic_key)
      .expect("BundledFetcher::fetch resolves synthetic Vary key");
    assert_eq!(
      fetched.content_type.as_deref(),
      Some("application/octet-stream")
    );
    assert_eq!(fetched.bytes, expected_bytes);
  }

  #[test]
  fn synthetic_vary_manifest_key_missing_variant_has_actionable_error() {
    let (tmp, synthetic_key, _expected_bytes) = create_minimal_bundle_with_vary_manifest_key();
    let missing_key = synthetic_key.replace("test-key", "missing-key");

    let bundle = Bundle::load(tmp.path()).expect("load bundle");

    let err = bundle
      .fetch_manifest_entry(&missing_key)
      .expect_err("missing Vary variant should error");
    let message = err.to_string();
    assert!(message.contains("Vary variant"), "unexpected error: {message}");
    assert!(message.contains(&missing_key), "unexpected error: {message}");

    let fetcher = BundledFetcher::new(bundle);
    let err = fetcher
      .fetch(&missing_key)
      .expect_err("missing Vary variant should error");
    let message = err.to_string();
    assert!(message.contains("Vary variant"), "unexpected error: {message}");
    assert!(message.contains(&missing_key), "unexpected error: {message}");
  }

  #[test]
  fn non_ascii_whitespace_parse_vary_partitioned_resource_key_does_not_trim_nbsp() {
    let nbsp = "\u{00A0}";
    let key = format!("https://example.com/{BUNDLE_VARY_KEY_SENTINEL}{nbsp}");
    let (base, vary_key) = parse_vary_partitioned_resource_key(&key).expect("parse");
    assert_eq!(base, "https://example.com/");
    assert_eq!(vary_key, nbsp);
  }

  #[test]
  fn bundle_loader_loads_tar_archives() {
    let document_bytes = "<!doctype html><html></html>".as_bytes().to_vec();
    let asset_bytes = b"ok".to_vec();

    let manifest = BundleManifest {
      version: BUNDLE_VERSION,
      original_url: "https://example.com/".to_string(),
      document: BundledDocument {
        path: "document.html".to_string(),
        content_type: Some("text/html".to_string()),
        nosniff: false,
        final_url: "https://example.com/".to_string(),
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
        viewport: (1200, 800),
        device_pixel_ratio: 1.0,
        scroll_x: 0.0,
        scroll_y: 0.0,
        full_page: false,
        same_origin_subresources: false,
        allowed_subresource_origins: Vec::new(),
        compat_profile: CompatProfile::default(),
        dom_compat_mode: DomCompatibilityMode::default(),
      },
      fetch_profile: BundleFetchProfile::default(),
      resources: BTreeMap::from([(
        "https://example.com/asset.bin".to_string(),
        BundledResourceInfo {
          path: "asset.bin".to_string(),
          content_type: Some("application/octet-stream".to_string()),
          nosniff: false,
          status: Some(200),
          final_url: Some("https://example.com/asset.bin".to_string()),
          etag: None,
          last_modified: None,
          response_referrer_policy: None,
          response_headers: None,
          vary: None,
          access_control_allow_origin: None,
          timing_allow_origin: None,
          access_control_allow_credentials: false,
        },
      )]),
    };
    let manifest_bytes = serde_json::to_vec_pretty(&manifest).expect("serialize manifest");

    let mut tar_bytes = Vec::new();
    {
      let mut builder = tar::Builder::new(&mut tar_bytes);

      let mut header = tar::Header::new_gnu();
      header.set_mode(0o644);
      header.set_size(document_bytes.len() as u64);
      header.set_cksum();
      builder
        .append_data(&mut header, "document.html", Cursor::new(&document_bytes))
        .expect("append document");

      let mut header = tar::Header::new_gnu();
      header.set_mode(0o644);
      header.set_size(asset_bytes.len() as u64);
      header.set_cksum();
      builder
        .append_data(&mut header, "asset.bin", Cursor::new(&asset_bytes))
        .expect("append asset");

      let mut header = tar::Header::new_gnu();
      header.set_mode(0o644);
      header.set_size(manifest_bytes.len() as u64);
      header.set_cksum();
      builder
        .append_data(&mut header, BUNDLE_MANIFEST, Cursor::new(&manifest_bytes))
        .expect("append manifest");

      builder.finish().expect("finish tar");
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let tar_path = tmp.path().join("bundle.tar");
    std::fs::write(&tar_path, &tar_bytes).expect("write tar");

    let bundle = Bundle::load(&tar_path).expect("load bundle");
    let fetcher = BundledFetcher::new(bundle);
    let doc = fetcher.fetch("https://example.com/").expect("fetch doc");
    assert_eq!(doc.bytes, document_bytes);
    let asset = fetcher
      .fetch("https://example.com/asset.bin")
      .expect("fetch asset");
    assert_eq!(asset.bytes, asset_bytes);
  }

  #[test]
  fn bundled_fetcher_roundtrips_cors_headers() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
      tmp.path().join("document.html"),
      "<!doctype html><html></html>",
    )
    .expect("write doc");
    std::fs::write(tmp.path().join("style.css"), "body{}").expect("write css");

    let manifest = BundleManifest {
      version: BUNDLE_VERSION,
      original_url: "https://example.com/".to_string(),
      document: BundledDocument {
        path: "document.html".to_string(),
        content_type: Some("text/html".to_string()),
        nosniff: false,
        final_url: "https://example.com/".to_string(),
        status: Some(200),
        etag: None,
        last_modified: None,
        response_referrer_policy: None,
        response_headers: None,
        access_control_allow_origin: Some("*".to_string()),
        timing_allow_origin: Some("https://timing.example".to_string()),
        vary: Some("accept-encoding".to_string()),
      },
      render: BundleRenderConfig {
        viewport: (1200, 800),
        device_pixel_ratio: 1.0,
        scroll_x: 0.0,
        scroll_y: 0.0,
        full_page: false,
        same_origin_subresources: false,
        allowed_subresource_origins: Vec::new(),
        compat_profile: CompatProfile::default(),
        dom_compat_mode: DomCompatibilityMode::default(),
      },
      fetch_profile: BundleFetchProfile::default(),
      resources: BTreeMap::from([(
        "https://example.com/style.css".to_string(),
        BundledResourceInfo {
          path: "style.css".to_string(),
          content_type: Some("text/css".to_string()),
          nosniff: false,
          status: Some(200),
          final_url: Some("https://example.com/style.css".to_string()),
          etag: None,
          last_modified: None,
          response_referrer_policy: None,
          response_headers: None,
          vary: Some("origin".to_string()),
          access_control_allow_origin: Some("https://example.com".to_string()),
          timing_allow_origin: Some("*".to_string()),
          access_control_allow_credentials: true,
        },
      )]),
    };
    std::fs::write(
      tmp.path().join(BUNDLE_MANIFEST),
      serde_json::to_vec_pretty(&manifest).expect("serialize manifest"),
    )
    .expect("write manifest");

    let bundle = Bundle::load(tmp.path()).expect("load bundle");
    let fetcher = BundledFetcher::new(bundle);

    let doc = fetcher.fetch("https://example.com/").expect("fetch doc");
    assert_eq!(doc.access_control_allow_origin.as_deref(), Some("*"));
    assert_eq!(
      doc.timing_allow_origin.as_deref(),
      Some("https://timing.example")
    );
    assert_eq!(doc.vary.as_deref(), Some("accept-encoding"));

    let css = fetcher
      .fetch("https://example.com/style.css")
      .expect("fetch css");
    assert_eq!(
      css.access_control_allow_origin.as_deref(),
      Some("https://example.com")
    );
    assert_eq!(css.timing_allow_origin.as_deref(), Some("*"));
    assert_eq!(css.vary.as_deref(), Some("origin"));
    assert!(css.access_control_allow_credentials);
  }

  #[test]
  fn bundled_fetcher_roundtrips_referrer_policy_header() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
      tmp.path().join("document.html"),
      "<!doctype html><html></html>",
    )
    .expect("write doc");
    std::fs::write(tmp.path().join("style.css"), "body{}").expect("write css");

    let manifest = BundleManifest {
      version: BUNDLE_VERSION,
      original_url: "https://example.com/".to_string(),
      document: BundledDocument {
        path: "document.html".to_string(),
        content_type: Some("text/html".to_string()),
        nosniff: false,
        final_url: "https://example.com/".to_string(),
        status: Some(200),
        etag: None,
        last_modified: None,
        response_referrer_policy: Some("origin".to_string()),
        response_headers: None,
        access_control_allow_origin: None,
        timing_allow_origin: None,
        vary: None,
      },
      render: BundleRenderConfig {
        viewport: (1200, 800),
        device_pixel_ratio: 1.0,
        scroll_x: 0.0,
        scroll_y: 0.0,
        full_page: false,
        same_origin_subresources: false,
        allowed_subresource_origins: Vec::new(),
        compat_profile: CompatProfile::default(),
        dom_compat_mode: DomCompatibilityMode::default(),
      },
      fetch_profile: BundleFetchProfile::default(),
      resources: BTreeMap::from([(
        "https://example.com/style.css".to_string(),
        BundledResourceInfo {
          path: "style.css".to_string(),
          content_type: Some("text/css".to_string()),
          nosniff: false,
          status: Some(200),
          final_url: Some("https://example.com/style.css".to_string()),
          etag: None,
          last_modified: None,
          response_referrer_policy: Some("no-referrer".to_string()),
          response_headers: None,
          access_control_allow_origin: None,
          timing_allow_origin: None,
          vary: None,
          access_control_allow_credentials: false,
        },
      )]),
    };
    std::fs::write(
      tmp.path().join(BUNDLE_MANIFEST),
      serde_json::to_vec_pretty(&manifest).expect("serialize manifest"),
    )
    .expect("write manifest");

    let bundle = Bundle::load(tmp.path()).expect("load bundle");
    let fetcher = BundledFetcher::new(bundle);

    let doc = fetcher.fetch("https://example.com/").expect("fetch doc");
    assert_eq!(doc.response_referrer_policy, Some(ReferrerPolicy::Origin));

    let css = fetcher
      .fetch("https://example.com/style.css")
      .expect("fetch css");
    assert_eq!(
      css.response_referrer_policy,
      Some(ReferrerPolicy::NoReferrer)
    );
  }

  #[test]
  fn bundled_fetcher_roundtrips_response_headers_for_csp_policy() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
      tmp.path().join("document.html"),
      "<!doctype html><html></html>",
    )
    .expect("write doc");

    let csp_value = "default-src 'self'";
    let original_url = "https://example.com/".to_string();
    let manifest = BundleManifest {
      version: BUNDLE_VERSION,
      original_url: original_url.clone(),
      document: BundledDocument {
        path: "document.html".to_string(),
        content_type: Some("text/html".to_string()),
        nosniff: false,
        final_url: original_url.clone(),
        status: Some(200),
        etag: None,
        last_modified: None,
        response_referrer_policy: None,
        response_headers: Some(vec![(
          "Content-Security-Policy".to_string(),
          csp_value.to_string(),
        )]),
        access_control_allow_origin: None,
        timing_allow_origin: None,
        vary: None,
      },
      render: BundleRenderConfig {
        viewport: (1200, 800),
        device_pixel_ratio: 1.0,
        scroll_x: 0.0,
        scroll_y: 0.0,
        full_page: false,
        same_origin_subresources: false,
        allowed_subresource_origins: Vec::new(),
        compat_profile: CompatProfile::default(),
        dom_compat_mode: DomCompatibilityMode::default(),
      },
      fetch_profile: BundleFetchProfile::default(),
      resources: BTreeMap::new(),
    };

    std::fs::write(
      tmp.path().join(BUNDLE_MANIFEST),
      serde_json::to_vec_pretty(&manifest).expect("serialize manifest"),
    )
    .expect("write manifest");

    let bundle = Bundle::load(tmp.path()).expect("load bundle");
    let fetcher = BundledFetcher::new(bundle);

    let res = fetcher.fetch(original_url.as_str()).expect("fetch doc");
    assert_eq!(
      res.header_values("Content-Security-Policy"),
      vec![csp_value]
    );
    assert!(CspPolicy::from_response_headers(&res).is_some());
  }

  #[test]
  fn bundled_fetcher_roundtrips_nosniff_stylesheet_metadata() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
      tmp.path().join("document.html"),
      "<!doctype html><html></html>",
    )
    .expect("write doc");
    std::fs::write(tmp.path().join("style.css"), "body{}").expect("write css");

    let css_url = "https://example.com/style.css";
    let manifest = BundleManifest {
      version: BUNDLE_VERSION,
      original_url: "https://example.com/".to_string(),
      document: BundledDocument {
        path: "document.html".to_string(),
        content_type: Some("text/html".to_string()),
        nosniff: false,
        final_url: "https://example.com/".to_string(),
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
        viewport: (1200, 800),
        device_pixel_ratio: 1.0,
        scroll_x: 0.0,
        scroll_y: 0.0,
        full_page: false,
        same_origin_subresources: false,
        allowed_subresource_origins: Vec::new(),
        compat_profile: CompatProfile::default(),
        dom_compat_mode: DomCompatibilityMode::default(),
      },
      fetch_profile: BundleFetchProfile::default(),
      resources: BTreeMap::from([(
        css_url.to_string(),
        BundledResourceInfo {
          path: "style.css".to_string(),
          content_type: Some("text/plain".to_string()),
          nosniff: true,
          status: Some(200),
          final_url: Some(css_url.to_string()),
          etag: None,
          last_modified: None,
          response_referrer_policy: None,
          response_headers: None,
          access_control_allow_origin: None,
          timing_allow_origin: None,
          vary: None,
          access_control_allow_credentials: false,
        },
      )]),
    };

    std::fs::write(
      tmp.path().join(BUNDLE_MANIFEST),
      serde_json::to_vec_pretty(&manifest).expect("serialize manifest"),
    )
    .expect("write manifest");

    let bundle = Bundle::load(tmp.path()).expect("load bundle");
    let fetcher = BundledFetcher::new(bundle);

    let toggles = Arc::new(RuntimeToggles::from_map(HashMap::from([(
      "FASTR_FETCH_STRICT_MIME".to_string(),
      "1".to_string(),
    )])));

    with_thread_runtime_toggles(toggles, || {
      let css = fetcher.fetch(css_url).expect("fetch css");
      assert!(css.nosniff);

      let err = crate::resource::ensure_stylesheet_mime_sane(&css, css_url)
        .expect_err("expected nosniff stylesheet MIME enforcement");
      assert!(
        err.to_string().contains("unexpected content-type"),
        "unexpected error: {err}"
      );
    });
  }

  #[test]
  fn bundle_loads_v1_manifest_without_cors_fields() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
      tmp.path().join("document.html"),
      "<!doctype html><html></html>",
    )
    .expect("write doc");
    std::fs::write(tmp.path().join("style.css"), "body{}").expect("write css");

    // Older bundles (v1) may not include CORS header fields; they should deserialize as `None`,
    // and `Access-Control-Allow-Credentials` should default to `false`.
    let manifest_json = serde_json::json!({
      "version": BUNDLE_VERSION,
      "original_url": "https://example.com/",
      "document": {
        "path": "document.html",
        "content_type": "text/html",
        "final_url": "https://example.com/",
        "status": 200,
        "etag": null,
        "last_modified": null
      },
      "render": {
        "viewport": [1200, 800],
        "device_pixel_ratio": 1.0,
        "scroll_x": 0.0,
        "scroll_y": 0.0,
        "full_page": false,
        "same_origin_subresources": false,
        "allowed_subresource_origins": [],
        "compat_profile": CompatProfile::default(),
        "dom_compat_mode": DomCompatibilityMode::default()
      },
      "resources": {
        "https://example.com/style.css": {
          "path": "style.css",
          "content_type": "text/css",
          "status": 200,
          "final_url": "https://example.com/style.css",
          "etag": null,
          "last_modified": null
        }
      }
    });
    std::fs::write(
      tmp.path().join(BUNDLE_MANIFEST),
      serde_json::to_vec_pretty(&manifest_json).expect("serialize manifest json"),
    )
    .expect("write manifest");

    let bundle = Bundle::load(tmp.path()).expect("load bundle");
    let fetcher = BundledFetcher::new(bundle);
    assert_eq!(
      fetcher.bundle.manifest.fetch_profile.user_agent.as_str(),
      super::super::DEFAULT_USER_AGENT,
      "expected missing fetch_profile.user_agent to default for back-compat"
    );
    assert_eq!(
      fetcher
        .bundle
        .manifest
        .fetch_profile
        .accept_language
        .as_str(),
      super::super::DEFAULT_ACCEPT_LANGUAGE,
      "expected missing fetch_profile.accept_language to default for back-compat"
    );
    let doc = fetcher.fetch("https://example.com/").expect("fetch doc");
    assert_eq!(doc.access_control_allow_origin, None);
    assert_eq!(doc.timing_allow_origin, None);
    assert_eq!(doc.vary, None);

    let css = fetcher
      .fetch("https://example.com/style.css")
      .expect("fetch css");
    assert_eq!(css.bytes, b"body{}");
    assert_eq!(css.access_control_allow_origin, None);
    assert_eq!(css.timing_allow_origin, None);
    assert_eq!(css.vary, None);
    assert!(!css.access_control_allow_credentials);
  }

  #[test]
  fn bundled_fetcher_uses_request_partitioned_entries_for_cors_mode_requests() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
      tmp.path().join("document.html"),
      "<!doctype html><html></html>",
    )
    .expect("write doc");

    let url = "https://cdn.example/font.woff2";
    let origin_a = origin_from_url("https://a.test/page.html").expect("origin A");
    let origin_b = origin_from_url("https://b.test/page.html").expect("origin B");

    let key_a = request_partitioned_resource_key_with_credentials(
      FetchContextKind::Font,
      url,
      &origin_a,
      FetchCredentialsMode::Include,
    );
    let key_b = request_partitioned_resource_key_with_credentials(
      FetchContextKind::Font,
      url,
      &origin_b,
      FetchCredentialsMode::Include,
    );

    std::fs::write(tmp.path().join("font_raw.woff2"), b"raw").expect("write raw font");
    std::fs::write(tmp.path().join("font_a.woff2"), b"a").expect("write font a");
    std::fs::write(tmp.path().join("font_b.woff2"), b"b").expect("write font b");

    let manifest = BundleManifest {
      version: BUNDLE_VERSION,
      original_url: "https://example.com/".to_string(),
      document: BundledDocument {
        path: "document.html".to_string(),
        content_type: Some("text/html".to_string()),
        nosniff: false,
        final_url: "https://example.com/".to_string(),
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
        viewport: (1200, 800),
        device_pixel_ratio: 1.0,
        scroll_x: 0.0,
        scroll_y: 0.0,
        full_page: false,
        same_origin_subresources: false,
        allowed_subresource_origins: Vec::new(),
        compat_profile: CompatProfile::default(),
        dom_compat_mode: DomCompatibilityMode::default(),
      },
      fetch_profile: BundleFetchProfile::default(),
      resources: BTreeMap::from([
        (
          url.to_string(),
          BundledResourceInfo {
            path: "font_raw.woff2".to_string(),
            content_type: Some("font/woff2".to_string()),
            nosniff: false,
            status: Some(200),
            final_url: Some(url.to_string()),
            etag: None,
            last_modified: None,
            response_referrer_policy: None,
            response_headers: None,
            vary: None,
            access_control_allow_origin: Some("https://a.test".to_string()),
            timing_allow_origin: None,
            access_control_allow_credentials: false,
          },
        ),
        (
          key_a,
          BundledResourceInfo {
            path: "font_a.woff2".to_string(),
            content_type: Some("font/woff2".to_string()),
            nosniff: false,
            status: Some(200),
            final_url: Some(url.to_string()),
            etag: None,
            last_modified: None,
            response_referrer_policy: None,
            response_headers: None,
            vary: None,
            access_control_allow_origin: Some("https://a.test".to_string()),
            timing_allow_origin: None,
            access_control_allow_credentials: false,
          },
        ),
        (
          key_b,
          BundledResourceInfo {
            path: "font_b.woff2".to_string(),
            content_type: Some("font/woff2".to_string()),
            nosniff: false,
            status: Some(200),
            final_url: Some(url.to_string()),
            etag: None,
            last_modified: None,
            response_referrer_policy: None,
            response_headers: None,
            vary: None,
            access_control_allow_origin: Some("https://b.test".to_string()),
            timing_allow_origin: None,
            access_control_allow_credentials: false,
          },
        ),
      ]),
    };

    std::fs::write(
      tmp.path().join(BUNDLE_MANIFEST),
      serde_json::to_vec_pretty(&manifest).expect("serialize manifest"),
    )
    .expect("write manifest");

    let bundle = Bundle::load(tmp.path()).expect("load bundle");
    let fetcher = BundledFetcher::new(bundle);

    let toggles = Arc::new(RuntimeToggles::from_map(HashMap::from([
      ("FASTR_FETCH_ENFORCE_CORS".to_string(), "0".to_string()),
      (
        "FASTR_FETCH_PARTITION_CORS_CACHE".to_string(),
        "1".to_string(),
      ),
    ])));

    with_thread_runtime_toggles(toggles, || {
      let a = fetcher
        .fetch_with_request(
          FetchRequest::new(url, FetchDestination::Font)
            .with_referrer_url("https://a.test/page.html")
            .with_credentials_mode(FetchCredentialsMode::Include),
        )
        .expect("fetch origin A");
      assert_eq!(a.bytes, b"a");

      let b = fetcher
        .fetch_with_request(
          FetchRequest::new(url, FetchDestination::Font)
            .with_referrer_url("https://b.test/page.html")
            .with_credentials_mode(FetchCredentialsMode::Include),
        )
        .expect("fetch origin B");
      assert_eq!(b.bytes, b"b");
    });
  }

  #[test]
  fn bundled_fetcher_prefers_request_partitioned_v2_entries_for_cors_mode_requests() {
    if !super::super::http_browser_headers_enabled() {
      eprintln!(
        "skipping bundled_fetcher_prefers_request_partitioned_v2_entries_for_cors_mode_requests: browser-like request headers are disabled"
      );
      return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
      tmp.path().join("document.html"),
      "<!doctype html><html></html>",
    )
    .expect("write doc");

    let url = "https://cdn.example/font.woff2";
    let key = request_partitioned_resource_key_v2(
      FetchContextKind::Font,
      url,
      "https://a.test|cred=include",
    );

    std::fs::write(tmp.path().join("font_raw.woff2"), b"raw").expect("write raw font");
    std::fs::write(tmp.path().join("font_v2.woff2"), b"v2").expect("write v2 font");

    let base_info = BundledResourceInfo {
      path: "font_raw.woff2".to_string(),
      content_type: Some("font/woff2".to_string()),
      nosniff: false,
      status: Some(200),
      final_url: Some(url.to_string()),
      etag: None,
      last_modified: None,
      response_referrer_policy: None,
      response_headers: None,
      access_control_allow_origin: Some("https://a.test".to_string()),
      timing_allow_origin: None,
      vary: None,
      access_control_allow_credentials: true,
    };

    let v2_info = BundledResourceInfo {
      path: "font_v2.woff2".to_string(),
      ..base_info.clone()
    };

    let manifest = BundleManifest {
      version: BUNDLE_VERSION,
      original_url: "https://example.com/".to_string(),
      document: BundledDocument {
        path: "document.html".to_string(),
        content_type: Some("text/html".to_string()),
        nosniff: false,
        final_url: "https://example.com/".to_string(),
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
        viewport: (1200, 800),
        device_pixel_ratio: 1.0,
        scroll_x: 0.0,
        scroll_y: 0.0,
        full_page: false,
        same_origin_subresources: false,
        allowed_subresource_origins: Vec::new(),
        compat_profile: CompatProfile::default(),
        dom_compat_mode: DomCompatibilityMode::default(),
      },
      fetch_profile: BundleFetchProfile::default(),
      resources: BTreeMap::from([(url.to_string(), base_info), (key, v2_info)]),
    };

    std::fs::write(
      tmp.path().join(BUNDLE_MANIFEST),
      serde_json::to_vec_pretty(&manifest).expect("serialize manifest"),
    )
    .expect("write manifest");

    let bundle = Bundle::load(tmp.path()).expect("load bundle");
    let fetcher = BundledFetcher::new(bundle);

    let toggles = Arc::new(RuntimeToggles::from_map(HashMap::from([
      (
        "FASTR_FETCH_PARTITION_CORS_CACHE".to_string(),
        "1".to_string(),
      ),
      ("FASTR_FETCH_ENFORCE_CORS".to_string(), "0".to_string()),
    ])));
    with_thread_runtime_toggles(toggles, || {
      let res = fetcher
        .fetch_with_request(
          FetchRequest::new(url, FetchDestination::Font)
            .with_referrer_url("https://a.test/page.html")
            .with_credentials_mode(FetchCredentialsMode::Include),
        )
        .expect("fetch v2 entry");
      assert_eq!(
        res.bytes, b"v2",
        "expected request-partitioned entry to override unpartitioned URL entry"
      );
    });
  }

  #[test]
  fn bundled_fetcher_selects_request_partitioned_v3_entries_by_credentials_mode() {
    if !super::super::http_browser_headers_enabled() {
      eprintln!(
        "skipping bundled_fetcher_selects_request_partitioned_v3_entries_by_credentials_mode: browser-like request headers are disabled"
      );
      return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
      tmp.path().join("document.html"),
      "<!doctype html><html></html>",
    )
    .expect("write doc");

    let url = "https://cdn.example/font.woff2".to_string();
    let partition_key = "https://cdn.example|cred=include";
    let key_include = request_partitioned_resource_key_v3(
      FetchContextKind::Font,
      &url,
      partition_key,
      FetchCredentialsMode::Include,
    );
    let key_same_origin = request_partitioned_resource_key_v3(
      FetchContextKind::Font,
      &url,
      partition_key,
      FetchCredentialsMode::SameOrigin,
    );

    std::fs::write(tmp.path().join("font_include.woff2"), b"include").expect("write include font");
    std::fs::write(tmp.path().join("font_same.woff2"), b"same-origin").expect("write same font");

    let base_info = |path: &str| BundledResourceInfo {
      path: path.to_string(),
      content_type: Some("font/woff2".to_string()),
      nosniff: false,
      status: Some(200),
      final_url: Some(url.to_string()),
      etag: None,
      last_modified: None,
      response_referrer_policy: None,
      response_headers: None,
      access_control_allow_origin: Some("https://cdn.example".to_string()),
      timing_allow_origin: None,
      vary: None,
      access_control_allow_credentials: true,
    };

    let manifest = BundleManifest {
      version: BUNDLE_VERSION,
      original_url: "https://example.com/".to_string(),
      document: BundledDocument {
        path: "document.html".to_string(),
        content_type: Some("text/html".to_string()),
        nosniff: false,
        final_url: "https://example.com/".to_string(),
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
        viewport: (1200, 800),
        device_pixel_ratio: 1.0,
        scroll_x: 0.0,
        scroll_y: 0.0,
        full_page: false,
        same_origin_subresources: false,
        allowed_subresource_origins: Vec::new(),
        compat_profile: CompatProfile::default(),
        dom_compat_mode: DomCompatibilityMode::default(),
      },
      fetch_profile: BundleFetchProfile::default(),
      resources: BTreeMap::from([
        (key_include, base_info("font_include.woff2")),
        (key_same_origin, base_info("font_same.woff2")),
      ]),
    };

    std::fs::write(
      tmp.path().join(BUNDLE_MANIFEST),
      serde_json::to_vec_pretty(&manifest).expect("serialize manifest"),
    )
    .expect("write manifest");

    let bundle = Bundle::load(tmp.path()).expect("load bundle");
    let fetcher = BundledFetcher::new(bundle);

    let client_origin = origin_from_url("https://cdn.example/page.html").expect("client origin");

    let toggles = Arc::new(RuntimeToggles::from_map(HashMap::from([
      (
        "FASTR_FETCH_PARTITION_CORS_CACHE".to_string(),
        "1".to_string(),
      ),
      ("FASTR_FETCH_ENFORCE_CORS".to_string(), "0".to_string()),
    ])));
    with_thread_runtime_toggles(toggles, || {
      let include_req = FetchRequest::new(&url, FetchDestination::Font)
        .with_client_origin(&client_origin)
        .with_credentials_mode(FetchCredentialsMode::Include);
      let same_req = FetchRequest::new(&url, FetchDestination::Font)
        .with_client_origin(&client_origin)
        .with_credentials_mode(FetchCredentialsMode::SameOrigin);

      assert_eq!(
        super::super::cors_cache_partition_key(&include_req).as_deref(),
        Some(partition_key),
        "expected include request to use the test partition key"
      );
      assert_eq!(
        super::super::cors_cache_partition_key(&same_req).as_deref(),
        Some(partition_key),
        "expected same-origin request to share the test partition key with include"
      );

      let include_res = fetcher
        .fetch_with_request(include_req)
        .expect("fetch include entry");
      assert_eq!(include_res.bytes, b"include");

      let same_res = fetcher
        .fetch_with_request(same_req)
        .expect("fetch same-origin entry");
      assert_eq!(same_res.bytes, b"same-origin");
    });
  }

  #[test]
  fn bundled_fetcher_rejects_unhandled_vary_for_request_partitioned_v2_entries() {
    if !super::super::http_browser_headers_enabled() {
      eprintln!(
        "skipping bundled_fetcher_rejects_unhandled_vary_for_request_partitioned_v2_entries: browser-like request headers are disabled"
      );
      return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
      tmp.path().join("document.html"),
      "<!doctype html><html></html>",
    )
    .expect("write doc");
    std::fs::write(tmp.path().join("font_v2.woff2"), b"v2").expect("write v2 font");

    let url = "https://cdn.example/font.woff2";
    let key = request_partitioned_resource_key_v2(
      FetchContextKind::Font,
      url,
      "https://a.test|cred=include",
    );

    let info = BundledResourceInfo {
      path: "font_v2.woff2".to_string(),
      content_type: Some("font/woff2".to_string()),
      nosniff: false,
      status: Some(200),
      final_url: Some(url.to_string()),
      etag: None,
      last_modified: None,
      response_referrer_policy: None,
      response_headers: None,
      access_control_allow_origin: Some("https://a.test".to_string()),
      timing_allow_origin: None,
      vary: Some("x-foo".to_string()),
      access_control_allow_credentials: true,
    };

    let manifest = BundleManifest {
      version: BUNDLE_VERSION,
      original_url: "https://example.com/".to_string(),
      document: BundledDocument {
        path: "document.html".to_string(),
        content_type: Some("text/html".to_string()),
        nosniff: false,
        final_url: "https://example.com/".to_string(),
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
        viewport: (1200, 800),
        device_pixel_ratio: 1.0,
        scroll_x: 0.0,
        scroll_y: 0.0,
        full_page: false,
        same_origin_subresources: false,
        allowed_subresource_origins: Vec::new(),
        compat_profile: CompatProfile::default(),
        dom_compat_mode: DomCompatibilityMode::default(),
      },
      fetch_profile: BundleFetchProfile::default(),
      resources: BTreeMap::from([(key, info)]),
    };

    std::fs::write(
      tmp.path().join(BUNDLE_MANIFEST),
      serde_json::to_vec_pretty(&manifest).expect("serialize manifest"),
    )
    .expect("write manifest");

    let bundle = Bundle::load(tmp.path()).expect("load bundle");
    let fetcher = BundledFetcher::new(bundle);

    let toggles = Arc::new(RuntimeToggles::from_map(HashMap::from([
      (
        "FASTR_FETCH_PARTITION_CORS_CACHE".to_string(),
        "1".to_string(),
      ),
      ("FASTR_FETCH_ENFORCE_CORS".to_string(), "0".to_string()),
      (
        "FASTR_CACHE_ALLOW_VARY_UNHANDLED".to_string(),
        "0".to_string(),
      ),
    ])));
    with_thread_runtime_toggles(toggles, || {
      let err = fetcher
        .fetch_with_request(
          FetchRequest::new(url, FetchDestination::Font)
            .with_referrer_url("https://a.test/page.html")
            .with_credentials_mode(FetchCredentialsMode::Include),
        )
        .unwrap_err();
      match err {
        Error::Other(message) => assert!(
          message.contains("unhandled Vary"),
          "unexpected error message: {message}"
        ),
        other => panic!("expected Error::Other, got {other:?}"),
      }
    });
  }

  #[test]
  fn bundle_loader_allows_multiple_manifest_keys_to_share_one_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
      tmp.path().join("document.html"),
      "<!doctype html><html></html>",
    )
    .expect("write doc");

    let url = "https://cdn.example/font.woff2";
    let origin_a = origin_from_url("https://a.test/page.html").expect("origin A");
    let origin_b = origin_from_url("https://b.test/page.html").expect("origin B");

    let key_a = request_partitioned_resource_key_with_credentials(
      FetchContextKind::Font,
      url,
      &origin_a,
      FetchCredentialsMode::Include,
    );
    let key_b = request_partitioned_resource_key_with_credentials(
      FetchContextKind::Font,
      url,
      &origin_b,
      FetchCredentialsMode::Include,
    );

    std::fs::write(tmp.path().join("font.woff2"), b"ok").expect("write font");

    let shared_info = |allow_origin: &str| BundledResourceInfo {
      path: "font.woff2".to_string(),
      content_type: Some("font/woff2".to_string()),
      nosniff: false,
      status: Some(200),
      final_url: Some(url.to_string()),
      etag: None,
      last_modified: None,
      response_referrer_policy: None,
      response_headers: None,
      vary: None,
      access_control_allow_origin: Some(allow_origin.to_string()),
      timing_allow_origin: None,
      access_control_allow_credentials: false,
    };

    let manifest = BundleManifest {
      version: BUNDLE_VERSION,
      original_url: "https://example.com/".to_string(),
      document: BundledDocument {
        path: "document.html".to_string(),
        content_type: Some("text/html".to_string()),
        nosniff: false,
        final_url: "https://example.com/".to_string(),
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
        viewport: (1200, 800),
        device_pixel_ratio: 1.0,
        scroll_x: 0.0,
        scroll_y: 0.0,
        full_page: false,
        same_origin_subresources: false,
        allowed_subresource_origins: Vec::new(),
        compat_profile: CompatProfile::default(),
        dom_compat_mode: DomCompatibilityMode::default(),
      },
      fetch_profile: BundleFetchProfile::default(),
      resources: BTreeMap::from([
        (url.to_string(), shared_info("https://a.test")),
        (key_a, shared_info("https://a.test")),
        (key_b, shared_info("https://b.test")),
      ]),
    };

    std::fs::write(
      tmp.path().join(BUNDLE_MANIFEST),
      serde_json::to_vec_pretty(&manifest).expect("serialize manifest"),
    )
    .expect("write manifest");

    let bundle = Bundle::load(tmp.path()).expect("load bundle");
    let fetcher = BundledFetcher::new(bundle);

    let toggles = Arc::new(RuntimeToggles::from_map(HashMap::from([
      ("FASTR_FETCH_ENFORCE_CORS".to_string(), "0".to_string()),
      (
        "FASTR_FETCH_PARTITION_CORS_CACHE".to_string(),
        "1".to_string(),
      ),
    ])));

    with_thread_runtime_toggles(toggles, || {
      let a = fetcher
        .fetch_with_request(
          FetchRequest::new(url, FetchDestination::Font)
            .with_referrer_url("https://a.test/page.html")
            .with_credentials_mode(FetchCredentialsMode::Include),
        )
        .expect("fetch origin A");
      assert_eq!(a.bytes, b"ok");
      assert_eq!(
        a.access_control_allow_origin.as_deref(),
        Some("https://a.test")
      );

      let b = fetcher
        .fetch_with_request(
          FetchRequest::new(url, FetchDestination::Font)
            .with_referrer_url("https://b.test/page.html")
            .with_credentials_mode(FetchCredentialsMode::Include),
        )
        .expect("fetch origin B");
      assert_eq!(b.bytes, b"ok");
      assert_eq!(
        b.access_control_allow_origin.as_deref(),
        Some("https://b.test")
      );
    });
  }

  #[test]
  fn bundled_fetcher_rejects_unhandled_vary_entries() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
      tmp.path().join("document.html"),
      "<!doctype html><html></html>",
    )
    .expect("write doc");
    std::fs::write(tmp.path().join("asset.bin"), b"ok").expect("write asset");

    let url = "https://example.com/asset.bin";
    let manifest = BundleManifest {
      version: BUNDLE_VERSION,
      original_url: "https://example.com/".to_string(),
      document: BundledDocument {
        path: "document.html".to_string(),
        content_type: Some("text/html".to_string()),
        nosniff: false,
        final_url: "https://example.com/".to_string(),
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
        viewport: (1200, 800),
        device_pixel_ratio: 1.0,
        scroll_x: 0.0,
        scroll_y: 0.0,
        full_page: false,
        same_origin_subresources: false,
        allowed_subresource_origins: Vec::new(),
        compat_profile: CompatProfile::default(),
        dom_compat_mode: DomCompatibilityMode::default(),
      },
      fetch_profile: BundleFetchProfile::default(),
      resources: BTreeMap::from([(
        url.to_string(),
        BundledResourceInfo {
          path: "asset.bin".to_string(),
          content_type: Some("application/octet-stream".to_string()),
          nosniff: false,
          status: Some(200),
          final_url: Some(url.to_string()),
          etag: None,
          last_modified: None,
          response_referrer_policy: None,
          response_headers: None,
          access_control_allow_origin: None,
          timing_allow_origin: None,
          vary: Some("x-foo".to_string()),
          access_control_allow_credentials: false,
        },
      )]),
    };

    std::fs::write(
      tmp.path().join(BUNDLE_MANIFEST),
      serde_json::to_vec_pretty(&manifest).expect("serialize manifest"),
    )
    .expect("write manifest");

    let bundle = Bundle::load(tmp.path()).expect("load bundle");
    let fetcher = BundledFetcher::new(bundle);

    let err = fetcher.fetch(url).unwrap_err();
    match err {
      Error::Other(message) => assert!(
        message.contains("unhandled Vary"),
        "unexpected error message: {message}"
      ),
      other => panic!("expected Error::Other, got {other:?}"),
    }
  }

  #[test]
  fn bundled_fetcher_allows_vary_origin_only_when_request_partitioned() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
      tmp.path().join("document.html"),
      "<!doctype html><html></html>",
    )
    .expect("write doc");
    std::fs::write(tmp.path().join("font.woff2"), b"ok").expect("write font");

    let url = "https://cdn.example/font.woff2";
    let origin = origin_from_url("https://a.test/page.html").expect("origin");
    let key = request_partitioned_resource_key(FetchContextKind::Font, url, &origin);

    let info = BundledResourceInfo {
      path: "font.woff2".to_string(),
      content_type: Some("font/woff2".to_string()),
      nosniff: false,
      status: Some(200),
      final_url: Some(url.to_string()),
      etag: None,
      last_modified: None,
      response_referrer_policy: None,
      response_headers: None,
      vary: Some("origin".to_string()),
      access_control_allow_origin: Some("https://a.test".to_string()),
      timing_allow_origin: None,
      access_control_allow_credentials: false,
    };

    let manifest = BundleManifest {
      version: BUNDLE_VERSION,
      original_url: "https://example.com/".to_string(),
      document: BundledDocument {
        path: "document.html".to_string(),
        content_type: Some("text/html".to_string()),
        nosniff: false,
        final_url: "https://example.com/".to_string(),
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
        viewport: (1200, 800),
        device_pixel_ratio: 1.0,
        scroll_x: 0.0,
        scroll_y: 0.0,
        full_page: false,
        same_origin_subresources: false,
        allowed_subresource_origins: Vec::new(),
        compat_profile: CompatProfile::default(),
        dom_compat_mode: DomCompatibilityMode::default(),
      },
      fetch_profile: BundleFetchProfile::default(),
      resources: BTreeMap::from([(url.to_string(), info.clone()), (key, info)]),
    };

    std::fs::write(
      tmp.path().join(BUNDLE_MANIFEST),
      serde_json::to_vec_pretty(&manifest).expect("serialize manifest"),
    )
    .expect("write manifest");

    let bundle = Bundle::load(tmp.path()).expect("load bundle");
    let fetcher = BundledFetcher::new(bundle);

    let err = fetcher.fetch(url).unwrap_err();
    match err {
      Error::Other(message) => assert!(
        message.contains("unhandled Vary"),
        "unexpected error message: {message}"
      ),
      other => panic!("expected Error::Other, got {other:?}"),
    }

    let toggles = Arc::new(RuntimeToggles::from_map(HashMap::from([(
      "FASTR_FETCH_PARTITION_CORS_CACHE".to_string(),
      "1".to_string(),
    )])));
    with_thread_runtime_toggles(toggles, || {
      let res = fetcher
        .fetch_with_request(
          FetchRequest::new(url, FetchDestination::Font)
            .with_referrer_url("https://a.test/page.html"),
        )
        .expect("fetch request-partitioned entry");
      assert_eq!(res.bytes, b"ok");
    });
  }

  #[test]
  fn bundled_fetcher_fetch_rejects_vary_origin_for_cors_mode_image_and_stylesheet_entries() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
      tmp.path().join("document.html"),
      "<!doctype html><html></html>",
    )
    .expect("write doc");
    std::fs::write(tmp.path().join("image.png"), b"img").expect("write image");
    std::fs::write(tmp.path().join("style.css"), "body{}").expect("write css");

    let image_url = "https://cdn.example/image.png";
    let style_url = "https://cdn.example/style.css";

    let image_key =
      request_partitioned_resource_key_v2(FetchContextKind::ImageCors, image_url, "https://a.test");
    let style_key = request_partitioned_resource_key_v2(
      FetchContextKind::StylesheetCors,
      style_url,
      "https://a.test",
    );

    let image_info = BundledResourceInfo {
      path: "image.png".to_string(),
      content_type: Some("image/png".to_string()),
      nosniff: false,
      status: Some(200),
      final_url: Some(image_url.to_string()),
      etag: None,
      last_modified: None,
      response_referrer_policy: None,
      response_headers: None,
      vary: Some(" Origin ".to_string()),
      access_control_allow_origin: Some("https://a.test".to_string()),
      timing_allow_origin: None,
      access_control_allow_credentials: false,
    };

    let style_info = BundledResourceInfo {
      path: "style.css".to_string(),
      content_type: Some("text/css".to_string()),
      nosniff: false,
      status: Some(200),
      final_url: Some(style_url.to_string()),
      etag: None,
      last_modified: None,
      response_referrer_policy: None,
      response_headers: None,
      vary: Some("Origin".to_string()),
      access_control_allow_origin: Some("https://a.test".to_string()),
      timing_allow_origin: None,
      access_control_allow_credentials: false,
    };

    let manifest = BundleManifest {
      version: BUNDLE_VERSION,
      original_url: "https://example.com/".to_string(),
      document: BundledDocument {
        path: "document.html".to_string(),
        content_type: Some("text/html".to_string()),
        nosniff: false,
        final_url: "https://example.com/".to_string(),
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
        viewport: (1200, 800),
        device_pixel_ratio: 1.0,
        scroll_x: 0.0,
        scroll_y: 0.0,
        full_page: false,
        same_origin_subresources: false,
        allowed_subresource_origins: Vec::new(),
        compat_profile: CompatProfile::default(),
        dom_compat_mode: DomCompatibilityMode::default(),
      },
      fetch_profile: BundleFetchProfile::default(),
      resources: BTreeMap::from([(image_key, image_info), (style_key, style_info)]),
    };

    std::fs::write(
      tmp.path().join(BUNDLE_MANIFEST),
      serde_json::to_vec_pretty(&manifest).expect("serialize manifest"),
    )
    .expect("write manifest");

    let bundle = Bundle::load(tmp.path()).expect("load bundle");
    let fetcher = BundledFetcher::new(bundle);

    for url in [image_url, style_url] {
      let err = fetcher.fetch(url).unwrap_err();
      match err {
        Error::Other(message) => assert!(
          message.contains("unhandled Vary"),
          "unexpected error message: {message}"
        ),
        other => panic!("expected Error::Other, got {other:?}"),
      }
    }
  }

  #[test]
  fn bundled_fetcher_rejects_vary_origin_when_partitioning_enabled_but_entry_unpartitioned() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
      tmp.path().join("document.html"),
      "<!doctype html><html></html>",
    )
    .expect("write doc");
    std::fs::write(tmp.path().join("image.png"), b"img").expect("write image");
    std::fs::write(tmp.path().join("style.css"), "body{}").expect("write css");

    let image_url = "https://cdn.example/image.png";
    let style_url = "https://cdn.example/style.css";

    let image_info = BundledResourceInfo {
      path: "image.png".to_string(),
      content_type: Some("image/png".to_string()),
      nosniff: false,
      status: Some(200),
      final_url: Some(image_url.to_string()),
      etag: None,
      last_modified: None,
      response_referrer_policy: None,
      response_headers: None,
      vary: Some("origin".to_string()),
      access_control_allow_origin: Some("https://a.test".to_string()),
      timing_allow_origin: None,
      access_control_allow_credentials: false,
    };

    let style_info = BundledResourceInfo {
      path: "style.css".to_string(),
      content_type: Some("text/css".to_string()),
      nosniff: false,
      status: Some(200),
      final_url: Some(style_url.to_string()),
      etag: None,
      last_modified: None,
      response_referrer_policy: None,
      response_headers: None,
      vary: Some("origin".to_string()),
      access_control_allow_origin: Some("https://a.test".to_string()),
      timing_allow_origin: None,
      access_control_allow_credentials: false,
    };

    let manifest = BundleManifest {
      version: BUNDLE_VERSION,
      original_url: "https://example.com/".to_string(),
      document: BundledDocument {
        path: "document.html".to_string(),
        content_type: Some("text/html".to_string()),
        nosniff: false,
        final_url: "https://example.com/".to_string(),
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
        viewport: (1200, 800),
        device_pixel_ratio: 1.0,
        scroll_x: 0.0,
        scroll_y: 0.0,
        full_page: false,
        same_origin_subresources: false,
        allowed_subresource_origins: Vec::new(),
        compat_profile: CompatProfile::default(),
        dom_compat_mode: DomCompatibilityMode::default(),
      },
      fetch_profile: BundleFetchProfile::default(),
      resources: BTreeMap::from([
        (image_url.to_string(), image_info),
        (style_url.to_string(), style_info),
      ]),
    };

    std::fs::write(
      tmp.path().join(BUNDLE_MANIFEST),
      serde_json::to_vec_pretty(&manifest).expect("serialize manifest"),
    )
    .expect("write manifest");

    let bundle = Bundle::load(tmp.path()).expect("load bundle");
    let fetcher = BundledFetcher::new(bundle);

    let toggles = Arc::new(RuntimeToggles::from_map(HashMap::from([
      (
        "FASTR_FETCH_PARTITION_CORS_CACHE".to_string(),
        "1".to_string(),
      ),
      ("FASTR_FETCH_ENFORCE_CORS".to_string(), "0".to_string()),
    ])));

    with_thread_runtime_toggles(toggles, || {
      for (url, destination) in [
        (image_url, FetchDestination::ImageCors),
        (style_url, FetchDestination::StyleCors),
      ] {
        let err = fetcher
          .fetch_with_request(
            FetchRequest::new(url, destination).with_referrer_url("https://a.test/page.html"),
          )
          .unwrap_err();
        match err {
          Error::Other(message) => assert!(
            message.contains("unhandled Vary"),
            "unexpected error message: {message}"
          ),
          other => panic!("expected Error::Other, got {other:?}"),
        }
      }
    });
  }

  #[test]
  fn bundled_fetcher_selects_vary_origin_variants() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
      tmp.path().join("document.html"),
      "<!doctype html><html></html>",
    )
    .expect("write doc");
    std::fs::write(tmp.path().join("font_a.woff2"), b"a").expect("write font a");
    std::fs::write(tmp.path().join("font_b.woff2"), b"b").expect("write font b");

    let url = "https://cdn.example/font.woff2";
    let http = crate::resource::HttpFetcher::new();
    let req_a =
      FetchRequest::new(url, FetchDestination::Font).with_referrer_url("https://a.test/page.html");
    let req_b =
      FetchRequest::new(url, FetchDestination::Font).with_referrer_url("https://b.test/page.html");

    let vary_key_a =
      super::super::compute_vary_key_for_request(&http, req_a, Some("origin")).expect("vary key A");
    let vary_key_b =
      super::super::compute_vary_key_for_request(&http, req_b, Some("origin")).expect("vary key B");

    let key_a = vary_partitioned_resource_key(url, &vary_key_a);
    let key_b = vary_partitioned_resource_key(url, &vary_key_b);

    let manifest = BundleManifest {
      version: BUNDLE_VERSION,
      original_url: "https://example.com/".to_string(),
      document: BundledDocument {
        path: "document.html".to_string(),
        content_type: Some("text/html".to_string()),
        nosniff: false,
        final_url: "https://example.com/".to_string(),
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
        viewport: (1200, 800),
        device_pixel_ratio: 1.0,
        scroll_x: 0.0,
        scroll_y: 0.0,
        full_page: false,
        same_origin_subresources: false,
        allowed_subresource_origins: Vec::new(),
        compat_profile: CompatProfile::default(),
        dom_compat_mode: DomCompatibilityMode::default(),
      },
      fetch_profile: BundleFetchProfile::default(),
      resources: BTreeMap::from([
        (
          key_a,
          BundledResourceInfo {
            path: "font_a.woff2".to_string(),
            content_type: Some("font/woff2".to_string()),
            nosniff: false,
            status: Some(200),
            final_url: Some(url.to_string()),
            etag: None,
            last_modified: None,
            response_referrer_policy: None,
            response_headers: None,
            vary: Some("origin".to_string()),
            access_control_allow_origin: None,
            timing_allow_origin: None,
            access_control_allow_credentials: false,
          },
        ),
        (
          key_b,
          BundledResourceInfo {
            path: "font_b.woff2".to_string(),
            content_type: Some("font/woff2".to_string()),
            nosniff: false,
            status: Some(200),
            final_url: Some(url.to_string()),
            etag: None,
            last_modified: None,
            response_referrer_policy: None,
            response_headers: None,
            vary: Some("origin".to_string()),
            access_control_allow_origin: None,
            timing_allow_origin: None,
            access_control_allow_credentials: false,
          },
        ),
      ]),
    };

    std::fs::write(
      tmp.path().join(BUNDLE_MANIFEST),
      serde_json::to_vec_pretty(&manifest).expect("serialize manifest"),
    )
    .expect("write manifest");

    let bundle = Bundle::load(tmp.path()).expect("load bundle");
    let fetcher = BundledFetcher::new(bundle);

    let a = fetcher.fetch_with_request(req_a).expect("fetch origin A");
    assert_eq!(a.bytes, b"a");

    let b = fetcher.fetch_with_request(req_b).expect("fetch origin B");
    assert_eq!(b.bytes, b"b");
  }

  #[test]
  fn bundled_fetcher_selects_vary_user_agent_variants_using_manifest_profile() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
      tmp.path().join("document.html"),
      "<!doctype html><html></html>",
    )
    .expect("write doc");
    std::fs::write(tmp.path().join("ua_foo.woff2"), b"foo").expect("write foo variant");
    std::fs::write(tmp.path().join("ua_bar.woff2"), b"bar").expect("write bar variant");

    let url = "https://cdn.example/font.woff2";

    #[derive(Clone)]
    struct FixedUserAgentFetcher {
      user_agent: String,
    }

    impl ResourceFetcher for FixedUserAgentFetcher {
      fn fetch(&self, _url: &str) -> Result<FetchedResource> {
        Err(Error::Other("not implemented".to_string()))
      }

      fn request_header_value(&self, _req: FetchRequest<'_>, header_name: &str) -> Option<String> {
        if header_name.eq_ignore_ascii_case("user-agent") {
          Some(self.user_agent.clone())
        } else {
          None
        }
      }
    }

    let req = FetchRequest::new(url, FetchDestination::Font);
    let vary_key_foo = super::super::compute_vary_key_for_request(
      &FixedUserAgentFetcher {
        user_agent: "Foo/1.0".to_string(),
      },
      req,
      Some("user-agent"),
    )
    .expect("vary key foo");
    let vary_key_bar = super::super::compute_vary_key_for_request(
      &FixedUserAgentFetcher {
        user_agent: "Bar/1.0".to_string(),
      },
      req,
      Some("user-agent"),
    )
    .expect("vary key bar");

    let key_foo = vary_partitioned_resource_key(url, &vary_key_foo);
    let key_bar = vary_partitioned_resource_key(url, &vary_key_bar);

    let manifest = BundleManifest {
      version: BUNDLE_VERSION,
      original_url: "https://example.com/".to_string(),
      document: BundledDocument {
        path: "document.html".to_string(),
        content_type: Some("text/html".to_string()),
        nosniff: false,
        final_url: "https://example.com/".to_string(),
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
        viewport: (1200, 800),
        device_pixel_ratio: 1.0,
        scroll_x: 0.0,
        scroll_y: 0.0,
        full_page: false,
        same_origin_subresources: false,
        allowed_subresource_origins: Vec::new(),
        compat_profile: CompatProfile::default(),
        dom_compat_mode: DomCompatibilityMode::default(),
      },
      fetch_profile: BundleFetchProfile {
        user_agent: "Foo/1.0".to_string(),
        accept_language: super::super::DEFAULT_ACCEPT_LANGUAGE.to_string(),
      },
      resources: BTreeMap::from([
        (
          key_foo,
          BundledResourceInfo {
            path: "ua_foo.woff2".to_string(),
            content_type: Some("font/woff2".to_string()),
            nosniff: false,
            status: Some(200),
            final_url: Some(url.to_string()),
            etag: None,
            last_modified: None,
            response_referrer_policy: None,
            response_headers: None,
            vary: Some("user-agent".to_string()),
            access_control_allow_origin: None,
            timing_allow_origin: None,
            access_control_allow_credentials: false,
          },
        ),
        (
          key_bar,
          BundledResourceInfo {
            path: "ua_bar.woff2".to_string(),
            content_type: Some("font/woff2".to_string()),
            nosniff: false,
            status: Some(200),
            final_url: Some(url.to_string()),
            etag: None,
            last_modified: None,
            response_referrer_policy: None,
            response_headers: None,
            vary: Some("user-agent".to_string()),
            access_control_allow_origin: None,
            timing_allow_origin: None,
            access_control_allow_credentials: false,
          },
        ),
      ]),
    };

    std::fs::write(
      tmp.path().join(BUNDLE_MANIFEST),
      serde_json::to_vec_pretty(&manifest).expect("serialize manifest"),
    )
    .expect("write manifest");

    let bundle = Bundle::load(tmp.path()).expect("load bundle");
    let fetcher = BundledFetcher::new(bundle);

    let res = fetcher.fetch(url).expect("fetch bundle");
    assert_eq!(res.bytes, b"foo");
  }

  #[test]
  fn bundled_fetcher_selects_vary_variants_when_vary_casing_or_order_differs() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
      tmp.path().join("document.html"),
      "<!doctype html><html></html>",
    )
    .expect("write doc");
    std::fs::write(tmp.path().join("font.woff2"), b"ok").expect("write font");

    let url = "https://cdn.example/font.woff2";
    let http = crate::resource::HttpFetcher::new();
    let req =
      FetchRequest::new(url, FetchDestination::Font).with_referrer_url("https://a.test/page.html");

    // Compute the manifest vary-key from one representation of the vary list...
    let vary_key =
      super::super::compute_vary_key_for_request(&http, req, Some("origin, accept-language"))
        .expect("vary key");
    let key = vary_partitioned_resource_key(url, &vary_key);

    // ...but store a semantically equivalent vary value with different casing + ordering. Bundle
    // replay should still select the correct variant deterministically.
    let manifest = BundleManifest {
      version: BUNDLE_VERSION,
      original_url: "https://example.com/".to_string(),
      document: BundledDocument {
        path: "document.html".to_string(),
        content_type: Some("text/html".to_string()),
        nosniff: false,
        final_url: "https://example.com/".to_string(),
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
        viewport: (1200, 800),
        device_pixel_ratio: 1.0,
        scroll_x: 0.0,
        scroll_y: 0.0,
        full_page: false,
        same_origin_subresources: false,
        allowed_subresource_origins: Vec::new(),
        compat_profile: CompatProfile::default(),
        dom_compat_mode: DomCompatibilityMode::default(),
      },
      fetch_profile: BundleFetchProfile::default(),
      resources: BTreeMap::from([(
        key,
        BundledResourceInfo {
          path: "font.woff2".to_string(),
          content_type: Some("font/woff2".to_string()),
          nosniff: false,
          status: Some(200),
          final_url: Some(url.to_string()),
          etag: None,
          last_modified: None,
          response_referrer_policy: None,
          response_headers: None,
          vary: Some("Accept-Language, Origin".to_string()),
          access_control_allow_origin: None,
          timing_allow_origin: None,
          access_control_allow_credentials: false,
        },
      )]),
    };

    std::fs::write(
      tmp.path().join(BUNDLE_MANIFEST),
      serde_json::to_vec_pretty(&manifest).expect("serialize manifest"),
    )
    .expect("write manifest");

    let bundle = Bundle::load(tmp.path()).expect("load bundle");
    let fetcher = BundledFetcher::new(bundle);
    let res = fetcher.fetch_with_request(req).expect("fetch bundle");
    assert_eq!(res.bytes, b"ok");
  }
}
