use crate::abi::{RtShapeDescriptor, RtShapeId};
use crate::ffi::abort_on_panic;
use crate::gc::{ObjHeader, TypeDescriptor};
use crate::sync::GcAwareMutex;
use crate::trap;
use arc_swap::ArcSwap;
use once_cell::sync::Lazy;
use std::mem;
use std::sync::Arc;

#[derive(Clone, Copy)]
struct ShapeEntry {
  rt_desc: &'static RtShapeDescriptor,
  type_desc: &'static TypeDescriptor,
}

#[derive(Clone, Default)]
struct ShapeRegistryTables {
  shapes: Vec<ShapeEntry>,
}

struct ShapeRegistry {
  tables: ArcSwap<ShapeRegistryTables>,
  /// Serialize registration updates. Readers use `tables` lock-free.
  write_lock: GcAwareMutex<()>,
}

impl ShapeRegistry {
  fn new() -> Self {
    Self {
      tables: ArcSwap::from_pointee(ShapeRegistryTables::default()),
      write_lock: GcAwareMutex::new(()),
    }
  }
}

static SHAPES: Lazy<ShapeRegistry> = Lazy::new(ShapeRegistry::new);

/// Register a global shape table (legacy single-module API).
///
/// Intended to be called once during program initialization by compiler-emitted code.
///
/// New embeddings that load multiple native modules (dlopen/JIT) should prefer
/// [`rt_register_shape_table_extend`] (or the equivalent alias [`rt_register_shape_table_append`]).
///
/// # Safety
/// - `ptr` must point to an array of `len` [`RtShapeDescriptor`] values that is valid to read for
///   the duration of this call.
/// - If any descriptor has `ptr_offsets_len != 0`, its `ptr_offsets` must be a valid pointer to
///   `ptr_offsets_len` `u32` entries for the duration of this call.
///
/// Note: the runtime copies the pointer-offset arrays into runtime-owned memory, so the
/// caller-provided arrays do not need to outlive this call.
#[no_mangle]
pub unsafe extern "C" fn rt_register_shape_table(ptr: *const RtShapeDescriptor, len: usize) {
  abort_on_panic(|| register_shape_table(ptr, len));
}

/// Append additional shapes to the global shape registry and return the first assigned shape id.
///
/// This is intended for dlopen/JIT-style embeddings that load additional native code modules after
/// process initialization.
///
/// The runtime copies all shape metadata into process-owned memory so callers do not need to keep
/// `table` (or any of its `ptr_offsets` arrays) alive after this call returns.
///
/// Returns the first assigned shape id for the appended block:
/// `base, base+1, ..., base+len-1`.
///
/// # Safety
/// - `ptr` must point to an array of `len` [`RtShapeDescriptor`] values that is valid to read for
///   the duration of this call.
/// - If any descriptor has `ptr_offsets_len != 0`, its `ptr_offsets` must be a valid pointer to
///   `ptr_offsets_len` `u32` entries for the duration of this call.
#[no_mangle]
pub unsafe extern "C" fn rt_register_shape_table_extend(
  ptr: *const RtShapeDescriptor,
  len: usize,
) -> RtShapeId {
  abort_on_panic(|| register_shape_table_impl(ptr, len, RegisterMode::Extend))
}

/// Convenience wrapper: register a single shape descriptor and return its assigned id.
///
/// This is equivalent to calling [`rt_register_shape_table_extend`] with `len = 1`.
///
/// # Safety
/// - `desc` must be valid to read for the duration of this call.
#[no_mangle]
pub unsafe extern "C" fn rt_register_shape(desc: *const RtShapeDescriptor) -> RtShapeId {
  abort_on_panic(|| rt_register_shape_table_extend(desc, 1))
}

pub(crate) unsafe fn register_shape_table(ptr: *const RtShapeDescriptor, len: usize) {
  let base = register_shape_table_impl(ptr, len, RegisterMode::Single);
  debug_assert_eq!(
    base.0, 1,
    "legacy rt_register_shape_table must assign base id 1"
  );
}

#[inline]
pub fn shape_count() -> usize {
  SHAPES.tables.load().shapes.len()
}

