//! Serde helpers for deterministic debug output.
//!
//! Many analysis result structs store intermediate data in `HashMap`s for
//! performance. When serializing (e.g. in debug builds), we want stable output
//! across runs, so we serialize maps in sorted-key order.
//!
//! This module is only compiled when the crate is built with `feature = "serde"`.

#![cfg(feature = "serde")]

use ahash::HashMap;
use serde::ser::{Serialize, SerializeMap, Serializer};

pub(crate) fn serialize_hashmap_sorted<K, V, S>(
  map: &HashMap<K, V>,
  serializer: S,
) -> Result<S::Ok, S::Error>
where
  K: Ord + Serialize,
  V: Serialize,
  S: Serializer,
{
  let mut entries: Vec<_> = map.iter().collect();
  entries.sort_by(|(a, _), (b, _)| a.cmp(b));

  let mut out = serializer.serialize_map(Some(entries.len()))?;
  for (k, v) in entries {
    out.serialize_entry(k, v)?;
  }
  out.end()
}

