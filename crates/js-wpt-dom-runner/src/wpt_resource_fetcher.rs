use crate::cookie_jar::CookieJar;
use crate::wpt_fs::{WptFs, WptFsError};
use fastrender::error::{Error, ResourceError, Result};
use fastrender::resource::{FetchedResource, ResourceFetcher};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use url::Url;

/// Offline-only `ResourceFetcher` implementation for the curated WPT DOM corpus.
///
/// URL mapping follows `tests/wpt_dom/README.md`:
/// - `https://web-platform.test/<path>` → `<root>/tests/<path>`
/// - `https://web-platform.test/resources/<path>` → `<root>/resources/<path>`
///
/// Any other `http(s)` origin is rejected deterministically (no network fetch).
#[derive(Debug, Clone)]
pub struct WptResourceFetcher {
  tests_root: PathBuf,
  resources_root: PathBuf,
  cookie_jar: Arc<Mutex<CookieJar>>,
}

impl WptResourceFetcher {
  /// Create a new fetcher rooted at `tests/wpt_dom/`.
  pub fn new(root: impl AsRef<Path>) -> std::result::Result<Self, WptFsError> {
    let fs = WptFs::new(root)?;
    Ok(Self::from_wpt_fs(&fs))
  }

  /// Create a fetcher from an existing [`WptFs`] instance.
  pub fn from_wpt_fs(fs: &WptFs) -> Self {
    Self {
      tests_root: fs.tests_root().to_path_buf(),
      resources_root: fs.resources_root().to_path_buf(),
      cookie_jar: Arc::new(Mutex::new(CookieJar::new())),
    }
  }

  fn map_url_to_path(&self, url: &str) -> Result<PathBuf> {
    let parsed = Url::parse(url)
      .map_err(|err| Error::Resource(ResourceError::new(url, format!("invalid URL: {err}"))))?;

    let scheme = parsed.scheme();
    match scheme {
      "http" | "https" => {}
      _ => {
        return Err(Error::Resource(ResourceError::new(
          url,
          format!("unsupported URL scheme for WPT offline fetcher: {scheme}"),
        )));
      }
    }

    let host = parsed.host_str().unwrap_or_default();
    if !host.eq_ignore_ascii_case("web-platform.test") {
      return Err(Error::Resource(ResourceError::new(
        url,
        format!("offline WPT fetcher blocked network request to non-WPT origin: {host}"),
      )));
    }

    // `url::Url` normalizes dot-segments in `.path()`; we want to reject path traversal attempts
    // even when they would normalize back into the corpus. Extract the raw path from the original
    // URL string instead.
    let raw_path = extract_raw_http_path(url).ok_or_else(|| {
      Error::Resource(ResourceError::new(
        url,
        "failed to extract path from URL for WPT offline fetcher",
      ))
    })?;
    let (base, rel) = match raw_path.strip_prefix("/resources/") {
      Some(rel) => (&self.resources_root, rel),
      None => (&self.tests_root, raw_path.trim_start_matches('/')),
    };

    join_under_corpus(base, rel).map_err(|reason| {
      Error::Resource(ResourceError::new(
        url,
        format!("invalid WPT corpus path (refusing to escape corpus root): {reason}"),
      ))
    })
  }
}

