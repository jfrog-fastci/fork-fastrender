//! Media frame provider implementations used by the in-process media pipeline.
//!
//! The paint pipeline calls [`crate::media::MediaFrameProvider::video_frame`] from one or more
//! threads and expects it to be **non-blocking** (no decoding / scaling / I/O work).
//!
//! The implementations in this module therefore:
//! - cache decoded (or downscaled) frames for fast access during paint, and
//! - accept decoded frames via explicit update methods that are expected to run on background
//!   decode threads.

use crate::media::{MediaFrameProvider, MediaFrameSizeHint};
use crate::paint::display_list::ImageData;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::hash::{BuildHasher, Hash, Hasher};
use std::sync::Arc;
use std::sync::mpsc;

/// Maximum width/height in pixels for a cached video frame.
///
/// This is a conservative cap to avoid accidental OOMs when a video is decoded at an unexpectedly
/// large resolution (e.g. corrupted headers, adversarial inputs, or extremely large screens).
///
/// The cap is applied both to the requested size hint and to the cached scaled output.
const MAX_VIDEO_FRAME_DIMENSION: u32 = 4096;

/// When the decoded frame is larger than the target size by more than this factor, we downscale.
///
/// Expressed as a rational `NUM/DEN` so it stays deterministic (no float rounding noise).
const DOWNSCALE_TRIGGER_NUM: u64 = 3;
const DOWNSCALE_TRIGGER_DEN: u64 = 2; // 1.5x

