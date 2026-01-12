use core::ptr;

use ahash::AHashMap;
use once_cell::sync::Lazy;

use crate::abi::{InternedId, StringRef};
use crate::ffi::abort_on_panic;
use crate::interner;
use crate::sync::GcAwareMutex;
use crate::trap;
use crate::{gc::ObjHeader, gc::TypeDescriptor, roots::GcPtr};

const RT_STRING_KIND_HEAP: u8 = 1;
const RT_STRING_ENCODING_UTF8: u8 = 1;

#[repr(C)]
struct RtStringHeader {
  header: ObjHeader,
  kind: u8,
  encoding: u8,
  /// Reserved for future string flags / small-header variants; must be `0` for now.
  flags: u16,
  len: usize,
  data: [u8; 0],
}

pub(crate) const RT_STRING_DATA_OFFSET: usize = core::mem::size_of::<RtStringHeader>();

const _: () = {
  // Keep the GC prefix at offset 0 (object base pointer).
  assert!(core::mem::offset_of!(RtStringHeader, header) == 0);
  assert!(core::mem::offset_of!(RtStringHeader, data) == RT_STRING_DATA_OFFSET);
  // 64-bit ABI only, so this should be stable.
  assert!(RT_STRING_DATA_OFFSET == 32);
};

static NO_PTR_OFFSETS: [u32; 0] = [];

static RT_STRING_DESC_CACHE: Lazy<GcAwareMutex<AHashMap<usize, &'static TypeDescriptor>>> =
  Lazy::new(|| GcAwareMutex::new(AHashMap::new()));

fn rt_string_desc_for_size(size: usize) -> &'static TypeDescriptor {
  // Fast uncontended path.
  if let Some(existing) = RT_STRING_DESC_CACHE.try_lock().and_then(|m| m.get(&size).copied()) {
    return existing;
  }

  let mut cache = RT_STRING_DESC_CACHE.lock();
  if let Some(existing) = cache.get(&size).copied() {
    return existing;
  }

  let desc = Box::leak(Box::new(TypeDescriptor::new(size, &NO_PTR_OFFSETS)));
  cache.insert(size, desc);
  desc
}

fn rt_string_object_size_for_len(len: usize) -> usize {
  // Avoid next_power_of_two(0) == 0 corner case by keeping the empty string at capacity 0.
  let cap = if len == 0 {
    0
  } else {
    len
      .checked_next_power_of_two()
      .unwrap_or_else(|| trap::rt_trap_invalid_arg("rt_string: length overflow"))
      .max(16)
  };

  RT_STRING_DATA_OFFSET
    .checked_add(cap)
    .unwrap_or_else(|| trap::rt_trap_invalid_arg("rt_string: object size overflow"))
}

fn alloc_rt_string_with_len(
  len: usize,
  old_only: bool,
  entry_fp: u64,
  entry_name: &'static str,
) -> *mut u8 {
  let size = rt_string_object_size_for_len(len);
  let desc = rt_string_desc_for_size(size);

  let obj = if old_only {
    crate::rt_alloc::alloc_typed_old_with_entry(desc, entry_fp, entry_name)
  } else {
    crate::rt_alloc::alloc_typed_with_entry(desc, entry_fp, entry_name)
  };

  // SAFETY: `obj` points to a valid allocation of `desc.size` bytes.
  unsafe {
    let header = &mut *(obj as *mut RtStringHeader);
    header.kind = RT_STRING_KIND_HEAP;
    header.encoding = RT_STRING_ENCODING_UTF8;
    header.flags = 0;
    header.len = len;
  }

  obj
}

fn alloc_rt_string_from_utf8(
  bytes: &[u8],
  old_only: bool,
  entry_fp: u64,
  entry_name: &'static str,
) -> *mut u8 {
  let len = bytes.len();
  let obj = alloc_rt_string_with_len(len, old_only, entry_fp, entry_name);

  // SAFETY: `obj` points to an allocation with capacity >= `len`.
  unsafe {
    if len != 0 {
      ptr::copy_nonoverlapping(bytes.as_ptr(), obj.add(RT_STRING_DATA_OFFSET), len);
    }
  }

  obj
}

