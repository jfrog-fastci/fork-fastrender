use super::iovec::PinnedIoVec;
#[cfg(unix)]
use super::iovec::PinnedMsgHdr;
use super::limits::{IoLimitError, IoLimiter, IoPermit};
use crate::buffer::{
  ArrayBuffer, ArrayBufferError, BackingStore, BorrowError, BorrowGuardRead, BorrowGuardWrite,
  PinnedBackingStore, TypedArrayError, Uint8Array,
};
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;

/// Kernel-pointer view of a pinned buffer range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IoBuf {
  ptr: *const u8,
  len: usize,
}

// SAFETY: `IoBuf` is a raw kernel pointer view into a pinned backing store. The owning `IoOp`
// keeps the backing store alive for the duration of the operation; dereferencing the pointer is
// always unsafe.
unsafe impl Send for IoBuf {}
unsafe impl Sync for IoBuf {}

impl IoBuf {
  #[inline]
  pub fn as_ptr(&self) -> *const u8 {
    self.ptr
  }

  #[inline]
  pub fn as_mut_ptr(&self) -> *mut u8 {
    self.ptr as *mut u8
  }

  #[inline]
  pub fn len(&self) -> usize {
    self.len
  }
}

/// An in-flight I/O operation.
///
/// This type owns:
/// - pinned backing stores (to keep the pointers alive),
/// - optional `iovec[]` descriptor memory for vectored syscalls/io_uring, and
/// - an accounting permit that is released on drop (completion/cancellation).
#[must_use = "IoOp must be kept alive to keep backing stores pinned/borrowed and to hold limiter permits"]
#[derive(Debug)]
pub struct IoOp {
  bufs: Vec<IoBuf>,
  // NOTE: keep this after `bufs` so backing stores outlive the pointer descriptors.
  _pinned: Vec<PinnedBackingStore>,
  // For write-like ops (`write(2)`, `send(2)`): kernel reads from the buffer.
  _borrows_read: Vec<BorrowGuardRead>,
  // For read-like ops (`read(2)`, `recv(2)`): kernel writes into the buffer.
  _borrows_write: Vec<BorrowGuardWrite>,
  #[allow(dead_code)]
  pinned_iovecs: Option<PinnedIoVec>,
  #[cfg(unix)]
  #[allow(dead_code)]
  pinned_msghdr: Option<PinnedMsgHdr>,
  _permit: IoPermit,
}

impl IoOp {
  /// Pins a single [`BackingStore`] range for an I/O operation.
  pub fn pin_backing_store_range(
    limiter: &Arc<IoLimiter>,
    store: BackingStore,
    range: Range<usize>,
  ) -> Result<Self, IoLimitError> {
    Self::pin_vectored(limiter, vec![(store, range)])
  }

  /// Pins a single [`BackingStore`] range for a read-like I/O operation.
  ///
  /// This is intended for `read(2)`/`recv(2)`-style ops where the kernel may write into the user
  /// buffer. It acquires an exclusive write borrow on the backing store for the lifetime of the op.
  pub fn pin_backing_store_range_for_read(
    limiter: &Arc<IoLimiter>,
    store: BackingStore,
    range: Range<usize>,
  ) -> Result<Self, IoLimitError> {
    if range.start > range.end || range.end > store.byte_len() {
      return Err(IoLimitError::InvalidRange);
    }
    let total_pinned_bytes = store.alloc_len();
    let permit = limiter.try_acquire(total_pinned_bytes)?;

    let borrow = store.try_borrow_io_write().map_err(|err| match err {
      BorrowError::Borrowed => IoLimitError::BufferBorrowed,
      BorrowError::ReadBorrowOverflow => IoLimitError::LimitExceeded("max read borrows"),
      BorrowError::NotUnique => IoLimitError::BufferBorrowed,
    })?;

    let pinned = store.pin_guard();
    let base = pinned.as_ptr();
    // SAFETY: bounds checked above.
    let ptr = unsafe { base.add(range.start) } as *const u8;
    let len = range.end - range.start;

    Ok(Self {
      bufs: vec![IoBuf { ptr, len }],
      _pinned: vec![pinned],
      _borrows_read: Vec::new(),
      _borrows_write: vec![borrow],
      pinned_iovecs: None,
      #[cfg(unix)]
      pinned_msghdr: None,
      _permit: permit,
    })
  }

