//! Thread-local in-memory IndexedDB backend (databases + object stores + records).
//!
//! FastRender stores IndexedDB state in a thread-local hub, similar to `web_storage`.
//!
//! ## Why thread-local?
//!
//! Rust's test harness may reuse worker threads between tests. A thread-local default hub keeps
//! IndexedDB deterministic and avoids cross-test leakage between unrelated tests that happen to run
//! on the same OS thread.
//!
//! Note: this is an in-process backend. It does not attempt to model disk persistence or cross-tab
//! coordination.

use parking_lot::Mutex;
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

// --- DoS resistance limits ---
//
// These are not spec-defined; they are hard caps intended to keep in-process IndexedDB safe under
// hostile inputs.
pub const DEFAULT_MAX_DBS_PER_ORIGIN: usize = 32;
pub const DEFAULT_MAX_STORES_PER_DB: usize = 128;
pub const DEFAULT_MAX_RECORDS_PER_STORE: usize = 100_000;
pub const DEFAULT_MAX_BYTES_PER_ORIGIN: usize = 32 * 1024 * 1024; // 32MiB (rough accounting)

// Rough per-entity overheads used by `bytes_used` accounting.
const DB_OVERHEAD_BYTES: usize = 128;
const STORE_OVERHEAD_BYTES: usize = 128;
const RECORD_OVERHEAD_BYTES: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexedDbLimits {
  pub max_dbs_per_origin: usize,
  pub max_stores_per_db: usize,
  pub max_records_per_store: usize,
  pub max_bytes_per_origin: usize,
}