/// Allocate a GC-managed UTF-8 string directly into old-gen (Immix/LOS).
///
/// This is intended for weakly-interned strings: allocating into old-gen avoids
/// being cleared by minor GC unless the runtime runs a major sweep.
#[allow(dead_code)] // Used by future interner integration.
pub(crate) fn alloc_rt_string_from_utf8_old(bytes: &[u8]) -> *mut u8 {
  let entry_fp = crate::stackwalk::current_frame_pointer();
  alloc_rt_string_from_utf8(bytes, true, entry_fp, "alloc_rt_string_from_utf8_old")
}

#[inline]
unsafe fn rt_string_follow_forwarding(mut obj: *mut u8) -> *mut u8 {
  let header = &*crate::gc::header_from_obj(obj);
  if header.is_forwarded() {
    obj = header.forwarding_ptr();
  }
  obj
}

fn validate_rt_string(obj: *mut u8, context: &'static str) -> *mut u8 {
  if obj.is_null() {
    trap::rt_trap_invalid_arg(context);
  }

  // SAFETY: caller promises `obj` is a valid GC object base pointer.
  unsafe {
    let obj = rt_string_follow_forwarding(obj);
    let header = &*(obj as *const RtStringHeader);
    if header.kind != RT_STRING_KIND_HEAP
      || header.encoding != RT_STRING_ENCODING_UTF8
      || header.flags != 0
    {
      trap::rt_trap_invalid_arg(context);
    }

    let desc = (&*crate::gc::header_from_obj(obj)).type_desc();
    if desc.size < RT_STRING_DATA_OFFSET {
      trap::rt_trap_invalid_arg(context);
    }
    let cap = desc.size - RT_STRING_DATA_OFFSET;
    if header.len > cap {
      trap::rt_trap_invalid_arg(context);
    }

    obj
  }
}

fn bytes_from_raw<'a>(ptr: *const u8, len: usize, context: &'static str) -> &'a [u8] {
  if len == 0 {
    return &[];
  }
  if ptr.is_null() {
    trap::rt_trap_invalid_arg(context);
  }

  // Safety: The caller promises `ptr..ptr+len` is a readable byte range.
  unsafe { std::slice::from_raw_parts(ptr, len) }
}

#[repr(C)]
struct StringAllocHeader {
  magic: u64,
  size: usize,
  align: usize,
}

const STRING_ALLOC_MAGIC: u64 = u64::from_ne_bytes(*b"RTSTRCON");

/// Concatenate two UTF-8 byte strings into a new allocation.
///
/// The returned bytes are allocated outside the GC heap (stable across GC) and must be freed by
/// calling [`rt_string_free`] (or the legacy alias [`rt_stringref_free`]).
#[no_mangle]
pub extern "C" fn rt_string_concat(a: *const u8, a_len: usize, b: *const u8, b_len: usize) -> StringRef {
  abort_on_panic(|| {
    let a_bytes = bytes_from_raw(
      a,
      a_len,
      "rt_string_concat: `a` was null but `a_len` was non-zero",
    );
    let b_bytes = bytes_from_raw(
      b,
      b_len,
      "rt_string_concat: `b` was null but `b_len` was non-zero",
    );

    if std::str::from_utf8(a_bytes).is_err() {
      trap::rt_trap_invalid_arg("rt_string_concat: `a` was not valid UTF-8");
    }
    if std::str::from_utf8(b_bytes).is_err() {
      trap::rt_trap_invalid_arg("rt_string_concat: `b` was not valid UTF-8");
    }

    let len = a_len
      .checked_add(b_len)
      .unwrap_or_else(|| trap::rt_trap_invalid_arg("rt_string_concat: length overflow"));

    if len == 0 {
      return StringRef::empty();
    }

    let header_size = core::mem::size_of::<StringAllocHeader>();
    let align = core::mem::align_of::<StringAllocHeader>();
    let alloc_size = header_size
      .checked_add(len)
      .unwrap_or_else(|| trap::rt_trap_invalid_arg("rt_string_concat: length overflow"));
    let layout = std::alloc::Layout::from_size_align(alloc_size, align)
      .unwrap_or_else(|_| trap::rt_trap_invalid_arg("rt_string_concat: invalid allocation layout"));
    let base = unsafe { std::alloc::alloc(layout) };
    if base.is_null() {
      trap::rt_trap_oom(alloc_size, "rt_string_concat");
    }

    unsafe {
      // `base` points to a unique allocation of `alloc_size` bytes.
      (base as *mut StringAllocHeader).write(StringAllocHeader {
        magic: STRING_ALLOC_MAGIC,
        size: alloc_size,
        align,
      });

      let out = base.add(header_size);
      ptr::copy_nonoverlapping(a_bytes.as_ptr(), out, a_len);
      ptr::copy_nonoverlapping(b_bytes.as_ptr(), out.add(a_len), b_len);
      StringRef { ptr: out, len }
    }
  })
}

