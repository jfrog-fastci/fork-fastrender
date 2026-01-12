use crate::api::{FileId, FileKey};
use ahash::AHashMap;

const FALLBACK_START: u32 = 1 << 31;
const RESERVED_FILE_ID: u32 = u32::MAX;

const STABLE_HASH_OFFSET: u64 = 0xcbf29ce484222325;
const STABLE_HASH_PRIME: u64 = 0x100000001b3;

/// Distinguish between user-provided source files and library inputs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum FileOrigin {
  Source,
  Lib,
}

impl FileOrigin {
  fn stable_discriminant(&self) -> u8 {
    match self {
      FileOrigin::Source => 0,
      FileOrigin::Lib => 1,
    }
  }
}

fn preferred_file_id(key: &FileKey) -> Option<u32> {
  let name = key.as_str();
  let remainder = name.strip_prefix("file")?;
  let stripped = remainder
    .strip_suffix(".ts")
    .or_else(|| remainder.strip_suffix(".tsx"))?;
  let preferred = stripped.parse::<u32>().ok()?;
  if preferred == RESERVED_FILE_ID {
    return None;
  }
  Some(preferred)
}

/// Deterministic hash used for fallback [`FileId`] allocation.
///
/// Rust's `std::collections::hash_map::DefaultHasher` is explicitly *not* a
/// cross-version stability contract, so using it for `FileId` generation breaks
/// reproducibility and incremental caching.
///
/// This uses a 64-bit FNV-1a hash with the same parameters as `hir-js`'s stable
/// hasher (offset basis `0xcbf29ce484222325`, prime `0x100000001b3`). The input
/// stream is:
/// - [`FileOrigin`] discriminant (`u8`)
/// - deterministic collision salt (`u64`, little-endian bytes)
/// - file key byte length (`u32`, little-endian bytes)
/// - file key bytes (`&str`, UTF-8)
///
/// The final value is folded to `u32` by XORing the high and low 32-bit halves,
/// mirroring other stable IDs in the toolchain.
fn stable_hash_u32(key: &FileKey, origin: FileOrigin, salt: u64) -> u32 {
  #[derive(Clone, Copy)]
  struct StableHasher(u64);

  impl StableHasher {
    fn new() -> Self {
      Self(STABLE_HASH_OFFSET)
    }

    fn write_bytes(&mut self, bytes: &[u8]) {
      for byte in bytes {
        self.0 ^= *byte as u64;
        self.0 = self.0.wrapping_mul(STABLE_HASH_PRIME);
      }
    }

    fn write_u8(&mut self, value: u8) {
      self.write_bytes(&[value]);
    }

    fn write_u32(&mut self, value: u32) {
      self.write_bytes(&value.to_le_bytes());
    }

    fn write_u64(&mut self, value: u64) {
      self.write_bytes(&value.to_le_bytes());
    }

    fn write_str(&mut self, value: &str) {
      self.write_u32(value.len() as u32);
      self.write_bytes(value.as_bytes());
    }

    fn finish_u32(self) -> u32 {
      (self.0 ^ (self.0 >> 32)) as u32
    }
  }

  let mut hasher = StableHasher::new();
  hasher.write_u8(origin.stable_discriminant());
  hasher.write_u64(salt);
  hasher.write_str(key.as_str());
  hasher.finish_u32()
}

/// Deterministic registry mapping [`FileKey`]s to [`FileId`]s.
///
/// IDs are stable for the lifetime of the registry and derived purely from the
/// key to avoid order-dependent allocation. Keys matching the `file{N}.ts` or
/// `file{N}.tsx` pattern reserve `FileId(N)` when available. `FileId(u32::MAX)` is
/// reserved for synthetic namespaces (for example, packed HIR IDs) and will
/// always be remapped into the fallback range.
///
/// All other keys use a stable hash-based fallback in a high numeric range to
/// avoid colliding with small test IDs.
#[derive(Clone, Debug, Default)]
pub(crate) struct FileRegistry {
  keys: AHashMap<FileKey, FileRegistryEntry>,
  id_to_key: AHashMap<FileId, FileKey>,
  id_to_origin: AHashMap<FileId, FileOrigin>,
}