  /// Pins a single [`ArrayBuffer`] range for an I/O operation.
  pub fn pin_array_buffer_range(
    limiter: &Arc<IoLimiter>,
    buf: &ArrayBuffer,
    range: Range<usize>,
  ) -> Result<Self, IoLimitError> {
    if buf.is_detached() {
      return Err(IoLimitError::BufferNotAlive);
    }
    if range.start > range.end || range.end > buf.byte_len() {
      return Err(IoLimitError::InvalidRange);
    }

    let Some(store) = buf.backing_store_handle() else {
      return Err(IoLimitError::BufferNotAlive);
    };
    Self::pin_backing_store_range(limiter, store, range)
  }

  /// Pins a single [`ArrayBuffer`] range for a read-like I/O operation.
  pub fn pin_array_buffer_range_for_read(
    limiter: &Arc<IoLimiter>,
    buf: &ArrayBuffer,
    range: Range<usize>,
  ) -> Result<Self, IoLimitError> {
    if buf.is_detached() {
      return Err(IoLimitError::BufferNotAlive);
    }
    if range.start > range.end || range.end > buf.byte_len() {
      return Err(IoLimitError::InvalidRange);
    }

    let Some(store) = buf.backing_store_handle() else {
      return Err(IoLimitError::BufferNotAlive);
    };
    Self::pin_backing_store_range_for_read(limiter, store, range)
  }

  /// Pins a [`Uint8Array`] sub-range for an I/O operation.
  ///
  /// The provided `range` is relative to the start of the view (not the underlying `ArrayBuffer`).
  pub fn pin_uint8_array_range(
    limiter: &Arc<IoLimiter>,
    view: &Uint8Array,
    range: Range<usize>,
  ) -> Result<Self, IoLimitError> {
    if view.is_detached() {
      return Err(IoLimitError::BufferNotAlive);
    }
    if range.start > range.end || range.end > view.length() {
      return Err(IoLimitError::InvalidRange);
    }

    let store = view.backing_store_handle().map_err(|err| match err {
      TypedArrayError::Buffer(ArrayBufferError::Detached) => IoLimitError::BufferNotAlive,
      _ => IoLimitError::InvalidRange,
    })?;

    let view_base = view.byte_offset();
    let abs_start = view_base
      .checked_add(range.start)
      .ok_or(IoLimitError::InvalidRange)?;
    let abs_end = view_base.checked_add(range.end).ok_or(IoLimitError::InvalidRange)?;

    Self::pin_backing_store_range(limiter, store, abs_start..abs_end)
  }

  /// Pins an entire [`Uint8Array`] view for an I/O operation.
  pub fn pin_uint8_array(limiter: &Arc<IoLimiter>, view: &Uint8Array) -> Result<Self, IoLimitError> {
    Self::pin_uint8_array_range(limiter, view, 0..view.length())
  }

  /// Pins a [`Uint8Array`] sub-range for a read-like I/O operation.
  ///
  /// The kernel may write into the buffer (`read(2)`, `recv(2)`), so this acquires an exclusive
  /// write borrow for the lifetime of the op.
  ///
  /// The provided `range` is relative to the start of the view (not the underlying `ArrayBuffer`).
  pub fn pin_uint8_array_range_for_read(
    limiter: &Arc<IoLimiter>,
    view: &Uint8Array,
    range: Range<usize>,
  ) -> Result<Self, IoLimitError> {
    if view.is_detached() {
      return Err(IoLimitError::BufferNotAlive);
    }
    if range.start > range.end || range.end > view.length() {
      return Err(IoLimitError::InvalidRange);
    }

    let store = view.backing_store_handle().map_err(|err| match err {
      TypedArrayError::Buffer(ArrayBufferError::Detached) => IoLimitError::BufferNotAlive,
      _ => IoLimitError::InvalidRange,
    })?;

    let view_base = view.byte_offset();
    let abs_start = view_base
      .checked_add(range.start)
      .ok_or(IoLimitError::InvalidRange)?;
    let abs_end = view_base.checked_add(range.end).ok_or(IoLimitError::InvalidRange)?;

    Self::pin_backing_store_range_for_read(limiter, store, abs_start..abs_end)
  }

  /// Pins an entire [`Uint8Array`] view for a read-like I/O operation.
  pub fn pin_uint8_array_for_read(
    limiter: &Arc<IoLimiter>,
    view: &Uint8Array,
  ) -> Result<Self, IoLimitError> {
    Self::pin_uint8_array_range_for_read(limiter, view, 0..view.length())
  }

