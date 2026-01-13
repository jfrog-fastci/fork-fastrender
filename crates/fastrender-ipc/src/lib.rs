#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

/// Maximum size (in bytes) for a single IPC message payload.
///
/// This is a hard safety cap to prevent untrusted peers from forcing unbounded allocations in the
/// browser/renderer IPC layer.
///
/// Note: pixel buffers can be large (e.g. 1080p RGBA8 is ~8.3 MiB). The long-term plan is to move
/// large frame transfers to shared memory, but for early development we allow moderately-sized
/// inline buffers.
pub const MAX_IPC_MESSAGE_BYTES: usize = 64 * 1024 * 1024; // 64 MiB

/// Identifier for a frame (tab/iframe) hosted inside a renderer process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FrameId(pub u64);

/// Axis-aligned rectangle in the embedder's coordinate space.
///
/// All units are in the coordinate space implied by the embedding metadata (typically parent
/// device-space pixels).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rect {
  pub x: f32,
  pub y: f32,
  pub width: f32,
  pub height: f32,
}

impl Rect {
  #[inline]
  pub fn contains_point(self, x: f32, y: f32) -> bool {
    x >= self.x
      && y >= self.y
      && x < self.x + self.width
      && y < self.y + self.height
  }
}

/// Border radius for a rounded-rectangle clip.
///
/// Radii are in the same coordinate space as [`Rect`]. Currently this models circular radii (one
/// scalar per corner). This is sufficient for the MVP compositor and can be extended to elliptical
/// radii if needed.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BorderRadius {
  pub top_left: f32,
  pub top_right: f32,
  pub bottom_right: f32,
  pub bottom_left: f32,
}

impl BorderRadius {
  pub const ZERO: Self = Self {
    top_left: 0.0,
    top_right: 0.0,
    bottom_right: 0.0,
    bottom_left: 0.0,
  };
}

/// Clip shape stack used when compositing a subframe surface into its parent.
///
/// Each item represents an axis-aligned rectangle in the *parent* coordinate space. The effective
/// clip is the intersection of all items in order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClipItem {
  pub rect: Rect,
  pub radius: BorderRadius,
}

/// 2D affine transform.
///
/// This maps `(x, y)` from the subframe's local space into the parent frame's coordinate space:
///
/// ```text
/// x' = a*x + c*y + e
/// y' = b*x + d*y + f
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AffineTransform {
  pub a: f32,
  pub b: f32,
  pub c: f32,
  pub d: f32,
  pub e: f32,
  pub f: f32,
}

impl AffineTransform {
  pub const IDENTITY: Self = Self {
    a: 1.0,
    b: 0.0,
    c: 0.0,
    d: 1.0,
    e: 0.0,
    f: 0.0,
  };

  #[inline]
  pub fn is_axis_aligned(self) -> bool {
    self.b == 0.0 && self.c == 0.0
  }
}

/// Metadata describing how a child frame's surface should be composited into its embedder.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubframeInfo {
  pub child: FrameId,
  /// Affine transform from subframe-local space into the parent frame coordinate space.
  pub transform: AffineTransform,
  /// Clip stack to apply in parent space before drawing the child surface.
  pub clip_stack: Vec<ClipItem>,
  /// Stable key that defines z-order between subframes.
  pub z_index: u64,
}

/// Pixel buffer for a fully-rendered frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameBuffer {
  pub width: u32,
  pub height: u32,
  /// RGBA8 pixel data, row-major, length = `width * height * 4`.
  pub rgba8: Vec<u8>,
}

/// Messages sent from the browser process to a renderer process.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum BrowserToRenderer {
  /// Create per-frame state for `frame_id`.
  ///
  /// The browser is responsible for choosing a unique `FrameId` within the target renderer
  /// process.
  CreateFrame { frame_id: FrameId },
  /// Destroy per-frame state for `frame_id`.
  DestroyFrame { frame_id: FrameId },
  /// Navigate the given frame to `url`.
  Navigate { frame_id: FrameId, url: String },
  /// Resize the viewport for the given frame (CSS pixels).
  Resize {
    frame_id: FrameId,
    width: u32,
    height: u32,
    dpr: f32,
  },
  /// Request a repaint of the given frame.
  RequestRepaint { frame_id: FrameId },
  /// Terminate the renderer process.
  Shutdown,
}

/// Messages sent from a renderer process back to the browser process.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RendererToBrowser {
  /// A rendered frame is ready for presentation.
  FrameReady {
    frame_id: FrameId,
    buffer: FrameBuffer,
    /// Subframe embeddings present in this frame (to be composited by the browser).
    subframes: Vec<SubframeInfo>,
  },
  /// Report a recoverable error related to a specific frame (if any).
  Error {
    frame_id: Option<FrameId>,
    message: String,
  },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompositeError {
  NonAxisAlignedTransform,
  InvalidTransform,
  BufferSizeMismatch,
}