#[derive(Clone, Debug, Default)]
struct FileRegistryEntry {
  source: Option<FileId>,
  lib: Option<FileId>,
}

impl FileRegistry {
  pub(crate) fn new() -> Self {
    Self::default()
  }

  pub(crate) fn lookup_key(&self, id: FileId) -> Option<FileKey> {
    self.id_to_key.get(&id).cloned()
  }

  pub(crate) fn lookup_origin(&self, id: FileId) -> Option<FileOrigin> {
    self.id_to_origin.get(&id).copied()
  }

  pub(crate) fn lookup_id(&self, key: &FileKey) -> Option<FileId> {
    let entry = self.keys.get(key)?;
    entry.source.or(entry.lib)
  }

  pub(crate) fn lookup_id_with_origin(&self, key: &FileKey, origin: FileOrigin) -> Option<FileId> {
    let entry = self.keys.get(key)?;
    match origin {
      FileOrigin::Source => entry.source,
      FileOrigin::Lib => entry.lib,
    }
  }

  pub(crate) fn ids_for_key(&self, key: &FileKey) -> Vec<FileId> {
    let entry = self.keys.get(key);
    let mut ids = Vec::new();
    if let Some(entry) = entry {
      if let Some(id) = entry.source {
        ids.push(id);
      }
      if let Some(id) = entry.lib {
        ids.push(id);
      }
    }
    ids
  }

  pub(crate) fn intern(&mut self, key: &FileKey, origin: FileOrigin) -> FileId {
    if let Some(id) = self.lookup_id_with_origin(key, origin) {
      return id;
    }

    let id = match origin {
      FileOrigin::Source => {
        if let Some(preferred) = preferred_file_id(key) {
          let preferred_id = FileId(preferred);
          if !self.id_to_key.contains_key(&preferred_id) {
            preferred_id
          } else {
            self.allocate_fallback(key, origin)
          }
        } else {
          self.allocate_fallback(key, origin)
        }
      }
      FileOrigin::Lib => self.allocate_fallback(key, origin),
    };
    debug_assert_ne!(id, FileId(RESERVED_FILE_ID));

    let entry = self.keys.entry(key.clone()).or_default();
    match origin {
      FileOrigin::Source => entry.source = Some(id),
      FileOrigin::Lib => entry.lib = Some(id),
    }
    self.id_to_key.insert(id, key.clone());
    self.id_to_origin.insert(id, origin);
    id
  }