/// Lookup the registered [`RtShapeDescriptor`] for `id`.
///
/// Panics if the table is not registered or the id is invalid/out-of-bounds.
#[inline]
pub fn lookup_rt_descriptor(id: RtShapeId) -> &'static RtShapeDescriptor {
  if !id.is_valid() {
    panic!("RtShapeId(0) is reserved/invalid");
  }
  let tables = SHAPES.tables.load();
  if tables.shapes.is_empty() {
    panic!("shape table not registered (call rt_register_shape_table first)");
  }
  let idx = (id.0 - 1) as usize;
  if idx >= tables.shapes.len() {
    panic!(
      "RtShapeId({}) out of bounds: shape table has {} shapes",
      id.0,
      tables.shapes.len()
    );
  }
  tables.shapes[idx].rt_desc
}

/// Lookup the internal GC [`TypeDescriptor`] for `id`.
///
/// Panics if the table is not registered or the id is invalid/out-of-bounds.
#[inline]
pub fn lookup_type_descriptor(id: RtShapeId) -> &'static TypeDescriptor {
  if !id.is_valid() {
    panic!("RtShapeId(0) is reserved/invalid");
  }
  let tables = SHAPES.tables.load();
  if tables.shapes.is_empty() {
    panic!("shape table not registered (call rt_register_shape_table first)");
  }
  let idx = (id.0 - 1) as usize;
  if idx >= tables.shapes.len() {
    panic!(
      "RtShapeId({}) out of bounds: shape table has {} shapes",
      id.0,
      tables.shapes.len()
    );
  }
  tables.shapes[idx].type_desc
}

/// Validate an allocation request for `shape` and return the registered descriptors.
///
/// The runtime's shape table is the source of truth for object layout and size. If the caller
/// passes a `size` that does not match the registered descriptor size, we abort to avoid
/// out-of-bounds tracing and heap corruption.
///
/// This helper is intended for use by FFI entrypoints (`rt_alloc*`) and must never unwind.
#[inline]
pub(crate) fn validate_alloc_request(
  size: usize,
  shape: RtShapeId,
) -> (&'static RtShapeDescriptor, &'static TypeDescriptor) {
  if !shape.is_valid() {
    trap::rt_trap_invalid_arg("shape id 0 is reserved/invalid");
  }

  let tables = SHAPES.tables.load();
  if tables.shapes.is_empty() {
    trap::rt_trap_invalid_arg("shape table not registered (call rt_register_shape_table first)");
  }

  let idx = (shape.0 - 1) as usize;
  if idx >= tables.shapes.len() {
    trap::rt_trap_invalid_arg_fmt(format_args!(
      "RtShapeId({}) out of bounds: shape table has {} shapes",
      shape.0,
      tables.shapes.len()
    ));
  }

  let rt_desc = tables.shapes[idx].rt_desc;
  let expected = rt_desc.size as usize;
  if size != expected {
    trap::rt_trap_invalid_arg_fmt(format_args!(
      "allocation size mismatch for shape {}: requested {} bytes, but descriptor size is {} bytes",
      shape.0,
      size,
      expected
    ));
  }

  let type_desc = tables.shapes[idx].type_desc;
  (rt_desc, type_desc)
}