  /// Pins multiple ranges for a single vectored I/O operation.
  ///
  /// Accounting charges the sum of `alloc_len()` for all backing stores retained by this op.
  /// Backing stores are deduplicated within the op (charging each allocation once), but the op still
  /// counts as **one** in-flight operation.
  pub fn pin_vectored(
    limiter: &Arc<IoLimiter>,
    bufs: Vec<(BackingStore, Range<usize>)>,
  ) -> Result<Self, IoLimitError> {
    // Validate ranges and compute total bytes up-front so error paths don't affect counters.
    //
    // IMPORTANT: async ops retain the *entire* backing store allocation against detach/free while
    // pinned, even if the syscall uses only a sub-range. Charge `alloc_len()` (not range length).
    let mut total_pinned_bytes: usize = 0;
    let mut seen_store_ids: HashSet<usize> = HashSet::with_capacity(bufs.len());
    for (store, range) in bufs.iter() {
      if range.start > range.end || range.end > store.byte_len() {
        return Err(IoLimitError::InvalidRange);
      }

      if seen_store_ids.insert(store.id()) {
        total_pinned_bytes = total_pinned_bytes
          .checked_add(store.alloc_len())
          .ok_or(IoLimitError::LimitExceeded("max pinned bytes"))?;
      }
    }

    // Apply backpressure (deterministic error) before producing kernel pointers.
    let permit = limiter.try_acquire(total_pinned_bytes)?;

    // Acquire shared "kernel reads from buffer" borrows for each unique backing store.
    let mut borrows: Vec<BorrowGuardRead> = Vec::with_capacity(seen_store_ids.len());
    let mut borrow_order: Vec<BackingStore> = Vec::with_capacity(seen_store_ids.len());
    let mut seen_for_borrow: HashSet<usize> = HashSet::with_capacity(seen_store_ids.len());
    for (store, _) in bufs.iter() {
      if seen_for_borrow.insert(store.id()) {
        borrow_order.push(store.clone());
      }
    }
    borrow_order.sort_by_key(|s| s.id());
    for store in borrow_order {
      let guard = store.try_borrow_io_read().map_err(|err| match err {
        BorrowError::Borrowed => IoLimitError::BufferBorrowed,
        BorrowError::ReadBorrowOverflow => IoLimitError::LimitExceeded("max read borrows"),
        BorrowError::NotUnique => IoLimitError::BufferBorrowed,
      })?;
      borrows.push(guard);
    }

    let mut io_bufs: Vec<IoBuf> = Vec::with_capacity(bufs.len());
    let mut pinned: Vec<PinnedBackingStore> = Vec::with_capacity(seen_store_ids.len());
    let mut pinned_index: HashMap<usize, usize> = HashMap::with_capacity(seen_store_ids.len());

    // Pin unique backing stores first, then create OS-visible pointer descriptors.
    for (store, _) in bufs.iter() {
      let id = store.id();
      if pinned_index.contains_key(&id) {
        continue;
      }
      pinned_index.insert(id, pinned.len());
      pinned.push(store.pin_guard());
    }

    for (store, range) in bufs {
      let id = store.id();
      let idx = *pinned_index.get(&id).expect("store pinned above");

      let base = pinned[idx].as_ptr();
      // SAFETY: bounds checked above.
      let ptr = unsafe { base.add(range.start) } as *const u8;
      let len = range.end - range.start;
      io_bufs.push(IoBuf { ptr, len });
    }

    Ok(Self {
      bufs: io_bufs,
      _pinned: pinned,
      _borrows_read: borrows,
      _borrows_write: Vec::new(),
      pinned_iovecs: None,
      #[cfg(unix)]
      pinned_msghdr: None,
      _permit: permit,
    })
  }

