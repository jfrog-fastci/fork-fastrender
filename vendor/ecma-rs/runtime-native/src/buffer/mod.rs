//! Runtime-native buffer primitives.
//!
//! ## MVP decision: detach/transfer are implemented; resizable buffers are not (yet)
//!
//! Even with a non-moving backing store, JS-level operations like ArrayBuffer detach/transfer
//! (structured clone / postMessage) and future resizable ArrayBuffers can invalidate raw pointers
//! handed to the OS for async I/O.
//!
//! `runtime-native` enforces a strict rule:
//!
//! > Any operation that would change a buffer's backing store identity or size (detach, transfer,
//! > resize) must fail while the backing store is pinned (`pin_count > 0`).
//!
//! Detach/transfer are implemented with a simple state machine:
//! `Alive -> Detached` (`ArrayBuffer::is_detached`).
//!
//! Resizable ArrayBuffers are not supported in MVP, but `ArrayBuffer::resize(..)` exists as a
//! placeholder and still enforces the same pin-count check before returning
//! `ArrayBufferError::Unimplemented`.
//!
//! See also: `runtime-native/docs/buffers.md`.

pub mod array_buffer;
pub mod backing_store;
pub mod typed_array;

pub use array_buffer::{ArrayBuffer, ArrayBufferError, PinnedArrayBuffer};
pub use backing_store::{
  global_backing_store_allocator, BackingStore, BackingStoreAllocError, BackingStoreAllocator,
  BackingStoreDetachError, BackingStorePinError, GlobalBackingStoreAllocator, PinnedBackingStore,
  BACKING_STORE_MIN_ALIGN,
};
pub use typed_array::{PinnedUint8Array, TypedArrayError, Uint8Array};
