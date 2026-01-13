//! Security-relevant IPC limits.
//!
//! This module centralizes constants that define **hard bounds** for the browser↔renderer IPC layer.
//! Any change to these values is security-sensitive: treat increases as a review-worthy decision.
//!
//! These constants are referenced by:
//! - `src/ipc/framing.rs` (length-prefixed message framing)
//! - `src/ipc/protocol.rs` (message schema + validation)
//! - `src/ipc/frame_pool.rs` (shared-memory-backed frame buffer pool)

/// Current IPC protocol version.
///
/// Bump when message shapes or semantics change incompatibly.
pub const IPC_PROTOCOL_VERSION: u32 = 1;

/// Pixel format is fixed to premultiplied RGBA8.
pub const BYTES_PER_PIXEL: usize = 4;

/// Maximum payload size accepted by the framing layer (`read_frame` / `write_frame`).
///
/// This is a hard cap to prevent untrusted peers from forcing unbounded allocations when parsing a
/// length-prefixed stream.
pub const MAX_IPC_MESSAGE_BYTES: usize = 8 * 1024 * 1024; // 8 MiB

/// Hard cap for the number of frame buffers the browser can advertise to a renderer.
///
/// This should stay small: frame buffers are large (MiB-scale) and are typically double/triple
/// buffered. Keeping this bounded prevents accidental "allocate 100 buffers" style bugs.
pub const MAX_FRAME_BUFFERS: usize = 8;

/// Upper bound for shared-memory identifiers (`FrameBufferDesc::shmem_id`), in bytes.
///
/// This is a *protocol* limit to keep allocations bounded when decoding messages.
pub const MAX_ID_LEN: usize = 256;

/// Upper bound for renderer crash reason strings, in bytes.
pub const MAX_CRASH_REASON_LEN: usize = 1024;

/// Sane bounds for device pixel ratio (DPR).
///
/// - `MIN_DPR` allows unusual zoom states without rejecting legitimate pages.
/// - `MAX_DPR` prevents pathological values.
pub const MIN_DPR: f32 = 0.1;
pub const MAX_DPR: f32 = 16.0;

/// Maximum POSIX shared-memory object name length on macOS.
///
/// Darwin enforces a short name limit for `shm_open` (historically `PSHMNAMLEN = 31` bytes).
/// If a shared-memory backend uses `shm_open` on macOS, `FrameBufferDesc::shmem_id` must fit within
/// this bound (in addition to [`MAX_ID_LEN`]).
pub const MACOS_POSIX_SHM_NAME_MAX_LEN: usize = 31;

