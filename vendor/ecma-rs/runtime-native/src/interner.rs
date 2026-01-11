use std::hash::BuildHasher;
use std::hash::Hasher;
use std::ptr;
use std::sync::Arc;

use ahash::AHashMap;
use ahash::RandomState;
use arc_swap::ArcSwap;
use once_cell::sync::Lazy;

use crate::abi::InternedId;
use crate::abi::StringRef;
use crate::gc;
use crate::gc::ObjHeader;
use crate::gc::TypeDescriptor;
use crate::sync::GcAwareMutex;

/// Interned strings are represented by stable [`InternedId`]s.
///
/// # ID lifetime
/// `InternedId`s are **monotonically allocated and never reused**. If a non-pinned interned string
/// is reclaimed (via GC + weak interner pruning), its ID becomes invalid and will never be
/// reassigned to a different string.
///
/// Re-interning the same bytes after reclamation yields a **new** `InternedId` (i.e. this interner
/// is "weak" and does not guarantee that equal strings always map to the same ID forever).
///
/// This matches the design goal in `EXEC.plan.md §5.3`: pin common strings (keywords/property
/// names) permanently, but allow opportunistic reclamation of unused interned strings.

// -----------------------------------------------------------------------------
// Hashing
// -----------------------------------------------------------------------------

static HASH_STATE: Lazy<RandomState> = Lazy::new(RandomState::new);

#[inline]
fn hash_bytes(bytes: &[u8]) -> u64 {
  let mut h = HASH_STATE.build_hasher();
  h.write(bytes);
  h.finish()
}

// -----------------------------------------------------------------------------
// GC-backed string storage
// -----------------------------------------------------------------------------

// Layout of an interned GC string object:
//   ObjHeader
//   usize len
//   u8 bytes[..] (inline, capacity is a bucketed size)
const INTERNED_PREFIX_SIZE: usize = std::mem::size_of::<ObjHeader>() + std::mem::size_of::<usize>();

// Pointer offsets for interned strings: no GC pointer fields.
//
// `TypeDescriptor` stores offsets as `u32` byte offsets from the object base pointer.
static NO_PTR_OFFSETS: [u32; 0] = [];

/// We allocate interned strings as *fixed-size* GC objects by rounding their inline byte storage up
/// to a bucketed capacity (powers of two).
///
/// This keeps the GC's [`TypeDescriptor`] model happy (it needs a static object size), while still
/// letting us store arbitrary-length strings without owning non-GC memory.
fn interned_object_size_for_len(len: usize) -> usize {
  // Avoid next_power_of_two(0) == 0 corner case by keeping the empty string at capacity 0.
  let cap = if len == 0 {
    0
  } else {
    len.next_power_of_two().max(16)
  };

  let size = INTERNED_PREFIX_SIZE
    .checked_add(cap)
    .unwrap_or_else(|| crate::trap::rt_trap_invalid_arg("interned string size overflow"));
  gc::align_up(size, std::mem::align_of::<ObjHeader>())
}

static INTERNED_DESC_CACHE: Lazy<GcAwareMutex<AHashMap<usize, &'static TypeDescriptor>>> =
  Lazy::new(|| GcAwareMutex::new(AHashMap::new()));

fn interned_desc_for_size(size: usize) -> &'static TypeDescriptor {
  // Fast uncontended path.
  if let Some(existing) = INTERNED_DESC_CACHE.try_lock().and_then(|m| m.get(&size).copied()) {
    return existing;
  }

  let mut cache = INTERNED_DESC_CACHE.lock();
  if let Some(existing) = cache.get(&size).copied() {
    return existing;
  }

  let desc = Box::leak(Box::new(TypeDescriptor::new(size, &NO_PTR_OFFSETS)));
  cache.insert(size, desc);
  desc
}

static INTERN_HEAP: Lazy<GcAwareMutex<gc::GcHeap>> =
  Lazy::new(|| GcAwareMutex::new(gc::GcHeap::with_nursery_size(1024 * 1024)));

