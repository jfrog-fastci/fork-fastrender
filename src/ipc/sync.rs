//! Shared-memory synchronization helpers for multiprocess rendering.
//!
//! The renderer process writes pixels into a shared-memory mapped frame buffer and then signals the
//! browser process (via an IPC message such as `RendererToBrowser::FrameReady`) that the frame is
//! ready to consume.
//!
//! Even though the frame buffer mapping is shared between processes, **cache coherence alone is not
//! enough** to guarantee correct visibility:
//!
//! - CPUs and compilers may reorder ordinary loads/stores around the IPC send/recv boundary.
//! - The readiness signal travels through a *different* mechanism (socket/pipe) than the pixels
//!   (shared memory mapping), so we need an explicit ordering point on each side.
//!
//! We implement this as a Release fence on the producer (renderer) and an Acquire fence on the
//! consumer (browser), forming a "publish/consume" pair around the IPC message.
//!
//! Note: These fences don't create a synchronization primitive by themselves; they provide the
//! necessary *ordering* guarantees given that the IPC channel already provides the control-flow
//! synchronization ("the browser won't read until it has received FrameReady").

use std::sync::atomic::{fence, Ordering};

#[cfg(test)]
use std::sync::atomic::AtomicUsize;

#[cfg(test)]
static SHM_PUBLISH_COUNT: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
static SHM_CONSUME_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Publish all prior shared-memory writes (pixel buffer) before sending `FrameReady`.
#[inline]
pub fn shm_publish_frame() {
  // Ensure all prior pixel writes become visible before we notify the other process.
  fence(Ordering::Release);
  #[cfg(test)]
  SHM_PUBLISH_COUNT.fetch_add(1, Ordering::Relaxed);
}

/// Ensure subsequent shared-memory reads (pixel buffer upload) observe the published frame.
#[inline]
pub fn shm_consume_frame() {
  // Ensure we don't read from the shared buffer until after we've observed the readiness signal.
  fence(Ordering::Acquire);
  #[cfg(test)]
  SHM_CONSUME_COUNT.fetch_add(1, Ordering::Relaxed);
}

#[cfg(test)]
pub(crate) fn shm_publish_count_for_test() -> usize {
  SHM_PUBLISH_COUNT.load(Ordering::Relaxed)
}

#[cfg(test)]
pub(crate) fn shm_consume_count_for_test() -> usize {
  SHM_CONSUME_COUNT.load(Ordering::Relaxed)
}