  /// Pins a list of backing-store rooted buffers described by a [`PinnedIoVec`].
  ///
  /// This is useful for operations that work with JS `ArrayBuffer`/`TypedArray` backing stores: the
  /// returned [`PinnedIoVec`] owns pin guards that prevent detach/transfer/resize while in flight.
  ///
  /// The `PinnedIoVec` also provides stable `iov_base` pointers; this method charges the
  /// corresponding byte ranges against the [`IoLimiter`] and exposes the buffers as [`IoBuf`]s.
  pub fn pin_iovecs(
    limiter: &Arc<IoLimiter>,
    pinned_iovecs: PinnedIoVec,
  ) -> Result<Self, IoLimitError> {
    let total_pinned_bytes = pinned_iovecs
      .retained_alloc_len_deduped()
      .ok_or(IoLimitError::LimitExceeded("max pinned bytes"))?;

    let permit = limiter.try_acquire(total_pinned_bytes)?;

    let mut io_bufs: Vec<IoBuf> = Vec::with_capacity(pinned_iovecs.len());
    for iov in pinned_iovecs.as_iovecs() {
      io_bufs.push(IoBuf {
        ptr: iov.iov_base as *const u8,
        len: iov.iov_len,
      });
    }

    let mut stores = pinned_iovecs.unique_backing_stores();
    stores.sort_by_key(|s| s.id());
    let mut borrows: Vec<BorrowGuardRead> = Vec::with_capacity(stores.len());
    for store in stores {
      let guard = store.try_borrow_io_read().map_err(|err| match err {
        BorrowError::Borrowed => IoLimitError::BufferBorrowed,
        BorrowError::ReadBorrowOverflow => IoLimitError::LimitExceeded("max read borrows"),
        BorrowError::NotUnique => IoLimitError::BufferBorrowed,
      })?;
      borrows.push(guard);
    }

    Ok(Self {
      bufs: io_bufs,
      // The backing stores are owned (and pinned) by the `PinnedIoVec` itself.
      _pinned: Vec::new(),
      _borrows_read: borrows,
      _borrows_write: Vec::new(),
      pinned_iovecs: Some(pinned_iovecs),
      #[cfg(unix)]
      pinned_msghdr: None,
      _permit: permit,
    })
  }

  /// Pins a list of backing-store rooted buffers described by a [`PinnedIoVec`] for a read-like op.
  ///
  /// This is intended for `read(2)`/`recv(2)`-style operations where the kernel may write into the
  /// provided buffers. It acquires an exclusive write borrow on each unique backing store for the
  /// lifetime of the op.
  pub fn pin_iovecs_for_read(
    limiter: &Arc<IoLimiter>,
    pinned_iovecs: PinnedIoVec,
  ) -> Result<Self, IoLimitError> {
    let total_pinned_bytes = pinned_iovecs
      .retained_alloc_len_deduped()
      .ok_or(IoLimitError::LimitExceeded("max pinned bytes"))?;
 
    let permit = limiter.try_acquire(total_pinned_bytes)?;
 
    let mut io_bufs: Vec<IoBuf> = Vec::with_capacity(pinned_iovecs.len());
    for iov in pinned_iovecs.as_iovecs() {
      io_bufs.push(IoBuf {
        ptr: iov.iov_base as *const u8,
        len: iov.iov_len,
      });
    }
 
    let mut stores = pinned_iovecs.unique_backing_stores();
    stores.sort_by_key(|s| s.id());
    let mut borrows: Vec<BorrowGuardWrite> = Vec::with_capacity(stores.len());
    for store in stores {
      let guard = store.try_borrow_io_write().map_err(|err| match err {
        BorrowError::Borrowed => IoLimitError::BufferBorrowed,
        BorrowError::ReadBorrowOverflow => IoLimitError::LimitExceeded("max read borrows"),
        BorrowError::NotUnique => IoLimitError::BufferBorrowed,
      })?;
      borrows.push(guard);
    }
 
    Ok(Self {
      bufs: io_bufs,
      // The backing stores are owned (and pinned) by the `PinnedIoVec` itself.
      _pinned: Vec::new(),
      _borrows_read: Vec::new(),
      _borrows_write: borrows,
      pinned_iovecs: Some(pinned_iovecs),
      #[cfg(unix)]
      pinned_msghdr: None,
      _permit: permit,
    })
  }

  /// Attaches a pinned `iovec[]` descriptor list to this op.
  ///
  /// This is intended for io_uring and other async APIs that require the `iovec[]` array itself to
  /// remain valid until completion.
  pub fn set_pinned_iovecs(&mut self, pinned_iovecs: PinnedIoVec) {
    self.pinned_iovecs = Some(pinned_iovecs);
  }

  pub fn pinned_iovecs(&self) -> Option<&PinnedIoVec> {
    self.pinned_iovecs.as_ref()
  }

  /// Attaches a pinned `msghdr` descriptor to this op.
  ///
  /// `msghdr`-based syscalls and io_uring operations require the `msghdr` struct (and any pointed-to
  /// buffers like `iovec[]` and `msg_control`) to remain valid until completion.
  #[cfg(unix)]
  pub fn set_pinned_msghdr(&mut self, pinned_msghdr: PinnedMsgHdr) {
    self.pinned_msghdr = Some(pinned_msghdr);
  }