/// Maximum number of queued scale jobs.
///
/// Scaling is CPU-bound and can be slower than decode on some machines; keep the queue bounded so
/// repeated paint calls (e.g. during resize) cannot accumulate unbounded work.
const SCALE_JOB_QUEUE_CAPACITY: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct VideoKey {
  box_id: Option<usize>,
  src: Arc<str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingScale {
  revision: u64,
  max_w: u32,
  max_h: u32,
}

#[derive(Debug)]
struct VideoEntry {
  src: Arc<str>,
  last_size_hint: Option<MediaFrameSizeHint>,
  frame: Option<Arc<ImageData>>,
  revision: u64,
  pending_scale: Option<PendingScale>,
}

impl VideoEntry {
  fn new(src: Arc<str>) -> Self {
    Self {
      src,
      last_size_hint: None,
      frame: None,
      revision: 0,
      pending_scale: None,
    }
  }
}

struct ScaleJob {
  box_id: Option<usize>,
  src: Arc<str>,
  revision: u64,
  max_w: u32,
  max_h: u32,
  frame: Arc<ImageData>,
}

#[derive(Debug)]
struct FrameScaler {
  sender: Option<mpsc::SyncSender<ScaleJob>>,
  handle: Option<std::thread::JoinHandle<()>>,
}

impl FrameScaler {
  fn new(videos: Arc<Mutex<HashMap<VideoKey, VideoEntry>>>) -> Self {
    let (sender, receiver) = mpsc::sync_channel::<ScaleJob>(SCALE_JOB_QUEUE_CAPACITY);
    let handle = std::thread::spawn(move || {
      while let Ok(job) = receiver.recv() {
        let Some(scaled) = downscale_frame_to_bounds(&job.frame, job.max_w, job.max_h) else {
          continue;
        };
        let mut guard = videos.lock();
        if let Some(entry) = guard.get_mut(&VideoKey {
          box_id: job.box_id,
          src: Arc::clone(&job.src),
        }) {
          if entry.revision != job.revision {
            continue;
          }
          if entry
            .pending_scale
            .is_some_and(|p| p.revision == job.revision && p.max_w == job.max_w && p.max_h == job.max_h)
          {
            entry.frame = Some(scaled);
            entry.pending_scale = None;
            entry.revision = entry.revision.wrapping_add(1);
          }
        }
      }
    });
    Self {
      sender: Some(sender),
      handle: Some(handle),
    }
  }

  fn try_schedule(&self, job: ScaleJob) -> bool {
    self
      .sender
      .as_ref()
      .is_some_and(|sender| sender.try_send(job).is_ok())
  }
}

impl Drop for FrameScaler {
  fn drop(&mut self) {
    // Dropping the sender closes the channel, which makes the worker thread exit its recv loop.
    self.sender.take();
    if let Some(handle) = self.handle.take() {
      let _ = handle.join();
    }
  }
}

/// A [`MediaFrameProvider`] implementation that:
///
/// - tracks the latest [`MediaFrameSizeHint`] per `<video>` element, and
/// - allows the media decode pipeline to publish decoded frames,
///   downscaling them to the most recent size hint **off the paint thread**.
///
/// This type is intentionally minimal and deterministic. It does **not** perform any decoding on
/// its own; instead, decoded frames are pushed in via [`Self::update_video_frame`].
#[derive(Debug)]
pub struct SizeHintMediaFrameProvider {
  videos: Arc<Mutex<HashMap<VideoKey, VideoEntry>>>,
  scaler: FrameScaler,
}

impl SizeHintMediaFrameProvider {
  /// Creates an empty provider.
  pub fn new() -> Self {
    let videos = Arc::new(Mutex::new(HashMap::new()));
    let scaler = FrameScaler::new(Arc::clone(&videos));
    Self { videos, scaler }
  }

  /// Publishes a decoded video frame for (`box_id`, `src`).
  ///
  /// This method is expected to be called from a background decode thread. It may perform
  /// downscaling work to honor the most recently observed size hint for the element.
  pub fn update_video_frame(&self, box_id: Option<usize>, src: &str, frame: Arc<ImageData>) {
    let hint = {
      let mut guard = self.videos.lock();
      video_entry_mut(&mut guard, box_id, src).last_size_hint
    };

    let scaled = downscale_frame_to_hint(&frame, hint).unwrap_or_else(|| Arc::clone(&frame));

    let mut guard = self.videos.lock();
    let entry = video_entry_mut(&mut guard, box_id, src);
    entry.frame = Some(scaled);
    entry.pending_scale = None;
    entry.revision = entry.revision.wrapping_add(1);
  }
}

impl Default for SizeHintMediaFrameProvider {
  fn default() -> Self {
    Self::new()
  }
}

impl MediaFrameProvider for SizeHintMediaFrameProvider {
  fn video_frame(
    &self,
    box_id: Option<usize>,
    src: &str,
    size_hint: Option<MediaFrameSizeHint>,
  ) -> Option<Arc<ImageData>> {
    let mut job: Option<ScaleJob> = None;
    let frame = {
      let mut guard = self.videos.lock();
      let entry = video_entry_mut(&mut guard, box_id, src);
      if size_hint.is_some() && entry.last_size_hint != size_hint {
        entry.last_size_hint = size_hint;
        entry.pending_scale = None;
        entry.revision = entry.revision.wrapping_add(1);
      }

      let frame = entry.frame.as_ref().map(Arc::clone);
      if let (Some(hint), Some(frame)) = (entry.last_size_hint, frame.as_ref()) {
        if let Some((max_w, max_h)) = hint_device_pixel_bounds(hint) {
          if should_downscale(frame.width, frame.height, max_w, max_h) {
            let pending = PendingScale {
              revision: entry.revision,
              max_w,
              max_h,
            };
            if entry.pending_scale != Some(pending) {
              entry.pending_scale = Some(pending);
              job = Some(ScaleJob {
                box_id,
                src: Arc::clone(&entry.src),
                revision: entry.revision,
                max_w,
                max_h,
                frame: Arc::clone(frame),
              });
            }
          }
        }
      }
      frame
    };

    if let Some(job) = job {
      if !self.scaler.try_schedule(job) {
        // Queue is full/disconnected: clear the pending marker so we can retry later.
        let mut guard = self.videos.lock();
        let entry = video_entry_mut(&mut guard, box_id, src);
        entry.pending_scale = None;
      }
    }

    frame
  }
}

fn video_entry_mut<'a>(
  map: &'a mut HashMap<VideoKey, VideoEntry>,
  box_id: Option<usize>,
  src: &str,
) -> &'a mut VideoEntry {
  use std::collections::hash_map::RawEntryMut;

  let mut hasher = map.hasher().build_hasher();
  box_id.hash(&mut hasher);
  src.hash(&mut hasher);
  let hash = hasher.finish();

  match map
    .raw_entry_mut()
    .from_hash(hash, |k| k.box_id == box_id && k.src.as_ref() == src)
  {
    RawEntryMut::Occupied(entry) => entry.into_mut(),
    RawEntryMut::Vacant(entry) => {
      let src_arc: Arc<str> = Arc::from(src);
      entry
        .insert_hashed_nocheck(
          hash,
          VideoKey {
            box_id,
            src: Arc::clone(&src_arc),
          },
          VideoEntry::new(src_arc),
        )
        .1
    }
  }
}