  fn allocate_fallback(&mut self, key: &FileKey, origin: FileOrigin) -> FileId {
    let mut salt = 0u64;
    loop {
      let raw = stable_hash_u32(key, origin, salt);
      let mut candidate = raw | FALLBACK_START;
      if candidate == u32::MAX {
        candidate = FALLBACK_START;
      }
      if !self.id_to_key.contains_key(&FileId(candidate)) {
        return FileId(candidate);
      }
      salt = salt.wrapping_add(1);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn deterministic_across_interning_order() {
    let file0 = FileKey::new("file0.ts");
    let file10 = FileKey::new("file10.ts");
    let other = FileKey::new("x.ts");

    let mut registry_a = FileRegistry::new();
    registry_a.intern(&file10, FileOrigin::Source);
    registry_a.intern(&other, FileOrigin::Source);
    registry_a.intern(&file0, FileOrigin::Source);

    let mut registry_b = FileRegistry::new();
    registry_b.intern(&file0, FileOrigin::Source);
    registry_b.intern(&file10, FileOrigin::Source);
    registry_b.intern(&other, FileOrigin::Source);

    assert_eq!(registry_a.lookup_id(&file0), Some(FileId(0)));
    assert_eq!(registry_b.lookup_id(&file0), Some(FileId(0)));
    assert_eq!(registry_a.lookup_id(&file10), Some(FileId(10)));
    assert_eq!(registry_b.lookup_id(&file10), Some(FileId(10)));

    let other_id_a = registry_a.lookup_id(&other).expect("other file id");
    let other_id_b = registry_b.lookup_id(&other).expect("other file id");
    assert_eq!(other_id_a, other_id_b);
    assert_eq!(registry_a.lookup_key(other_id_a), Some(other.clone()));
    assert_eq!(registry_b.lookup_key(other_id_b), Some(other));
  }

  #[test]
  fn origin_distinguishes_colliding_keys() {
    let key = FileKey::new("lib:lib.es5.d.ts");
    let mut registry_a = FileRegistry::new();
    let source_a = registry_a.intern(&key, FileOrigin::Source);
    let lib_a = registry_a.intern(&key, FileOrigin::Lib);
    assert_ne!(source_a, lib_a);
    assert_eq!(registry_a.lookup_id(&key), Some(source_a));
    assert_eq!(registry_a.ids_for_key(&key), vec![source_a, lib_a]);

    // Swapping interning order must not change which ID is assigned to which
    // origin. This relies on the origin discriminant being included in the
    // stable hash, rather than only being handled by collision resolution.
    let mut registry_b = FileRegistry::new();
    let lib_b = registry_b.intern(&key, FileOrigin::Lib);
    let source_b = registry_b.intern(&key, FileOrigin::Source);
    assert_eq!(source_a, source_b);
    assert_eq!(lib_a, lib_b);
  }

  #[test]
  fn reserved_file_id_is_never_allocated() {
    let reserved = FileKey::new("file4294967295.ts");
    let other = FileKey::new("x.ts");

    let mut registry_a = FileRegistry::new();
    registry_a.intern(&reserved, FileOrigin::Source);
    registry_a.intern(&other, FileOrigin::Source);

    let mut registry_b = FileRegistry::new();
    registry_b.intern(&other, FileOrigin::Source);
    registry_b.intern(&reserved, FileOrigin::Source);

    let reserved_id_a = registry_a.lookup_id(&reserved).expect("reserved file id");
    let reserved_id_b = registry_b.lookup_id(&reserved).expect("reserved file id");
    assert_ne!(reserved_id_a, FileId(RESERVED_FILE_ID));
    assert_ne!(reserved_id_b, FileId(RESERVED_FILE_ID));
    assert_eq!(reserved_id_a, reserved_id_b);
  }

  #[test]
  fn fallback_id_is_stable_and_versioned() {
    let key = FileKey::new("x.ts");

    let mut registry_a = FileRegistry::new();
    registry_a.intern(&FileKey::new("file0.ts"), FileOrigin::Source);
    let id_a = registry_a.intern(&key, FileOrigin::Source);

    let mut registry_b = FileRegistry::new();
    let id_b = registry_b.intern(&key, FileOrigin::Source);
    registry_b.intern(&FileKey::new("file0.ts"), FileOrigin::Source);

    assert_eq!(id_a, id_b);
    // Pinned numeric value to ensure the fallback allocation stays stable across
    // Rust versions. If this changes, it is likely a deliberate hashing scheme
    // change and should be accompanied by an incremental cache version bump.
    assert_eq!(id_a, FileId(0xC105_EF2E));
  }

  #[test]
  fn hash_derived_file_ids_are_stable() {
    // These values are derived from the stable hashing algorithm in
    // `stable_hash_u32` (FNV-1a 64-bit, folded to `u32`, then OR'd with
    // `FALLBACK_START`). This test ensures we don't accidentally change the
    // hashing inputs or algorithm.
    let mut registry = FileRegistry::new();

    let source_key = FileKey::new("/a.ts");
    let lib_key = FileKey::new("lib:lib.es5.d.ts");

    assert_eq!(
      registry.intern(&source_key, FileOrigin::Source),
      FileId(0xC4F3_04E4)
    );
    assert_eq!(
      registry.intern(&lib_key, FileOrigin::Lib),
      FileId(0xEE4D_1A26)
    );
  }
}
