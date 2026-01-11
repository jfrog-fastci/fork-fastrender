use core::ptr::NonNull;

use crate::async_rt::gc_handle::AsyncHandle;
use crate::async_rt::gc_handle::OwnedAsyncHandle;
use crate::gc::HandleTable;
use crate::sync::GcAwareMutex;

#[test]
fn owning_handle_keeps_value_reachable() {
  let table: GcAwareMutex<HandleTable<usize>> = GcAwareMutex::new(HandleTable::new());
  let mut value = Box::new(123usize);
  let ptr = NonNull::from(value.as_mut());
  let owned: OwnedAsyncHandle<'_, usize> = OwnedAsyncHandle::new(&table, ptr);

  let raw: AsyncHandle<usize> = owned.raw();
  assert_eq!(table.lock().get(raw.into()).unwrap().as_ptr(), ptr.as_ptr());
}

#[test]
fn discard_frees_entry() {
  let table: GcAwareMutex<HandleTable<usize>> = GcAwareMutex::new(HandleTable::new());
  let mut value = Box::new(123usize);
  let ptr = NonNull::from(value.as_mut());

  let owned: OwnedAsyncHandle<'_, usize> = OwnedAsyncHandle::new(&table, ptr);
  let raw: AsyncHandle<usize> = owned.raw();
  owned.discard();

  assert_eq!(table.lock().get(raw.into()), None);
}

#[test]
fn drop_frees_entry() {
  let table: GcAwareMutex<HandleTable<usize>> = GcAwareMutex::new(HandleTable::new());
  let mut value = Box::new(123usize);
  let ptr = NonNull::from(value.as_mut());

  let raw: AsyncHandle<usize> = {
    let owned: OwnedAsyncHandle<'_, usize> = OwnedAsyncHandle::new(&table, ptr);
    owned.raw()
  };

  assert_eq!(table.lock().get(raw.into()), None);
}

#[test]
fn u64_round_trip() {
  let table: GcAwareMutex<HandleTable<usize>> = GcAwareMutex::new(HandleTable::new());
  let mut value = Box::new(123usize);
  let ptr = NonNull::from(value.as_mut());
  let owned: OwnedAsyncHandle<'_, usize> = OwnedAsyncHandle::new(&table, ptr);

  let raw: AsyncHandle<usize> = owned.raw();
  let raw_u64: u64 = raw.into();

  let raw2: AsyncHandle<usize> = raw_u64.into();
  assert_eq!(raw, raw2);
  assert_eq!(raw2.into_raw(), raw_u64);
}

#[test]
fn gc_handle_is_send_sync_regardless_of_t() {
  fn assert_send_sync<T: Send + Sync>() {}

  // `Rc` is not Send/Sync; the handle should still be.
  assert_send_sync::<AsyncHandle<std::rc::Rc<()>>>();
}