fn blend_src_over(dst: &mut [u8; 4], src: [u8; 4]) {
  let src_a = src[3] as u32;
  if src_a == 255 {
    *dst = src;
    return;
  }
  if src_a == 0 {
    return;
  }
  let inv_a = 255 - src_a;
  dst[0] = ((src[0] as u32 * src_a + dst[0] as u32 * inv_a) / 255) as u8;
  dst[1] = ((src[1] as u32 * src_a + dst[1] as u32 * inv_a) / 255) as u8;
  dst[2] = ((src[2] as u32 * src_a + dst[2] as u32 * inv_a) / 255) as u8;
  dst[3] = (src_a + (dst[3] as u32 * inv_a) / 255).min(255) as u8;
}

fn point_in_rounded_rect(rect: Rect, radius: BorderRadius, x: f32, y: f32) -> bool {
  if !rect.contains_point(x, y) {
    return false;
  }

  // Clamp radii to half the rect dimensions. This is not a full CSS corner-radius normalization,
  // but is sufficient for the MVP compositor (and prevents NaNs for tiny rects).
  let max_r = rect.width.min(rect.height) * 0.5;
  let tl = radius.top_left.clamp(0.0, max_r);
  let tr = radius.top_right.clamp(0.0, max_r);
  let br = radius.bottom_right.clamp(0.0, max_r);
  let bl = radius.bottom_left.clamp(0.0, max_r);

  let lx = x - rect.x;
  let ly = y - rect.y;
  let w = rect.width;
  let h = rect.height;

  // Top-left corner.
  if tl > 0.0 && lx < tl && ly < tl {
    let dx = lx - tl;
    let dy = ly - tl;
    return dx * dx + dy * dy <= tl * tl;
  }
  // Top-right corner.
  if tr > 0.0 && lx > w - tr && ly < tr {
    let dx = lx - (w - tr);
    let dy = ly - tr;
    return dx * dx + dy * dy <= tr * tr;
  }
  // Bottom-right corner.
  if br > 0.0 && lx > w - br && ly > h - br {
    let dx = lx - (w - br);
    let dy = ly - (h - br);
    return dx * dx + dy * dy <= br * br;
  }
  // Bottom-left corner.
  if bl > 0.0 && lx < bl && ly > h - bl {
    let dx = lx - bl;
    let dy = ly - (h - bl);
    return dx * dx + dy * dy <= bl * bl;
  }

  true
}

fn point_in_clip_stack(clip_stack: &[ClipItem], x: f32, y: f32) -> bool {
  clip_stack
    .iter()
    .all(|clip| point_in_rounded_rect(clip.rect, clip.radius, x, y))
}

/// Composite a single subframe buffer into `parent`, applying the transform + clip stack.
///
/// This is intended to run in the browser process when composing out-of-process iframes.
pub fn composite_subframe(
  parent: &mut FrameBuffer,
  child: &FrameBuffer,
  info: &SubframeInfo,
) -> Result<(), CompositeError> {
  if parent
    .width
    .checked_mul(parent.height)
    .and_then(|px| px.checked_mul(4))
    .map(|len| len as usize)
    != Some(parent.rgba8.len())
  {
    return Err(CompositeError::BufferSizeMismatch);
  }
  if child
    .width
    .checked_mul(child.height)
    .and_then(|px| px.checked_mul(4))
    .map(|len| len as usize)
    != Some(child.rgba8.len())
  {
    return Err(CompositeError::BufferSizeMismatch);
  }

  let t = info.transform;
  if !t.is_axis_aligned() {
    return Err(CompositeError::NonAxisAlignedTransform);
  }
  if !t.a.is_finite() || !t.d.is_finite() || !t.e.is_finite() || !t.f.is_finite() {
    return Err(CompositeError::InvalidTransform);
  }
  if t.a == 0.0 || t.d == 0.0 {
    return Err(CompositeError::InvalidTransform);
  }

  // Bounding box of the transformed child in parent space (conservative).
  let child_w = child.width as f32;
  let child_h = child.height as f32;
  let min_x_f = t.e.min(t.e + t.a * child_w);
  let max_x_f = t.e.max(t.e + t.a * child_w);
  let min_y_f = t.f.min(t.f + t.d * child_h);
  let max_y_f = t.f.max(t.f + t.d * child_h);

  let min_x = min_x_f.floor().max(0.0) as i32;
  let min_y = min_y_f.floor().max(0.0) as i32;
  let max_x = max_x_f.ceil().min(parent.width as f32) as i32;
  let max_y = max_y_f.ceil().min(parent.height as f32) as i32;

  if max_x <= min_x || max_y <= min_y {
    return Ok(());
  }

  // Precompute inverses for axis-aligned transform.
  let inv_a = 1.0 / t.a;
  let inv_d = 1.0 / t.d;

  for dy in min_y..max_y {
    for dx in min_x..max_x {
      let px = dx as f32 + 0.5;
      let py = dy as f32 + 0.5;
      if !point_in_clip_stack(&info.clip_stack, px, py) {
        continue;
      }

      // Map destination pixel center back into source space.
      let sx = (px - t.e) * inv_a;
      let sy = (py - t.f) * inv_d;
      let src_x = sx.floor() as i32;
      let src_y = sy.floor() as i32;

      if src_x < 0
        || src_y < 0
        || src_x >= child.width as i32
        || src_y >= child.height as i32
      {
        continue;
      }

      let src_idx = ((src_y as u32 * child.width + src_x as u32) * 4) as usize;
      let src_px = [
        child.rgba8[src_idx],
        child.rgba8[src_idx + 1],
        child.rgba8[src_idx + 2],
        child.rgba8[src_idx + 3],
      ];

      let dst_idx = ((dy as u32 * parent.width + dx as u32) * 4) as usize;
      let mut dst_px = [
        parent.rgba8[dst_idx],
        parent.rgba8[dst_idx + 1],
        parent.rgba8[dst_idx + 2],
        parent.rgba8[dst_idx + 3],
      ];
      blend_src_over(&mut dst_px, src_px);
      parent.rgba8[dst_idx] = dst_px[0];
      parent.rgba8[dst_idx + 1] = dst_px[1];
      parent.rgba8[dst_idx + 2] = dst_px[2];
      parent.rgba8[dst_idx + 3] = dst_px[3];
    }
  }

  Ok(())
}

