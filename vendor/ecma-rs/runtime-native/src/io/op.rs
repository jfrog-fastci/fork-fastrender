use super::iovec::{PinnedIoVec, PinnedMsgHdr};
use super::limits::{IoLimitError, IoLimiter, IoPermit};
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
  _backings: Vec<Arc<[u8]>>,
  #[allow(dead_code)]
  pinned_iovecs: Option<PinnedIoVec>,
  #[allow(dead_code)]
  pinned_msghdr: Option<PinnedMsgHdr>,
  _permit: IoPermit,
}

impl IoOp {
  /// Pins a single buffer range for an I/O operation.
  ///
  /// This is the "pin_range -> IoBuf" bridge: it is the only place that produces kernel pointers,
  /// so it is where limits/backpressure are enforced.
  pub fn pin_range(
    limiter: &Arc<IoLimiter>,
    backing: Arc<[u8]>,
    range: Range<usize>,
  ) -> Result<Self, IoLimitError> {
    Self::pin_vectored(limiter, vec![(backing, range)])
  }

  /// Pins multiple ranges for a single vectored I/O operation.
  ///
  /// Accounting charges the **sum** of all pinned ranges, but counts as **one** in-flight op.
  pub fn pin_vectored(
    limiter: &Arc<IoLimiter>,
    bufs: Vec<(Arc<[u8]>, Range<usize>)>,
  ) -> Result<Self, IoLimitError> {
    // Validate ranges and compute total bytes up-front so error paths don't affect counters.
    let mut total_pinned_bytes: usize = 0;
    for (backing, range) in bufs.iter() {
      let len = range
        .end
        .checked_sub(range.start)
        .ok_or(IoLimitError::InvalidRange)?;
      if range.end > backing.len() {
        return Err(IoLimitError::InvalidRange);
      }
      total_pinned_bytes = total_pinned_bytes
        .checked_add(len)
        .ok_or(IoLimitError::LimitExceeded("max pinned bytes"))?;
    }

    // Apply backpressure (deterministic error) before producing kernel pointers.
    let permit = limiter.try_acquire(total_pinned_bytes)?;

    let mut io_bufs: Vec<IoBuf> = Vec::with_capacity(bufs.len());
    let mut backings: Vec<Arc<[u8]>> = Vec::with_capacity(bufs.len());
    for (backing, range) in bufs {
      let slice = backing.get(range).ok_or(IoLimitError::InvalidRange)?;
      io_bufs.push(IoBuf {
        ptr: slice.as_ptr(),
        len: slice.len(),
      });
      backings.push(backing);
    }

    Ok(Self {
      bufs: io_bufs,
      _backings: backings,
      pinned_iovecs: None,
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
  pub fn set_pinned_msghdr(&mut self, pinned_msghdr: PinnedMsgHdr) {
    self.pinned_msghdr = Some(pinned_msghdr);
  }

  pub fn pinned_msghdr(&self) -> Option<&PinnedMsgHdr> {
    self.pinned_msghdr.as_ref()
  }

  #[inline]
  pub fn bufs(&self) -> &[IoBuf] {
    self.bufs.as_slice()
  }
}
