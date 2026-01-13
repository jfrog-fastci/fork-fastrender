#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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

/// Referrer policy applied when generating the `Referer` header for navigation/subresource requests.
///
/// This is intentionally aligned with FastRender's in-process [`crate::resource::ReferrerPolicy`]
/// enum so that browser/renderer processes can communicate the effective policy deterministically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ReferrerPolicy {
  /// Empty-string / unspecified referrer policy ("use default").
  EmptyString,
  NoReferrer,
  NoReferrerWhenDowngrade,
  Origin,
  OriginWhenCrossOrigin,
  SameOrigin,
  StrictOrigin,
  StrictOriginWhenCrossOrigin,
  UnsafeUrl,
}

impl Default for ReferrerPolicy {
  fn default() -> Self {
    Self::EmptyString
  }
}

impl ReferrerPolicy {
  /// Parse a referrer policy token (case-insensitive, trims ASCII whitespace).
  pub fn parse(raw: &str) -> Option<Self> {
    let token = raw.trim_matches(|c: char| {
      matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' ')
    });
    if token.is_empty() {
      return Some(Self::EmptyString);
    }
    if token.eq_ignore_ascii_case("no-referrer") {
      Some(Self::NoReferrer)
    } else if token.eq_ignore_ascii_case("no-referrer-when-downgrade") {
      Some(Self::NoReferrerWhenDowngrade)
    } else if token.eq_ignore_ascii_case("origin") {
      Some(Self::Origin)
    } else if token.eq_ignore_ascii_case("origin-when-cross-origin") {
      Some(Self::OriginWhenCrossOrigin)
    } else if token.eq_ignore_ascii_case("same-origin") {
      Some(Self::SameOrigin)
    } else if token.eq_ignore_ascii_case("strict-origin") {
      Some(Self::StrictOrigin)
    } else if token.eq_ignore_ascii_case("strict-origin-when-cross-origin") {
      Some(Self::StrictOriginWhenCrossOrigin)
    } else if token.eq_ignore_ascii_case("unsafe-url") {
      Some(Self::UnsafeUrl)
    } else {
      None
    }
  }

  /// Parse a `referrerpolicy` attribute value.
  ///
  /// Returns `None` when the attribute is missing, empty, or invalid, which signals that the
  /// request should use the document's default referrer policy.
  pub fn from_attribute(value: &str) -> Option<Self> {
    match Self::parse(value)? {
      Self::EmptyString => None,
      other => Some(other),
    }
  }
}

/// Parsed sandbox flags for an `<iframe sandbox>` attribute.
///
/// This is a conservative subset of the HTML sandboxing flags, represented as "allow-*" bits. The
/// empty value means the sandbox attribute is present with no allowances (maximum restrictions).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub struct SandboxFlags(pub u32);

impl SandboxFlags {
  pub const NONE: Self = Self(0);
  pub const ALLOW_SAME_ORIGIN: Self = Self(1 << 0);
  pub const ALLOW_SCRIPTS: Self = Self(1 << 1);
  pub const ALLOW_FORMS: Self = Self(1 << 2);
  pub const ALLOW_POPUPS: Self = Self(1 << 3);
  pub const ALLOW_TOP_NAVIGATION: Self = Self(1 << 4);

  #[inline]
  pub const fn contains(self, other: Self) -> bool {
    (self.0 & other.0) == other.0
  }

  #[inline]
  pub fn insert(&mut self, other: Self) {
    self.0 |= other.0;
  }

  #[inline]
  pub const fn is_empty(self) -> bool {
    self.0 == 0
  }
}

/// Origin-like tuple used to scope documents to renderer processes.
///
/// This is used by the browser to deterministically convey:
/// - inherited origins (about:blank, srcdoc), and
/// - opaque origins (sandboxed, data:, etc.)
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DocumentOrigin {
  pub scheme: String,
  pub host: Option<String>,
  pub port: Option<u16>,
}

