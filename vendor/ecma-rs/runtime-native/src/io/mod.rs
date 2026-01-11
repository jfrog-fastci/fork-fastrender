pub mod async_fd;
pub mod limits;
pub mod iovec;
pub mod op;
pub mod op_registry;
pub mod runtime;

// io_uring is Linux-only. Keep it out of non-Linux builds so the runtime-native
// reactor and other subsystems can compile on kqueue platforms.
#[cfg(target_os = "linux")]
pub mod uring;
#[cfg(target_os = "linux")]
pub mod uring_read;

pub use async_fd::{AsyncFd, Readable, Writable};
pub use iovec::{IoVecList, IoVecRange, PinnedIoVec};
#[cfg(unix)]
pub use iovec::PinnedMsgHdr;
pub use limits::{IoCounters, IoLimitError, IoLimits, IoLimiter};
pub use op::{IoBuf, IoOp};
pub use op_registry::IoOpDebugHooks;
pub use runtime::IoRuntime;
#[cfg(target_os = "linux")]
pub use uring_read::{CancellationToken as UringCancellationToken, IoError as UringIoError, UringDriver};
