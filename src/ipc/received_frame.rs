use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};

use super::protocol::renderer::BrowserToRenderer;

/// Callback invoked when a [`ReceivedFrame`] is acknowledged back to the renderer.
///
/// The callback receives the `frame_seq` that should be echoed in
/// [`BrowserToRenderer::FrameAck`].
pub type FrameAckCallback = Box<dyn FnOnce(u64) + Send + 'static>;

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
/// When dropped, this will acknowledge the underlying frame back to the renderer via
/// [`BrowserToRenderer::FrameAck`] (unless disarmed or stale).
pub struct ReceivedFrame {
  pub frame_seq: u64,
  pub meta: FrameMeta,
  pub bytes: ShmemSliceView,
  /// Optional epoch tracker used to suppress acknowledgements for stale generations/connections.
  ///
  /// If `Some`, `Drop` will only invoke the acknowledgement callback when
  /// `epoch == current_epoch.load(...)`.
  pub epoch: u64,
  current_epoch: Option<Arc<AtomicU64>>,
  on_drop_ack: Option<FrameAckCallback>,
}

impl ReceivedFrame {
  pub fn new(
    frame_seq: u64,
    meta: FrameMeta,
    bytes: ShmemSliceView,
    epoch: u64,
    current_epoch: Option<Arc<AtomicU64>>,
    on_drop_ack: Option<FrameAckCallback>,
  ) -> Self {
    Self {
      frame_seq,
      meta,
      bytes,
      epoch,
      current_epoch,
      on_drop_ack,
    }
  }

  /// Convenience helper for building an acknowledgement callback that forwards
  /// [`BrowserToRenderer::FrameAck`] onto an IPC sender.
  pub fn ack_callback_to_sender(sender: mpsc::Sender<BrowserToRenderer>) -> FrameAckCallback {
    Box::new(move |frame_seq| {
      // Drop must never panic; ignore send failures (renderer gone, channel closed, etc).
      let _ = sender.send(BrowserToRenderer::FrameAck { frame_seq });
    })
  }

  /// Explicitly acknowledge the frame now, preventing a subsequent acknowledgement on drop.
  pub fn ack(&mut self) {
    self.maybe_ack();
  }

  /// Prevent any acknowledgement action on drop (even for the current epoch).
  pub fn disarm(&mut self) {
    self.on_drop_ack = None;
  }

  fn epoch_is_current(&self) -> bool {
    match self.current_epoch.as_ref() {
      Some(cur) => cur.load(Ordering::Acquire) == self.epoch,
      None => true,
    }
  }

  fn maybe_ack(&mut self) {
    let Some(cb) = self.on_drop_ack.take() else {
      return;
    };

    if self.epoch_is_current() {
      cb(self.frame_seq);
    }
  }
}

impl Drop for ReceivedFrame {
  fn drop(&mut self) {
    self.maybe_ack();
  }
}

impl std::fmt::Debug for ReceivedFrame {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("ReceivedFrame")
      .field("frame_seq", &self.frame_seq)
      .field("epoch", &self.epoch)
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
    frame_seq: u64,
    epoch: u64,
    current_epoch: Arc<AtomicU64>,
    sender: mpsc::Sender<BrowserToRenderer>,
  ) -> ReceivedFrame {
    ReceivedFrame::new(
      frame_seq,
      FrameMeta::rgba8(2, 2),
      ShmemSliceView::from_vec(vec![0; 16]),
      epoch,
      Some(current_epoch),
      Some(ReceivedFrame::ack_callback_to_sender(sender)),
    )
  }

  #[test]
  fn dropping_triggers_ack_exactly_once() {
    let (tx, rx) = mpsc::channel();
    let current_epoch = Arc::new(AtomicU64::new(7));

    let frame = make_frame(3, 7, Arc::clone(&current_epoch), tx);
    drop(frame);

    assert_eq!(rx.try_recv().unwrap(), BrowserToRenderer::FrameAck { frame_seq: 3 });
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
  }

  #[test]
  fn overwriting_in_map_drops_old_frame_and_acks() {
    let (tx, rx) = mpsc::channel();
    let current_epoch = Arc::new(AtomicU64::new(1));

    let mut map: HashMap<u64, ReceivedFrame> = HashMap::new();
    map.insert(123, make_frame(10, 1, Arc::clone(&current_epoch), tx.clone()));
    // Overwrite the old frame for the same key; this should drop and ack the previous one.
    map.insert(123, make_frame(11, 1, Arc::clone(&current_epoch), tx));

    assert_eq!(rx.try_recv().unwrap(), BrowserToRenderer::FrameAck { frame_seq: 10 });

    drop(map);
    assert_eq!(rx.try_recv().unwrap(), BrowserToRenderer::FrameAck { frame_seq: 11 });
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
  }

  #[test]
  fn manual_ack_prevents_double_ack_on_drop() {
    let (tx, rx) = mpsc::channel();
    let current_epoch = Arc::new(AtomicU64::new(9));

    let mut frame = make_frame(42, 9, current_epoch, tx);
    frame.ack();
    drop(frame);

    assert_eq!(rx.try_recv().unwrap(), BrowserToRenderer::FrameAck { frame_seq: 42 });
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
  }

  #[test]
  fn stale_epoch_is_not_acked() {
    let (tx, rx) = mpsc::channel();
    let current_epoch = Arc::new(AtomicU64::new(1));

    let frame = make_frame(99, 1, Arc::clone(&current_epoch), tx);
    current_epoch.store(2, Ordering::Release);
    drop(frame);

    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
  }
}
