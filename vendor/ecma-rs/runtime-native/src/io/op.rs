use super::iovec::PinnedIoVec;
#[cfg(unix)]
use super::iovec::PinnedMsgHdr;
use super::limits::{IoLimitError, IoLimiter, IoPermit};
use crate::buffer::{ArrayBuffer, BackingStore, PinnedBackingStore, Uint8Array};
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;

/// Kernel-pointer view of a pinned buffer range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IoBuf {
  ptr: *const u8,
  len: usize,
}

impl IoBuf {
  #[inline]
  pub fn as_ptr(&self) -> *const u8 {
    self.ptr
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
#[derive(Debug)]
pub struct IoOp {
  bufs: Vec<IoBuf>,
  // NOTE: keep this after `bufs` so backing stores outlive the pointer descriptors.
  _pinned: Vec<PinnedBackingStore>,
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

  /// Pins a single [`ArrayBuffer`] range for an I/O operation.
  pub fn pin_array_buffer_range(
    limiter: &Arc<IoLimiter>,
    buf: &ArrayBuffer,
    range: Range<usize>,
  ) -> Result<Self, IoLimitError> {
    if range.start > range.end || range.end > buf.byte_len() {
      return Err(IoLimitError::InvalidRange);
    }

    let Some(store) = buf.backing_store_handle() else {
      return Err(IoLimitError::InvalidRange);
    };
    Self::pin_backing_store_range(limiter, store, range)
  }

  /// Pins a [`Uint8Array`] sub-range for an I/O operation.
  ///
  /// The provided `range` is relative to the start of the view (not the underlying `ArrayBuffer`).
  pub fn pin_uint8_array_range(
    limiter: &Arc<IoLimiter>,
    view: &Uint8Array,
    range: Range<usize>,
  ) -> Result<Self, IoLimitError> {
    if range.start > range.end || range.end > view.length() {
      return Err(IoLimitError::InvalidRange);
    }

    let store = view
      .backing_store_handle()
      .map_err(|_| IoLimitError::InvalidRange)?;

    let view_base = view.byte_offset();
    let abs_start = view_base
      .checked_add(range.start)
      .ok_or(IoLimitError::InvalidRange)?;
    let abs_end = view_base.checked_add(range.end).ok_or(IoLimitError::InvalidRange)?;

    Self::pin_backing_store_range(limiter, store, abs_start..abs_end)
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

    Ok(Self {
      bufs: io_bufs,
      _pinned: Vec::new(),
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