/// Composite multiple subframes onto a parent buffer using their stable z-order keys.
pub fn composite_subframes<'a>(
  mut parent: FrameBuffer,
  subframes: impl IntoIterator<Item = (&'a SubframeInfo, &'a FrameBuffer)>,
) -> Result<FrameBuffer, CompositeError> {
  let mut list: Vec<_> = subframes.into_iter().collect();
  list.sort_by_key(|(info, _)| (info.z_index, info.child.0));
  for (info, buffer) in list {
    composite_subframe(&mut parent, buffer, info)?;
  }
  Ok(parent)
}

#[cfg(test)]
mod compositor_tests {
  use super::*;

  fn solid_buffer(width: u32, height: u32, rgba: [u8; 4]) -> FrameBuffer {
    let mut data = vec![0u8; (width * height * 4) as usize];
    for px in data.chunks_exact_mut(4) {
      px.copy_from_slice(&rgba);
    }
    FrameBuffer {
      width,
      height,
      rgba8: data,
    }
  }

  fn pixel(buf: &FrameBuffer, x: u32, y: u32) -> [u8; 4] {
    let idx = ((y * buf.width + x) * 4) as usize;
    [buf.rgba8[idx], buf.rgba8[idx + 1], buf.rgba8[idx + 2], buf.rgba8[idx + 3]]
  }

  #[test]
  fn composites_with_translate_and_scale() {
    let parent = solid_buffer(10, 10, [0, 0, 0, 255]);
    let child = solid_buffer(2, 2, [255, 0, 0, 255]);

    let info = SubframeInfo {
      child: FrameId(2),
      transform: AffineTransform {
        a: 2.0,
        b: 0.0,
        c: 0.0,
        d: 2.0,
        e: 3.0,
        f: 1.0,
      },
      clip_stack: vec![ClipItem {
        rect: Rect {
          x: 0.0,
          y: 0.0,
          width: 10.0,
          height: 10.0,
        },
        radius: BorderRadius::ZERO,
      }],
      z_index: 0,
    };

    let out = composite_subframes(parent, [(&info, &child)]).unwrap();

    // Inside the transformed child.
    assert_eq!(pixel(&out, 3, 1), [255, 0, 0, 255]);
    assert_eq!(pixel(&out, 6, 4), [255, 0, 0, 255]);

    // Outside child bounds.
    assert_eq!(pixel(&out, 2, 1), [0, 0, 0, 255]);
    assert_eq!(pixel(&out, 7, 1), [0, 0, 0, 255]);
  }

