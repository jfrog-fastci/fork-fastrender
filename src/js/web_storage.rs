//! Thread-local Web Storage backend (localStorage / sessionStorage).
//!
//! FastRender stores Web Storage state in a thread-local hub.
//!
//! ## Deterministic tests
//!
//! Rust's test harness may reuse worker threads between tests. Because the default hub is
//! thread-local, storage state can leak between tests that happen to execute on the same thread.
//! This module therefore exposes **test-only** helpers:
//! - [`reset_default_web_storage_hub_for_tests`] resets all storage state for the current thread.
//! - [`set_default_storage_quota_for_tests`] overrides the per-area quota for the current thread.

use parking_lot::Mutex;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

/// Conservative per-origin quota for storage areas.
///
/// Web Storage quotas are implementation-defined. We pick a deterministic default so tests and
/// render outputs are stable.
pub const DEFAULT_STORAGE_QUOTA_BYTES: usize = 5 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorageLimits {
  pub max_bytes_per_area: usize,
}

impl Default for StorageLimits {
  fn default() -> Self {
    Self {
      max_bytes_per_area: DEFAULT_STORAGE_QUOTA_BYTES,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageKind {
  Local,
  Session,
}

/// Storage "origin key" derived from a document URL.
///
/// Opaque origins (e.g. `data:`/`about:`) map to `None` and must get a fresh, non-persistent storage
/// area for every request.
pub type StorageOriginKey = Option<String>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionNamespaceId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageChange {
  pub key: Option<String>,
  pub old_value: Option<String>,
  pub new_value: Option<String>,
  pub did_mutate: bool,
}

impl StorageChange {
  fn no_op(key: Option<String>) -> Self {
    Self {
      key,
      old_value: None,
      new_value: None,
      did_mutate: false,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageError {
  QuotaExceeded,
}

/// An in-memory Web Storage area (`localStorage` / `sessionStorage`) with deterministic key
/// ordering and quota enforcement.
///
/// This is not a general-purpose map:
/// - Key iteration order is insertion order (updates do not re-order existing keys).
/// - Quota accounting is based on UTF-8 byte length of key + value strings.
#[derive(Debug)]
pub struct StorageArea {
  values: HashMap<String, String>,
  key_order: Vec<String>,
  bytes_used: usize,
  quota_bytes: usize,
}

impl Default for StorageArea {
  fn default() -> Self {
    Self::new()
  }
}

impl StorageArea {
  pub fn new() -> Self {
    Self::new_with_quota(DEFAULT_STORAGE_QUOTA_BYTES)
  }

  pub fn new_with_quota(quota_bytes: usize) -> Self {
    Self {
      values: HashMap::new(),
      key_order: Vec::new(),
      bytes_used: 0,
      quota_bytes,
    }
  }

  fn set_quota_bytes(&mut self, quota_bytes: usize) {
    self.quota_bytes = quota_bytes;
  }

  pub fn get_item(&self, key: &str) -> Option<String> {
    self.values.get(key).cloned()
  }

  pub fn set_item(&mut self, key: &str, value: &str) -> Result<StorageChange, StorageError> {
    let old_value = self.values.get(key).cloned();
    let new_value = value.to_string();

    if old_value.as_deref() == Some(value) {
      return Ok(StorageChange {
        key: Some(key.to_string()),
        old_value,
        new_value: Some(new_value),
        did_mutate: false,
      });
    }

    let bytes_used_next = match &old_value {
      Some(old_value) => {
        // Updating an existing key does not change insertion order and does not re-count the key
        // bytes (only the value bytes change).
        let without_old_value = self
          .bytes_used
          .checked_sub(old_value.len())
          .ok_or(StorageError::QuotaExceeded)?;
        without_old_value
          .checked_add(value.len())
          .ok_or(StorageError::QuotaExceeded)?
      }
      None => self
        .bytes_used
        .checked_add(key.len())
        .and_then(|v| v.checked_add(value.len()))
        .ok_or(StorageError::QuotaExceeded)?,
    };

    if bytes_used_next > self.quota_bytes {
      return Err(StorageError::QuotaExceeded);
    }

    // Commit mutation.
    if old_value.is_none() {
      self.key_order.push(key.to_string());
    }
    self.values.insert(key.to_string(), new_value.clone());
    self.bytes_used = bytes_used_next;

    Ok(StorageChange {
      key: Some(key.to_string()),
      old_value,
      new_value: Some(new_value),
      did_mutate: true,
    })
  }

  pub fn remove_item(&mut self, key: &str) -> StorageChange {
    let Some(old_value) = self.values.remove(key) else {
      return StorageChange::no_op(Some(key.to_string()));
    };

    let delta = key.len().saturating_add(old_value.len());
    self.bytes_used = self.bytes_used.saturating_sub(delta);
    if let Some(pos) = self.key_order.iter().position(|k| k == key) {
      self.key_order.remove(pos);
    }

    StorageChange {
      key: Some(key.to_string()),
      old_value: Some(old_value),
      new_value: None,
      did_mutate: true,
    }
  }

  pub fn clear(&mut self) -> StorageChange {
    if self.values.is_empty() {
      return StorageChange::no_op(None);
    }
    self.values.clear();
    self.key_order.clear();
    self.bytes_used = 0;
    StorageChange {
      key: None,
      old_value: None,
      new_value: None,
      did_mutate: true,
    }
  }

  pub fn len(&self) -> usize {
    self.values.len()
  }

  pub fn key(&self, index: usize) -> Option<String> {
    self.key_order.get(index).cloned()
  }
}

#[derive(Debug, Default)]
pub struct WebStorageHub {
  limits: StorageLimits,
  pub local_areas: HashMap<String, Arc<Mutex<StorageArea>>>,
  pub session_areas: HashMap<(SessionNamespaceId, String), Arc<Mutex<StorageArea>>>,
}

impl WebStorageHub {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn set_limits(&mut self, limits: StorageLimits) {
    self.limits = limits;
    let quota_bytes = limits.max_bytes_per_area;
    for area in self.local_areas.values() {
      area.lock().set_quota_bytes(quota_bytes);
    }
    for area in self.session_areas.values() {
      area.lock().set_quota_bytes(quota_bytes);
    }
  }

  fn get_or_create_local_area(&mut self, origin: &str) -> Arc<Mutex<StorageArea>> {
    if let Some(area) = self.local_areas.get(origin) {
      return Arc::clone(area);
    }
    let quota_bytes = self.limits.max_bytes_per_area;
    let area = Arc::new(Mutex::new(StorageArea::new_with_quota(quota_bytes)));
    self.local_areas.insert(origin.to_string(), Arc::clone(&area));
    area
  }

  fn get_or_create_session_area(
    &mut self,
    session: SessionNamespaceId,
    origin: &str,
  ) -> Arc<Mutex<StorageArea>> {
    let key = (session, origin.to_string());
    if let Some(area) = self.session_areas.get(&key) {
      return Arc::clone(area);
    }
    let quota_bytes = self.limits.max_bytes_per_area;
    let area = Arc::new(Mutex::new(StorageArea::new_with_quota(quota_bytes)));
    self.session_areas.insert(key, Arc::clone(&area));
    area
  }
}

thread_local! {
  static DEFAULT_HUB: RefCell<WebStorageHub> = RefCell::new(WebStorageHub::new());
}

pub fn with_default_hub<R>(f: impl FnOnce(&WebStorageHub) -> R) -> R {
  DEFAULT_HUB.with(|hub| {
    let hub = hub.borrow();
    f(&hub)
  })
}

pub fn with_default_hub_mut<R>(f: impl FnOnce(&mut WebStorageHub) -> R) -> R {
  DEFAULT_HUB.with(|hub| {
    let mut hub = hub.borrow_mut();
    f(&mut hub)
  })
}

/// Clears the thread-local default Web Storage hub.
///
/// This is intended for test harnesses (e.g. the WPT runner) that need to ensure a clean
/// `localStorage`/`sessionStorage` state between runs on the same thread.
///
/// Note: FastRender does not currently model multi-realm browsing contexts with persistent storage.
/// The default hub is thread-local for determinism. Clearing it from production code would break the
/// expected storage persistence within a browsing session.
pub fn clear_default_web_storage_hub() {
  with_default_hub_mut(|hub| {
    // Clear underlying `StorageArea`s in case another part of the process still holds `Arc` handles
    // to them (e.g. a dropped-but-not-yet-freed JS realm).
    for area in hub.local_areas.values() {
      area.lock().clear();
    }
    for area in hub.session_areas.values() {
      area.lock().clear();
    }

    hub.local_areas.clear();
    hub.session_areas.clear();

    // Storage event listeners are not yet modelled in FastRender. If/when listener registries are
    // added to `WebStorageHub`, they must be cleared here as well to avoid cross-test leakage.
  });
}

/// Derive a storage origin key from a document URL.
///
/// Supports `http:` / `https:` / `file:`. Other schemes are treated as opaque and return `None`.
pub fn origin_key_from_document_url(url: &str) -> StorageOriginKey {
  let origin = crate::resource::origin_from_url(url)?;
  match origin.scheme() {
    "http" | "https" | "file" => Some(origin.to_string()),
    _ => None,
  }
}

pub fn get_local_area(origin: Option<&str>) -> Arc<Mutex<StorageArea>> {
  let Some(origin) = origin else {
    // Opaque origins get a fresh, non-persistent area on every request.
    let quota_bytes = with_default_hub(|hub| hub.limits.max_bytes_per_area);
    return Arc::new(Mutex::new(StorageArea::new_with_quota(quota_bytes)));
  };
  with_default_hub_mut(|hub| hub.get_or_create_local_area(origin))
}

pub fn get_session_area(session: SessionNamespaceId, origin: Option<&str>) -> Arc<Mutex<StorageArea>> {
  let Some(origin) = origin else {
    let quota_bytes = with_default_hub(|hub| hub.limits.max_bytes_per_area);
    return Arc::new(Mutex::new(StorageArea::new_with_quota(quota_bytes)));
  };
  with_default_hub_mut(|hub| hub.get_or_create_session_area(session, origin))
}

/// Reset the thread-local default Web Storage hub.
///
/// # WARNING
/// This function is **test-only**. It clears all storage areas (and any listener registrations held
/// by the hub) for the current thread.
#[cfg(test)]
pub fn reset_default_web_storage_hub_for_tests() {
  DEFAULT_HUB.with(|hub| {
    *hub.borrow_mut() = WebStorageHub::new();
  });
}

/// Override the per-area storage quota for the thread-local default Web Storage hub.
///
/// # WARNING
/// This function is **test-only**. It mutates global (thread-local) state and should not be used by
/// production code.
#[cfg(test)]
pub fn set_default_storage_quota_for_tests(bytes: usize) {
  with_default_hub_mut(|hub| {
    hub.set_limits(StorageLimits {
      max_bytes_per_area: bytes,
    });
  });
}

#[cfg(test)]
mod tests {
  use super::{
    get_local_area, reset_default_web_storage_hub_for_tests, set_default_storage_quota_for_tests,
    StorageArea, StorageError,
  };

  #[test]
  fn insertion_order_is_stable_on_update() {
    let mut area = StorageArea::new();
    area.set_item("a", "1").unwrap();
    area.set_item("b", "2").unwrap();
    assert_eq!(area.key(0).as_deref(), Some("a"));
    assert_eq!(area.key(1).as_deref(), Some("b"));

    // Updating an existing key must not change its position.
    area.set_item("a", "3").unwrap();
    assert_eq!(area.key(0).as_deref(), Some("a"));
    assert_eq!(area.key(1).as_deref(), Some("b"));
    assert_eq!(area.get_item("a").as_deref(), Some("3"));
  }

  #[test]
  fn remove_and_reinsert_moves_key_to_end() {
    let mut area = StorageArea::new();
    area.set_item("a", "1").unwrap();
    area.set_item("b", "2").unwrap();
    area.remove_item("a");
    area.set_item("a", "3").unwrap();
    assert_eq!(area.key(0).as_deref(), Some("b"));
    assert_eq!(area.key(1).as_deref(), Some("a"));
  }

  #[test]
  fn clear_empties_values_and_order() {
    let mut area = StorageArea::new();
    area.set_item("a", "1").unwrap();
    area.set_item("b", "2").unwrap();
    let change = area.clear();
    assert!(change.did_mutate);
    assert_eq!(area.len(), 0);
    assert_eq!(area.key(0), None);

    // Clearing again is a no-op.
    let change = area.clear();
    assert!(!change.did_mutate);
  }

  #[test]
  fn quota_failure_does_not_mutate() {
    let mut area = StorageArea::new_with_quota(6);
    area.set_item("a", "12").unwrap(); // 1 + 2 = 3 bytes
    assert_eq!(area.bytes_used, 3);

    // Would exceed: existing bytes 3, add new key "b"(1) + value "1234"(4) => 8.
    let err = area.set_item("b", "1234").unwrap_err();
    assert_eq!(err, StorageError::QuotaExceeded);
    assert_eq!(area.len(), 1);
    assert_eq!(area.key(0).as_deref(), Some("a"));
    assert_eq!(area.get_item("b"), None);
    assert_eq!(area.bytes_used, 3);

    // Updating an existing key should also fail without mutating.
    let err = area.set_item("a", "123456").unwrap_err();
    assert_eq!(err, StorageError::QuotaExceeded);
    assert_eq!(area.get_item("a").as_deref(), Some("12"));
    assert_eq!(area.bytes_used, 3);
  }

  #[test]
  fn reset_and_quota_overrides_are_deterministic() {
    reset_default_web_storage_hub_for_tests();

    {
      let area = get_local_area(Some("https://example.com"));
      area.lock().set_item("a", "123").unwrap();
    }

    reset_default_web_storage_hub_for_tests();

    {
      let area = get_local_area(Some("https://example.com"));
      assert_eq!(area.lock().get_item("a"), None);
    }

    // Prove that we can force a tiny quota without allocating huge values.
    reset_default_web_storage_hub_for_tests();

    let area = get_local_area(Some("https://example.com"));
    area.lock().set_item("k", "0123456789").unwrap();

    // Lower the quota enough that repeating a previously-valid set becomes invalid.
    set_default_storage_quota_for_tests(4);

    let err = area.lock().set_item("k", "9876543210").unwrap_err();
    assert_eq!(err, StorageError::QuotaExceeded);
    assert_eq!(area.lock().get_item("k").as_deref(), Some("0123456789"));
  }
}