/// Free an owned [`StringRef`] allocated by [`rt_string_concat`] or [`rt_string_to_owned_utf8`].
///
/// This is a no-op for `len == 0` (including `{NULL, 0}`).
#[no_mangle]
pub extern "C" fn rt_string_free(s: StringRef) {
  abort_on_panic(|| unsafe {
    rt_string_free_impl(s);
  })
}

unsafe fn rt_string_free_impl(s: StringRef) {
  if s.len == 0 {
    return;
  }
  if s.ptr.is_null() {
    trap::rt_trap_invalid_arg("rt_string_free: `s.ptr` was null but `s.len` was non-zero");
  }

  let header_size = core::mem::size_of::<StringAllocHeader>();
  // Use wrapping arithmetic for defensive behavior on bogus `s.ptr` values; this function is
  // expected to abort on misuse (e.g. freeing borrowed `StringRef`s).
  let base = (s.ptr as usize).wrapping_sub(header_size) as *mut u8;
  let expected_align = core::mem::align_of::<StringAllocHeader>();
  if (base as usize) % expected_align != 0 {
    trap::rt_trap_invalid_arg("rt_string_free: misaligned `StringRef` pointer");
  }

  let header = (base as *const StringAllocHeader).read();
  if header.magic != STRING_ALLOC_MAGIC {
    trap::rt_trap_invalid_arg(
      "rt_string_free: buffer was not allocated by rt_string_concat or rt_string_to_owned_utf8",
    );
  }
  if header.size < header_size {
    trap::rt_trap_invalid_arg("rt_string_free: invalid allocation header");
  }
  let payload_len = header.size - header_size;
  if payload_len != s.len {
    trap::rt_trap_invalid_arg("rt_string_free: length mismatch");
  }
  if header.align != expected_align {
    trap::rt_trap_invalid_arg("rt_string_free: invalid allocation alignment");
  }
  let layout = std::alloc::Layout::from_size_align(header.size, header.align)
    .unwrap_or_else(|_| trap::rt_trap_invalid_arg("rt_string_free: invalid allocation layout"));
  std::alloc::dealloc(base, layout);
}

/// Compatibility alias for older codegen/tests. Prefer [`rt_string_free`].
#[no_mangle]
pub extern "C" fn rt_stringref_free(s: StringRef) {
  abort_on_panic(|| unsafe {
    rt_string_free_impl(s);
  })
}

/// Allocate a GC-managed UTF-8 string and copy `bytes`.
#[no_mangle]
pub extern "C" fn rt_string_new_utf8(bytes: *const u8, len: usize) -> GcPtr {
  // Capture the frame pointer of this runtime entrypoint before entering `abort_on_panic`, which
  // may wrap the body in `catch_unwind` and break frame-pointer walking used by stackmap fixups.
  let entry_fp = crate::stackwalk::current_frame_pointer();
  abort_on_panic(|| {
    let bytes = bytes_from_raw(
      bytes,
      len,
      "rt_string_new_utf8: `bytes` was null but `len` was non-zero",
    );
    if std::str::from_utf8(bytes).is_err() {
      trap::rt_trap_invalid_arg("rt_string_new_utf8: input was not valid UTF-8");
    }

    alloc_rt_string_from_utf8(bytes, false, entry_fp, "rt_string_new_utf8")
  })
}