fn clamp_target_dimension(value: f32) -> Option<u32> {
  if !value.is_finite() || value <= 0.0 {
    return None;
  }
  let clamped = value
    .ceil()
    .clamp(1.0, MAX_VIDEO_FRAME_DIMENSION as f32) as u32;
  Some(clamped.max(1))
}

fn hint_device_pixel_bounds(hint: MediaFrameSizeHint) -> Option<(u32, u32)> {
  let device = hint.device_pixel_size();
  let w = clamp_target_dimension(device.width)?;
  let h = clamp_target_dimension(device.height)?;
  Some((w, h))
}

fn should_downscale(src_w: u32, src_h: u32, max_w: u32, max_h: u32) -> bool {
  if max_w == 0 || max_h == 0 {
    return false;
  }
  if src_w == 0 || src_h == 0 {
    return false;
  }

  // Always downscale if we exceed the hard dimension cap.
  if src_w > MAX_VIDEO_FRAME_DIMENSION || src_h > MAX_VIDEO_FRAME_DIMENSION {
    return true;
  }

  // Only downscale when the source is "much larger" than the target.
  let src_w = src_w as u64;
  let src_h = src_h as u64;
  let max_w = max_w as u64;
  let max_h = max_h as u64;

  src_w * DOWNSCALE_TRIGGER_DEN > max_w.saturating_mul(DOWNSCALE_TRIGGER_NUM)
    || src_h * DOWNSCALE_TRIGGER_DEN > max_h.saturating_mul(DOWNSCALE_TRIGGER_NUM)
}

fn fit_within(src_w: u32, src_h: u32, max_w: u32, max_h: u32) -> Option<(u32, u32)> {
  if src_w == 0 || src_h == 0 || max_w == 0 || max_h == 0 {
    return None;
  }

  if src_w <= max_w && src_h <= max_h {
    return Some((src_w, src_h));
  }

  // Determine which constraint is tighter without floats:
  // choose width-constrained scale when max_w/src_w <= max_h/src_h.
  let lhs = (max_w as u128).saturating_mul(src_h as u128);
  let rhs = (max_h as u128).saturating_mul(src_w as u128);

  if lhs <= rhs {
    // Width constraint.
    let out_w = max_w.max(1);
    let out_h = ((src_h as u128).saturating_mul(out_w as u128) / src_w as u128) as u32;
    Some((out_w, out_h.max(1)))
  } else {
    // Height constraint.
    let out_h = max_h.max(1);
    let out_w = ((src_w as u128).saturating_mul(out_h as u128) / src_h as u128) as u32;
    Some((out_w.max(1), out_h))
  }
}

fn downscale_frame_to_hint(
  frame: &Arc<ImageData>,
  hint: Option<MediaFrameSizeHint>,
) -> Option<Arc<ImageData>> {
  // Fall back to the hard cap even when we haven't observed a paint size hint yet. This keeps the
  // provider bounded/deterministic even if decode starts before first paint.
  let (max_w, max_h) = hint
    .and_then(hint_device_pixel_bounds)
    .unwrap_or((MAX_VIDEO_FRAME_DIMENSION, MAX_VIDEO_FRAME_DIMENSION));
  downscale_frame_to_bounds(frame, max_w, max_h)
}