impl Default for IndexedDbLimits {
  fn default() -> Self {
    Self {
      max_dbs_per_origin: DEFAULT_MAX_DBS_PER_ORIGIN,
      max_stores_per_db: DEFAULT_MAX_STORES_PER_DB,
      max_records_per_store: DEFAULT_MAX_RECORDS_PER_STORE,
      max_bytes_per_origin: DEFAULT_MAX_BYTES_PER_ORIGIN,
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexedDbError {
  QuotaExceeded,
  DatabaseNotFound,
  ObjectStoreNotFound,
  DatabaseAlreadyExists,
  ObjectStoreAlreadyExists,
  InvalidVersion,
  InvalidKey,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyPath {
  String(String),
  Array(Vec<String>),
}

impl KeyPath {
  fn estimated_bytes(&self) -> usize {
    match self {
      Self::String(s) => s.len(),
      Self::Array(items) => items.iter().map(|s| s.len()).sum(),
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum IdbKey {
  Int(u64),
  String(String),
  Binary(Vec<u8>),
  Array(Vec<IdbKey>),
}

impl IdbKey {
  fn variant_index(&self) -> u8 {
    match self {
      Self::Int(_) => 0,
      Self::String(_) => 1,
      Self::Binary(_) => 2,
      Self::Array(_) => 3,
    }
  }

  fn estimated_bytes(&self) -> usize {
    match self {
      Self::Int(_) => 8,
      Self::String(s) => s.len(),
      Self::Binary(bytes) => bytes.len(),
      Self::Array(items) => items.iter().map(Self::estimated_bytes).sum(),
    }
  }
}

impl Ord for IdbKey {
  fn cmp(&self, other: &Self) -> Ordering {
    let a = self.variant_index();
    let b = other.variant_index();
    if a != b {
      return a.cmp(&b);
    }
    match (self, other) {
      (Self::Int(a), Self::Int(b)) => a.cmp(b),
      (Self::String(a), Self::String(b)) => a.cmp(b),
      (Self::Binary(a), Self::Binary(b)) => a.cmp(b),
      (Self::Array(a), Self::Array(b)) => a.cmp(b),
      _ => Ordering::Equal,
    }
  }
}

impl PartialOrd for IdbKey {
  fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
    Some(self.cmp(other))
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdbValue {
  pub bytes: Vec<u8>,
}

impl IdbValue {
  pub fn new(bytes: Vec<u8>) -> Self {
    Self { bytes }
  }

  fn estimated_bytes(&self) -> usize {
    self.bytes.len()
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectStoreData {
  pub key_path: Option<KeyPath>,
  pub auto_increment: bool,
  pub next_key: u64,
  pub records: BTreeMap<IdbKey, IdbValue>,
}

impl ObjectStoreData {
  pub fn new(key_path: Option<KeyPath>, auto_increment: bool) -> Self {
    Self {
      key_path,
      auto_increment,
      next_key: 1,
      records: BTreeMap::new(),
    }
  }

  fn clear(&mut self) {
    self.records.clear();
    self.next_key = 1;
  }

  fn estimated_bytes_for_store(&self, store_name: &str) -> usize {
    let mut bytes = STORE_OVERHEAD_BYTES.saturating_add(store_name.len());
    if let Some(kp) = &self.key_path {
      bytes = bytes.saturating_add(kp.estimated_bytes());
    }
    for (key, value) in &self.records {
      bytes = bytes.saturating_add(estimated_record_bytes(key, value));
    }
    bytes
  }
}

#[derive(Debug)]
pub struct Database {
  pub name: String,
  pub version: u64,
  // Use a `BTreeMap` for deterministic store listing (e.g. `objectStoreNames`).
  pub stores: BTreeMap<String, ObjectStoreData>,
}

impl Database {
  pub fn new(name: String, version: u64) -> Self {
    Self {
      name,
      version,
      stores: BTreeMap::new(),
    }
  }

  fn clear(&mut self) {
    for store in self.stores.values_mut() {
      store.clear();
    }
    self.stores.clear();
    self.version = 0;
  }

  fn estimated_bytes(&self) -> usize {
    let mut bytes = DB_OVERHEAD_BYTES.saturating_add(self.name.len());
    for (store_name, store) in &self.stores {
      bytes = bytes.saturating_add(store.estimated_bytes_for_store(store_name));
    }
    bytes
  }

  pub fn object_store_names(&self) -> Vec<String> {
    self.stores.keys().cloned().collect()
  }
}

#[derive(Debug, Default)]
struct OriginDbState {
  // Deterministic listing by database name.
  dbs: BTreeMap<String, Arc<Mutex<Database>>>,
  bytes_used: usize,
}

impl OriginDbState {
  fn estimated_db_bytes_delta_for_create(db_name: &str) -> usize {
    DB_OVERHEAD_BYTES.saturating_add(db_name.len())
  }
}

#[derive(Debug, Default)]
pub struct IndexedDbHub {
  limits: IndexedDbLimits,
  origins: HashMap<String, OriginDbState>,
}

impl IndexedDbHub {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn limits(&self) -> IndexedDbLimits {
    self.limits
  }

  pub fn set_limits(&mut self, limits: IndexedDbLimits) {
    self.limits = limits;
  }

  pub fn origin_count(&self) -> usize {
    self.origins.len()
  }

  pub fn open_database(
    &mut self,
    origin_key: &str,
    db_name: &str,
    requested_version: u64,
  ) -> Result<Arc<Mutex<Database>>, IndexedDbError> {
    if requested_version == 0 {
      return Err(IndexedDbError::InvalidVersion);
    }

    let origin_state = self.origins.entry(origin_key.to_string()).or_default();

    if let Some(existing) = origin_state.dbs.get(db_name) {
      let mut db = existing.lock();
      if requested_version < db.version {
        return Err(IndexedDbError::InvalidVersion);
      }
      if requested_version > db.version {
        db.version = requested_version;
      }
      return Ok(Arc::clone(existing));
    }

    if origin_state.dbs.len() >= self.limits.max_dbs_per_origin {
      return Err(IndexedDbError::QuotaExceeded);
    }

    let delta = OriginDbState::estimated_db_bytes_delta_for_create(db_name);
    if origin_state.bytes_used.saturating_add(delta) > self.limits.max_bytes_per_origin {
      return Err(IndexedDbError::QuotaExceeded);
    }

    let db = Arc::new(Mutex::new(Database::new(db_name.to_string(), requested_version)));
    origin_state.bytes_used = origin_state.bytes_used.saturating_add(delta);
    origin_state
      .dbs
      .insert(db_name.to_string(), Arc::clone(&db));
    Ok(db)
  }

  pub fn delete_database(
    &mut self,
    origin_key: &str,
    db_name: &str,
  ) -> Result<bool, IndexedDbError> {
    let Some(origin_state) = self.origins.get_mut(origin_key) else {
      return Ok(false);
    };

    let Some(db) = origin_state.dbs.remove(db_name) else {
      return Ok(false);
    };

    let mut db_guard = db.lock();
    let bytes = db_guard.estimated_bytes();
    db_guard.clear();
    drop(db_guard);

    origin_state.bytes_used = origin_state.bytes_used.saturating_sub(bytes);

    if origin_state.dbs.is_empty() {
      self.origins.remove(origin_key);
    }

    Ok(true)
  }

  pub fn database_names(&self, origin_key: &str) -> Vec<String> {
    self
      .origins
      .get(origin_key)
      .map(|state| state.dbs.keys().cloned().collect())
      .unwrap_or_default()
  }

  pub fn create_object_store(
    &mut self,
    origin_key: &str,
    db_name: &str,
    store_name: &str,
    key_path: Option<KeyPath>,
    auto_increment: bool,
  ) -> Result<(), IndexedDbError> {
    let origin_state = self
      .origins
      .get_mut(origin_key)
      .ok_or(IndexedDbError::DatabaseNotFound)?;
    let db = origin_state
      .dbs
      .get(db_name)
      .cloned()
      .ok_or(IndexedDbError::DatabaseNotFound)?;

    // Enforce store count limit.
    {
      let db_guard = db.lock();
      if db_guard.stores.contains_key(store_name) {
        return Err(IndexedDbError::ObjectStoreAlreadyExists);
      }
      if db_guard.stores.len() >= self.limits.max_stores_per_db {
        return Err(IndexedDbError::QuotaExceeded);
      }
    }

    // Enforce rough per-origin byte quota before mutating.
    let kp_bytes = key_path.as_ref().map_or(0, KeyPath::estimated_bytes);
    let delta = STORE_OVERHEAD_BYTES
      .saturating_add(store_name.len())
      .saturating_add(kp_bytes);
    if origin_state.bytes_used.saturating_add(delta) > self.limits.max_bytes_per_origin {
      return Err(IndexedDbError::QuotaExceeded);
    }

    // Commit mutation.
    {
      let mut db_guard = db.lock();
      // Re-check in case another handle mutated the DB between the checks above (still deterministic;
      // the hub is thread-local, but callers may hold `Arc<Mutex<Database>>` handles).
      if db_guard.stores.contains_key(store_name) {
        return Err(IndexedDbError::ObjectStoreAlreadyExists);
      }
      if db_guard.stores.len() >= self.limits.max_stores_per_db {
        return Err(IndexedDbError::QuotaExceeded);
      }
      db_guard
        .stores
        .insert(store_name.to_string(), ObjectStoreData::new(key_path, auto_increment));
    }

    origin_state.bytes_used = origin_state.bytes_used.saturating_add(delta);
    Ok(())
  }

  pub fn delete_object_store(
    &mut self,
    origin_key: &str,
    db_name: &str,
    store_name: &str,
  ) -> Result<(), IndexedDbError> {
    let origin_state = self
      .origins
      .get_mut(origin_key)
      .ok_or(IndexedDbError::DatabaseNotFound)?;
    let db = origin_state
      .dbs
      .get(db_name)
      .cloned()
      .ok_or(IndexedDbError::DatabaseNotFound)?;

    let removed = {
      let mut db_guard = db.lock();
      db_guard
        .stores
        .remove(store_name)
        .ok_or(IndexedDbError::ObjectStoreNotFound)?
    };

    // Approximate byte accounting by scanning the removed store.
    let store_bytes = removed.estimated_bytes_for_store(store_name);
    origin_state.bytes_used = origin_state.bytes_used.saturating_sub(store_bytes);
    Ok(())
  }

  pub fn put_record(
    &mut self,
    origin_key: &str,
    db_name: &str,
    store_name: &str,
    key: Option<IdbKey>,
    value: IdbValue,
  ) -> Result<IdbKey, IndexedDbError> {
    let origin_state = self
      .origins
      .get_mut(origin_key)
      .ok_or(IndexedDbError::DatabaseNotFound)?;
    let db = origin_state
      .dbs
      .get(db_name)
      .cloned()
      .ok_or(IndexedDbError::DatabaseNotFound)?;

    let mut db_guard = db.lock();
    let store = db_guard
      .stores
      .get_mut(store_name)
      .ok_or(IndexedDbError::ObjectStoreNotFound)?;

    let key = match key {
      Some(k) => k,
      None => {
        if !store.auto_increment {
          return Err(IndexedDbError::InvalidKey);
        }
        IdbKey::Int(store.next_key)
      }
    };

    let replacing = store.records.contains_key(&key);
    if !replacing && store.records.len() >= self.limits.max_records_per_store {
      return Err(IndexedDbError::QuotaExceeded);
    }

    let old_bytes = store
      .records
      .get(&key)
      .map(|old_value| estimated_record_bytes(&key, old_value))
      .unwrap_or(0);
    let new_bytes = estimated_record_bytes(&key, &value);
    let delta = new_bytes.saturating_sub(old_bytes);

    if origin_state.bytes_used.saturating_add(delta) > self.limits.max_bytes_per_origin {
      return Err(IndexedDbError::QuotaExceeded);
    }

    store.records.insert(key.clone(), value);
    if store.auto_increment {
      if let IdbKey::Int(n) = &key {
        // Spec nuance: auto-increment uses a "key generator" that may advance even if a larger key
        // is manually provided. For this backend we only advance on generated keys or if the caller
        // explicitly wrote the same integer key.
        store.next_key = store.next_key.max(n.saturating_add(1));
      }
    }

    origin_state.bytes_used = origin_state.bytes_used.saturating_add(delta);
    Ok(key)
  }

  pub fn get_record(
    &self,
    origin_key: &str,
    db_name: &str,
    store_name: &str,
    key: &IdbKey,
  ) -> Result<Option<IdbValue>, IndexedDbError> {
    let origin_state = self
      .origins
      .get(origin_key)
      .ok_or(IndexedDbError::DatabaseNotFound)?;
    let db = origin_state
      .dbs
      .get(db_name)
      .cloned()
      .ok_or(IndexedDbError::DatabaseNotFound)?;
    let db_guard = db.lock();
    let store = db_guard
      .stores
      .get(store_name)
      .ok_or(IndexedDbError::ObjectStoreNotFound)?;
    Ok(store.records.get(key).cloned())
  }

  fn clear_all(&mut self) {
    // Clear underlying `Database` allocations in case another part of the process still holds
    // `Arc<Mutex<Database>>` handles (e.g. a still-alive JS realm).
    for origin_state in self.origins.values_mut() {
      for db in origin_state.dbs.values() {
        db.lock().clear();
      }
      origin_state.dbs.clear();
      origin_state.bytes_used = 0;
    }
    self.origins.clear();
  }
}

fn estimated_record_bytes(key: &IdbKey, value: &IdbValue) -> usize {
  RECORD_OVERHEAD_BYTES
    .saturating_add(key.estimated_bytes())
    .saturating_add(value.estimated_bytes())
}

thread_local! {
  static DEFAULT_HUB: RefCell<IndexedDbHub> = RefCell::new(IndexedDbHub::new());
}

pub fn with_default_hub<R>(f: impl FnOnce(&IndexedDbHub) -> R) -> R {
  DEFAULT_HUB.with(|hub| {
    let hub = hub.borrow();
    f(&hub)
  })
}

pub fn with_default_hub_mut<R>(f: impl FnOnce(&mut IndexedDbHub) -> R) -> R {
  DEFAULT_HUB.with(|hub| {
    let mut hub = hub.borrow_mut();
    f(&mut hub)
  })
}

/// Derive an IndexedDB origin key from a document URL.
///
/// Only `http:` / `https:` documents get a persistent origin key. Everything else is treated as an
/// opaque origin and is mapped to `opaque:<window_id>`, ensuring:
/// - the key is stable within a single `WindowRealm`/window
/// - the key is not shared across realms for opaque documents (matching `window.origin === "null"`)
pub fn origin_key_from_document_url(document_url: &str, window_id: u64) -> String {
  let Some(origin) = crate::resource::origin_from_url(document_url) else {
    return format!("opaque:{window_id}");
  };
  if !matches!(origin.scheme(), "http" | "https") {
    return format!("opaque:{window_id}");
  }
  if origin.host().is_none() {
    return format!("opaque:{window_id}");
  }
  origin.to_string()
}

/// Clears the thread-local default IndexedDB hub.
///
/// This is intended for test harnesses that need a clean IndexedDB state between runs on the same
/// thread. It is safe to call even if other parts of the process still hold `Arc<Mutex<Database>>`
/// handles; those handles are cleared in place.
pub fn clear_default_indexeddb_hub_for_tests() {
  with_default_hub_mut(|hub| hub.clear_all());
}

#[cfg(test)]
mod tests {
  use super::{
    clear_default_indexeddb_hub_for_tests, origin_key_from_document_url, with_default_hub,
    with_default_hub_mut, IdbValue, IndexedDbError, IndexedDbHub,
  };

  #[test]
  fn origin_key_maps_http_https_and_opaque_documents() {
    assert_eq!(
      origin_key_from_document_url("https://example.com/path", 7),
      "https://example.com:443"
    );
    assert_eq!(
      origin_key_from_document_url("http://example.com/path", 7),
      "http://example.com:80"
    );
    assert_eq!(
      origin_key_from_document_url("about:blank", 7),
      "opaque:7"
    );
    assert_eq!(
      origin_key_from_document_url("data:text/plain,hello", 7),
      "opaque:7"
    );
  }

  #[test]
  fn create_and_delete_databases_and_object_stores() {
    clear_default_indexeddb_hub_for_tests();
    // Ensure this test leaves the thread-local hub in a clean state even if it fails (the Rust test
    // harness may reuse worker threads between tests).
    struct ResetGuard;
    impl Drop for ResetGuard {
      fn drop(&mut self) {
        clear_default_indexeddb_hub_for_tests();
      }
    }
    let _guard = ResetGuard;

    let origin = "https://example.com:443";

    let db = with_default_hub_mut(|hub| hub.open_database(origin, "mydb", 1)).unwrap();
    with_default_hub_mut(|hub| {
      hub
        .create_object_store(origin, "mydb", "store_a", None, false)
        .unwrap();
      hub
        .create_object_store(origin, "mydb", "store_b", None, false)
        .unwrap();
    });

    assert_eq!(db.lock().object_store_names(), vec!["store_a", "store_b"]);
    assert_eq!(with_default_hub(|hub| hub.database_names(origin)), vec!["mydb"]);

    with_default_hub_mut(|hub| hub.delete_object_store(origin, "mydb", "store_a")).unwrap();
    assert_eq!(db.lock().object_store_names(), vec!["store_b"]);

    let did_delete = with_default_hub_mut(|hub| hub.delete_database(origin, "mydb")).unwrap();
    assert!(did_delete);

    assert_eq!(with_default_hub(|hub| hub.database_names(origin)), Vec::<String>::new());
  }

  #[test]
  fn clear_is_safe_with_live_database_handles() {
    clear_default_indexeddb_hub_for_tests();
    struct ResetGuard;
    impl Drop for ResetGuard {
      fn drop(&mut self) {
        clear_default_indexeddb_hub_for_tests();
      }
    }
    let _guard = ResetGuard;

    let origin = "https://example.com:443";

    let db = with_default_hub_mut(|hub| hub.open_database(origin, "mydb", 1)).unwrap();
    with_default_hub_mut(|hub| {
      hub
        .create_object_store(origin, "mydb", "store", None, true)
        .unwrap();
      hub
        .put_record(
          origin,
          "mydb",
          "store",
          None,
          IdbValue::new(b"hello".to_vec()),
        )
        .unwrap();
    });
    assert_eq!(db.lock().object_store_names(), vec!["store"]);

    // Clear the hub while a database handle is still live.
    clear_default_indexeddb_hub_for_tests();

    // The live handle should have been cleared in-place.
    assert_eq!(db.lock().object_store_names(), Vec::<String>::new());
    assert_eq!(db.lock().version, 0);
    assert_eq!(with_default_hub(|hub| hub.origin_count()), 0);

    // Re-opening should allocate a fresh DB.
    let db2 = with_default_hub_mut(|hub| hub.open_database(origin, "mydb", 1)).unwrap();
    assert_eq!(db2.lock().version, 1);
    assert_eq!(db2.lock().object_store_names(), Vec::<String>::new());
  }

  #[test]
  fn limits_are_enforced() {
    let mut hub = IndexedDbHub::new();
    hub.set_limits(super::IndexedDbLimits {
      max_dbs_per_origin: 1,
      max_stores_per_db: 1,
      max_records_per_store: 1,
      max_bytes_per_origin: 1024,
    });

    let origin = "https://example.com:443";

    hub.open_database(origin, "db1", 1).unwrap();
    let err = hub.open_database(origin, "db2", 1).unwrap_err();
    assert_eq!(err, IndexedDbError::QuotaExceeded);

    hub.create_object_store(origin, "db1", "s1", None, false)
      .unwrap();
    let err = hub
      .create_object_store(origin, "db1", "s2", None, false)
      .unwrap_err();
    assert_eq!(err, IndexedDbError::QuotaExceeded);
  }
}