/// Site isolation key used by the browser to decide which renderer process hosts a frame.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SiteKey {
  Origin(DocumentOrigin),
  /// Unique per-document opaque origin key.
  Opaque(u64),
}

impl Default for SiteKey {
  fn default() -> Self {
    // Keep `0` reserved as an "invalid/unspecified" opaque key.
    Self::Opaque(0)
  }
}

/// Contextual metadata for a navigation request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NavigationContext {
  /// URL used to populate the `Referer` header, when allowed by `referrer_policy`.
  pub referrer_url: Option<String>,
  pub referrer_policy: ReferrerPolicy,
  /// Site isolation / origin key computed by the browser for this navigation.
  pub site_key: SiteKey,
}

impl Default for NavigationContext {
  fn default() -> Self {
    Self {
      referrer_url: None,
      referrer_policy: ReferrerPolicy::default(),
      site_key: SiteKey::default(),
    }
  }
}

impl NavigationContext {
  /// Construct a navigation context for a child-frame navigation.
  ///
  /// `iframe_referrer_policy` is sourced from the `<iframe referrerpolicy>` attribute when
  /// present; when it is `None`, the embedding document's policy is used instead.
  pub fn for_subframe_navigation(
    referrer_url: String,
    parent_referrer_policy: ReferrerPolicy,
    iframe_referrer_policy: Option<ReferrerPolicy>,
    site_key: SiteKey,
  ) -> Self {
    Self {
      referrer_url: Some(referrer_url),
      referrer_policy: iframe_referrer_policy.unwrap_or(parent_referrer_policy),
      site_key,
    }
  }
}

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
    // Allow tiny floating-point noise from transform resolution while still rejecting true
    // non-axis-aligned transforms like rotations/skews.
    const EPS: f32 = 1e-6;
    self.b.abs() <= EPS && self.c.abs() <= EPS
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
  /// Optional `<iframe referrerpolicy>` attribute override.
  pub referrer_policy: Option<ReferrerPolicy>,
  /// Parsed sandbox allowlist flags for the `<iframe sandbox>` attribute.
  pub sandbox_flags: SandboxFlags,
  /// True when the subframe's origin must be treated as opaque (e.g. sandbox without
  /// `allow-same-origin`).
  pub opaque_origin: bool,
}

