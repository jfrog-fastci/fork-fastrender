//! Process-global backing store for `blob:` object URLs created via `URL.createObjectURL()`.
//!
//! This is a minimal implementation intended to unblock common real-world patterns:
//! - In-memory image previews (`img.src = URL.createObjectURL(blob)`)
//! - `fetch(URL.createObjectURL(blob))`
//! - Revocation via `URL.revokeObjectURL(url)`
//!
//! The registry is process-global (mirroring browsers) and guarded by a `Mutex` so it can be used
//! from the various `vm-js` bindings that may run on different threads.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

/// Maximum number of live object URLs allowed at once.
pub(crate) const MAX_LIVE_OBJECT_URLS: usize = 10_000;

/// Maximum total bytes stored across all live object URLs.
pub(crate) const MAX_TOTAL_OBJECT_URL_BYTES: usize = 128 * 1024 * 1024; // 128 MiB

#[derive(Debug, Clone)]
pub(crate) struct ObjectUrlEntry {
  pub(crate) bytes: Vec<u8>,
  pub(crate) content_type: String,
  pub(crate) origin: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CreateObjectUrlError {
  TooManyUrls,
  TooManyBytes,
}

#[derive(Default)]
struct ObjectUrlRegistry {
  entries: HashMap<String, ObjectUrlEntry>,
  total_bytes: usize,
}

static REGISTRY: OnceLock<Mutex<ObjectUrlRegistry>> = OnceLock::new();
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn registry() -> &'static Mutex<ObjectUrlRegistry> {
  REGISTRY.get_or_init(|| Mutex::new(ObjectUrlRegistry::default()))
}

pub(crate) fn create_object_url(
  origin: &str,
  bytes: Vec<u8>,
  content_type: String,
) -> Result<String, CreateObjectUrlError> {
  let mut lock = registry()
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner());

  if lock.entries.len() >= MAX_LIVE_OBJECT_URLS {
    return Err(CreateObjectUrlError::TooManyUrls);
  }

  let add = bytes.len();
  let next_total = lock.total_bytes.checked_add(add).unwrap_or(usize::MAX);
  if next_total > MAX_TOTAL_OBJECT_URL_BYTES {
    return Err(CreateObjectUrlError::TooManyBytes);
  }

  let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
  let url = format!("blob:{origin}/{id}");

  lock.total_bytes = next_total;
  lock.entries.insert(
    url.clone(),
    ObjectUrlEntry {
      bytes,
      content_type,
      origin: origin.to_string(),
    },
  );

  Ok(url)
}

pub(crate) fn revoke_object_url(url: &str) {
  let mut lock = registry()
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner());
  if let Some(entry) = lock.entries.remove(url) {
    lock.total_bytes = lock.total_bytes.saturating_sub(entry.bytes.len());
  }
}

pub(crate) fn get_object_url(url: &str) -> Option<ObjectUrlEntry> {
  let lock = registry()
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner());
  lock.entries.get(url).cloned()
}

#[cfg(test)]
mod tests {
  use super::*;

  struct RevokeOnDrop(Option<String>);

  impl RevokeOnDrop {
    fn new(url: String) -> Self {
      Self(Some(url))
    }

    fn disarm(&mut self) {
      self.0.take();
    }

    fn revoke(&mut self) {
      if let Some(url) = self.0.take() {
        revoke_object_url(&url);
      }
    }
  }

  impl Drop for RevokeOnDrop {
    fn drop(&mut self) {
      self.revoke();
    }
  }

  #[test]
  fn create_and_revoke_object_url_roundtrip() {
    let url = create_object_url(
      "https://example.com",
      vec![1, 2, 3],
      "text/plain".to_string(),
    )
    .expect("create_object_url should succeed");

    let mut cleanup = RevokeOnDrop::new(url.clone());

    assert!(
      url.starts_with("blob:https://example.com/"),
      "unexpected object URL: {url}"
    );

    let entry = get_object_url(&url).expect("object URL should be registered");
    assert_eq!(entry.bytes, vec![1, 2, 3]);
    assert_eq!(entry.content_type, "text/plain");
    assert_eq!(entry.origin, "https://example.com");

    revoke_object_url(&url);
    assert!(get_object_url(&url).is_none());

    cleanup.disarm();
  }

  #[test]
  fn revoke_is_idempotent() {
    revoke_object_url("blob:https://example.com/does-not-exist");
  }
}
