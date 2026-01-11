//! Linux `io_uring` driver building blocks.
//!
//! This crate contains:
//! - A GC-safe low-level driver (`IoUringDriver`) built around an explicit [`IoBuf`] abstraction.
//! - A mock moving/compacting GC (`MockGc`) used by tests.
//! - A small "legacy" driver (`Driver`) with `PreparedOp` helpers (read/openat/statx),
//!   linked timeouts, and cancellation probing.
//! - A provided-buffer pool (`ProvidedBufPool`) for pointer-free `recv`/`read` submissions via
//!   `IORING_OP_PROVIDE_BUFFERS` + `IOSQE_BUFFER_SELECT` (useful to avoid passing pointers into
//!   movable/GC-managed memory).

pub mod buf;
pub mod driver;
pub mod gc;
pub mod mock_gc;
pub mod pool;

mod legacy;
mod timeout;

pub use buf::{GcIoBuf, IoBuf, IoBufMut, OwnedIoBuf};
pub use driver::{IoOp, IoUringDriver, OpCompletion, OpId};
pub use gc::{GcHooks, GcPinGuard, GcRoot};
pub use legacy::{
    is_async_cancel_supported, is_link_timeout_supported, is_provide_buffers_supported, Completion,
    Driver, OpWithTimeout, PreparedOp,
};
pub use pool::{LeasedBuf, PoolStats, ProvidedBufPool};

#[cfg(test)]
mod tests;