/// Pixel buffer for a fully-rendered frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameBuffer {
  pub width: u32,
  pub height: u32,
  /// RGBA8 pixel data, row-major, length = `width * height * 4`.
  ///
  /// The pixel format is **premultiplied alpha** (matching `tiny-skia`'s `Pixmap` storage): RGB
  /// channels have already been multiplied by `alpha/255`. This makes compositor blending
  /// straightforward and avoids per-frame conversion when the renderer is backed by `tiny-skia`.
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
  Navigate {
    frame_id: FrameId,
    url: String,
    context: NavigationContext,
  },
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
  // Premultiplied-alpha SRC_OVER:
  //   out.rgb = src.rgb + dst.rgb * (1 - src.a)
  //   out.a   = src.a   + dst.a   * (1 - src.a)
  let inv_a = 255 - src_a;
  for chan in 0..3 {
    let blended = src[chan] as u32 + (dst[chan] as u32 * inv_a + 127) / 255;
    dst[chan] = blended.min(255) as u8;
  }
  let out_a = src_a + (dst[3] as u32 * inv_a + 127) / 255;
  dst[3] = out_a.min(255) as u8;
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

  let mut t = info.transform;
  if !t.is_axis_aligned() {
    return Err(CompositeError::NonAxisAlignedTransform);
  }
  // Treat near-zero shear terms as zero for the MVP axis-aligned compositor.
  t.b = 0.0;
  t.c = 0.0;
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
    let a = rgba[3] as u32;
    let premul = if a == 255 {
      rgba
    } else {
      [
        ((rgba[0] as u32 * a + 127) / 255) as u8,
        ((rgba[1] as u32 * a + 127) / 255) as u8,
        ((rgba[2] as u32 * a + 127) / 255) as u8,
        rgba[3],
      ]
    };
    for px in data.chunks_exact_mut(4) {
      px.copy_from_slice(&premul);
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
      referrer_policy: None,
      sandbox_flags: SandboxFlags::NONE,
      opaque_origin: false,
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
      referrer_policy: None,
      sandbox_flags: SandboxFlags::NONE,
      opaque_origin: false,
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
      referrer_policy: None,
      sandbox_flags: SandboxFlags::NONE,
      opaque_origin: false,
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
      referrer_policy: None,
      sandbox_flags: SandboxFlags::NONE,
      opaque_origin: false,
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
      referrer_policy: None,
      sandbox_flags: SandboxFlags::NONE,
      opaque_origin: false,
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
      referrer_policy: None,
      sandbox_flags: SandboxFlags::NONE,
      opaque_origin: false,
    };

    let err = composite_subframe(&mut parent, &child, &info).unwrap_err();
    assert_eq!(err, CompositeError::NonAxisAlignedTransform);
  }

  #[test]
  fn composites_premultiplied_alpha() {
    let parent = solid_buffer(2, 2, [0, 255, 0, 255]);
    let child = solid_buffer(2, 2, [255, 0, 0, 128]);

    let info = SubframeInfo {
      child: FrameId(1),
      transform: AffineTransform::IDENTITY,
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
      referrer_policy: None,
      sandbox_flags: SandboxFlags::NONE,
      opaque_origin: false,
    };

    let out = composite_subframes(parent, [(&info, &child)]).unwrap();

    // src-over premultiplied with alpha=128 over opaque green:
    // out.r = 128, out.g = 127 (rounded), out.a = 255.
    assert_eq!(pixel(&out, 0, 0), [128, 127, 0, 255]);
  }
}

#[cfg(test)]
mod navigation_tests {
  use super::*;

  #[test]
  fn subframe_referrer_policy_is_forwarded_into_child_navigation_context() {
    let subframe = SubframeInfo {
      child: FrameId(2),
      transform: AffineTransform::IDENTITY,
      clip_stack: vec![],
      z_index: 0,
      referrer_policy: Some(ReferrerPolicy::NoReferrer),
      sandbox_flags: SandboxFlags::NONE,
      opaque_origin: false,
    };

    let ctx = NavigationContext::for_subframe_navigation(
      "https://parent.example/".to_string(),
      ReferrerPolicy::StrictOriginWhenCrossOrigin,
      subframe.referrer_policy,
      SiteKey::Opaque(1),
    );

    assert_eq!(ctx.referrer_url.as_deref(), Some("https://parent.example/"));
    assert_eq!(ctx.referrer_policy, ReferrerPolicy::NoReferrer);
  }

  #[test]
  fn subframe_without_override_inherits_parent_referrer_policy() {
    let ctx = NavigationContext::for_subframe_navigation(
      "https://parent.example/".to_string(),
      ReferrerPolicy::OriginWhenCrossOrigin,
      None,
      SiteKey::Opaque(1),
    );

    assert_eq!(ctx.referrer_policy, ReferrerPolicy::OriginWhenCrossOrigin);
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

// -----------------------------------------------------------------------------
// WebSocket IPC (renderer ↔ network process)
// -----------------------------------------------------------------------------

/// Stable identifier for a renderer process participating in network-process IPC.
///
/// This is distinct from [`FrameId`]: a single renderer process can host many frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RendererId(pub u64);

/// Identifier for a WebSocket connection scoped to a single renderer process.
///
/// This is chosen by the renderer and therefore **untrusted** when received by the network process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WebSocketConnId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WebSocketErrorKind {
  DuplicateConnId,
  /// Renderer attempted to open more WebSockets than allowed for its IPC channel.
  PerRendererLimitExceeded,
  /// Global WebSocket connection limit for the network process was exceeded.
  GlobalLimitExceeded,
}

/// Hard caps for network-process WebSocket bookkeeping.
///
/// These limits are enforced by [`NetworkWebSocketManager`] to ensure a compromised renderer cannot
/// exhaust network-process resources by opening unbounded WebSockets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetworkWebSocketManagerLimits {
  pub max_active_per_renderer: usize,
  pub max_active_total: usize,
}

