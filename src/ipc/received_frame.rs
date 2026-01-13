use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};

use super::protocol::BrowserToRenderer;

pub type FrameReleaseCallback = Box<dyn FnOnce(u64) + Send + 'static>;

/// Metadata describing a rendered frame that lives in a shared frame-buffer pool.
///
/// This struct is intentionally small; it is part of the browser↔renderer IPC surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameMeta {
  pub width_px: u32,
  pub height_px: u32,
  /// Bytes per row.
  pub stride_bytes: u32,
}

impl FrameMeta {
  pub fn rgba8(width_px: u32, height_px: u32) -> Self {
    Self {
      width_px,
      height_px,
      stride_bytes: width_px.saturating_mul(4),
    }
  }
}

/// A cheap view into a shared-memory byte slice.
///
/// For unit tests, this can be backed by an `Arc<[u8]>`. Real IPC integrations can replace the
/// backing store with an actual shared-memory mapping while keeping the same API surface.
#[derive(Clone)]
pub struct ShmemSliceView {
  backing: Arc<[u8]>,
  range: Range<usize>,
}

impl ShmemSliceView {
  pub fn new(backing: Arc<[u8]>, range: Range<usize>) -> Self {
    debug_assert!(range.start <= range.end);
    debug_assert!(range.end <= backing.len());
    Self { backing, range }
  }

  pub fn from_vec(bytes: Vec<u8>) -> Self {
    let backing: Arc<[u8]> = bytes.into();
    let len = backing.len();
    Self {
      backing,
      range: 0..len,
    }
  }

  pub fn as_slice(&self) -> &[u8] {
    &self.backing[self.range.clone()]
  }

  pub fn len(&self) -> usize {
    self.range.end.saturating_sub(self.range.start)
  }

  pub fn is_empty(&self) -> bool {
    self.len() == 0
  }
}

impl AsRef<[u8]> for ShmemSliceView {
  fn as_ref(&self) -> &[u8] {
    self.as_slice()
  }
}

impl std::fmt::Debug for ShmemSliceView {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("ShmemSliceView")
      .field("len", &self.len())
      .finish()
  }
}

/// An RAII handle for a frame received from the renderer.
///
/// When dropped, this will release the underlying shared frame buffer back to the renderer's pool
/// via `on_drop_release` (unless disarmed or stale).
pub struct ReceivedFrame {
  pub generation: u64,
  pub frame_seq: u64,
  pub meta: FrameMeta,
  pub bytes: ShmemSliceView,
  /// Optional generation tracker used to suppress releases for stale generations.
  ///
  /// If `Some`, `Drop` will only invoke the release callback when
  /// `generation == current_generation.load(...)`.
  current_generation: Option<Arc<AtomicU64>>,
  on_drop_release: Option<FrameReleaseCallback>,
}

impl ReceivedFrame {
  pub fn new(
    generation: u64,
    frame_seq: u64,
    meta: FrameMeta,
    bytes: ShmemSliceView,
    current_generation: Option<Arc<AtomicU64>>,
    on_drop_release: Option<FrameReleaseCallback>,
  ) -> Self {
    Self {
      generation,
      frame_seq,
      meta,
      bytes,
      current_generation,
      on_drop_release,
    }
  }

  /// Convenience helper for building a release callback that forwards
  /// [`BrowserToRenderer::FrameAck`] onto an IPC sender.
  pub fn release_callback_to_sender(
    sender: mpsc::Sender<BrowserToRenderer>,
  ) -> FrameReleaseCallback {
    Box::new(move |frame_seq| {
      // Drop must never panic; ignore send failures (renderer gone, channel closed, etc).
      let _ = sender.send(BrowserToRenderer::FrameAck { frame_seq });
    })
  }

  /// Explicitly release the frame buffer now, preventing a subsequent release on drop.
  pub fn release(&mut self) {
    self.maybe_release();
  }

  /// Prevent any release action on drop (even for the current generation).
  pub fn disarm(&mut self) {
    self.on_drop_release = None;
  }

  fn generation_is_current(&self) -> bool {
    match self.current_generation.as_ref() {
      Some(cur) => cur.load(Ordering::Acquire) == self.generation,
      None => true,
    }
  }

  fn maybe_release(&mut self) {
    let Some(cb) = self.on_drop_release.take() else {
      return;
    };

    if self.generation_is_current() {
      cb(self.frame_seq);
    }
  }
}

impl Drop for ReceivedFrame {
  fn drop(&mut self) {
    self.maybe_release();
  }
}

impl std::fmt::Debug for ReceivedFrame {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("ReceivedFrame")
      .field("generation", &self.generation)
      .field("frame_seq", &self.frame_seq)
      .field("meta", &self.meta)
      .field("bytes", &self.bytes)
      .finish_non_exhaustive()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::collections::HashMap;
  use std::sync::mpsc::TryRecvError;

  fn make_frame(
    generation: u64,
    frame_seq: u64,
    current_generation: Arc<AtomicU64>,
    sender: mpsc::Sender<BrowserToRenderer>,
  ) -> ReceivedFrame {
    ReceivedFrame::new(
      generation,
      frame_seq,
      FrameMeta::rgba8(2, 2),
      ShmemSliceView::from_vec(vec![0; 16]),
      Some(current_generation),
      Some(ReceivedFrame::release_callback_to_sender(sender)),
    )
  }

  #[test]
  fn dropping_triggers_release_exactly_once() {
    let (tx, rx) = mpsc::channel();
    let current_generation = Arc::new(AtomicU64::new(7));

    let frame = make_frame(7, 3, Arc::clone(&current_generation), tx);
    drop(frame);

    assert_eq!(
      rx.try_recv().unwrap(),
      BrowserToRenderer::FrameAck { frame_seq: 3 }
    );
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
  }

  #[test]
  fn overwriting_in_map_drops_old_frame_and_releases_buffer() {
    let (tx, rx) = mpsc::channel();
    let current_generation = Arc::new(AtomicU64::new(1));

    let mut map: HashMap<u64, ReceivedFrame> = HashMap::new();
    map.insert(
      123,
      make_frame(1, 10, Arc::clone(&current_generation), tx.clone()),
    );
    // Overwrite the old frame for the same key; this should drop and release the previous one.
    map.insert(123, make_frame(1, 11, Arc::clone(&current_generation), tx));

    assert_eq!(
      rx.try_recv().unwrap(),
      BrowserToRenderer::FrameAck { frame_seq: 10 }
    );

    drop(map);
    assert_eq!(
      rx.try_recv().unwrap(),
      BrowserToRenderer::FrameAck { frame_seq: 11 }
    );
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
  }

  #[test]
  fn manual_release_prevents_double_release_on_drop() {
    let (tx, rx) = mpsc::channel();
    let current_generation = Arc::new(AtomicU64::new(9));

    let mut frame = make_frame(9, 42, current_generation, tx);
    frame.release();
    drop(frame);

    assert_eq!(
      rx.try_recv().unwrap(),
      BrowserToRenderer::FrameAck { frame_seq: 42 }
    );
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
  }

  #[test]
  fn stale_generation_is_not_released() {
    let (tx, rx) = mpsc::channel();
    let current_generation = Arc::new(AtomicU64::new(1));

    let frame = make_frame(1, 99, Arc::clone(&current_generation), tx);
    current_generation.store(2, Ordering::Release);
    drop(frame);

    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
  }
}
