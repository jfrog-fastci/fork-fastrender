//! Linux `io_uring` driver building blocks.
//!
//! This crate contains:
//! - A GC-safe low-level driver ([`IoUringDriver`]) built around an explicit [`IoBuf`] abstraction.
//! - A mock moving/compacting GC (`mock_gc::MockGc`) used by tests.
//! - A small "legacy" driver ([`Driver`]) with `PreparedOp` helpers
//!   (read/openat/statx/connect/accept), linked timeouts, and cancellation probing.
//! - A provided-buffer pool ([`ProvidedBufPool`]) for pointer-free `recv`/`read` submissions via
//!   `IORING_OP_PROVIDE_BUFFERS` + `IOSQE_BUFFER_SELECT` (useful to avoid passing pointers into
//!   movable/GC-managed memory).
//! - Explicit multi-shot ops (currently `recvmsg`) that keep kernel-referenced metadata alive until
//!   the final CQE.
//!
//! ## Safety / pointer lifetime model
//! Any user memory referenced by an SQE **must remain valid and stable** until the kernel produces
//! the corresponding CQE, and userspace has processed it.
//!
//! This includes buffers, iovec metadata, path strings, `Timespec` values used by linked timeouts,
//! and any extra keep-alive resources stored alongside ops.
//!
//! ## Drop / teardown semantics
//! - Dropping an [`IoOp`] handle detaches the caller from the completion result, but does **not**
//!   free any SQE-referenced resources early. Resources are owned by the driver and are released
//!   exactly once by CQE processing.
//! - Driver teardown policy (policy **B**):
//!   - Drivers must be explicitly driven to completion (and/or canceled) before being dropped.
//!   - In debug builds (or with `debug_stability`), dropping a driver with in-flight ops panics
//!     (unless already panicking).
//!   - In release builds, dropping a driver with in-flight ops leaks the ring + in-flight state to
//!     prevent use-after-free.
//!
//! Optional feature flags:
//! - `debug_stability`: records kernel pointer graphs at submission time and asserts they are
//!   unchanged when processing CQEs. This is intended as a development-time safety net for moving
//!   GC integrations (missing pinning/roots).
//! - `send_zc`: enables `IORING_OP_SEND_ZC` support (see policy below).
//!
//! ## `IORING_OP_SEND_ZC` policy
//!
//! Zero-copy send (`IORING_OP_SEND_ZC`) is a sharp edge:
//! - The initial CQE only reports completion of the send request.
//! - The kernel may keep user pages pinned and continue accessing them afterwards.
//! - A **notification CQE** (`IORING_CQE_F_NOTIF`) indicates when the kernel is done with the
//!   user pages.
//!
//! For runtimes with moving GC, releasing pins/roots after the initial CQE can lead to
//! use-after-free. To prevent accidental misuse, `SEND_ZC` support is **disabled by default** and
//! must be enabled explicitly via the crate feature `send_zc`.

#[cfg(target_os = "linux")]
mod debug_stability;

pub mod buf;
pub mod driver;
pub mod gc;
pub mod mock_gc;
pub mod pool;
pub mod multishot;

#[cfg(target_os = "linux")]
mod op_connect_accept;

#[cfg(target_os = "linux")]
pub use op_connect_accept::{AcceptAddr, ConnectAddr};

mod legacy;
mod timeout;

#[cfg(target_os = "linux")]
mod op_readv_writev;
#[cfg(target_os = "linux")]
mod op_sendmsg_recvmsg;

#[cfg(target_os = "linux")]
pub use op_sendmsg_recvmsg::{RecvMsg, RecvMsgResource, SendMsg};
#[cfg(all(target_os = "linux", feature = "send_zc"))]
mod send_zc;

pub use buf::{GcIoBuf, IoBuf, IoBufMut, OwnedIoBuf};
pub use driver::{IoOp, IoUringDriver, OpCompletion, OpId};
pub use gc::{GcHooks, GcPinGuard, GcRoot};
pub use legacy::{
    is_accept_supported, is_async_cancel_supported, is_connect_supported, is_link_timeout_supported,
    is_provide_buffers_supported, is_remove_buffers_supported, Completion, Driver, OpWithTimeout,
    PreparedOp, WeakDriver,
};
pub use multishot::{
    MultiShotEnd, MultiShotHandle, MultiShotId, MultiShotRecvMsgErr, MultiShotRecvMsgEvent,
    MultiShotRecvMsgShot,
};
pub use pool::{LeasedBuf, PoolStats, ProvidedBufPool};

#[cfg(all(target_os = "linux", feature = "send_zc"))]
pub use send_zc::{SendZcFlags, SendZcNotif, SendZcResource};

#[cfg(test)]
mod tests;