fn alloc_interned_object(bytes: &[u8]) -> (gc::WeakHandle, usize) {
  let len = bytes.len();
  let size = interned_object_size_for_len(len);
  let desc = interned_desc_for_size(size);

  let mut heap = INTERN_HEAP.lock();
  let obj = heap.alloc_old(desc);

  // SAFETY: `obj` points to a valid allocation of `desc.size` bytes.
  unsafe {
    // Write length.
    let len_slot = obj.add(std::mem::size_of::<ObjHeader>()) as *mut usize;
    *len_slot = len;

    // Copy bytes.
    if len != 0 {
      let dst = obj.add(INTERNED_PREFIX_SIZE);
      ptr::copy_nonoverlapping(bytes.as_ptr(), dst, len);
    }
  }

  // Interner entries keep only a weak reference to the GC object.
  let handle = crate::gc::weak::global_weak_add(obj);
  (handle, len)
}

#[inline]
fn bytes_from_obj<'a>(obj: *mut u8, len: usize) -> &'a [u8] {
  // Safety: object layout is stable (`ObjHeader + len + bytes`), and the caller ensures the object
  // is alive for the duration of the borrow (e.g. stop-the-world GC contract).
  unsafe { std::slice::from_raw_parts(obj.add(INTERNED_PREFIX_SIZE), len) }
}

// -----------------------------------------------------------------------------
// Interner tables (lock-free reads via ArcSwap)
// -----------------------------------------------------------------------------

#[derive(Clone)]
enum Entry {
  /// Slot never assigned (should not occur for valid IDs).
  Dead,
  /// Permanently pinned bytes owned by the interner.
  Pinned { bytes: Arc<[u8]> },
  /// Weak reference to a GC object containing the bytes.
  Weak { len: usize, handle: gc::WeakHandle },
}

#[derive(Clone, Default)]
struct Tables {
  /// Hash buckets → candidate IDs.
  ids_by_hash: AHashMap<u64, Vec<InternedId>>,
  /// Stable ID → entry.
  entries: Vec<Entry>,
}

impl Tables {
  fn find_in_bucket(&self, hash: u64, bytes: &[u8]) -> Option<InternedId> {
    let candidates = self.ids_by_hash.get(&hash)?;
    for &id in candidates {
      let idx = usize::try_from(id.0).ok()?;
      let Some(entry) = self.entries.get(idx) else {
        continue;
      };

      match entry {
        Entry::Pinned { bytes: b } => {
          if &**b == bytes {
            return Some(id);
          }
        }
        Entry::Weak { len, handle } => {
          let Some(obj) = crate::gc::weak::global_weak_get(*handle) else {
            continue;
          };
          if *len != bytes.len() {
            continue;
          }
          if bytes_from_obj(obj, *len) == bytes {
            return Some(id);
          }
        }
        Entry::Dead => {}
      }
    }
    None
  }
}

struct GlobalInterner {
  tables: ArcSwap<Tables>,
  /// Serialize writers/pruners. Readers use `tables` lock-free.
  write_lock: GcAwareMutex<()>,
}

impl GlobalInterner {
  fn new() -> Self {
    // Register weak cleanup hook once the interner is instantiated.
    gc::register_weak_cleanup(interner_weak_cleanup);
    Self {
      tables: ArcSwap::from_pointee(Tables::default()),
      write_lock: GcAwareMutex::new(()),
    }
  }
}

static INTERNER: Lazy<GlobalInterner> = Lazy::new(GlobalInterner::new);

fn interner_weak_cleanup(_heap: &mut gc::GcHeap) {
  // Avoid deadlocking the GC if a mutator is mid-intern. If we can't acquire the write lock, we'll
  // try again on the next GC cycle or the next explicit prune call.
  let Some(_guard) = INTERNER.write_lock.try_lock() else {
    return;
  };

  let current = INTERNER.tables.load_full();
  let mut next = (*current).clone();
  let mut changed = false;

  // Mark dead weak entries and release their global weak-handle slots.
  for entry in &mut next.entries {
    let Entry::Weak { handle, .. } = entry else {
      continue;
    };
    if crate::gc::weak::global_weak_get(*handle).is_some() {
      continue;
    }
    crate::gc::weak::global_weak_remove(*handle);
    *entry = Entry::Dead;
    changed = true;
  }

  if !changed {
    return;
  }

  // Prune dead IDs out of hash buckets.
  next.ids_by_hash.retain(|_, ids| {
    ids.retain(|id| {
      let idx = match usize::try_from(id.0) {
        Ok(v) => v,
        Err(_) => return false,
      };
      match next.entries.get(idx) {
        Some(Entry::Pinned { .. } | Entry::Weak { .. }) => true,
        _ => false,
      }
    });
    !ids.is_empty()
  });

  INTERNER.tables.store(Arc::new(next));
}