/// Return the length (in UTF-8 bytes) of a GC-managed string.
#[no_mangle]
pub extern "C" fn rt_string_len(s: GcPtr) -> usize {
  abort_on_panic(|| {
    let s = validate_rt_string(s, "rt_string_len: `s` was null or not a runtime string");
    // SAFETY: `validate_rt_string` checked the header layout.
    unsafe { (*(s as *const RtStringHeader)).len }
  })
}

/// Borrow the UTF-8 bytes of a GC-managed string.
///
/// The returned view points into the GC heap and is only valid until the next GC
/// safepoint/collection (the string may be relocated).
#[no_mangle]
pub extern "C" fn rt_string_as_utf8(s: GcPtr) -> StringRef {
  abort_on_panic(|| {
    let s = validate_rt_string(s, "rt_string_as_utf8: `s` was null or not a runtime string");
    // SAFETY: `validate_rt_string` checked the header layout.
    unsafe {
      let len = (*(s as *const RtStringHeader)).len;
      if len == 0 {
        return StringRef::empty();
      }
      StringRef {
        ptr: s.add(RT_STRING_DATA_OFFSET) as *const u8,
        len,
      }
    }
  })
}

/// Allocate and return an owned copy of the UTF-8 bytes of a GC-managed string.
///
/// The returned [`StringRef`] must be freed via [`rt_string_free`] (or [`rt_stringref_free`]).
#[no_mangle]
pub extern "C" fn rt_string_to_owned_utf8(s: GcPtr) -> StringRef {
  abort_on_panic(|| {
    let s = validate_rt_string(s, "rt_string_to_owned_utf8: `s` was null or not a runtime string");
    // SAFETY: `validate_rt_string` checked the header layout.
    unsafe {
      let len = (*(s as *const RtStringHeader)).len;
      if len == 0 {
        return StringRef::empty();
      }
      let bytes = core::slice::from_raw_parts(s.add(RT_STRING_DATA_OFFSET), len);

      let header_size = core::mem::size_of::<StringAllocHeader>();
      let align = core::mem::align_of::<StringAllocHeader>();
      let alloc_size = header_size
        .checked_add(len)
        .unwrap_or_else(|| trap::rt_trap_invalid_arg("rt_string_to_owned_utf8: length overflow"));
      let layout = std::alloc::Layout::from_size_align(alloc_size, align)
        .unwrap_or_else(|_| trap::rt_trap_invalid_arg("rt_string_to_owned_utf8: invalid allocation layout"));
      let base = std::alloc::alloc(layout);
      if base.is_null() {
        trap::rt_trap_oom(alloc_size, "rt_string_to_owned_utf8");
      }

      (base as *mut StringAllocHeader).write(StringAllocHeader {
        magic: STRING_ALLOC_MAGIC,
        size: alloc_size,
        align,
      });

      let out = base.add(header_size);
      ptr::copy_nonoverlapping(bytes.as_ptr(), out, len);
      StringRef { ptr: out, len }
    }
  })
}

