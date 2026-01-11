//! Linux `io_uring` driver building blocks.
//!
//! This crate contains:
//! - A GC-safe low-level driver (`IoUringDriver`) built around an explicit [`IoBuf`] abstraction.
//! - A mock moving/compacting GC (`MockGc`) used by tests.
//! - A small "legacy" driver (`Driver`) with `PreparedOp` helpers (read/openat/statx/connect/accept),
//!   linked timeouts, and cancellation probing.
//! - A provided-buffer pool (`ProvidedBufPool`) for pointer-free `recv`/`read` submissions via
//!   `IORING_OP_PROVIDE_BUFFERS` + `IOSQE_BUFFER_SELECT` (useful to avoid passing pointers into
//!   movable/GC-managed memory).

pub mod buf;
pub mod driver;
pub mod gc;
pub mod mock_gc;
pub mod pool;

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

pub use buf::{GcIoBuf, IoBuf, IoBufMut, OwnedIoBuf};
pub use driver::{IoOp, IoUringDriver, OpCompletion, OpId};
pub use gc::{GcHooks, GcPinGuard, GcRoot};
pub use legacy::{
    is_accept_supported, is_async_cancel_supported, is_connect_supported, is_link_timeout_supported,
    is_provide_buffers_supported, Completion, Driver, OpWithTimeout, PreparedOp, WeakDriver,
};
pub use pool::{LeasedBuf, PoolStats, ProvidedBufPool};

#[cfg(test)]
mod tests;