/// Register a shape table by appending it to the process-global shape-id space.
///
/// Returns the **base id** assigned to the first descriptor in this table (1-indexed). A module's
/// local shape index `i` (0-based) maps to global `RtShapeId(base + i)`.
///
/// This is intended for multi-module programs (dlopen/JIT/plugin architectures).
///
/// # Safety
/// - `ptr` must point to an array of `len` [`RtShapeDescriptor`] values that is valid to read for
///   the duration of this call.
/// - If any descriptor has `ptr_offsets_len != 0`, its `ptr_offsets` must be a valid pointer to
///   `ptr_offsets_len` `u32` entries for the duration of this call.
///
/// Note: the runtime copies the pointer-offset arrays into runtime-owned memory, so the
/// caller-provided arrays do not need to outlive this call.
#[no_mangle]
pub unsafe extern "C" fn rt_register_shape_table_append(
  ptr: *const RtShapeDescriptor,
  len: usize,
) -> RtShapeId {
  abort_on_panic(|| register_shape_table_impl(ptr, len, RegisterMode::Append))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RegisterMode {
  /// Legacy single-assignment `rt_register_shape_table` (must be first).
  Single,
  /// Multi-module append (`rt_register_shape_table_append`).
  Append,
  /// Multi-module extend (`rt_register_shape_table_extend`).
  Extend,
}

unsafe fn register_shape_table_impl(ptr: *const RtShapeDescriptor, len: usize, mode: RegisterMode) -> RtShapeId {
  let func = match mode {
    RegisterMode::Single => "rt_register_shape_table",
    RegisterMode::Append => "rt_register_shape_table_append",
    RegisterMode::Extend => "rt_register_shape_table_extend",
  };

  if ptr.is_null() {
    panic!("{func}: null table pointer");
  }
  if len == 0 {
    panic!("{func}: len must be > 0");
  }

  let table = std::slice::from_raw_parts(ptr, len);
  validate_shape_table(table);

  let _guard = SHAPES.write_lock.lock();
  let current = SHAPES.tables.load_full();

  if mode == RegisterMode::Single && !current.shapes.is_empty() {
    panic!("rt_register_shape_table: shape table already registered");
  }

  let cur_len = current.shapes.len();
  let total_len = cur_len
    .checked_add(len)
    .unwrap_or_else(|| panic!("{func}: shape table size overflow"));
  if total_len > (u32::MAX as usize) {
    panic!("{func}: shape table too large for RtShapeId");
  }

  let base_u32 = u32::try_from(cur_len).expect("shape id space exhausted");
  let base_id = RtShapeId(base_u32 + 1);

  // Copy-on-write snapshot update: keep lookups lock-free by publishing a new `Arc` each append.
  let mut next = (*current).clone();
  next.shapes.reserve(len);

  // This is used to canonicalize "no pointer fields" slices so registrations don't retain foreign
  // module memory unnecessarily.
  static EMPTY_PTR_OFFSETS: [u32; 0] = [];

  for desc in table {
    // Copy the pointer-offset array into runtime-owned memory so the registration is safe even if
    // the caller's module/JIT memory is later unmapped.
    let (ptr_offsets, ptr_offsets_len) = if desc.ptr_offsets_len == 0 {
      (EMPTY_PTR_OFFSETS.as_ptr(), 0)
    } else {
      debug_assert!(!desc.ptr_offsets.is_null());
      // SAFETY: validated above (`validate_shape_table`).
      let offsets =
        unsafe { std::slice::from_raw_parts(desc.ptr_offsets, desc.ptr_offsets_len as usize) };
      let owned: Box<[u32]> = offsets.to_vec().into_boxed_slice();
      let leaked: &'static [u32] = Box::leak(owned);
      (leaked.as_ptr(), leaked.len() as u32)
    };

    let mut rt_desc = *desc;
    rt_desc.ptr_offsets = ptr_offsets;
    rt_desc.ptr_offsets_len = ptr_offsets_len;
    let rt_desc: &'static RtShapeDescriptor = Box::leak(Box::new(rt_desc));

    let align = (desc.align as usize).max(crate::gc::OBJ_ALIGN);
    let type_desc = unsafe {
      TypeDescriptor::from_raw_parts(desc.size as usize, align, ptr_offsets, ptr_offsets_len)
    };
    let type_desc: &'static TypeDescriptor = Box::leak(Box::new(type_desc));

    next.shapes.push(ShapeEntry { rt_desc, type_desc });
  }

  SHAPES.tables.store(Arc::new(next));
  base_id
}

// --- Shared test shape table ---------------------------------------------------------------------
//
// `RtShapeId` tables are process-global and append-only. Unit tests within this crate run in a
// single process and can execute in parallel, so any test that needs `rt_alloc` must agree on a
// stable base table.
//
// Keep this table minimal and append-only so existing shape-id assumptions remain stable.
#[cfg(test)]
pub(crate) const TEST_PROMISE_U64_SHAPE_ID: RtShapeId = RtShapeId(4);
#[cfg(test)]
pub(crate) const TEST_CORO_AWAIT_THEN_FULFILL_SHAPE_ID: RtShapeId = RtShapeId(5);

#[cfg(test)]
#[repr(C)]
pub(crate) struct TestPromiseU64 {
  pub header: crate::async_abi::PromiseHeader,
  pub payload: u64,
}

#[cfg(test)]
#[repr(C)]
pub(crate) struct TestCoroAwaitThenFulfill {
  pub header: crate::async_abi::Coroutine,
  pub state: *mut core::sync::atomic::AtomicUsize,
  pub awaited: crate::async_abi::PromiseRef,
}

#[cfg(test)]
static TEST_LEAF_PTR_OFFSETS: [u32; 0] = [];
#[cfg(test)]
static TEST_SINGLE_PTR_OFFSETS: [u32; 1] = [mem::size_of::<ObjHeader>() as u32];
#[cfg(test)]
static TEST_WEIRD_PTR_OFFSETS: [u32; 1] =
  [(mem::size_of::<ObjHeader>() + mem::size_of::<*mut u8>()) as u32];

#[cfg(test)]
static TEST_PROMISE_U64_PTR_OFFSETS: [u32; 0] = [];

#[cfg(test)]
static TEST_CORO_AWAIT_THEN_FULFILL_PTR_OFFSETS: [u32; 3] = [
  mem::offset_of!(crate::async_abi::Coroutine, promise) as u32,
  mem::offset_of!(crate::async_abi::Coroutine, next_waiter) as u32,
  mem::offset_of!(TestCoroAwaitThenFulfill, awaited) as u32,
];

#[cfg(test)]
static TEST_SHAPE_TABLE: [RtShapeDescriptor; 5] = [
  // Shape 1: leaf object (header only).
  RtShapeDescriptor {
    size: mem::size_of::<ObjHeader>() as u32,
    align: mem::align_of::<ObjHeader>() as u16,
    flags: 0,
    ptr_offsets: TEST_LEAF_PTR_OFFSETS.as_ptr(),
    ptr_offsets_len: TEST_LEAF_PTR_OFFSETS.len() as u32,
    reserved: 0,
  },
  // Shape 2: single pointer slot (immediately after ObjHeader).
  RtShapeDescriptor {
    size: (mem::size_of::<ObjHeader>() + mem::size_of::<*mut u8>()) as u32,
    align: mem::align_of::<ObjHeader>() as u16,
    flags: 0,
    ptr_offsets: TEST_SINGLE_PTR_OFFSETS.as_ptr(),
    ptr_offsets_len: TEST_SINGLE_PTR_OFFSETS.len() as u32,
    reserved: 0,
  },
  // Shape 3: two pointer slots, but only the *second* one is traced.
  RtShapeDescriptor {
    size: (mem::size_of::<ObjHeader>() + 2 * mem::size_of::<*mut u8>()) as u32,
    align: mem::align_of::<ObjHeader>() as u16,
    flags: 0,
    ptr_offsets: TEST_WEIRD_PTR_OFFSETS.as_ptr(),
    ptr_offsets_len: TEST_WEIRD_PTR_OFFSETS.len() as u32,
    reserved: 0,
  },
  // Shape 4: promise header + u64 payload (used by async ABI unit tests).
  RtShapeDescriptor {
    size: mem::size_of::<TestPromiseU64>() as u32,
    align: mem::align_of::<TestPromiseU64>() as u16,
    flags: 0,
    ptr_offsets: TEST_PROMISE_U64_PTR_OFFSETS.as_ptr(),
    ptr_offsets_len: TEST_PROMISE_U64_PTR_OFFSETS.len() as u32,
    reserved: 0,
  },
  // Shape 5: coroutine frame used by async ABI unit tests.
  RtShapeDescriptor {
    size: mem::size_of::<TestCoroAwaitThenFulfill>() as u32,
    align: mem::align_of::<TestCoroAwaitThenFulfill>() as u16,
    flags: 0,
    ptr_offsets: TEST_CORO_AWAIT_THEN_FULFILL_PTR_OFFSETS.as_ptr(),
    ptr_offsets_len: TEST_CORO_AWAIT_THEN_FULFILL_PTR_OFFSETS.len() as u32,
    reserved: 0,
  },
];

#[cfg(test)]
pub(crate) fn ensure_test_shape_table_registered() {
  use std::sync::Once;

  static ONCE: Once = Once::new();
  ONCE.call_once(|| unsafe {
    if shape_count() != 0 {
      return;
    }
    register_shape_table(TEST_SHAPE_TABLE.as_ptr(), TEST_SHAPE_TABLE.len());
  });
}

#[cfg(feature = "gc_debug")]
#[no_mangle]
pub extern "C" fn rt_debug_shape_count() -> usize {
  abort_on_panic(|| shape_count())
}

#[cfg(feature = "gc_debug")]
#[no_mangle]
pub extern "C" fn rt_debug_shape_descriptor(id: RtShapeId) -> *const RtShapeDescriptor {
  abort_on_panic(|| {
    if !id.is_valid() {
      return std::ptr::null();
    }
    let tables = SHAPES.tables.load();
    let idx = (id.0 - 1) as usize;
    if idx < tables.shapes.len() {
      return tables.shapes[idx].rt_desc as *const RtShapeDescriptor;
    }
    std::ptr::null()
  })
}

#[cfg(feature = "gc_debug")]
#[no_mangle]
pub extern "C" fn rt_debug_validate_heap() {
  // The runtime does not yet have a global heap instance wired to the exported ABI. The most
  // helpful invariant we can validate today is that the shape table is present and internally
  // consistent (which registration already checks).
  abort_on_panic(|| {
    if shape_count() == 0 {
      panic!("rt_debug_validate_heap: shape table not registered");
    }
    // Ensure the global heap has been initialized so debug tooling (like `rt_gc_get_young_range`)
    // reports a valid nursery range.
    crate::rt_alloc::ensure_global_heap_init();
  })
}

fn validate_shape_table(table: &[RtShapeDescriptor]) {
  for (i, desc) in table.iter().enumerate() {
    validate_descriptor(i, desc);
  }
}

fn validate_descriptor(index: usize, desc: &RtShapeDescriptor) {
  if desc.flags != 0 {
    panic!("shape[{index}]: flags must be 0 (got {})", desc.flags);
  }
  if desc.reserved != 0 {
    panic!("shape[{index}]: reserved must be 0 (got {})", desc.reserved);
  }

  let size = desc.size as usize;
  if size < mem::size_of::<ObjHeader>() {
    panic!(
      "shape[{index}]: size {} too small for ObjHeader ({} bytes)",
      size,
      mem::size_of::<ObjHeader>()
    );
  }

  let align = desc.align as usize;
  if align == 0 || !align.is_power_of_two() {
    panic!("shape[{index}]: align must be a non-zero power of two (got {})", align);
  }
  if align < mem::align_of::<ObjHeader>() {
    panic!(
      "shape[{index}]: align {} smaller than ObjHeader alignment {}",
      align,
      mem::align_of::<ObjHeader>()
    );
  }

  let ptr_align = mem::align_of::<*mut u8>();
  let ptr_size = mem::size_of::<*mut u8>();
  let header_size = mem::size_of::<ObjHeader>();

  let offsets = if desc.ptr_offsets_len == 0 {
    &[][..]
  } else {
    if desc.ptr_offsets.is_null() {
      panic!("shape[{index}]: ptr_offsets_len is non-zero but ptr_offsets is null");
    }
    // SAFETY: caller promises the table is readable for the duration of this call.
    unsafe { std::slice::from_raw_parts(desc.ptr_offsets, desc.ptr_offsets_len as usize) }
  };

  let mut last: Option<u32> = None;
  for &off_u32 in offsets {
    let off = off_u32 as usize;
    if off < header_size {
      panic!(
        "shape[{index}]: ptr offset {} is inside ObjHeader ({} bytes); offsets must be from object base and point into the payload",
        off, header_size
      );
    }
    if off.checked_add(ptr_size).map_or(true, |end| end > size) {
      panic!("shape[{index}]: ptr offset {} out of bounds for size {}", off, size);
    }
    if off % ptr_align != 0 {
      panic!(
        "shape[{index}]: ptr offset {} is not pointer-aligned (align {})",
        off, ptr_align
      );
    }
    if let Some(prev) = last {
      if off_u32 <= prev {
        panic!(
          "shape[{index}]: ptr_offsets must be strictly increasing ({} then {})",
          prev, off_u32
        );
      }
    }
    last = Some(off_u32);
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::roots::Root;
  use crate::test_util::TestRuntimeGuard;

  #[test]
  fn register_table_and_trace_uses_offsets() {
    let _rt = TestRuntimeGuard::new();

    // Invalid registration is rejected (offset inside ObjHeader).
    static HEADER_PTR_OFFSETS: [u32; 1] = [0];
    static HEADER_TABLE: [RtShapeDescriptor; 1] = [RtShapeDescriptor {
      size: mem::size_of::<ObjHeader>() as u32,
      align: mem::align_of::<ObjHeader>() as u16,
      flags: 0,
      ptr_offsets: HEADER_PTR_OFFSETS.as_ptr(),
      ptr_offsets_len: HEADER_PTR_OFFSETS.len() as u32,
      reserved: 0,
    }];
    assert!(std::panic::catch_unwind(|| validate_shape_table(&HEADER_TABLE)).is_err());

    // Invalid registration is rejected (offset out of bounds).
    static INVALID_PTR_OFFSETS: [u32; 1] = [0xffff_ff00];
    static INVALID_TABLE: [RtShapeDescriptor; 1] = [RtShapeDescriptor {
      size: mem::size_of::<ObjHeader>() as u32,
      align: mem::align_of::<ObjHeader>() as u16,
      flags: 0,
      ptr_offsets: INVALID_PTR_OFFSETS.as_ptr(),
      ptr_offsets_len: INVALID_PTR_OFFSETS.len() as u32,
      reserved: 0,
    }];
    assert!(std::panic::catch_unwind(|| validate_shape_table(&INVALID_TABLE)).is_err());

    ensure_test_shape_table_registered();

    // Shape ids are 1-indexed.
    let leaf = RtShapeId(1);
    let weird = RtShapeId(3);

    let weird_desc = lookup_type_descriptor(weird);
    assert_eq!(weird_desc.ptr_offsets(), TEST_WEIRD_PTR_OFFSETS);

    unsafe {
      // `rt_alloc` requires the current thread be registered so it can participate in stop-the-world
      // safepoints if allocation triggers GC.
      let was_registered = crate::threading::registry::current_thread_id().is_some();
      if !was_registered {
        crate::rt_thread_init(0);
      }

      {
        // Allocate objects via the runtime ABI (`rt_alloc`) so we exercise the shape table wiring.
        //
        // Use a `Root` so this test is safe under parallel execution: other tests may trigger
        // stop-the-world GC during these allocations, and GC-managed pointers stored in plain Rust
        // locals are not scanned unless rooted.
        let wrapper = Root::<u8>::new(crate::rt_alloc(weird_desc.size, weird));

        // Wrapper has two pointer-sized slots at offsets header+0 and header+ptr_size, but the
        // descriptor only lists the second one (WEIRD_PTR_OFFSETS). Store the live value in the
        // traced slot before the next allocation (which may trigger GC).
        let should_live = crate::rt_alloc(mem::size_of::<ObjHeader>(), leaf);
        assert!(!should_live.is_null());
        let base = wrapper.get() as *mut u8;
        *base
          .add(mem::size_of::<ObjHeader>() + mem::size_of::<*mut u8>())
          .cast::<*mut u8>() = should_live;

        let should_die = crate::rt_alloc(mem::size_of::<ObjHeader>(), leaf);
        assert!(!should_die.is_null());
        let base = wrapper.get() as *mut u8;
        *base.add(mem::size_of::<ObjHeader>()).cast::<*mut u8>() = should_die;

        let mut seen = Vec::<usize>::new();
        crate::gc::for_each_ptr_slot(base, |slot| {
          seen.push(slot as usize);
        });

        let expected_slot = base
          .add(mem::size_of::<ObjHeader>() + mem::size_of::<*mut u8>())
          .cast::<*mut u8>() as usize;
        assert_eq!(seen, vec![expected_slot]);
      }

      if !was_registered {
        crate::rt_thread_deinit();
      }
    }
  }
}
