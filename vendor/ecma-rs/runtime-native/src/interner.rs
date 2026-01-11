use std::sync::Arc;

use ahash::AHashMap;
use once_cell::sync::Lazy;
use parking_lot::RwLock;

use crate::abi::InternedId;
use crate::abi::StringRef;
use crate::trap;

struct Interner {
  ids_by_bytes: AHashMap<Arc<[u8]>, InternedId>,
  bytes_by_id: Vec<Arc<[u8]>>,
}

impl Interner {
  fn new() -> Self {
    Self {
      ids_by_bytes: AHashMap::new(),
      bytes_by_id: Vec::new(),
    }
  }
}

static INTERNER: Lazy<RwLock<Interner>> = Lazy::new(|| RwLock::new(Interner::new()));

/// Intern a UTF-8 byte string.
///
/// Milestone-1 behavior: interned strings are kept alive for the lifetime of the
/// process (no GC / interner eviction yet).
pub(crate) fn intern(bytes: &[u8]) -> InternedId {
  {
    let interner = INTERNER.read();
    if let Some(id) = interner.ids_by_bytes.get(bytes) {
      return *id;
    }
  }

  let mut interner = INTERNER.write();
  if let Some(id) = interner.ids_by_bytes.get(bytes) {
    return *id;
  }

  let id_u32 = u32::try_from(interner.bytes_by_id.len())
    .unwrap_or_else(|_| trap::rt_trap_invalid_arg("too many interned strings"));
  let id = InternedId(id_u32);

  let stored: Arc<[u8]> = Arc::from(bytes);
  interner.bytes_by_id.push(stored.clone());
  interner.ids_by_bytes.insert(stored, id);

  id
}

/// Lookup interned bytes by ID.
pub(crate) fn lookup(id: InternedId) -> StringRef {
  let interner = INTERNER.read();
  let idx = usize::try_from(id.0)
    .unwrap_or_else(|_| trap::rt_trap_invalid_arg("invalid InternedId"));

  let bytes = interner
    .bytes_by_id
    .get(idx)
    .unwrap_or_else(|| trap::rt_trap_invalid_arg("invalid InternedId"));
  StringRef {
    ptr: bytes.as_ptr(),
    len: bytes.len(),
  }
}