impl ResourceFetcher for WptResourceFetcher {
  fn fetch(&self, url: &str) -> Result<FetchedResource> {
    let path = self.map_url_to_path(url)?;
    let trace = std::env::var("FASTERENDER_WPT_DOM_TRACE_FETCH")
      .ok()
      .is_some_and(|v| !v.trim().is_empty() && v.trim() != "0");
    if trace {
      eprintln!("[wpt_dom fetch] {url} -> {}", path.display());
    }

    match std::fs::read(&path) {
      Ok(bytes) => {
        let mut res = FetchedResource::new(bytes, sniff_content_type(&path));
        res.status = Some(200);
        if trace {
          eprintln!(
            "[wpt_dom fetch] {url} status=200 bytes={}",
            res.bytes.len()
          );
        }
        Ok(res)
      }
      Err(err)
        if matches!(
          err.kind(),
          std::io::ErrorKind::NotFound | std::io::ErrorKind::IsADirectory
        ) =>
      {
        let mut res = FetchedResource::new(Vec::new(), None);
        res.status = Some(404);
        if trace {
          eprintln!("[wpt_dom fetch] {url} status=404");
        }
        Ok(res)
      }
      Err(err) => Err(Error::Resource(
        ResourceError::new(url, format!("failed to read {}", path.display())).with_source(err),
      )),
    }
  }

  fn cookie_header_value(&self, _url: &str) -> Option<String> {
    let lock = self
      .cookie_jar
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    Some(lock.cookie_string())
  }

  fn store_cookie_from_document(&self, _url: &str, cookie_string: &str) {
    let mut lock = self
      .cookie_jar
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    lock.set_cookie_string(cookie_string);
  }
}

fn sniff_content_type(path: &Path) -> Option<String> {
  let ext = path.extension()?.to_str()?.to_ascii_lowercase();
  match ext.as_str() {
    "js" => Some("application/javascript".to_string()),
    "html" | "htm" => Some("text/html".to_string()),
    "css" => Some("text/css".to_string()),
    "json" => Some("application/json".to_string()),
    "txt" => Some("text/plain".to_string()),
    _ => None,
  }
}

fn join_under_corpus(base: &Path, rel: &str) -> std::result::Result<PathBuf, &'static str> {
  // Normalize dot segments in `rel` to match browser URL resolution semantics.
  //
  // The browser tab backend can request URLs whose raw path still contains `..` segments
  // (e.g. `https://web-platform.test/dom/ranges/../common.js`). These are not path traversal
  // attempts once resolved against the document URL, so accept them as long as the normalized path
  // stays within `base`.
  let mut normalized = PathBuf::new();
  for component in Path::new(rel).components() {
    match component {
      Component::Normal(part) => normalized.push(part),
      Component::CurDir => {}
      Component::ParentDir => {
        if !normalized.pop() {
          return Err("path contains '..' that would escape corpus root");
        }
      }
      Component::RootDir | Component::Prefix(_) => return Err("path is absolute"),
    }
  }

  let joined = base.join(&normalized);

  // Best-effort symlink escape check when the file exists. This intentionally does not canonicalize
  // missing paths so the caller can surface a 404 response (canonicalize requires existence).
  if joined.exists() {
    let canonical = joined
      .canonicalize()
      .map_err(|_| "failed to canonicalize existing path")?;
    if !canonical.starts_with(base) {
      return Err("resolved path escapes corpus root");
    }
    return Ok(canonical);
  }

  Ok(joined)
}

fn extract_raw_http_path(url: &str) -> Option<String> {
  let scheme_end = url.find(':')?;
  if url.get((scheme_end + 1)..(scheme_end + 3))? != "//" {
    return None;
  }
  let after_scheme = &url[(scheme_end + 3)..];

  // Find the first delimiter after the authority component.
  let mut delim_idx: Option<usize> = None;
  for (idx, b) in after_scheme.as_bytes().iter().enumerate() {
    if matches!(*b, b'/' | b'?' | b'#') {
      delim_idx = Some(idx);
      break;
    }
  }

  let Some(idx) = delim_idx else {
    // No path/query/fragment: `https://web-platform.test` (treat as `/`).
    return Some("/".to_string());
  };

  match after_scheme.as_bytes()[idx] {
    b'/' => {
      let rest = &after_scheme[idx..];
      let end = rest.find(['?', '#']).unwrap_or_else(|| rest.len());
      Some(rest[..end].to_string())
    }
    // Query or fragment with no explicit path.
    b'?' | b'#' => Some("/".to_string()),
    _ => None,
  }
}
