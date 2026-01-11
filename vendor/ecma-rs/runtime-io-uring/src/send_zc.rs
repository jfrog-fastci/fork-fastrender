//! `IORING_OP_SEND_ZC` support (zero-copy send).
//!
//! # Buffer lifetime
//!
//! `SEND_ZC` can outlive the initial CQE: the kernel may keep user pages pinned
//! after the send CQE has been reported. The **notification CQE** (`IORING_CQE_F_NOTIF`)
//! indicates when those pages are released and it is safe to drop/unpin/reuse the
//! buffer. See crate-level docs for the policy and safety rationale.

use std::os::fd::RawFd;

use io_uring::opcode;
use io_uring::squeue;
use io_uring::types;

use crate::buf::IoBuf;

// CQE flag bits from `linux/io_uring.h` (kept local to avoid relying on libc headers).
pub(crate) const IORING_CQE_F_MORE: u32 = 1 << 1;
pub(crate) const IORING_CQE_F_NOTIF: u32 = 1 << 3;

/// Flags for `IORING_OP_SEND_ZC`.
#[derive(Clone, Copy, Debug, Default)]
pub struct SendZcFlags {
  /// Message flags passed to `send(2)` (e.g. `MSG_NOSIGNAL`).
  pub msg_flags: i32,
  /// `SEND_ZC`-specific flags (e.g. `IORING_SEND_ZC_REPORT_USAGE`).
  pub zc_flags: u16,
}

/// The notification CQE emitted by the kernel when it releases pinned user pages.
#[derive(Clone, Copy, Debug)]
pub struct SendZcNotif {
  pub result: i32,
  pub flags: u32,
}

/// Resource returned by the driver for `SEND_ZC`.
#[derive(Debug)]
pub struct SendZcResource<B> {
  pub buf: B,
  /// Notification CQE if the kernel pinned pages and emitted one.
  pub notif: Option<SendZcNotif>,
  /// CQE flags from the main send CQE (e.g. includes `IORING_CQE_F_MORE`).
  pub send_flags: u32,
}

pub(crate) fn build_sqe<B: IoBuf>(fd: RawFd, buf: &B, flags: SendZcFlags) -> squeue::Entry {
  let ptr = buf.stable_ptr().as_ptr() as *const u8;
  let len = buf.len();

  opcode::SendZc::new(types::Fd(fd), ptr, len as _)
    .flags(flags.msg_flags)
    .zc_flags(flags.zc_flags)
    .build()
}