/// Concatenate two GC-managed UTF-8 strings into a new GC-managed string.
#[no_mangle]
pub extern "C" fn rt_string_concat_gc(a: GcPtr, b: GcPtr) -> GcPtr {
  let entry_fp = crate::stackwalk::current_frame_pointer();
  abort_on_panic(|| {
    if a.is_null() {
      trap::rt_trap_invalid_arg("rt_string_concat_gc: `a` was null");
    }
    if b.is_null() {
      trap::rt_trap_invalid_arg("rt_string_concat_gc: `b` was null");
    }

    let mut ts = crate::threading::registry::current_thread_state_ptr();
    if ts.is_null() {
      // Match `rt_alloc`'s behavior for embedders/tests that call into the runtime
      // without explicit `rt_thread_init`.
      crate::threading::register_current_thread(crate::threading::ThreadKind::External);
      ts = crate::threading::registry::current_thread_state_ptr();
    }
    if ts.is_null() {
      trap::rt_trap_invalid_arg("rt_string_concat_gc: current thread is not registered");
    }
    // SAFETY: non-null thread state pointer.
    let ts = unsafe { &*ts };
    let base_len = ts.handle_stack_len();

    let mut a = a;
    let mut b = b;
    ts.handle_stack_push(&mut a as *mut *mut u8);
    ts.handle_stack_push(&mut b as *mut *mut u8);

    struct Pop<'a> {
      ts: &'a crate::threading::registry::ThreadState,
      len: usize,
    }
    impl Drop for Pop<'_> {
      fn drop(&mut self) {
        self.ts.handle_stack_truncate(self.len);
      }
    }
    let _pop = Pop { ts, len: base_len };

    a = validate_rt_string(a, "rt_string_concat_gc: `a` was not a runtime string");
    b = validate_rt_string(b, "rt_string_concat_gc: `b` was not a runtime string");

    // SAFETY: validated headers.
    let a_len = unsafe { (*(a as *const RtStringHeader)).len };
    let b_len = unsafe { (*(b as *const RtStringHeader)).len };

    if a_len == 0 {
      return b;
    }
    if b_len == 0 {
      return a;
    }

    let len = a_len
      .checked_add(b_len)
      .unwrap_or_else(|| trap::rt_trap_invalid_arg("rt_string_concat_gc: length overflow"));

    let out = alloc_rt_string_with_len(len, false, entry_fp, "rt_string_concat_gc");

    // SAFETY: `out` has capacity >= `len` by construction.
    unsafe {
      let dst = out.add(RT_STRING_DATA_OFFSET);
      ptr::copy_nonoverlapping(a.add(RT_STRING_DATA_OFFSET), dst, a_len);
      ptr::copy_nonoverlapping(b.add(RT_STRING_DATA_OFFSET), dst.add(a_len), b_len);
    }

    out
  })
}

/// Intern a UTF-8 byte string and return a stable ID.
#[no_mangle]
pub extern "C" fn rt_string_intern(s: *const u8, len: usize) -> InternedId {
  let entry_fp = crate::stackwalk::current_frame_pointer();
  abort_on_panic(|| {
    let bytes = bytes_from_raw(
      s,
      len,
      "rt_string_intern: `s` was null but `len` was non-zero",
    );

    if std::str::from_utf8(bytes).is_err() {
      trap::rt_trap_invalid_arg("rt_string_intern: input was not valid UTF-8");
    }

    interner::intern_with_entry(bytes, entry_fp)
  })
}

/// Permanently pin an interned string so it survives GC sweeps and interner pruning.
///
/// This is intended for common interned strings like property names and keywords.
#[no_mangle]
pub extern "C" fn rt_string_pin_interned(id: InternedId) {
  abort_on_panic(|| {
    interner::pin_interned(id);
  })
}

/// Lookup an interned string by stable ID.
///
/// Returns `{ptr = NULL, len = 0}` if the ID is invalid or the entry was reclaimed.
#[no_mangle]
pub extern "C" fn rt_string_lookup(id: InternedId) -> StringRef {
  abort_on_panic(|| interner::lookup(id).unwrap_or(StringRef { ptr: ptr::null(), len: 0 }))
}

/// Look up the UTF-8 bytes for a pinned interned string ID.
///
/// This API is intended for callers that require a GC-stable byte pointer: pinned interned strings
/// are stored out-of-line (owned by the interner) so their bytes are stable for the lifetime of the
/// process.
///
/// Returns false if `id` is invalid, was reclaimed, or is not pinned.
///
/// # Safety
/// `out` must be a valid, aligned pointer to a writable [`StringRef`].
#[no_mangle]
pub unsafe extern "C" fn rt_string_lookup_pinned(id: InternedId, out: *mut StringRef) -> bool {
  abort_on_panic(|| unsafe {
    if out.is_null() {
      trap::rt_trap_invalid_arg("rt_string_lookup_pinned: `out` was null");
    }

    if let Some(s) = interner::lookup_pinned(id) {
      *out = s;
      true
    } else {
      // Use a valid empty slice (non-null pointer) so callers can safely treat `out` as a slice even
      // on failure (as long as they also check `len`).
      *out = StringRef::empty();
      false
    }
  })
}