// -----------------------------------------------------------------------------
// Public (crate-internal) API
// -----------------------------------------------------------------------------

/// Intern a UTF-8 byte string.
///
/// Thread-safe and optimized for concurrent reads.
pub(crate) fn intern(bytes: &[u8]) -> InternedId {
  let hash = hash_bytes(bytes);

  {
    let tables = INTERNER.tables.load_full();
    if let Some(id) = tables.find_in_bucket(hash, bytes) {
      return id;
    }
  }

  let _guard = INTERNER.write_lock.lock();

  let tables = INTERNER.tables.load_full();
  if let Some(id) = tables.find_in_bucket(hash, bytes) {
    return id;
  }

  let id_u32 = u32::try_from(tables.entries.len()).expect("too many interned strings");
  let id = InternedId(id_u32);

  let (handle, len) = alloc_interned_object(bytes);

  let mut next = (*tables).clone();
  next.entries.push(Entry::Weak { len, handle });
  next.ids_by_hash.entry(hash).or_default().push(id);

  INTERNER.tables.store(Arc::new(next));

  id
}

/// Permanently pin an interned string so it survives GC sweeps and interner pruning.
///
/// Pinned strings are stored as owned `Arc<[u8]>` values and are treated as permanent roots within
/// the interner (they never become weak/collectible again).
pub(crate) fn pin_interned(id: InternedId) {
  let _guard = INTERNER.write_lock.lock();

  let tables = INTERNER.tables.load_full();
  let idx = usize::try_from(id.0).unwrap_or(usize::MAX);
  let Some(entry) = tables.entries.get(idx) else {
    return;
  };

  // Idempotent.
  if matches!(entry, Entry::Pinned { .. }) {
    return;
  }

  let Entry::Weak { handle, len } = entry else {
    return;
  };

  let Some(obj) = crate::gc::weak::global_weak_get(*handle) else {
    return;
  };

  let bytes = bytes_from_obj(obj, *len);
  let stored: Arc<[u8]> = Arc::from(bytes);

  // Convert to pinned and release the weak-handle slot.
  crate::gc::weak::global_weak_remove(*handle);

  let mut next = (*tables).clone();
  next.entries[idx] = Entry::Pinned { bytes: stored };

  INTERNER.tables.store(Arc::new(next));
}

/// Resolve an interned ID back to a [`StringRef`].
///
/// Returns `None` if the ID is invalid or if the interned entry was reclaimed.
pub(crate) fn lookup(id: InternedId) -> Option<StringRef> {
  let tables = INTERNER.tables.load_full();
  let idx = usize::try_from(id.0).ok()?;
  match tables.entries.get(idx)? {
    Entry::Pinned { bytes } => Some(StringRef {
      ptr: bytes.as_ptr(),
      len: bytes.len(),
    }),
    Entry::Weak { handle, len } => {
      let obj = crate::gc::weak::global_weak_get(*handle)?;
      Some(StringRef {
        ptr: unsafe { obj.add(INTERNED_PREFIX_SIZE) },
        len: *len,
      })
    }
    Entry::Dead => None,
  }
}

#[cfg(test)]
pub(crate) fn with_test_lock<T>(f: impl FnOnce() -> T) -> T {
  static TEST_LOCK: Lazy<parking_lot::Mutex<()>> = Lazy::new(|| parking_lot::Mutex::new(()));
  let _g = TEST_LOCK.lock();
  f()
}

/// Force a GC cycle for the interner's internal heap (test-only).
#[cfg(test)]
pub(crate) fn collect_garbage_for_tests() {
  let mut heap = INTERN_HEAP.lock();
  let mut roots = gc::RootStack::new();
  let mut remembered = gc::SimpleRememberedSet::new();
  heap.collect_major(&mut roots, &mut remembered);
}
