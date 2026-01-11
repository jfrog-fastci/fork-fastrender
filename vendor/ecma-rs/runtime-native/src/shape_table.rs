use crate::metadata::TypeDescriptor;
use std::sync::{OnceLock, RwLock};

/// Raw pointers don't implement `Send`/`Sync`, but our shape table only stores
/// immutable `TypeDescriptor` pointers that are safe to read concurrently.
#[derive(Copy, Clone)]
#[repr(transparent)]
struct DescPtr(*const TypeDescriptor);

// Safety: `TypeDescriptor` values are immutable metadata and must outlive any
// objects whose headers point at them. We never mutate through this pointer.
unsafe impl Send for DescPtr {}
unsafe impl Sync for DescPtr {}

impl DescPtr {
  const NULL: Self = Self(core::ptr::null::<TypeDescriptor>());
}

static SHAPES: OnceLock<RwLock<Vec<DescPtr>>> = OnceLock::new();

fn shapes() -> &'static RwLock<Vec<DescPtr>> {
  SHAPES.get_or_init(|| RwLock::new(Vec::new()))
}

/// Register a shape → type descriptor mapping.
///
/// Intended to be called from generated code during program initialization.
#[no_mangle]
pub extern "C" fn rt_register_shape(shape: u32, desc: *const TypeDescriptor) {
  debug_assert!(!desc.is_null());

  let mut table = shapes().write().unwrap();
  let idx = shape as usize;
  if table.len() <= idx {
    table.resize(idx + 1, DescPtr::NULL);
  }
  table[idx] = DescPtr(desc);
}

/// Register a dense shape table.
///
/// The table is indexed directly by `shape` (i.e. `table[shape]`).
///
/// Intended to be called once during program initialization.
///
/// # Safety
/// `ptr` must point to an array of `len` pointers which remains valid for the
/// duration of this call.
#[no_mangle]
pub unsafe extern "C" fn rt_register_shape_table(ptr: *const *const TypeDescriptor, len: usize) {
  let slice = unsafe { std::slice::from_raw_parts(ptr, len) };
  let mut table = shapes().write().unwrap();
  table.clear();
  table.extend(slice.iter().copied().map(DescPtr));
}

/// Lookup a type descriptor from a shape id.
#[inline]
pub fn lookup_shape(shape: u32) -> Option<*const TypeDescriptor> {
  let table = shapes().read().unwrap();
  let idx = shape as usize;
  let desc = table.get(idx)?.0;
  if desc.is_null() { None } else { Some(desc) }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::metadata::{TypeDescriptor, TypeKind};

  static PTR_OFFSETS: [u32; 0] = [];
  static DUMMY_DESC: TypeDescriptor = TypeDescriptor {
    kind: TypeKind::Fixed,
    size: 0,
    ptr_offsets: &PTR_OFFSETS,
  };

  static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

  #[test]
  fn shape_registry_basic_lookup() {
    let _guard = TEST_LOCK.lock().unwrap();

    rt_register_shape(1234, &DUMMY_DESC as *const _);
    assert_eq!(lookup_shape(1234), Some(&DUMMY_DESC as *const _));
    assert_eq!(lookup_shape(1235), None);
  }

  #[test]
  fn shape_registry_table_registration() {
    let _guard = TEST_LOCK.lock().unwrap();

    let table: [*const TypeDescriptor; 2] = [&DUMMY_DESC as *const _, core::ptr::null()];
    unsafe { rt_register_shape_table(table.as_ptr(), table.len()) };

    assert_eq!(lookup_shape(0), Some(&DUMMY_DESC as *const _));
    assert_eq!(lookup_shape(1), None);
  }
}