  #[test]
  fn clips_child_pixels_with_overflow_rect() {
    let parent = solid_buffer(10, 10, [0, 255, 0, 255]);
    let child = solid_buffer(6, 6, [255, 0, 0, 255]);

    let info = SubframeInfo {
      child: FrameId(1),
      transform: AffineTransform {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 2.0,
        f: 2.0,
      },
      clip_stack: vec![ClipItem {
        rect: Rect {
          x: 4.0,
          y: 4.0,
          width: 2.0,
          height: 2.0,
        },
        radius: BorderRadius::ZERO,
      }],
      z_index: 0,
    };

    let out = composite_subframes(parent, [(&info, &child)]).unwrap();

    // Inside the clip rect.
    assert_eq!(pixel(&out, 4, 4), [255, 0, 0, 255]);
    assert_eq!(pixel(&out, 5, 5), [255, 0, 0, 255]);

    // Covered by the child but outside clip rect => keep parent background.
    assert_eq!(pixel(&out, 3, 3), [0, 255, 0, 255]);
    assert_eq!(pixel(&out, 7, 7), [0, 255, 0, 255]);
  }

  #[test]
  fn clips_with_border_radius() {
    let parent = solid_buffer(10, 10, [0, 0, 0, 255]);
    let child = solid_buffer(10, 10, [255, 0, 0, 255]);

    let info = SubframeInfo {
      child: FrameId(3),
      transform: AffineTransform::IDENTITY,
      clip_stack: vec![ClipItem {
        rect: Rect {
          x: 2.0,
          y: 2.0,
          width: 6.0,
          height: 6.0,
        },
        radius: BorderRadius {
          top_left: 3.0,
          top_right: 3.0,
          bottom_right: 3.0,
          bottom_left: 3.0,
        },
      }],
      z_index: 0,
    };

    let out = composite_subframes(parent, [(&info, &child)]).unwrap();

    // Corner pixel should be clipped away by the rounded corner.
    assert_eq!(pixel(&out, 2, 2), [0, 0, 0, 255]);
    // Inner pixel should be painted.
    assert_eq!(pixel(&out, 4, 4), [255, 0, 0, 255]);
  }

  #[test]
  fn composites_subframes_in_stable_z_order() {
    let parent = solid_buffer(4, 4, [0, 0, 0, 255]);
    let red = solid_buffer(2, 2, [255, 0, 0, 255]);
    let blue = solid_buffer(2, 2, [0, 0, 255, 255]);

    let base_transform = AffineTransform {
      a: 1.0,
      b: 0.0,
      c: 0.0,
      d: 1.0,
      e: 1.0,
      f: 1.0,
    };

    let info_red = SubframeInfo {
      child: FrameId(10),
      transform: base_transform,
      clip_stack: vec![ClipItem {
        rect: Rect {
          x: 0.0,
          y: 0.0,
          width: 4.0,
          height: 4.0,
        },
        radius: BorderRadius::ZERO,
      }],
      z_index: 1,
    };

    let info_blue = SubframeInfo {
      child: FrameId(11),
      transform: base_transform,
      clip_stack: vec![ClipItem {
        rect: Rect {
          x: 0.0,
          y: 0.0,
          width: 4.0,
          height: 4.0,
        },
        radius: BorderRadius::ZERO,
      }],
      z_index: 2,
    };

    // Provide in reverse input order; blue should still end up on top due to z_index sorting.
    let out = composite_subframes(parent, [(&info_blue, &blue), (&info_red, &red)]).unwrap();
    assert_eq!(pixel(&out, 1, 1), [0, 0, 255, 255]);
  }

  #[test]
  fn rejects_non_axis_aligned_transform() {
    let mut parent = solid_buffer(2, 2, [0, 0, 0, 255]);
    let child = solid_buffer(1, 1, [255, 0, 0, 255]);
    let info = SubframeInfo {
      child: FrameId(1),
      transform: AffineTransform {
        a: 1.0,
        b: 1.0,
        c: 0.0,
        d: 1.0,
        e: 0.0,
        f: 0.0,
      },
      clip_stack: vec![ClipItem {
        rect: Rect {
          x: 0.0,
          y: 0.0,
          width: 2.0,
          height: 2.0,
        },
        radius: BorderRadius::ZERO,
      }],
      z_index: 0,
    };

    let err = composite_subframe(&mut parent, &child, &info).unwrap_err();
    assert_eq!(err, CompositeError::NonAxisAlignedTransform);
  }
}

/// Abstract transport for browser↔renderer IPC.
///
/// This is intentionally minimal so unit tests can use an in-memory transport, while production
/// builds can use pipes/sockets/shared-memory.
pub trait IpcTransport {
  type Error;

  /// Receive the next message from the browser.
  ///
  /// Returning `Ok(None)` indicates the transport is closed (renderer should shut down).
  fn recv(&mut self) -> Result<Option<BrowserToRenderer>, Self::Error>;

  /// Send a message to the browser.
  fn send(&mut self, msg: RendererToBrowser) -> Result<(), Self::Error>;
}
