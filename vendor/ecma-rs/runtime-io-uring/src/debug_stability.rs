use crate::OpId;

#[cfg(feature = "debug_stability")]
use std::fmt;

#[cfg(feature = "debug_stability")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum PtrKind {
  /// Data pointer returned from an [`IoBuf`] passed to the kernel.
  IoBufData { index: usize },
  /// Pointer to a `__kernel_timespec` / `Timespec` struct.
  Timespec,
  /// Pointer to a NUL-terminated pathname (`CString`) passed to the kernel.
  Path,
  /// Pointer to an output buffer (e.g. `statx` out-parameter).
  OutParam,
  /// Pointer to an `iovec[]` array.
  IovecArray,
  /// Pointer to a `msghdr`.
  MsgHdr,
  /// Pointer to a `msghdr.msg_control` buffer (ancillary data).
  MsgControl,
  /// Pointer to a `sockaddr` / `sockaddr_storage`.
  SockAddr,
  /// Base pointer for a provided-buffer pool.
  ProvidedBufPoolBase { group_id: u16 },
}

#[cfg(feature = "debug_stability")]
impl fmt::Display for PtrKind {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::IoBufData { index } => write!(f, "IoBufData[{index}]"),
      Self::Timespec => write!(f, "timespec"),
      Self::Path => write!(f, "path"),
      Self::OutParam => write!(f, "out_param"),
      Self::IovecArray => write!(f, "iovec[]"),
      Self::MsgHdr => write!(f, "msghdr"),
      Self::MsgControl => write!(f, "msg_control"),
      Self::SockAddr => write!(f, "sockaddr"),
      Self::ProvidedBufPoolBase { group_id } => write!(f, "provided_buf_pool_base[group={group_id}]"),
    }
  }
}

#[cfg(feature = "debug_stability")]
#[derive(Clone, Copy, Debug)]
struct PtrRecord {
  kind: PtrKind,
  addr: usize,
}

#[cfg(feature = "debug_stability")]
#[derive(Debug)]
pub(crate) struct OpStability {
  op_id: OpId,
  ptrs: Vec<PtrRecord>,
}

#[cfg(feature = "debug_stability")]
#[derive(Debug)]
pub(crate) struct Recorder {
  ptrs: Vec<PtrRecord>,
}

#[cfg(feature = "debug_stability")]
impl Recorder {
  #[inline]
  pub(crate) fn ptr(&mut self, kind: PtrKind, ptr: *const u8) {
    self.ptrs.push(PtrRecord {
      kind,
      addr: ptr as usize,
    });
  }
}

#[cfg(feature = "debug_stability")]
#[inline]
pub(crate) fn record(op_id: OpId, f: impl FnOnce(&mut Recorder)) -> OpStability {
  let mut rec = Recorder { ptrs: Vec::new() };
  f(&mut rec);
  OpStability {
    op_id,
    ptrs: rec.ptrs,
  }
}

#[cfg(feature = "debug_stability")]
#[inline]
pub(crate) fn assert_stable(stability: &OpStability, f: impl FnOnce(&mut Recorder)) {
  let mut rec = Recorder { ptrs: Vec::new() };
  f(&mut rec);

  if stability.ptrs.len() != rec.ptrs.len() {
    panic!(
      "runtime-io-uring debug_stability: op_id={:?} pointer set changed: expected {} ptrs, got {}",
      stability.op_id,
      stability.ptrs.len(),
      rec.ptrs.len(),
    );
  }

  for (expected, actual) in stability.ptrs.iter().zip(rec.ptrs.iter()) {
    if expected.kind != actual.kind {
      panic!(
        "runtime-io-uring debug_stability: op_id={:?} pointer kind mismatch: expected {}, got {}",
        stability.op_id, expected.kind, actual.kind
      );
    }
    if expected.addr != actual.addr {
      panic!(
        "runtime-io-uring debug_stability: op_id={:?} pointer moved for {}: expected {:#x}, got {:#x}",
        stability.op_id, expected.kind, expected.addr, actual.addr
      );
    }
  }
}

// No-op versions for release builds / when instrumentation is disabled.
#[cfg(not(feature = "debug_stability"))]
#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
pub(crate) enum PtrKind {
  IoBufData { index: usize },
  Timespec,
  Path,
  OutParam,
  IovecArray,
  MsgHdr,
  MsgControl,
  SockAddr,
  ProvidedBufPoolBase { group_id: u16 },
}

#[cfg(not(feature = "debug_stability"))]
pub(crate) type OpStability = ();

#[cfg(not(feature = "debug_stability"))]
pub(crate) struct Recorder;

#[cfg(not(feature = "debug_stability"))]
impl Recorder {
  #[inline]
  pub(crate) fn ptr(&mut self, _kind: PtrKind, _ptr: *const u8) {}
}

#[cfg(not(feature = "debug_stability"))]
#[inline]
pub(crate) fn record(_op_id: OpId, _f: impl FnOnce(&mut Recorder)) -> OpStability {}

#[cfg(not(feature = "debug_stability"))]
#[inline]
pub(crate) fn assert_stable(_stability: &OpStability, _f: impl FnOnce(&mut Recorder)) {}