fn downscale_frame_to_bounds(
  frame: &Arc<ImageData>,
  max_w: u32,
  max_h: u32,
) -> Option<Arc<ImageData>> {

  if !should_downscale(frame.width, frame.height, max_w, max_h) {
    return None;
  }

  let (out_w, out_h) = fit_within(frame.width, frame.height, max_w, max_h)?;
  if out_w == frame.width && out_h == frame.height {
    return None;
  }

  // Hard cap on output dimensions (defense-in-depth in case hint bounds were bypassed).
  let out_w = out_w.min(MAX_VIDEO_FRAME_DIMENSION).max(1);
  let out_h = out_h.min(MAX_VIDEO_FRAME_DIMENSION).max(1);

  let src_pixels = frame.pixels.as_ref().as_slice();
  let expected_len = (frame.width as usize)
    .checked_mul(frame.height as usize)?
    .checked_mul(4)?;
  if src_pixels.len() != expected_len {
    return None;
  }

  let out_len = (out_w as usize)
    .checked_mul(out_h as usize)?
    .checked_mul(4)?;
  let mut out = vec![0u8; out_len];

  let src_w = frame.width as usize;
  let src_h = frame.height as usize;
  let out_w_usize = out_w as usize;
  let out_h_usize = out_h as usize;

  // Nearest-neighbor sampling. This is fast (O(dst_pixels)) and deterministic.
  for y_out in 0..out_h_usize {
    let src_y = (y_out * src_h) / out_h_usize;
    for x_out in 0..out_w_usize {
      let src_x = (x_out * src_w) / out_w_usize;
      let src_idx = (src_y * src_w + src_x) * 4;
      let dst_idx = (y_out * out_w_usize + x_out) * 4;

      let a = src_pixels[src_idx + 3] as u16;
      let (mut r, mut g, mut b) = (
        src_pixels[src_idx] as u16,
        src_pixels[src_idx + 1] as u16,
        src_pixels[src_idx + 2] as u16,
      );

      if !frame.premultiplied {
        // Premultiply into output so paint can upload/draw without per-frame scratch buffers.
        r = (r * a + 127) / 255;
        g = (g * a + 127) / 255;
        b = (b * a + 127) / 255;
      }

      let a8 = a.min(255) as u8;
      out[dst_idx] = (r.min(a) as u8).min(a8);
      out[dst_idx + 1] = (g.min(a) as u8).min(a8);
      out[dst_idx + 2] = (b.min(a) as u8).min(a8);
      out[dst_idx + 3] = a8;
    }
  }

  Some(Arc::new(ImageData {
    width: out_w,
    height: out_h,
    css_width: frame.css_width,
    css_height: frame.css_height,
    has_intrinsic_ratio: frame.has_intrinsic_ratio,
    premultiplied: true,
    pixels: Arc::new(out),
  }))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::geometry::Size;

  #[test]
  fn size_hint_downscales_video_frames() {
    let provider = SizeHintMediaFrameProvider::new();
    let box_id = Some(1);
    let src = "v.mp4";

    let hint = MediaFrameSizeHint::new(Size::new(10.0, 10.0), 1.0);

    // Paint observes the element size first, so the provider learns the hint.
    assert!(provider.video_frame(box_id, src, Some(hint)).is_none());

    // Decoder publishes a much larger frame.
    let big_pixels = vec![255u8, 0, 0, 255].repeat(100 * 100);
    let frame = Arc::new(ImageData::new_premultiplied(100, 100, 100.0, 100.0, big_pixels));
    provider.update_video_frame(box_id, src, frame);

    let out = provider
      .video_frame(box_id, src, Some(hint))
      .expect("scaled frame should be cached");

    let (max_w, max_h) = hint_device_pixel_bounds(hint).expect("valid hint");
    assert!(
      out.width <= max_w && out.height <= max_h,
      "expected downscaled frame to fit within {max_w}x{max_h}, got {}x{}",
      out.width,
      out.height
    );
  }
}