  #[cfg(unix)]
  pub fn pinned_msghdr(&self) -> Option<&PinnedMsgHdr> {
    self.pinned_msghdr.as_ref()
  }

  #[inline]
  pub fn bufs(&self) -> &[IoBuf] {
    self.bufs.as_slice()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::buffer::{ArrayBuffer, ArrayBufferError, BorrowError, GlobalBackingStoreAllocator};
  use crate::io::limits::IoLimits;
  use std::sync::Arc;

  fn limiter_with_max_bytes(max_pinned_bytes: usize) -> Arc<IoLimiter> {
    Arc::new(IoLimiter::new(IoLimits {
      max_pinned_bytes,
      max_inflight_ops: 8,
      max_pinned_bytes_per_op: None,
    }))
  }

  #[test]
  fn pin_backing_store_range_charges_alloc_len_not_range_len() {
    let alloc = GlobalBackingStoreAllocator::default();
    let buf = ArrayBuffer::new_zeroed_in(&alloc, 16).unwrap();
    let store = buf.backing_store_handle().unwrap();

    // Range is only 1 byte, but the op retains the entire backing store allocation.
    let limiter = limiter_with_max_bytes(1);
    assert_eq!(
      IoOp::pin_backing_store_range(&limiter, store.clone(), 0..1).unwrap_err(),
      IoLimitError::LimitExceeded("max pinned bytes")
    );
    assert_eq!(limiter.counters().pinned_bytes_current, 0);
    assert_eq!(limiter.counters().inflight_ops_current, 0);
  }

  #[test]
  fn pin_backing_store_range_charges_alloc_len_for_adopted_vec_capacity() {
    let alloc = GlobalBackingStoreAllocator::default();

    // Construct an ArrayBuffer from a Vec that intentionally has len << capacity. When we can adopt
    // the Vec allocation without copying, `BackingStore::alloc_len()` should reflect `capacity`, and
    // I/O accounting must charge that full allocation (even if we only pin a 1-byte range).
    let (buf, store) = (0..128)
      .find_map(|_| {
        let mut bytes = Vec::with_capacity(1024);
        bytes.push(0u8);
        let buf = ArrayBuffer::from_bytes_in(&alloc, bytes).ok()?;
        let store = buf.backing_store_handle()?;
        (store.alloc_len() > buf.byte_len()).then_some((buf, store))
      })
      .expect("failed to construct an adopted Vec-backed store with alloc_len > byte_len");

    let alloc_len = store.alloc_len();
    let byte_len = buf.byte_len();
    assert!(alloc_len > byte_len);

    // Global limit large enough for the visible range but smaller than the retained allocation.
    let limiter = limiter_with_max_bytes(byte_len);
    assert_eq!(
      IoOp::pin_backing_store_range(&limiter, store.clone(), 0..1).unwrap_err(),
      IoLimitError::LimitExceeded("max pinned bytes")
    );

    let ok_limiter = limiter_with_max_bytes(alloc_len);
    let op = IoOp::pin_backing_store_range(&ok_limiter, store.clone(), 0..1).unwrap();
    assert_eq!(ok_limiter.counters().pinned_bytes_current, alloc_len);
    drop(op);
    assert_eq!(ok_limiter.counters().pinned_bytes_current, 0);
  }

  #[test]
  fn pin_vectored_dedupes_backing_store_for_limiter_and_pins_once() {
    let alloc = GlobalBackingStoreAllocator::default();
    let buf = ArrayBuffer::new_zeroed_in(&alloc, 16).unwrap();
    let store = buf.backing_store_handle().unwrap();
    let alloc_len = store.alloc_len();

    let limiter = limiter_with_max_bytes(1024);

    assert_eq!(buf.pin_count(), 0);
    assert!(!store.is_io_borrowed());

    let op = IoOp::pin_vectored(
      &limiter,
      vec![(store.clone(), 0..1), (store.clone(), 1..2)],
    )
    .unwrap();

    assert_eq!(op.bufs().len(), 2);
    assert_eq!(buf.pin_count(), 1, "IoOp should pin each unique store once");
    assert!(store.is_io_borrowed(), "IoOp must hold I/O borrows until dropped");

    let counters = limiter.counters();
    assert_eq!(counters.pinned_bytes_current, alloc_len);
    assert_eq!(counters.inflight_ops_current, 1);

    // Safe access to the backing bytes should be blocked while the op is in flight.
    assert_eq!(
      buf.data_ptr().unwrap_err(),
      ArrayBufferError::Borrow(BorrowError::Borrowed)
    );

    drop(op);

    let counters = limiter.counters();
    assert_eq!(counters.pinned_bytes_current, 0);
    assert_eq!(counters.inflight_ops_current, 0);
    assert_eq!(buf.pin_count(), 0);
    assert!(!store.is_io_borrowed());
    assert!(buf.data_ptr().is_ok());
  }

  #[test]
  fn pin_vectored_rolls_back_permit_when_borrow_fails() {
    let alloc = GlobalBackingStoreAllocator::default();
    let buf = ArrayBuffer::new_zeroed_in(&alloc, 16).unwrap();
    let store = buf.backing_store_handle().unwrap();
    let alloc_len = store.alloc_len();

    let limiter = limiter_with_max_bytes(1024);

    // Hold an exclusive write borrow; this should block the read borrow IoOp tries to take.
    let _write = store.try_borrow_io_write().unwrap();
    assert!(store.is_io_borrowed());

    assert_eq!(
      IoOp::pin_backing_store_range(&limiter, store.clone(), 0..1).unwrap_err(),
      IoLimitError::BufferBorrowed
    );

    // Permit must have been dropped on the error path (no leaked accounting).
    let counters = limiter.counters();
    assert_eq!(counters.pinned_bytes_current, 0);
    assert_eq!(counters.inflight_ops_current, 0);

    // The store should not have been pinned (borrow acquisition happens before pinning).
    assert_eq!(buf.pin_count(), 0);

    // Sanity check: if we allow the borrow, the op is now allowed and charges alloc_len.
    drop(_write);
    let op = IoOp::pin_backing_store_range(&limiter, store.clone(), 0..1).unwrap();
    assert_eq!(limiter.counters().pinned_bytes_current, alloc_len);
    drop(op);
    assert_eq!(limiter.counters().pinned_bytes_current, 0);
  }

  #[test]
  fn pin_backing_store_range_for_read_acquires_write_borrow_and_charges_alloc_len() {
    let alloc = GlobalBackingStoreAllocator::default();
    let buf = ArrayBuffer::new_zeroed_in(&alloc, 16).unwrap();
    let store = buf.backing_store_handle().unwrap();
    let alloc_len = store.alloc_len();

    let limiter = limiter_with_max_bytes(1024);

    let op = IoOp::pin_backing_store_range_for_read(&limiter, store.clone(), 0..1).unwrap();
    assert_eq!(op.bufs().len(), 1);
    assert_eq!(buf.pin_count(), 1);
    assert!(store.is_io_borrowed());

    assert_eq!(
      buf.try_with_slice(|_| ()).unwrap_err(),
      ArrayBufferError::Borrow(BorrowError::Borrowed)
    );
    assert_eq!(store.try_with_slice(|_| ()), Err(BorrowError::Borrowed));

    let counters = limiter.counters();
    assert_eq!(counters.pinned_bytes_current, alloc_len);
    assert_eq!(counters.inflight_ops_current, 1);

    drop(op);

    assert_eq!(buf.pin_count(), 0);
    assert!(!store.is_io_borrowed());

    let counters = limiter.counters();
    assert_eq!(counters.pinned_bytes_current, 0);
    assert_eq!(counters.inflight_ops_current, 0);
  }

  #[test]
  fn pin_backing_store_range_for_read_rolls_back_permit_when_borrow_fails() {
    let alloc = GlobalBackingStoreAllocator::default();
    let buf = ArrayBuffer::new_zeroed_in(&alloc, 16).unwrap();
    let store = buf.backing_store_handle().unwrap();

    let limiter = limiter_with_max_bytes(1024);

    // Hold a shared read borrow; this blocks the exclusive write borrow required for reads.
    let _read = store.try_borrow_io_read().unwrap();
    assert!(store.is_io_borrowed());

    assert_eq!(
      IoOp::pin_backing_store_range_for_read(&limiter, store.clone(), 0..1).unwrap_err(),
      IoLimitError::BufferBorrowed
    );

    let counters = limiter.counters();
    assert_eq!(counters.pinned_bytes_current, 0);
    assert_eq!(counters.inflight_ops_current, 0);

    assert_eq!(buf.pin_count(), 0);
  }
}
