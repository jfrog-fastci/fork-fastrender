//! IndexedDB host infrastructure.
//!
//! This module provides a thread-local in-memory backend for IndexedDB. The core state lives in the
//! [`hub`] submodule.

pub mod hub;

pub use hub::{
  clear_default_indexeddb_hub_for_tests, origin_key_from_document_url, with_default_hub,
  with_default_hub_mut, Database, IdbKey, IdbValue, IndexedDbError, IndexedDbHub, IndexedDbLimits,
  KeyPath, ObjectStoreData,
};