impl Default for NetworkWebSocketManagerLimits {
  fn default() -> Self {
    Self {
      // Enough for most pages while still preventing resource exhaustion attacks.
      max_active_per_renderer: 256,
      // Backstop when many renderer processes are alive.
      max_active_total: 4096,
    }
  }
}

/// Renderer → network-process WebSocket commands.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum WebSocketCommand {
  Connect {
    conn_id: WebSocketConnId,
    url: String,
  },
  SendText {
    conn_id: WebSocketConnId,
    text: String,
  },
  Close {
    conn_id: WebSocketConnId,
  },
  /// Best-effort abort used during renderer teardown.
  Shutdown {
    conn_id: WebSocketConnId,
  },
}

/// Network-process → renderer WebSocket events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum WebSocketEvent {
  Error {
    conn_id: WebSocketConnId,
    kind: WebSocketErrorKind,
  },
  Close {
    conn_id: WebSocketConnId,
  },
}

impl WebSocketEvent {
  pub fn conn_id(&self) -> WebSocketConnId {
    match *self {
      WebSocketEvent::Error { conn_id, .. } => conn_id,
      WebSocketEvent::Close { conn_id } => conn_id,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WebSocketConnState {
  Connecting,
  Closed,
}

#[derive(Debug)]
struct WebSocketConnEntry {
  #[allow(dead_code)]
  url: String,
  state: WebSocketConnState,
}

/// Network-process side WebSocket connection registry keyed by `(renderer_id, conn_id)`.
///
/// Security invariants:
/// - The renderer is untrusted and may send duplicate or unknown `conn_id` values.
/// - Duplicate `Connect` attempts are rejected deterministically. We **do not** replace the existing
///   entry, preventing a compromised renderer from overriding a live connection by reusing IDs.
/// - Commands for unknown `conn_id` values are ignored (no panic).
#[derive(Debug, Default)]
pub struct NetworkWebSocketManager {
  conns: HashMap<RendererId, HashMap<WebSocketConnId, WebSocketConnEntry>>,
  limits: NetworkWebSocketManagerLimits,
  active_total: usize,
}

impl NetworkWebSocketManager {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn new_with_limits(limits: NetworkWebSocketManagerLimits) -> Self {
    Self {
      limits,
      ..Self::default()
    }
  }

  /// Drop all WebSocket connections associated with `renderer_id`.
  ///
  /// This is intended to be called when the renderer process disconnects/crashes so the network
  /// process does not retain stale connection state.
  pub fn shutdown_renderer(&mut self, renderer_id: RendererId) -> Vec<WebSocketEvent> {
    let Some(conns) = self.conns.remove(&renderer_id) else {
      return Vec::new();
    };
    self.active_total = self.active_total.saturating_sub(conns.len());
    // Return deterministic event ordering for tests/logging.
    let mut ids: Vec<WebSocketConnId> = conns.keys().copied().collect();
    ids.sort_by_key(|id| id.0);
    ids
      .into_iter()
      .map(|conn_id| WebSocketEvent::Close { conn_id })
      .collect()
  }

  pub fn connection_count_for_test(&self, renderer_id: RendererId) -> usize {
    self
      .conns
      .get(&renderer_id)
      .map(|m| m.len())
      .unwrap_or(0)
  }

  pub fn handle_command(&mut self, renderer_id: RendererId, cmd: WebSocketCommand) -> Vec<WebSocketEvent> {
    match cmd {
      WebSocketCommand::Connect { conn_id, url } => {
        let renderer_count = match self.conns.get(&renderer_id) {
          Some(renderer_conns) => {
            if renderer_conns.contains_key(&conn_id) {
              // Deterministic behaviour: reject the duplicate without touching the existing entry.
              return vec![
                WebSocketEvent::Error {
                  conn_id,
                  kind: WebSocketErrorKind::DuplicateConnId,
                },
                WebSocketEvent::Close { conn_id },
              ];
            }
            renderer_conns.len()
          }
          None => 0,
        };

        if renderer_count >= self.limits.max_active_per_renderer {
          return vec![
            WebSocketEvent::Error {
              conn_id,
              kind: WebSocketErrorKind::PerRendererLimitExceeded,
            },
            WebSocketEvent::Close { conn_id },
          ];
        }

        if self.active_total >= self.limits.max_active_total {
          return vec![
            WebSocketEvent::Error {
              conn_id,
              kind: WebSocketErrorKind::GlobalLimitExceeded,
            },
            WebSocketEvent::Close { conn_id },
          ];
        }

        let renderer_conns = self.conns.entry(renderer_id).or_default();
        // Double-check in case the limits are zero; this also prevents any future refactors from
        // inserting before checking counts.
        if renderer_conns.len() >= self.limits.max_active_per_renderer {
          return vec![
            WebSocketEvent::Error {
              conn_id,
              kind: WebSocketErrorKind::PerRendererLimitExceeded,
            },
            WebSocketEvent::Close { conn_id },
          ];
        }

        renderer_conns.insert(
          conn_id,
          WebSocketConnEntry {
            url,
            state: WebSocketConnState::Connecting,
          },
        );
        self.active_total = self.active_total.saturating_add(1);

        // Connection establishment is async in production; no immediate event is generated here.
        Vec::new()
      }

      WebSocketCommand::SendText { conn_id, .. } => {
        let Some(renderer_conns) = self.conns.get_mut(&renderer_id) else {
          return Vec::new();
        };
        let Some(conn) = renderer_conns.get_mut(&conn_id) else {
          return Vec::new();
        };
        if conn.state == WebSocketConnState::Closed {
          return Vec::new();
        }
        // In the real implementation this would write to the socket; keep this logic-only manager
        // silent to avoid turning unknown IDs into an amplification vector.
        Vec::new()
      }

      WebSocketCommand::Close { conn_id } | WebSocketCommand::Shutdown { conn_id } => {
        let Some(renderer_conns) = self.conns.get_mut(&renderer_id) else {
          return Vec::new();
        };
        let Some(mut conn) = renderer_conns.remove(&conn_id) else {
          return Vec::new();
        };
        conn.state = WebSocketConnState::Closed;
        self.active_total = self.active_total.saturating_sub(1);
        if renderer_conns.is_empty() {
          self.conns.remove(&renderer_id);
        }
        vec![WebSocketEvent::Close { conn_id }]
      }
    }
  }
}

/// Renderer-side WebSocket IPC backend.
///
/// The renderer may observe events for unknown `conn_id` values due to teardown races (e.g. the
/// network process has a queued `Close` after the renderer dropped its local state). These events
/// must be ignored without panicking.
#[derive(Debug, Default)]
pub struct RendererWebSocketBackend {
  conns: HashMap<WebSocketConnId, ()>,
  delivered: Vec<WebSocketEvent>,
}

impl RendererWebSocketBackend {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn register_conn(&mut self, conn_id: WebSocketConnId) {
    self.conns.insert(conn_id, ());
  }

  pub fn unregister_conn(&mut self, conn_id: WebSocketConnId) {
    self.conns.remove(&conn_id);
  }

  /// Handle a network-originated event.
  ///
  /// Returns `true` if the event was delivered to a known connection.
  pub fn handle_event(&mut self, event: WebSocketEvent) -> bool {
    if !self.conns.contains_key(&event.conn_id()) {
      return false;
    }
    self.delivered.push(event);
    true
  }

  pub fn delivered_for_test(&self) -> &[WebSocketEvent] {
    &self.delivered
  }
}

#[cfg(test)]
mod websocket_ipc_tests {
  use super::*;

  #[test]
  fn duplicate_connect_is_rejected_deterministically() {
    let mut mgr = NetworkWebSocketManager::new();
    let renderer = RendererId(1);
    let conn_id = WebSocketConnId(99);

    let first = mgr.handle_command(
      renderer,
      WebSocketCommand::Connect {
        conn_id,
        url: "ws://example.invalid/".to_string(),
      },
    );
    assert!(first.is_empty());
    assert_eq!(mgr.connection_count_for_test(renderer), 1);

    let second = mgr.handle_command(
      renderer,
      WebSocketCommand::Connect {
        conn_id,
        url: "ws://example.invalid/again".to_string(),
      },
    );
    assert_eq!(
      second,
      vec![
        WebSocketEvent::Error {
          conn_id,
          kind: WebSocketErrorKind::DuplicateConnId
        },
        WebSocketEvent::Close { conn_id }
      ]
    );
    // Existing connection remains registered.
    assert_eq!(mgr.connection_count_for_test(renderer), 1);
  }

  #[test]
  fn send_unknown_conn_id_is_ignored() {
    let mut mgr = NetworkWebSocketManager::new();
    let renderer = RendererId(1);

    let events = mgr.handle_command(
      renderer,
      WebSocketCommand::SendText {
        conn_id: WebSocketConnId(123),
        text: "hi".to_string(),
      },
    );
    assert!(events.is_empty());
    assert_eq!(mgr.connection_count_for_test(renderer), 0);
  }

  #[test]
  fn renderer_backend_drops_unknown_conn_id_events() {
    let mut backend = RendererWebSocketBackend::new();
    // No connections registered yet.
    let delivered = backend.handle_event(WebSocketEvent::Close {
      conn_id: WebSocketConnId(55),
    });
    assert!(!delivered);
    assert!(backend.delivered_for_test().is_empty());
  }

  #[test]
  fn shutdown_renderer_drops_all_connections() {
    let mut mgr = NetworkWebSocketManager::new();
    let renderer = RendererId(9);

    for conn_id in [WebSocketConnId(2), WebSocketConnId(1)] {
      let events = mgr.handle_command(
        renderer,
        WebSocketCommand::Connect {
          conn_id,
          url: "ws://example.invalid/".to_string(),
        },
      );
      assert!(events.is_empty());
    }
    assert_eq!(mgr.connection_count_for_test(renderer), 2);

    let events = mgr.shutdown_renderer(renderer);
    assert_eq!(
      events,
      vec![
        WebSocketEvent::Close {
          conn_id: WebSocketConnId(1)
        },
        WebSocketEvent::Close {
          conn_id: WebSocketConnId(2)
        }
      ]
    );
    assert_eq!(mgr.connection_count_for_test(renderer), 0);

    // Further commands for stale conn_ids should remain benign.
    let ignored = mgr.handle_command(
      renderer,
      WebSocketCommand::SendText {
        conn_id: WebSocketConnId(1),
        text: "hello".to_string(),
      },
    );
    assert!(ignored.is_empty());
  }

  #[test]
  fn per_renderer_connection_cap_rejects_and_releases_on_close() {
    let limits = NetworkWebSocketManagerLimits {
      max_active_per_renderer: 2,
      max_active_total: 100,
    };
    let mut mgr = NetworkWebSocketManager::new_with_limits(limits);
    let renderer = RendererId(1);

    assert!(mgr
      .handle_command(
        renderer,
        WebSocketCommand::Connect {
          conn_id: WebSocketConnId(1),
          url: "ws://example.invalid/1".to_string(),
        },
      )
      .is_empty());
    assert!(mgr
      .handle_command(
        renderer,
        WebSocketCommand::Connect {
          conn_id: WebSocketConnId(2),
          url: "ws://example.invalid/2".to_string(),
        },
      )
      .is_empty());
    assert_eq!(mgr.connection_count_for_test(renderer), 2);

    let rejected = mgr.handle_command(
      renderer,
      WebSocketCommand::Connect {
        conn_id: WebSocketConnId(3),
        url: "ws://example.invalid/3".to_string(),
      },
    );
    assert_eq!(
      rejected,
      vec![
        WebSocketEvent::Error {
          conn_id: WebSocketConnId(3),
          kind: WebSocketErrorKind::PerRendererLimitExceeded
        },
        WebSocketEvent::Close {
          conn_id: WebSocketConnId(3)
        }
      ]
    );
    assert_eq!(mgr.connection_count_for_test(renderer), 2);

    let close = mgr.handle_command(
      renderer,
      WebSocketCommand::Close {
        conn_id: WebSocketConnId(1),
      },
    );
    assert_eq!(
      close,
      vec![WebSocketEvent::Close {
        conn_id: WebSocketConnId(1)
      }]
    );
    assert_eq!(mgr.connection_count_for_test(renderer), 1);

    // After closing, a new connect should succeed.
    assert!(mgr
      .handle_command(
        renderer,
        WebSocketCommand::Connect {
          conn_id: WebSocketConnId(4),
          url: "ws://example.invalid/4".to_string(),
        },
      )
      .is_empty());
    assert_eq!(mgr.connection_count_for_test(renderer), 2);
  }

  #[test]
  fn global_connection_cap_is_enforced() {
    let limits = NetworkWebSocketManagerLimits {
      max_active_per_renderer: 10,
      max_active_total: 2,
    };
    let mut mgr = NetworkWebSocketManager::new_with_limits(limits);
    let r1 = RendererId(1);
    let r2 = RendererId(2);

    assert!(mgr
      .handle_command(
        r1,
        WebSocketCommand::Connect {
          conn_id: WebSocketConnId(1),
          url: "ws://example.invalid/a".to_string(),
        },
      )
      .is_empty());
    assert!(mgr
      .handle_command(
        r2,
        WebSocketCommand::Connect {
          conn_id: WebSocketConnId(2),
          url: "ws://example.invalid/b".to_string(),
        },
      )
      .is_empty());

    let rejected = mgr.handle_command(
      r1,
      WebSocketCommand::Connect {
        conn_id: WebSocketConnId(3),
        url: "ws://example.invalid/c".to_string(),
      },
    );
    assert_eq!(
      rejected,
      vec![
        WebSocketEvent::Error {
          conn_id: WebSocketConnId(3),
          kind: WebSocketErrorKind::GlobalLimitExceeded
        },
        WebSocketEvent::Close {
          conn_id: WebSocketConnId(3)
        }
      ]
    );
    assert_eq!(mgr.connection_count_for_test(r1), 1);
    assert_eq!(mgr.connection_count_for_test(r2), 1);
  }

  #[test]
  fn spam_connect_over_limit_does_not_grow_state() {
    let limits = NetworkWebSocketManagerLimits {
      max_active_per_renderer: 1,
      max_active_total: 1,
    };
    let mut mgr = NetworkWebSocketManager::new_with_limits(limits);
    let renderer = RendererId(1);

    assert!(mgr
      .handle_command(
        renderer,
        WebSocketCommand::Connect {
          conn_id: WebSocketConnId(1),
          url: "ws://example.invalid/1".to_string(),
        },
      )
      .is_empty());
    assert_eq!(mgr.connection_count_for_test(renderer), 1);

    for i in 2..10_000u64 {
      let _ = mgr.handle_command(
        renderer,
        WebSocketCommand::Connect {
          conn_id: WebSocketConnId(i),
          url: format!("ws://example.invalid/{i}"),
        },
      );
    }

    assert_eq!(mgr.connection_count_for_test(renderer), 1);
  }
}
