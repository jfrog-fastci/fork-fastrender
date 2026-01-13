//! Canvas wrapper for tiny-skia 2D graphics library
//!
//! This module provides a high-level abstraction over tiny-skia for painting
//! display items to pixels. It handles:
//!
//! - Rectangle filling and stroking (with optional rounded corners)
//! - Text/glyph rendering
//! - State management (transforms, clips, opacity)
//! - Color conversion between CSS and tiny-skia formats
//!
//! # Architecture
//!
//! The Canvas wraps a tiny-skia `Pixmap` and maintains a stack of graphics states.
//! Each state includes the current transform, clip region, and opacity. States
//! can be pushed/popped to implement CSS effects like opacity layers.
//!
//! # Example
//!
//! ```rust,ignore
//! use fastrender::paint::canvas::Canvas;
//! use fastrender::geometry::{Point, Rect, Size};
//! use fastrender::Rgba;
//!
//! // Create a canvas
//! let mut canvas = Canvas::new(800, 600, Rgba::WHITE)?;
//!
//! // Draw a red rectangle
//! let rect = Rect::from_xywh(100.0, 100.0, 200.0, 150.0);
//! canvas.draw_rect(rect, Rgba::rgb(255, 0, 0));
//!
//! // Draw a rounded rectangle
//! canvas.draw_rounded_rect(rect, 10.0, Rgba::rgb(0, 255, 0));
//!
//! // Get the resulting pixels
//! let pixmap = canvas.into_pixmap();
//! ```
//!
//! # CSS Specification References
//!
//! - CSS Backgrounds and Borders Level 3: Background/border painting
//! - CSS Color Level 4: Color handling
//! - CSS 2.1 Appendix E: Paint order

use super::display_list::BlendMode;
use super::display_list::BorderRadii;
#[cfg(test)]
use super::display_list::BorderRadius;
use super::display_list::FontVariation;
use crate::error::RenderError;
use crate::error::RenderStage;
use crate::error::Result;
use crate::geometry::Point;
use crate::geometry::Rect;
use crate::geometry::Size;
use crate::paint::clip_path::ResolvedClipPath;
use crate::paint::display_list::GlyphInstance;
use crate::paint::display_list::TextItem;
use crate::paint::pixmap::{new_pixmap, new_pixmap_with_context};
use crate::paint::text_rasterize::{
  concat_transforms, rotation_transform, GlyphCacheStats, TextRasterizer, TextRenderState,
  TextStroke,
};
use crate::paint::text_shadow::PathBounds;
use crate::render_control::{check_active, check_active_periodic};
use crate::style::color::Rgba;
use crate::style::types::FontSmoothing;
use crate::text::color_fonts::ColorGlyphRaster;
use crate::text::font_db::LoadedFont;
use crate::text::pipeline::{GlyphPosition, RunRotation, ShapedRun};
use rustybuzz::Variation as HbVariation;
use std::cell::RefCell;
use std::rc::Rc;
use std::ptr::NonNull;
use tiny_skia::BlendMode as SkiaBlendMode;
use tiny_skia::FillRule;
use tiny_skia::FilterQuality;
use tiny_skia::IntSize;
use tiny_skia::Mask;
use tiny_skia::MaskType;
use tiny_skia::Paint;
use tiny_skia::PathBuilder;
use tiny_skia::Pixmap;
use tiny_skia::PixmapPaint;
use tiny_skia::PixmapRef;
use tiny_skia::PixmapMut;
use tiny_skia::PremultipliedColorU8;
use tiny_skia::Rect as SkiaRect;
use tiny_skia::Stroke;
use tiny_skia::Transform;

type RenderResult<T> = std::result::Result<T, RenderError>;

#[derive(Default)]
struct RoundedRectPadScratch {
  pixmap: Option<Pixmap>,
  mask: Option<Mask>,
}

#[derive(Default)]
struct FillRectScratch {
  pixmap: Option<Pixmap>,
  mask: Option<Mask>,
}

thread_local! {
  static ROUNDED_RECT_PAD_SCRATCH: RefCell<RoundedRectPadScratch> =
    RefCell::new(RoundedRectPadScratch::default());
  static FILL_RECT_SCRATCH: RefCell<FillRectScratch> = RefCell::new(FillRectScratch::default());
}

/// Epsilon for treating device-space coordinates/translations as "integer aligned".
///
/// After subpixel layout, values that are conceptually integers frequently land at e.g. `1199.9998`
/// due to float noise. Snapping those to the nearest integer avoids falling back to tiny-skia
/// blending paths that differ from Chrome/Skia by ±1 in large translucent overlays.
///
/// This must remain small: it should absorb numerical noise, not quantize intentional subpixel
/// geometry (e.g. `translateX(0.5px)` animations).
const NEAR_INTEGER_EPSILON_PX: f32 = 1e-3;

// ============================================================================
// Canvas State
// ============================================================================

/// Graphics state for the canvas
///
/// Represents the current rendering state including transform, opacity, and clip.
/// States can be stacked to implement CSS effects like opacity layers.
#[derive(Debug, Clone)]
struct CanvasState {
  /// Current transform matrix
  transform: Transform,
  /// Current opacity (0.0 to 1.0)
  opacity: f32,
  /// Clip rectangle (if any)
  clip_rect: Option<Rect>,
  /// Clip mask (respects radii/intersections)
  clip_mask: Option<Rc<Mask>>,
  /// Blend mode
  blend_mode: SkiaBlendMode,
}

impl CanvasState {
  /// Creates a new default state
  fn new() -> Self {
    Self {
      transform: Transform::identity(),
      opacity: 1.0,
      clip_rect: None,
      clip_mask: None,
      blend_mode: SkiaBlendMode::SourceOver,
    }
  }

  /// Creates a paint with the current state applied
  fn create_paint(&self, color: Rgba) -> Paint<'static> {
    self.create_paint_with_blend(color, self.blend_mode)
  }

  /// Creates a paint with an explicit blend mode override
  fn create_paint_with_blend(&self, color: Rgba, blend_mode: SkiaBlendMode) -> Paint<'static> {
    let mut paint = Paint::default();
    // Apply opacity to alpha (color.a is already 0.0-1.0)
    let alpha = color.a * self.opacity;
    // Match Chrome/Skia: map float alpha to an 8-bit channel using rounding.
    //
    // This is important for half-alpha values like 0.5, which should map to 128 (not 127).
    // The exact rounding behavior affects many pixels in pageset comparisons.
    let alpha_u8 = (alpha * 255.0).round().clamp(0.0, 255.0) as u8;
    paint.set_color_rgba8(color.r, color.g, color.b, alpha_u8);
    paint.anti_alias = true;
    paint.blend_mode = blend_mode;
    paint
  }
}

impl Default for CanvasState {
  fn default() -> Self {
    Self::new()
  }
}

/// Backing storage for a canvas pixmap.
///
/// The vast majority of paint paths allocate an owned [`tiny_skia::Pixmap`]. The multiprocess
/// renderer, however, wants to paint directly into a shared-memory buffer, which is provided as a
/// `&mut [u8]`. `tiny-skia` supports painting into a borrowed buffer via [`tiny_skia::PixmapMut`],
/// so we keep a lightweight "external" variant that can materialize pixmap views on demand.
///
/// Safety: the external variant stores a raw pointer to avoid plumbing lifetimes through the paint
/// pipeline. It must only be constructed from a live `&mut [u8]` and must not outlive that slice.
#[derive(Debug)]
pub(crate) enum CanvasPixmap {
  Owned(Pixmap),
  External(ExternalPixmap),
}

#[derive(Debug)]
pub(crate) struct ExternalPixmap {
  ptr: NonNull<u8>,
  len: usize,
  width: u32,
  height: u32,
  stride_bytes: usize,
}

impl ExternalPixmap {
  fn new(data: &mut [u8], width: u32, height: u32, stride_bytes: usize) -> Result<Self> {
    // Keep consistent with `paint::pixmap::MAX_PIXMAP_BYTES`: even though we don't allocate the
    // buffer here (the caller owns it), we still want a hard cap on the amount of memory we will
    // treat as a render target to avoid pathological/DoS-sized canvases.
    if (stride_bytes as u64)
      .saturating_mul(height as u64)
      .saturating_mul(1)
      > crate::paint::pixmap::MAX_PIXMAP_BYTES
    {
      return Err(RenderError::InvalidParameters {
        message: format!(
          "external pixmap would use {} bytes (limit {})",
          (stride_bytes as u64).saturating_mul(height as u64),
          crate::paint::pixmap::MAX_PIXMAP_BYTES
        ),
      }
      .into());
    }
    let required = stride_bytes
      .checked_mul(height as usize)
      .ok_or_else(|| RenderError::InvalidParameters {
        message: "external pixmap size overflow".to_string(),
      })?;
    if required == 0 {
      return Err(RenderError::InvalidParameters {
        message: format!("external pixmap has zero size ({width}x{height})"),
      }
      .into());
    }
    if data.len() < required {
      return Err(RenderError::InvalidParameters {
        message: format!(
          "external pixmap buffer is too small: need {required} bytes, got {}",
          data.len()
        ),
      }
      .into());
    }
    let ptr = NonNull::new(data.as_mut_ptr()).ok_or_else(|| RenderError::InvalidParameters {
      message: "external pixmap buffer pointer is null".to_string(),
    })?;
    Ok(Self {
      ptr,
      len: required,
      width,
      height,
      stride_bytes,
    })
  }

  #[inline]
  unsafe fn as_slice(&self) -> &[u8] {
    std::slice::from_raw_parts(self.ptr.as_ptr(), self.len)
  }

  #[inline]
  unsafe fn as_slice_mut(&mut self) -> &mut [u8] {
    std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len)
  }

  #[inline]
  fn size(&self) -> IntSize {
    IntSize::from_wh(self.width, self.height).expect("validated external pixmap size")
  }
}

impl CanvasPixmap {
  #[inline]
  pub(crate) fn width(&self) -> u32 {
    match self {
      Self::Owned(pixmap) => pixmap.width(),
      Self::External(ext) => ext.width,
    }
  }

  #[inline]
  pub(crate) fn height(&self) -> u32 {
    match self {
      Self::Owned(pixmap) => pixmap.height(),
      Self::External(ext) => ext.height,
    }
  }

  #[inline]
  pub(crate) fn stride_bytes(&self) -> usize {
    match self {
      Self::Owned(pixmap) => pixmap.width() as usize * 4,
      Self::External(ext) => ext.stride_bytes,
    }
  }

  #[inline]
  pub(crate) fn data(&self) -> &[u8] {
    match self {
      Self::Owned(pixmap) => pixmap.data(),
      Self::External(ext) => unsafe { ext.as_slice() },
    }
  }

  #[inline]
  pub(crate) fn data_mut(&mut self) -> &mut [u8] {
    match self {
      Self::Owned(pixmap) => pixmap.data_mut(),
      Self::External(ext) => unsafe { ext.as_slice_mut() },
    }
  }

  pub(crate) fn clone_to_pixmap(&self) -> Option<Pixmap> {
    match self {
      Self::Owned(pixmap) => Some(pixmap.clone()),
      Self::External(ext) => {
        let width = ext.width;
        let height = ext.height;
        let mut pixmap = new_pixmap(width, height)?;
        // Copy row-by-row to tolerate an external stride larger than `width*4` (even though the
        // current no-copy paint path requires tight packing).
        let src_stride = ext.stride_bytes;
        let dst_stride = width as usize * 4;
        let row_bytes = dst_stride;
        let src = self.data();
        let dst = pixmap.data_mut();
        for row in 0..height as usize {
          let src_off = row * src_stride;
          let dst_off = row * dst_stride;
          dst[dst_off..dst_off + row_bytes].copy_from_slice(&src[src_off..src_off + row_bytes]);
        }
        Some(pixmap)
      }
    }
  }

  #[inline]
  pub(crate) fn as_ref(&self) -> PixmapRef<'_> {
    match self {
      Self::Owned(pixmap) => pixmap.as_ref(),
      Self::External(ext) => {
        // SAFETY: `ext.as_slice()` points to `ext.len` bytes of live memory. `size` and
        // dimensions were validated at construction.
        PixmapRef::from_bytes(unsafe { ext.as_slice() }, ext.width, ext.height)
          .expect("validated external pixmap view")
      }
    }
  }

  #[inline]
  pub(crate) fn as_mut(&mut self) -> PixmapMut<'_> {
    match self {
      Self::Owned(pixmap) => pixmap.as_mut(),
      Self::External(ext) => {
        let width = ext.width;
        let height = ext.height;
        // SAFETY: `ext.as_slice_mut()` points to `ext.len` bytes of live memory. `size` and
        // dimensions were validated at construction.
        PixmapMut::from_bytes(unsafe { ext.as_slice_mut() }, width, height)
          .expect("validated external pixmap view")
      }
    }
  }

  #[inline]
  pub(crate) fn fill(&mut self, color: tiny_skia::Color) {
    match self {
      Self::Owned(pixmap) => pixmap.fill(color),
      Self::External(_) => {
        let mut pixmap = self.as_mut();
        pixmap.fill(color);
      }
    }
  }

  #[inline]
  pub(crate) fn fill_path(
    &mut self,
    path: &tiny_skia::Path,
    paint: &Paint<'_>,
    fill_rule: FillRule,
    transform: Transform,
    clip: Option<&Mask>,
  ) {
    match self {
      Self::Owned(pixmap) => pixmap.fill_path(path, paint, fill_rule, transform, clip),
      Self::External(_) => {
        let mut pixmap = self.as_mut();
        pixmap.fill_path(path, paint, fill_rule, transform, clip)
      }
    }
  }

  #[inline]
  pub(crate) fn stroke_path(
    &mut self,
    path: &tiny_skia::Path,
    paint: &Paint<'_>,
    stroke: &Stroke,
    transform: Transform,
    clip: Option<&Mask>,
  ) {
    match self {
      Self::Owned(pixmap) => pixmap.stroke_path(path, paint, stroke, transform, clip),
      Self::External(_) => {
        let mut pixmap = self.as_mut();
        pixmap.stroke_path(path, paint, stroke, transform, clip)
      }
    }
  }

  #[inline]
  pub(crate) fn draw_pixmap(
    &mut self,
    x: i32,
    y: i32,
    src: PixmapRef<'_>,
    paint: &PixmapPaint,
    transform: Transform,
    clip: Option<&Mask>,
  ) {
    match self {
      Self::Owned(pixmap) => pixmap.draw_pixmap(x, y, src, paint, transform, clip),
      Self::External(_) => {
        let mut pixmap = self.as_mut();
        pixmap.draw_pixmap(x, y, src, paint, transform, clip)
      }
    }
  }

  #[inline]
  pub(crate) fn pixels(&self) -> &[PremultipliedColorU8] {
    match self {
      Self::Owned(pixmap) => pixmap.pixels(),
      Self::External(_) => self.as_ref().pixels(),
    }
  }

  #[inline]
  pub(crate) fn pixels_mut(&mut self) -> &mut [PremultipliedColorU8] {
    match self {
      Self::Owned(pixmap) => pixmap.pixels_mut(),
      Self::External(ext) => {
        // NOTE: The Canvas code assumes tight packing when it indexes into `pixels_mut()` using
        // `stride = width`. If we ever want to support external pixmaps with padding, we should
        // plumb stride-aware row accessors through the internal compositing paths instead of
        // exposing a flat `[PremultipliedColorU8]` slice.
        debug_assert_eq!(
          ext.stride_bytes,
          ext.width as usize * 4,
          "pixels_mut requires a tightly packed external pixmap"
        );
        let pixels = (ext.width as usize)
          .checked_mul(ext.height as usize)
          .expect("validated external pixmap size");
        // SAFETY: `PremultipliedColorU8` has 1-byte alignment, and the external buffer is a live
        // writable byte slice at least `width*height*4` bytes long (enforced by `ExternalPixmap::new`).
        unsafe { std::slice::from_raw_parts_mut(ext.ptr.as_ptr() as *mut PremultipliedColorU8, pixels) }
      }
    }
  }

  #[inline]
  pub(crate) fn into_owned(self) -> Option<Pixmap> {
    match self {
      Self::Owned(pixmap) => Some(pixmap),
      Self::External(_) => None,
    }
  }

  #[inline]
  pub(crate) fn as_owned(&self) -> Option<&Pixmap> {
    match self {
      Self::Owned(pixmap) => Some(pixmap),
      Self::External(_) => None,
    }
  }

  #[inline]
  pub(crate) fn as_owned_mut(&mut self) -> Option<&mut Pixmap> {
    match self {
      Self::Owned(pixmap) => Some(pixmap),
      Self::External(_) => None,
    }
  }
}

#[derive(Debug)]
pub(crate) struct LayerRecord {
  pub(crate) pixmap: CanvasPixmap,
  saved_state_depth: usize,
  parent_opacity: f32,
  parent_blend_mode: SkiaBlendMode,
  parent_transform: Transform,
  parent_clip_rect: Option<Rect>,
  parent_clip_mask: Option<Rc<Mask>>,
  opacity: f32,
  composite_blend: Option<SkiaBlendMode>,
  pub(crate) origin: (i32, i32),
  pub(crate) is_backdrop_root: bool,
  /// True when this layer's pixmap was initialized from the already-painted backdrop.
  ///
  /// This is used for non-isolated compositing groups (e.g. non-isolated `mix-blend-mode` groups),
  /// where the group surface starts as a copy of its parent surface.
  ///
  /// Note: even though the layer surface may contain backdrop pixels, backdrop *sampling* for
  /// descendant `backdrop-filter` effects must still respect Backdrop Root boundaries. See
  /// [`Canvas::fill_backdrop_root_region`] for how we strip the initialization backdrop when the
  /// layer is acting as the Backdrop Root.
  pub(crate) init_from_backdrop: bool,
  /// Tracks the alpha coverage of the layer's computed element when `init_from_backdrop` is true.
  ///
  /// The offscreen pixmap for non-isolated groups is initialized from the already-painted
  /// backdrop so descendants with blend modes can sample the correct backdrop. This makes the
  /// layer alpha ambiguous when the backdrop is fully opaque (`out_a` is always 1.0 under
  /// source-over compositing), so we also paint the layer content into a transparent surface to
  /// recover the source alpha during uncompositing.
  source_alpha: Option<Pixmap>,
}

impl LayerRecord {
  #[inline]
  pub(crate) fn effective_opacity(&self) -> f32 {
    (self.opacity * self.parent_opacity).clamp(0.0, 1.0)
  }

  #[inline]
  pub(crate) fn effective_blend_mode(&self) -> SkiaBlendMode {
    self.composite_blend.unwrap_or(self.parent_blend_mode)
  }
  #[inline]
  pub(crate) fn is_initialized_from_backdrop(&self) -> bool {
    self.init_from_backdrop
  }

  #[inline]
  pub(crate) fn source_alpha(&self) -> Option<&Pixmap> {
    self.source_alpha.as_ref()
  }
}

#[derive(Clone, Copy)]
pub(crate) struct LayerCompositeMetadata<'a> {
  pub origin: (i32, i32),
  pub opacity: f32,
  pub blend_mode: SkiaBlendMode,
  pub clip_mask: Option<&'a Mask>,
}

// ============================================================================
// Canvas
// ============================================================================

/// Canvas for 2D graphics rendering using tiny-skia
///
/// Provides a high-level API for drawing primitives (rectangles, text, etc.)
/// to a pixel buffer. Maintains a stack of graphics states for implementing
/// CSS effects like opacity layers and transforms.
///
/// # Thread Safety
///
/// Canvas is not thread-safe. Create separate Canvas instances for each thread
/// if parallel rendering is needed.
///
/// # Memory Usage
///
/// The canvas allocates memory for the pixel buffer (width × height × 4 bytes)
/// plus state stack overhead.
pub struct Canvas {
  /// The underlying pixel buffer
  pixmap: CanvasPixmap,
  /// Stack of graphics states
  state_stack: Vec<CanvasState>,
  /// Stack of offscreen layers for grouped effects
  layer_stack: Vec<LayerRecord>,
  /// Depth counter for source-alpha mirroring.
  ///
  /// When non-zero, drawing operations must not recursively mirror into `source_alpha` surfaces
  /// because an outer call is already replaying the same drawing operations onto both surfaces.
  source_alpha_recording_depth: usize,
  /// Current graphics state
  current_state: CanvasState,
  /// Cached text rasterizer
  text_rasterizer: TextRasterizer,
}

impl Canvas {
  /// Creates a new canvas with the given dimensions and background color
  ///
  /// # Arguments
  ///
  /// * `width` - Canvas width in pixels
  /// * `height` - Canvas height in pixels
  /// * `background` - Background fill color
  ///
  /// # Returns
  ///
  /// Returns a new Canvas or an error if the dimensions are invalid.
  ///
  /// # Errors
  ///
  /// Returns `RenderError::InvalidParameters` if:
  /// - Width or height is zero
  /// - Width × height would overflow
  /// - Allocation fails
  ///
  /// # Examples
  ///
  /// ```rust,ignore
  /// use fastrender::paint::canvas::Canvas;
  /// use fastrender::Rgba;
  ///
  /// let canvas = Canvas::new(800, 600, Rgba::WHITE)?;
  /// assert_eq!(canvas.width(), 800);
  /// assert_eq!(canvas.height(), 600);
  /// ```
  pub fn new(width: u32, height: u32, background: Rgba) -> Result<Self> {
    Self::new_with_text_rasterizer(width, height, background, TextRasterizer::new())
  }

  /// Creates a new canvas with an explicit text rasterizer (shared caches, etc.).
  pub fn new_with_text_rasterizer(
    width: u32,
    height: u32,
    background: Rgba,
    text_rasterizer: TextRasterizer,
  ) -> Result<Self> {
    let pixmap = CanvasPixmap::Owned(new_pixmap_with_context(width, height, "canvas")?);

    let mut canvas = Self {
      pixmap,
      state_stack: Vec::new(),
      layer_stack: Vec::new(),
      source_alpha_recording_depth: 0,
      current_state: CanvasState::new(),
      text_rasterizer,
    };

    // Fill with background color
    canvas.clear(background);

    Ok(canvas)
  }

  /// Wraps a packed RGBA buffer in a Canvas without allocating.
  ///
  /// The buffer must contain at least `stride_bytes * height` bytes.
  pub(crate) fn from_rgba_buffer(
    out: &mut [u8],
    width: u32,
    height: u32,
    stride_bytes: usize,
    background: Rgba,
    text_rasterizer: TextRasterizer,
  ) -> Result<Self> {
    let expected_stride = width as usize * 4;
    if stride_bytes != expected_stride {
      return Err(RenderError::InvalidParameters {
        message: format!(
          "external canvas pixmap must be tightly packed (stride_bytes={stride_bytes}, expected={expected_stride})"
        ),
      }
      .into());
    }
    let pixmap = CanvasPixmap::External(ExternalPixmap::new(out, width, height, stride_bytes)?);
    let mut canvas = Self {
      pixmap,
      state_stack: Vec::new(),
      layer_stack: Vec::new(),
      source_alpha_recording_depth: 0,
      current_state: CanvasState::new(),
      text_rasterizer,
    };
    canvas.clear(background);
    Ok(canvas)
  }

  /// Creates a new canvas with transparent background
  ///
  /// # Examples
  ///
  /// ```rust,ignore
  /// let canvas = Canvas::new_transparent(400, 300)?;
  /// ```
  pub fn new_transparent(width: u32, height: u32) -> Result<Self> {
    Self::new(width, height, Rgba::TRANSPARENT)
  }

  /// Wraps an existing pixmap in a Canvas without clearing it.
  pub fn from_pixmap(pixmap: Pixmap) -> Self {
    Self {
      pixmap: CanvasPixmap::Owned(pixmap),
      state_stack: Vec::new(),
      layer_stack: Vec::new(),
      source_alpha_recording_depth: 0,
      current_state: CanvasState::new(),
      text_rasterizer: TextRasterizer::new(),
    }
  }

  /// Wraps an existing pixmap in a Canvas using an explicit text rasterizer (shared caches, etc.)
  /// without clearing the pixmap.
  pub fn from_pixmap_with_text_rasterizer(pixmap: Pixmap, text_rasterizer: TextRasterizer) -> Self {
    Self {
      pixmap: CanvasPixmap::Owned(pixmap),
      state_stack: Vec::new(),
      layer_stack: Vec::new(),
      source_alpha_recording_depth: 0,
      current_state: CanvasState::new(),
      text_rasterizer,
    }
  }

  /// Returns the canvas width in pixels
  #[inline]
  pub fn width(&self) -> u32 {
    self.pixmap.width()
  }

  /// Returns the canvas height in pixels
  #[inline]
  pub fn height(&self) -> u32 {
    self.pixmap.height()
  }

  /// Returns the canvas size
  #[inline]
  pub fn size(&self) -> Size {
    Size::new(self.width() as f32, self.height() as f32)
  }

  /// Returns the canvas bounds as a rectangle
  #[inline]
  pub fn bounds(&self) -> Rect {
    Rect::from_xywh(0.0, 0.0, self.width() as f32, self.height() as f32)
  }

  /// Clears the canvas with the given color
  ///
  /// # Examples
  ///
  /// ```rust,ignore
  /// canvas.clear(Rgba::WHITE);
  /// ```
  pub fn clear(&mut self, color: Rgba) {
    let skia_color = tiny_skia::Color::from_rgba8(color.r, color.g, color.b, color.alpha_u8());
    self.pixmap.fill(skia_color);
  }

  /// Consumes the canvas and returns the underlying pixmap
  ///
  /// # Examples
  ///
  /// ```rust,ignore
  /// let pixmap = canvas.into_pixmap();
  /// pixmap.save_png("output.png")?;
  /// ```
  pub fn into_pixmap(self) -> Pixmap {
    self
      .pixmap
      .into_owned()
      .expect("into_pixmap is only valid for owned canvases")
  }

  /// Returns a reference to the underlying pixmap
  #[inline]
  pub fn pixmap(&self) -> &Pixmap {
    self
      .pixmap
      .as_owned()
      .expect("pixmap() is only valid for owned canvases")
  }

  /// Returns a borrowed pixmap view (works for both owned and external canvases).
  #[inline]
  pub(crate) fn pixmap_ref(&self) -> PixmapRef<'_> {
    self.pixmap.as_ref()
  }

  /// Returns a mutable reference to the underlying pixmap
  #[inline]
  pub fn pixmap_mut(&mut self) -> &mut Pixmap {
    self
      .pixmap
      .as_owned_mut()
      .expect("pixmap_mut() is only valid for owned canvases")
  }

  /// Runs a mutation against the active pixmap and (when present) the layer's source-alpha
  /// tracking surface.
  ///
  /// This is primarily used by paint code paths that need direct access to tiny-skia APIs.
  pub(crate) fn with_mirrored_pixmap_mut<F>(&mut self, mut f: F)
  where
    F: for<'a> FnMut(&mut PixmapMut<'a>),
  {
    self.mirror_to_source_alpha(|canvas| {
      let mut pixmap = canvas.pixmap.as_mut();
      f(&mut pixmap);
    });
  }

  pub(crate) fn with_mirrored_pixmap_mut_result<T, F>(&mut self, mut f: F) -> Result<T>
  where
    F: for<'a> FnMut(&mut PixmapMut<'a>) -> Result<T>,
  {
    self.mirror_to_source_alpha_result(|canvas| {
      let mut pixmap = canvas.pixmap.as_mut();
      f(&mut pixmap)
    })
  }

  fn mirror_to_source_alpha<F>(&mut self, mut draw: F)
  where
    F: FnMut(&mut Canvas),
  {
    if self.source_alpha_recording_depth > 0 {
      draw(self);
      return;
    }
    if self
      .layer_stack
      .last()
      .and_then(|record| record.source_alpha.as_ref())
      .is_none()
    {
      draw(self);
      return;
    }

    self.source_alpha_recording_depth = self.source_alpha_recording_depth.saturating_add(1);
    draw(self);

    let source_alpha = self
      .layer_stack
      .last_mut()
      .and_then(|record| record.source_alpha.take());
    if let Some(source_alpha) = source_alpha {
      let main = std::mem::replace(&mut self.pixmap, CanvasPixmap::Owned(source_alpha));
      draw(self);
      let source_alpha = std::mem::replace(&mut self.pixmap, main)
        .into_owned()
        .expect("source-alpha replay pixmap must be owned");
      if let Some(record) = self.layer_stack.last_mut() {
        record.source_alpha = Some(source_alpha);
      }
    }

    self.source_alpha_recording_depth = self.source_alpha_recording_depth.saturating_sub(1);
  }

  fn mirror_to_source_alpha_result<T, F>(&mut self, mut draw: F) -> Result<T>
  where
    F: FnMut(&mut Canvas) -> Result<T>,
  {
    if self.source_alpha_recording_depth > 0 {
      return draw(self);
    }
    if self
      .layer_stack
      .last()
      .and_then(|record| record.source_alpha.as_ref())
      .is_none()
    {
      return draw(self);
    }

    self.source_alpha_recording_depth = self.source_alpha_recording_depth.saturating_add(1);
    let value = match draw(self) {
      Ok(v) => v,
      Err(err) => {
        self.source_alpha_recording_depth = self.source_alpha_recording_depth.saturating_sub(1);
        return Err(err);
      }
    };

    let source_alpha = self
      .layer_stack
      .last_mut()
      .and_then(|record| record.source_alpha.take());
    if let Some(source_alpha) = source_alpha {
      let main = std::mem::replace(&mut self.pixmap, CanvasPixmap::Owned(source_alpha));
      let replay = draw(self);
      let source_alpha = std::mem::replace(&mut self.pixmap, main)
        .into_owned()
        .expect("source-alpha replay pixmap must be owned");
      if let Some(record) = self.layer_stack.last_mut() {
        record.source_alpha = Some(source_alpha);
      }
      self.source_alpha_recording_depth = self.source_alpha_recording_depth.saturating_sub(1);
      replay.map(|_| value)
    } else {
      self.source_alpha_recording_depth = self.source_alpha_recording_depth.saturating_sub(1);
      Ok(value)
    }
  }

  /// Returns glyph cache statistics for text rendering.
  pub fn text_cache_stats(&self) -> GlyphCacheStats {
    self.text_rasterizer.cache_stats()
  }

  /// Resets glyph cache stats without clearing cached outlines.
  pub fn reset_text_cache_stats(&mut self) {
    self.text_rasterizer.reset_cache_stats();
  }

  /// Returns a mutable reference to the pixmap that will receive composited output.
  ///
  /// When painting inside an offscreen layer, this refers to the parent layer's pixmap
  /// that already contains the backdrop content.
  pub(crate) fn backdrop_pixmap_mut(&mut self) -> &mut CanvasPixmap {
    self
      .layer_stack
      .last_mut()
      .map(|layer| &mut layer.pixmap)
      .unwrap_or(&mut self.pixmap)
  }

  /// Returns references to the backdrop (parent) pixmap and the current pixmap.
  ///
  /// This is primarily used for `backdrop-filter`, which samples from the already-painted
  /// backdrop while writing into the current offscreen layer.
  ///
  /// Returns `None` when there is no parent pixmap (i.e. no active offscreen layer).
  pub(crate) fn split_backdrop_and_pixmap_mut(&mut self) -> Option<(&CanvasPixmap, &mut Pixmap)> {
    let backdrop = self.layer_stack.last().map(|layer| &layer.pixmap)?;
    let pixmap = self.pixmap.as_owned_mut()?;
    Some((backdrop, pixmap))
  }

  /// Ensures the current layer has a source-alpha recording surface.
  ///
  /// This is primarily used when a non-isolated compositing group lazily injects backdrop pixels
  /// (via destination-over) partway through painting: we must capture the group's computed element
  /// *before* the backdrop injection, otherwise fully opaque backdrops would make the source alpha
  /// unrecoverable during uncompositing.
  pub(crate) fn ensure_current_layer_source_alpha(&mut self) -> Result<()> {
    let Some(record) = self.layer_stack.last_mut() else {
      return Ok(());
    };
    if record.source_alpha.is_some() {
      return Ok(());
    }
    let width = self.pixmap.width();
    let height = self.pixmap.height();
    let mut source_alpha = new_pixmap_with_context(width, height, "layer_source_alpha")?;
    source_alpha.data_mut().copy_from_slice(self.pixmap.data());
    record.source_alpha = Some(source_alpha);
    Ok(())
  }

  /// Marks the current (topmost) offscreen layer as having been initialized from the backdrop.
  ///
  /// This flag is used by [`Canvas::fill_backdrop_root_region`] to ensure Backdrop Root scoping is
  /// preserved even when a non-isolated compositing group surface lazily injects backdrop pixels
  /// (e.g. for descendant `mix-blend-mode` operations).
  pub(crate) fn mark_current_layer_initialized_from_backdrop(&mut self) {
    if let Some(record) = self.layer_stack.last_mut() {
      record.init_from_backdrop = true;
    }
  }

  pub(crate) fn layer_stack_len(&self) -> usize {
    self.layer_stack.len()
  }

  pub(crate) fn layer_stack(&self) -> &[LayerRecord] {
    &self.layer_stack
  }

  pub(crate) fn layer_stack_pixmap(&self, index: usize) -> Option<&CanvasPixmap> {
    self.layer_stack.get(index).map(|layer| &layer.pixmap)
  }

  pub(crate) fn layer_stack_child_origin(&self, index: usize) -> Option<(i32, i32)> {
    self.layer_stack.get(index).map(|layer| layer.origin)
  }

  pub(crate) fn split_layer_stack_and_pixmap_mut(&mut self) -> (&[LayerRecord], &mut Pixmap) {
    (
      &self.layer_stack,
      self
        .pixmap
        .as_owned_mut()
        .expect("split_layer_stack_and_pixmap_mut requires an owned active pixmap"),
    )
  }

  // ========================================================================
  // State Management
  // ========================================================================

  /// Saves the current graphics state to the stack
  ///
  /// The saved state can be restored later with `restore()`.
  /// Use this to implement CSS effects like opacity layers.
  ///
  /// # Examples
  ///
  /// ```rust,ignore
  /// canvas.save();
  /// canvas.set_opacity(0.5);
  /// canvas.draw_rect(rect, Rgba::RED);
  /// canvas.restore(); // Opacity is back to 1.0
  /// ```
  pub fn save(&mut self) {
    self.state_stack.push(self.current_state.clone());
  }

  /// Restores the previously saved graphics state
  ///
  /// Pops the most recently saved state from the stack.
  /// Does nothing if the stack is empty.
  pub fn restore(&mut self) {
    if let Some(state) = self.state_stack.pop() {
      self.current_state = state;
    }
  }

  /// Returns the current state stack depth
  #[inline]
  pub fn state_depth(&self) -> usize {
    self.state_stack.len()
  }

  /// Sets the current opacity (0.0 to 1.0)
  ///
  /// Opacity is multiplied with color alpha when drawing.
  ///
  /// # Examples
  ///
  /// ```rust,ignore
  /// canvas.set_opacity(0.5); // 50% opacity
  /// canvas.draw_rect(rect, Rgba::RED); // Draws at 50% opacity
  /// ```
  pub fn set_opacity(&mut self, opacity: f32) {
    self.current_state.opacity = opacity.clamp(0.0, 1.0);
  }

  /// Returns the current opacity
  #[inline]
  pub fn opacity(&self) -> f32 {
    self.current_state.opacity
  }

  /// Pushes a new offscreen layer for grouped compositing (e.g., opacity).
  pub fn push_layer(&mut self, opacity: f32) -> Result<()> {
    self.push_layer_with_blend_and_backdrop_root(opacity, None, false)
  }

  /// Pushes a new offscreen layer with explicit bounds.
  ///
  /// The layer pixmap will be clipped to the provided bounds and drawing inside the layer will be
  /// translated so global coordinates continue to work.
  pub fn push_layer_bounded(
    &mut self,
    opacity: f32,
    blend: Option<SkiaBlendMode>,
    bounds: Rect,
  ) -> Result<()> {
    self.push_layer_bounded_with_backdrop_root(opacity, blend, bounds, false)
  }

  pub(crate) fn push_layer_bounded_with_backdrop_root(
    &mut self,
    opacity: f32,
    blend: Option<SkiaBlendMode>,
    bounds: Rect,
    is_backdrop_root: bool,
  ) -> Result<()> {
    let (origin_x, origin_y, width, height) = match self.layer_bounds(bounds) {
      Some(b) => b,
      None => (0, 0, self.pixmap.width(), self.pixmap.height()),
    };
    self.push_layer_internal(
      opacity,
      blend,
      origin_x,
      origin_y,
      width,
      height,
      is_backdrop_root,
      false,
      true,
    )
  }

  /// Pushes a new offscreen layer with explicit bounds, without inheriting the current clip.
  ///
  /// This is primarily used for filter effects: ancestor clips should apply when the filtered
  /// result is composited back into the parent surface, not while rasterizing the filter input.
  pub(crate) fn push_layer_bounded_unclipped_with_backdrop_root(
    &mut self,
    opacity: f32,
    blend: Option<SkiaBlendMode>,
    bounds: Rect,
    is_backdrop_root: bool,
  ) -> Result<()> {
    let (origin_x, origin_y, width, height) = match self.layer_bounds_unclamped(bounds) {
      Some(b) => b,
      None => (0, 0, self.pixmap.width(), self.pixmap.height()),
    };
    self.push_layer_internal(
      opacity,
      blend,
      origin_x,
      origin_y,
      width,
      height,
      is_backdrop_root,
      false,
      false,
    )
  }

  /// Pushes a new offscreen layer with an explicit composite blend mode.
  pub fn push_layer_with_blend(
    &mut self,
    opacity: f32,
    blend: Option<SkiaBlendMode>,
  ) -> Result<()> {
    self.push_layer_with_blend_and_backdrop_root(opacity, blend, false)
  }

  pub(crate) fn push_layer_with_blend_and_backdrop_root(
    &mut self,
    opacity: f32,
    blend: Option<SkiaBlendMode>,
    is_backdrop_root: bool,
  ) -> Result<()> {
    self.push_layer_internal(
      opacity,
      blend,
      0,
      0,
      self.pixmap.width(),
      self.pixmap.height(),
      is_backdrop_root,
      false,
      true,
    )
  }

  pub(crate) fn push_layer_with_blend_initialized_from_backdrop_and_backdrop_root(
    &mut self,
    opacity: f32,
    blend: Option<SkiaBlendMode>,
    is_backdrop_root: bool,
  ) -> Result<()> {
    self.push_layer_internal(
      opacity,
      blend,
      0,
      0,
      self.pixmap.width(),
      self.pixmap.height(),
      is_backdrop_root,
      true,
      true,
    )
  }

  pub(crate) fn push_layer_bounded_initialized_from_backdrop_and_backdrop_root(
    &mut self,
    opacity: f32,
    blend: Option<SkiaBlendMode>,
    bounds: Rect,
    is_backdrop_root: bool,
  ) -> Result<()> {
    let (origin_x, origin_y, width, height) = match self.layer_bounds(bounds) {
      Some(b) => b,
      None => (0, 0, self.pixmap.width(), self.pixmap.height()),
    };
    self.push_layer_internal(
      opacity,
      blend,
      origin_x,
      origin_y,
      width,
      height,
      is_backdrop_root,
      true,
      true,
    )
  }

  pub(crate) fn push_layer_bounded_unclipped_initialized_from_backdrop_and_backdrop_root(
    &mut self,
    opacity: f32,
    blend: Option<SkiaBlendMode>,
    bounds: Rect,
    is_backdrop_root: bool,
  ) -> Result<()> {
    let (origin_x, origin_y, width, height) = match self.layer_bounds_unclamped(bounds) {
      Some(b) => b,
      None => (0, 0, self.pixmap.width(), self.pixmap.height()),
    };
    self.push_layer_internal(
      opacity,
      blend,
      origin_x,
      origin_y,
      width,
      height,
      is_backdrop_root,
      true,
      false,
    )
  }

  /// Pushes a new offscreen layer initialized from the already-painted backdrop.
  ///
  /// This is used for non-isolated compositing groups (e.g. CSS non-isolated `mix-blend-mode`
  /// groups) whose initial backdrop is the current contents of the canvas.
  pub fn push_layer_with_blend_initialized_from_backdrop(
    &mut self,
    opacity: f32,
    blend: Option<SkiaBlendMode>,
  ) -> Result<()> {
    self.push_layer_with_blend_initialized_from_backdrop_and_backdrop_root(opacity, blend, false)
  }

  /// Pushes a new bounded offscreen layer initialized from the already-painted backdrop.
  pub fn push_layer_bounded_initialized_from_backdrop(
    &mut self,
    opacity: f32,
    blend: Option<SkiaBlendMode>,
    bounds: Rect,
  ) -> Result<()> {
    self
      .push_layer_bounded_initialized_from_backdrop_and_backdrop_root(opacity, blend, bounds, false)
  }

  fn push_layer_internal(
    &mut self,
    opacity: f32,
    blend: Option<SkiaBlendMode>,
    origin_x: i32,
    origin_y: i32,
    width: u32,
    height: u32,
    is_backdrop_root: bool,
    init_from_backdrop: bool,
    inherit_clip: bool,
  ) -> Result<()> {
    let parent_width = self.pixmap.width();
    let parent_height = self.pixmap.height();
    let width = width.max(1);
    let height = height.max(1);

    let mut new_pixmap = new_pixmap_with_context(width, height, "layer")?;
    if init_from_backdrop {
      Self::copy_pixmap_region(&mut new_pixmap, &self.pixmap, origin_x, origin_y)?;
    }
    let source_alpha = init_from_backdrop
      .then(|| {
        let mut pixmap = new_pixmap_with_context(width, height, "layer_source_alpha")?;
        pixmap.data_mut().fill(0);
        Ok::<_, RenderError>(pixmap)
      })
      .transpose()?;

    let parent_transform = self.current_state.transform;

    let record = LayerRecord {
      pixmap: std::mem::replace(&mut self.pixmap, CanvasPixmap::Owned(new_pixmap)),
      saved_state_depth: self.state_stack.len(),
      parent_opacity: self.current_state.opacity,
      parent_blend_mode: self.current_state.blend_mode,
      parent_transform,
      parent_clip_rect: self.current_state.clip_rect,
      parent_clip_mask: self.current_state.clip_mask.clone(),
      opacity: opacity.clamp(0.0, 1.0),
      composite_blend: blend,
      origin: (origin_x, origin_y),
      is_backdrop_root,
      init_from_backdrop,
      source_alpha,
    };
    self.layer_stack.push(record);
    // Painting inside the layer should start from a neutral state.
    self.current_state.opacity = 1.0;
    self.current_state.blend_mode = SkiaBlendMode::SourceOver;

    if !inherit_clip {
      self.current_state.clip_rect = None;
      self.current_state.clip_mask = None;
    }

    if origin_x != 0 || origin_y != 0 || width != parent_width || height != parent_height {
      let layer_rect = Rect::from_xywh(
        origin_x as f32,
        origin_y as f32,
        width as f32,
        height as f32,
      );
      self.current_state.transform = self
        .current_state
        .transform
        .pre_translate(-(origin_x as f32), -(origin_y as f32));
      if inherit_clip {
        if let Some(clip_rect) = self.current_state.clip_rect.take() {
          let intersected = clip_rect.intersection(layer_rect).unwrap_or(Rect::ZERO);
          self.current_state.clip_rect =
            if intersected.width() <= 0.0 || intersected.height() <= 0.0 {
              Some(Rect::ZERO)
            } else {
              Some(Rect::from_xywh(
                intersected.x() - origin_x as f32,
                intersected.y() - origin_y as f32,
                intersected.width(),
                intersected.height(),
              ))
            };
        }
        if let Some(mask) = self.current_state.clip_mask.take() {
          self.current_state.clip_mask =
            crop_mask_i32(mask.as_ref(), origin_x, origin_y, width, height)?.map(Rc::new);
        }
      }
    }

    Ok(())
  }

  fn copy_pixmap_region(
    dst: &mut Pixmap,
    src: &CanvasPixmap,
    src_x: i32,
    src_y: i32,
  ) -> RenderResult<()> {
    let dst_w = dst.width() as i32;
    let dst_h = dst.height() as i32;
    if dst_w <= 0 || dst_h <= 0 {
      return Ok(());
    }
    let src_w = src.width() as i32;
    let src_h = src.height() as i32;
    if src_w <= 0 || src_h <= 0 {
      return Ok(());
    }

    let x0 = src_x.max(0);
    let y0 = src_y.max(0);
    let x1 = (src_x + dst_w).min(src_w);
    let y1 = (src_y + dst_h).min(src_h);
    if x0 >= x1 || y0 >= y1 {
      return Ok(());
    }

    let dst_off_x = (x0 - src_x) as usize;
    let dst_off_y = (y0 - src_y) as usize;
    let copy_w = (x1 - x0) as usize;
    let copy_h = (y1 - y0) as usize;

    let dst_stride = dst.width() as usize * 4;
    let src_stride = src.width() as usize * 4;
    let row_bytes = copy_w * 4;
    let src_data = src.data();
    let dst_data = dst.data_mut();

    let mut deadline_counter = 0usize;
    for row in 0..copy_h {
      check_active_periodic(&mut deadline_counter, 32, RenderStage::Paint)?;
      let src_idx = (y0 as usize + row) * src_stride + x0 as usize * 4;
      let dst_idx = (dst_off_y + row) * dst_stride + dst_off_x * 4;
      dst_data[dst_idx..dst_idx + row_bytes]
        .copy_from_slice(&src_data[src_idx..src_idx + row_bytes]);
    }

    Ok(())
  }

  fn layer_bounds(&self, bounds: Rect) -> Option<(i32, i32, u32, u32)> {
    if !bounds.x().is_finite()
      || !bounds.y().is_finite()
      || !bounds.width().is_finite()
      || !bounds.height().is_finite()
    {
      return None;
    }

    let x0f = bounds.min_x().floor();
    let y0f = bounds.min_y().floor();
    let x1f = bounds.max_x().ceil();
    let y1f = bounds.max_y().ceil();

    // Avoid saturating float→int casts producing surprising enormous bounds.
    if x0f < i32::MIN as f32
      || x0f > i32::MAX as f32
      || y0f < i32::MIN as f32
      || y0f > i32::MAX as f32
      || x1f < i32::MIN as f32
      || x1f > i32::MAX as f32
      || y1f < i32::MIN as f32
      || y1f > i32::MAX as f32
    {
      return None;
    }

    let x0 = x0f as i32;
    let y0 = y0f as i32;
    let x1 = x1f as i32;
    let y1 = y1f as i32;

    let width_i64 = x1 as i64 - x0 as i64;
    let height_i64 = y1 as i64 - y0 as i64;
    let Ok(width) = u32::try_from(width_i64) else {
      return None;
    };
    let Ok(height) = u32::try_from(height_i64) else {
      return None;
    };
    if width == 0 || height == 0 {
      return None;
    }
    Some((x0, y0, width, height))
  }

  fn layer_bounds_unclamped(&self, bounds: Rect) -> Option<(i32, i32, u32, u32)> {
    if !bounds.x().is_finite()
      || !bounds.y().is_finite()
      || !bounds.width().is_finite()
      || !bounds.height().is_finite()
    {
      return None;
    }

    let x0_f = bounds.min_x().floor();
    let y0_f = bounds.min_y().floor();
    let x1_f = bounds.max_x().ceil();
    let y1_f = bounds.max_y().ceil();
    if !x0_f.is_finite() || !y0_f.is_finite() || !x1_f.is_finite() || !y1_f.is_finite() {
      return None;
    }

    // Avoid relying on saturating float→int casts for pathological coordinates.
    let i32_min = i32::MIN as f32;
    let i32_max = i32::MAX as f32;
    if x0_f < i32_min || x0_f > i32_max || y0_f < i32_min || y0_f > i32_max {
      return None;
    }
    if x1_f < i32_min || x1_f > i32_max || y1_f < i32_min || y1_f > i32_max {
      return None;
    }

    let origin_x = x0_f as i32;
    let origin_y = y0_f as i32;
    let x1 = x1_f as i32;
    let y1 = y1_f as i32;

    let width_i64 = i64::from(x1) - i64::from(origin_x);
    let height_i64 = i64::from(y1) - i64::from(origin_y);
    let width = u32::try_from(width_i64).ok()?;
    let height = u32::try_from(height_i64).ok()?;
    if width == 0 || height == 0 {
      return None;
    }

    Some((origin_x, origin_y, width, height))
  }

  /// Pops the most recent offscreen layer without compositing it.
  ///
  /// Returns the layer pixmap, the effective opacity (including parent opacity),
  /// and any explicit composite blend mode that was requested.
  pub fn pop_layer_raw(&mut self) -> Result<(Pixmap, (i32, i32), f32, Option<SkiaBlendMode>)> {
    let Some(record) = self.layer_stack.pop() else {
      return Err(
        RenderError::InvalidParameters {
          message: "pop_layer without matching push".into(),
        }
        .into(),
      );
    };

    let mut layer_pixmap = std::mem::replace(&mut self.pixmap, record.pixmap)
      .into_owned()
      .expect("layer pixmap must be owned");
    self.state_stack.truncate(record.saved_state_depth);
    self.current_state.opacity = record.parent_opacity;
    self.current_state.blend_mode = record.parent_blend_mode;
    self.current_state.transform = record.parent_transform;
    self.current_state.clip_rect = record.parent_clip_rect;
    self.current_state.clip_mask = record.parent_clip_mask;
    if record.init_from_backdrop {
      uncomposite_layer_source_over_backdrop(
        &mut layer_pixmap,
        self.pixmap.as_ref(),
        record.origin,
        record.source_alpha.as_ref().map(|alpha| (alpha, (0, 0))),
      )?;
    }
    let opacity = (record.opacity * self.current_state.opacity).clamp(0.0, 1.0);
    Ok((layer_pixmap, record.origin, opacity, record.composite_blend))
  }

  /// Pops the most recent offscreen layer and composites it into the parent.
  pub fn pop_layer(&mut self) -> Result<()> {
    let (layer_pixmap, origin, opacity, composite_blend) = self.pop_layer_raw()?;

    self.composite_layer(&layer_pixmap, opacity, composite_blend, origin);
    Ok(())
  }

  pub(crate) fn composite_layer(
    &mut self,
    layer: &Pixmap,
    opacity: f32,
    composite_blend: Option<SkiaBlendMode>,
    origin: (i32, i32),
  ) {
    let blend_mode = composite_blend.unwrap_or(self.current_state.blend_mode);
    // `clip_rect` is frequently tracked without a full per-pixel mask (see
    // `set_clip_with_radii_impl`'s bounds-only optimization). Most draw paths can scissor against
    // `clip_rect`, but layer compositing needs to respect it explicitly or filter/opacity layers
    // will leak outside `overflow:hidden` clips.
    //
    // For non-source-over blend modes we rely on tiny-skia compositing, which only supports mask
    // clipping. Materialize a rectangular mask on demand in that case.
    let clip_rect = self.current_state.clip_rect;
    if blend_mode != SkiaBlendMode::SourceOver
      && clip_rect.is_some()
      && self.current_state.clip_mask.is_none()
    {
      self.materialize_rect_clip_mask_if_needed();
    }
    let clip_mask = self.current_state.clip_mask.clone();
    self.mirror_to_source_alpha(|canvas| {
      let mut target = canvas.pixmap.as_mut();
      composite_layer_into_pixmap_with_clip_rect(
        &mut target,
        layer,
        opacity,
        blend_mode,
        origin,
        clip_mask.as_deref(),
        clip_rect,
      );
    });
  }

  #[inline]
  pub(crate) fn layer_depth(&self) -> usize {
    self.layer_stack.len()
  }

  /// Returns the active pixmap at the given canvas layer depth.
  ///
  /// Depth 0 is the root canvas. Depth `layer_depth()` is the currently-active pixmap.
  pub(crate) fn pixmap_at_depth(&self, depth: usize) -> Option<PixmapRef<'_>> {
    let current_depth = self.layer_stack.len();
    if depth > current_depth {
      return None;
    }
    if current_depth == 0 {
      return (depth == 0).then_some(self.pixmap.as_ref());
    }
    if depth == current_depth {
      return Some(self.pixmap.as_ref());
    }
    self.layer_stack.get(depth).map(|record| record.pixmap.as_ref())
  }

  /// Returns metadata for compositing the given layer depth into its parent.
  pub(crate) fn layer_composite_metadata(
    &self,
    depth: usize,
  ) -> Option<LayerCompositeMetadata<'_>> {
    let record = self.layer_stack.get(depth.checked_sub(1)?)?;
    let opacity = (record.opacity * record.parent_opacity).clamp(0.0, 1.0);
    Some(LayerCompositeMetadata {
      origin: record.origin,
      opacity,
      blend_mode: record.composite_blend.unwrap_or(record.parent_blend_mode),
      clip_mask: record.parent_clip_mask.as_deref(),
    })
  }

  /// Snapshots the Backdrop Root Image into a scratch pixmap region.
  ///
  /// The `layer_stack` slice must come from [`Canvas::split_layer_stack_and_pixmap_mut`], and the
  /// region origin is interpreted in the *current layer's parent* coordinate space (i.e. the
  /// coordinate space of `layer_stack.last().pixmap`).
  ///
  /// This helper is used to implement spec-correct `backdrop-filter` sampling: stacking-context
  /// isolation layers (e.g. due to transforms/z-index) do not stop sampling, so we must flatten
  /// the intermediate layer surfaces between the nearest Backdrop Root and the element's parent
  /// into a temporary buffer.
  pub(crate) fn fill_backdrop_root_region(
    layer_stack: &[LayerRecord],
    region: &mut Pixmap,
    origin_in_parent: (i32, i32),
  ) -> RenderResult<()> {
    fn copy_pixmap_region_with_offset(
      dst: &mut Pixmap,
      src: &CanvasPixmap,
      src_x: i32,
      src_y: i32,
    ) -> RenderResult<()> {
      let dst_w = dst.width() as i32;
      let dst_h = dst.height() as i32;
      if dst_w <= 0 || dst_h <= 0 {
        return Ok(());
      }
      let dst_stride = dst_w as usize * 4;
      let dst_data = dst.data_mut();
      dst_data.fill(0);

      let src_w = src.width() as i32;
      let src_h = src.height() as i32;
      if src_w <= 0 || src_h <= 0 {
        return Ok(());
      }

      let x0 = src_x.max(0);
      let y0 = src_y.max(0);
      let x1 = (src_x + dst_w).min(src_w);
      let y1 = (src_y + dst_h).min(src_h);
      if x0 >= x1 || y0 >= y1 {
        return Ok(());
      }

      let dst_off_x = (x0 - src_x) as usize;
      let dst_off_y = (y0 - src_y) as usize;
      let copy_w = (x1 - x0) as usize;
      let copy_h = (y1 - y0) as usize;

      let src_stride = src.width() as usize * 4;
      let row_bytes = copy_w * 4;
      let src_data = src.data();

      let mut deadline_counter = 0usize;
      for row in 0..copy_h {
        check_active_periodic(&mut deadline_counter, 32, RenderStage::Paint)?;
        let src_idx = (y0 as usize + row) * src_stride + x0 as usize * 4;
        let dst_idx = (dst_off_y + row) * dst_stride + dst_off_x * 4;
        dst_data[dst_idx..dst_idx + row_bytes]
          .copy_from_slice(&src_data[src_idx..src_idx + row_bytes]);
      }
      Ok(())
    }

    let stack_len = layer_stack.len();
    if stack_len == 0 {
      region.data_mut().fill(0);
      return Ok(());
    }

      // The parent pixmap is stored in the top record.
      let parent_layer = stack_len - 1;

    // Find the nearest ancestor backdrop root record, excluding the current layer record.
    let mut root_layer = 0usize;
    if parent_layer > 0 {
      for idx in (0..parent_layer).rev() {
        if layer_stack[idx].is_backdrop_root {
          root_layer = idx + 1;
          break;
        }
      }
    }
    let root_initialized_from_backdrop =
      root_layer > 0 && layer_stack[root_layer - 1].init_from_backdrop;

    // Fast path: sampling starts at the immediate parent surface.
    if root_layer == parent_layer {
      let src = &layer_stack[parent_layer].pixmap;
      copy_pixmap_region_with_offset(region, src, origin_in_parent.0, origin_in_parent.1)?;
      if root_initialized_from_backdrop {
        let record = &layer_stack[root_layer - 1];
        let origin_in_backdrop = (
          origin_in_parent.0.saturating_add(record.origin.0),
          origin_in_parent.1.saturating_add(record.origin.1),
        );
        uncomposite_layer_source_over_backdrop(
          region,
          record.pixmap.as_ref(),
          origin_in_backdrop,
          record
            .source_alpha
            .as_ref()
            .map(|alpha| (alpha, origin_in_parent)),
        )?;
      }
      return Ok(());
    }

    // Pre-compute absolute origins (relative to the base pixmap) for every surface up to the
    // parent. `layer_stack[i].origin` is the origin of surface `i + 1` relative to surface `i`.
    let mut abs_origins: Vec<(i32, i32)> = Vec::with_capacity(parent_layer + 1);
    abs_origins.push((0, 0)); // Base surface.
    let mut acc_x = 0i32;
    let mut acc_y = 0i32;
    for layer in 1..=parent_layer {
      let origin = layer_stack[layer - 1].origin;
      acc_x = acc_x.saturating_add(origin.0);
      acc_y = acc_y.saturating_add(origin.1);
      abs_origins.push((acc_x, acc_y));
    }

    let parent_abs = abs_origins[parent_layer];
    let root_abs = abs_origins[root_layer];
    let start_src_x = origin_in_parent
      .0
      .saturating_add(parent_abs.0.saturating_sub(root_abs.0));
    let start_src_y = origin_in_parent
      .1
      .saturating_add(parent_abs.1.saturating_sub(root_abs.1));

    // Initialize the region from the root surface. Outside the root surface is transparent.
    let root_surface = &layer_stack[root_layer].pixmap;
    copy_pixmap_region_with_offset(region, root_surface, start_src_x, start_src_y)?;
    if root_initialized_from_backdrop {
      let record = &layer_stack[root_layer - 1];
      let origin_in_backdrop = (
        start_src_x.saturating_add(record.origin.0),
        start_src_y.saturating_add(record.origin.1),
      );
      uncomposite_layer_source_over_backdrop(
        region,
        record.pixmap.as_ref(),
        origin_in_backdrop,
        record
          .source_alpha
          .as_ref()
          .map(|alpha| (alpha, (start_src_x, start_src_y))),
      )?;
    }

    // Composite intermediate layer surfaces onto the region in order.
    let mut paint = PixmapPaint::default();
    paint.quality = FilterQuality::Nearest;
    let transform = Transform::identity();

    for layer in (root_layer + 1)..=parent_layer {
      // Layer `layer` is composited into its parent using the record at `layer - 1`.
      let record = &layer_stack[layer - 1];
      let opacity = record.effective_opacity();
      if opacity <= 0.0 {
        continue;
      }
      let blend = record.effective_blend_mode();

      let abs = abs_origins[layer];
      let dest_x = abs.0 - parent_abs.0 - origin_in_parent.0;
      let dest_y = abs.1 - parent_abs.1 - origin_in_parent.1;

      check_active(RenderStage::Paint)?;
      paint.opacity = opacity;
      paint.blend_mode = blend;
      let src = layer_stack[layer].pixmap.as_ref();
      if paint.blend_mode == SkiaBlendMode::Plus {
        draw_pixmap_with_plus_blend(
          region,
          dest_x,
          dest_y,
          src,
          opacity,
          paint.quality,
          transform,
          None,
        );
      } else {
        region.draw_pixmap(dest_x, dest_y, src, &paint, transform, None);
      }
    }

    Ok(())
  }

  /// Returns the current blend mode.
  #[inline]
  pub(crate) fn blend_mode(&self) -> SkiaBlendMode {
    self.current_state.blend_mode
  }

  /// Sets the current transform
  ///
  /// # Examples
  ///
  /// ```rust,ignore
  /// canvas.set_transform(Transform::from_translate(100.0, 50.0));
  /// ```
  pub fn set_transform(&mut self, transform: Transform) {
    self.current_state.transform = transform;
    // `overflow: hidden` and other rectangular clips may be tracked only as a bounds rect for
    // performance (see `set_clip_with_radii_impl`). That representation is sufficient for
    // axis-aligned drawing, but once a non-identity transform is applied we can no longer rely on
    // bounds-only scissoring: many tiny-skia operations (paths, pixmap draws, etc.) need a real
    // per-pixel mask to clip transformed output.
    //
    // Materialize a mask lazily when a transform is introduced so transformed descendants are
    // correctly clipped without forcing every `overflow:hidden` wrapper to allocate a mask.
    if transform != Transform::identity() {
      self.materialize_rect_clip_mask_if_needed();
    }
  }

  /// Returns the current transform
  #[inline]
  pub fn transform(&self) -> Transform {
    self.current_state.transform
  }

  /// Applies a translation to the current transform
  ///
  /// # Examples
  ///
  /// ```rust,ignore
  /// canvas.translate(100.0, 50.0);
  /// canvas.draw_rect(Rect::from_xywh(0.0, 0.0, 50.0, 50.0), Rgba::RED);
  /// // Rectangle is drawn at (100, 50)
  /// ```
  pub fn translate(&mut self, dx: f32, dy: f32) {
    self.current_state.transform = self.current_state.transform.pre_translate(dx, dy);
    if self.current_state.transform != Transform::identity() {
      self.materialize_rect_clip_mask_if_needed();
    }
  }

  /// Applies a scale to the current transform
  pub fn scale(&mut self, sx: f32, sy: f32) {
    self.current_state.transform = self.current_state.transform.pre_scale(sx, sy);
    if self.current_state.transform != Transform::identity() {
      self.materialize_rect_clip_mask_if_needed();
    }
  }

  fn materialize_rect_clip_mask_if_needed(&mut self) {
    if self.current_state.clip_mask.is_some() {
      return;
    }
    let Some(clip_rect) = self.current_state.clip_rect else {
      return;
    };
    if clip_rect.width() <= 0.0
      || clip_rect.height() <= 0.0
      || self.width() == 0
      || self.height() == 0
      || !clip_rect.x().is_finite()
      || !clip_rect.y().is_finite()
      || !clip_rect.width().is_finite()
      || !clip_rect.height().is_finite()
    {
      return;
    }

    // `clip_rect` is stored in device space, so build a mask directly in device pixels (ignoring
    // the current transform). Use the same pixel-center inclusion rule as
    // `build_clip_mask_fast_rect` so the resulting mask matches tiny-skia's non-AA `clipRect`.
    let Some(mut mask) = Mask::new(self.width(), self.height()) else {
      return;
    };
    mask.data_mut().fill(0);

    let w_i64 = self.width() as i64;
    let h_i64 = self.height() as i64;

    let min_x = clip_rect.min_x();
    let max_x = clip_rect.max_x();
    let min_y = clip_rect.min_y();
    let max_y = clip_rect.max_y();
    if !min_x.is_finite() || !max_x.is_finite() || !min_y.is_finite() || !max_y.is_finite() {
      return;
    }

    let x0 = (min_x - 0.5).ceil() as i64;
    let y0 = (min_y - 0.5).ceil() as i64;
    let x1 = (max_x - 0.5).floor() as i64 + 1;
    let y1 = (max_y - 0.5).floor() as i64 + 1;

    let x0 = x0.clamp(0, w_i64) as usize;
    let y0 = y0.clamp(0, h_i64) as usize;
    let x1 = x1.clamp(0, w_i64) as usize;
    let y1 = y1.clamp(0, h_i64) as usize;

    if x1 > x0 && y1 > y0 {
      let stride = self.width() as usize;
      let data = mask.data_mut();
      for y in y0..y1 {
        let start = y * stride + x0;
        data[start..start + (x1 - x0)].fill(255);
      }
    }

    let mask_rc = Rc::new(mask);
    self.current_state.clip_mask = Some(mask_rc.clone());
    // If we're inside a `save()/restore()` scope (e.g. `PushTransform`), propagate the materialized
    // mask into any saved state that has the same rectangular clip. This keeps the mask available
    // after `restore()`, avoiding repeated full-canvas mask allocations for sibling transforms.
    for state in self.state_stack.iter_mut() {
      if state.clip_mask.is_none() && state.clip_rect == Some(clip_rect) {
        state.clip_mask = Some(mask_rc.clone());
      }
    }
  }

  /// Sets the blend mode for subsequent drawing operations
  pub fn set_blend_mode(&mut self, mode: BlendMode) {
    self.current_state.blend_mode = mode.to_skia();
  }

  /// Sets a clip rectangle
  ///
  /// Subsequent drawing operations will be clipped to this rectangle.
  pub fn set_clip(&mut self, rect: Rect) -> Result<()> {
    self.set_clip_with_radii(rect, None)
  }

  /// Sets a clip rectangle and always builds a per-pixel clip mask.
  ///
  /// The default [`Canvas::set_clip_with_radii`] path may choose to represent simple rectangular
  /// clips using only [`CanvasState::clip_rect`] bounds (for performance on very large canvases).
  /// Most paint code can use bounds-based scissoring, but some operations (e.g. tiny-skia image
  /// draws with a destination clip) rely on a real mask.
  pub(crate) fn set_clip_force_mask(&mut self, rect: Rect) -> Result<()> {
    self.set_clip_with_radii_force_mask(rect, None)
  }

  /// Sets a clip rectangle with optional corner radii.
  pub fn set_clip_with_radii(&mut self, rect: Rect, radii: Option<BorderRadii>) -> Result<()> {
    self.set_clip_with_radii_impl(rect, radii.unwrap_or(BorderRadii::ZERO), false)
  }

  /// Like [`Canvas::set_clip_with_radii`], but always builds a per-pixel clip mask.
  pub(crate) fn set_clip_with_radii_force_mask(
    &mut self,
    rect: Rect,
    radii: Option<BorderRadii>,
  ) -> Result<()> {
    self.set_clip_with_radii_impl(rect, radii.unwrap_or(BorderRadii::ZERO), true)
  }

  fn set_clip_with_radii_impl(
    &mut self,
    rect: Rect,
    radii: BorderRadii,
    force_mask: bool,
  ) -> Result<()> {
    let transform = self.current_state.transform;
    let clip_bounds = if transform == Transform::identity() {
      rect
    } else {
      Self::transform_rect_aabb(rect, transform)
    };

    let prev_clip_rect = self.current_state.clip_rect;
    let prev_clip_mask = self.current_state.clip_mask.take();
    let base_clip = match prev_clip_rect {
      Some(existing) => existing.intersection(clip_bounds).unwrap_or(Rect::ZERO),
      None => clip_bounds,
    };
    self.current_state.clip_rect = Some(base_clip);

    // Performance fast path: the pageset renderer frequently pushes hundreds of rectangular clips
    // (e.g. `overflow:hidden` wrappers) onto extremely tall canvases. Building a full-size mask for
    // every clip is O(clips * canvas_pixels) and can dominate render time. For simple axis-aligned
    // clips under the identity transform, track only the clip bounds rectangle and let higher-level
    // paint code scissor work to those bounds.
    //
    // Callers that require a true per-pixel mask can use `*_force_mask`.
    if !force_mask
      && radii.is_zero()
      && transform == Transform::identity()
      && prev_clip_mask.is_none()
    {
      return Ok(());
    }

    let mut new_mask = self.build_clip_mask(rect, radii);
    // If we previously accumulated only bounds-based rectangular clips (no mask) and this clip
    // creates a mask (rounded corners, transformed rect, etc.), make sure the mask still respects
    // the current clip bounds intersection.
    if prev_clip_mask.is_none() && prev_clip_rect.is_some() && base_clip != clip_bounds {
      if let Some(mask) = new_mask.as_mut() {
        scissor_mask_to_rect(mask, base_clip)?;
      }
    }

    self.current_state.clip_mask = match (new_mask, prev_clip_mask) {
      (Some(mut next), Some(existing)) => {
        combine_masks(&mut next, existing.as_ref())?;
        Some(Rc::new(next))
      }
      (Some(mask), None) => Some(Rc::new(mask)),
      (None, existing) => existing,
    };
    Ok(())
  }

  /// Sets an arbitrary clip path (basic shapes)
  pub fn set_clip_path(&mut self, path: &ResolvedClipPath, scale: f32) -> Result<()> {
    // Parallel tiling renders each tile into a smaller pixmap with a translated canvas transform.
    // When a clip path extends beyond the tile's pixmap bounds, tiny-skia clips the path to the
    // mask extents during rasterization. That clipping can introduce subtle numerical differences
    // compared to rasterizing the same path on the full surface, showing up as tile seams near
    // anti-aliased clip edges (notably for `clip-path: polygon(...)`).
    //
    // To keep serial and tiled output pixel-identical, rasterize clip-path masks into a padded
    // scratch mask and then crop back down to the target pixmap size. This keeps the clip path's
    // geometry well away from mask boundaries so the clipped intermediate is stable.
    const CLIP_PATH_MASK_PADDING_PX: u32 = 32;

    let bounds = path.bounds();
    let scaled_bounds = Rect::from_xywh(
      bounds.x() * scale,
      bounds.y() * scale,
      bounds.width() * scale,
      bounds.height() * scale,
    );
    let transform = self.current_state.transform;
    let clip_bounds = if transform == Transform::identity() {
      scaled_bounds
    } else {
      Self::transform_rect_aabb(scaled_bounds, transform)
    };
    let base_clip = match self.current_state.clip_rect {
      Some(existing) => existing.intersection(clip_bounds).unwrap_or(Rect::ZERO),
      None => clip_bounds,
    };
    self.current_state.clip_rect = Some(base_clip);

    let pixmap_w = self.pixmap.width();
    let pixmap_h = self.pixmap.height();
    let new_mask = if let Some(size) = IntSize::from_wh(pixmap_w, pixmap_h) {
      let pad_needed = clip_bounds.min_x() < 0.0
        || clip_bounds.min_y() < 0.0
        || clip_bounds.max_x() > pixmap_w as f32
        || clip_bounds.max_y() > pixmap_h as f32;

      if !pad_needed || CLIP_PATH_MASK_PADDING_PX == 0 {
        path.mask(scale, size, self.current_state.transform)
      } else {
        let pad = CLIP_PATH_MASK_PADDING_PX;
        let fallback = || path.mask(scale, size, self.current_state.transform);
        let padded = (|| -> RenderResult<Option<Mask>> {
          let Some(padded_w) = pixmap_w.checked_add(pad.saturating_mul(2)) else {
            return Ok(fallback());
          };
          let Some(padded_h) = pixmap_h.checked_add(pad.saturating_mul(2)) else {
            return Ok(fallback());
          };
          let Some(padded_size) = IntSize::from_wh(padded_w, padded_h) else {
            return Ok(fallback());
          };
          let padded_transform = self
            .current_state
            .transform
            .post_translate(pad as f32, pad as f32);
          let Some(padded_mask) = path.mask(scale, padded_size, padded_transform) else {
            return Ok(fallback());
          };
          crop_mask(&padded_mask, pad, pad, pixmap_w, pixmap_h)
        })();
        padded?
      }
    } else {
      None
    };
    self.current_state.clip_mask = match (new_mask, self.current_state.clip_mask.take()) {
      (Some(mut next), Some(existing)) => {
        combine_masks(&mut next, existing.as_ref())?;
        Some(Rc::new(next))
      }
      (Some(mask), None) => Some(Rc::new(mask)),
      (None, existing) => existing,
    };
    Ok(())
  }

  /// Sets a clip mask from the alpha channel of an image.
  ///
  /// The provided `image` is mapped to `rect` in the canvas coordinate space before applying the
  /// current transform (matching how images are positioned in the display list). The resulting
  /// alpha coverage is intersected with any existing clip mask.
  pub fn set_clip_image_mask(
    &mut self,
    image: &Pixmap,
    rect: Rect,
    quality: FilterQuality,
  ) -> Result<()> {
    let pixmap_w = self.pixmap.width();
    let pixmap_h = self.pixmap.height();
    if pixmap_w == 0 || pixmap_h == 0 {
      self.current_state.clip_rect = Some(Rect::ZERO);
      self.current_state.clip_mask = None;
      return Ok(());
    }
    if rect.width() <= 0.0
      || rect.height() <= 0.0
      || !rect.x().is_finite()
      || !rect.y().is_finite()
      || !rect.width().is_finite()
      || !rect.height().is_finite()
    {
      return Ok(());
    }
    if image.width() == 0 || image.height() == 0 {
      return Ok(());
    }

    let transform = self.current_state.transform;
    let clip_bounds = if transform == Transform::identity() {
      rect
    } else {
      Self::transform_rect_aabb(rect, transform)
    };
    let base_clip = match self.current_state.clip_rect {
      Some(existing) => existing.intersection(clip_bounds).unwrap_or(Rect::ZERO),
      None => clip_bounds,
    };
    self.current_state.clip_rect = Some(base_clip);

    // Render the transformed image mask into a temporary RGBA surface covering the transformed
    // clip bounds, then extract its alpha into a full-size mask for intersection with existing
    // clips.
    let new_mask = (|| -> RenderResult<Option<Mask>> {
      let Some(mut mask) = Mask::new(pixmap_w, pixmap_h) else {
        return Ok(None);
      };
      mask.data_mut().fill(0);

      if base_clip.width() <= 0.0 || base_clip.height() <= 0.0 {
        return Ok(Some(mask));
      }

      let pad = match quality {
        FilterQuality::Nearest => 0,
        _ => 1,
      };
      let mut x0 = (clip_bounds.min_x().floor() as i32).saturating_sub(pad);
      let mut y0 = (clip_bounds.min_y().floor() as i32).saturating_sub(pad);
      let mut x1 = (clip_bounds.max_x().ceil() as i32).saturating_add(pad);
      let mut y1 = (clip_bounds.max_y().ceil() as i32).saturating_add(pad);
      let w_i32 = pixmap_w as i32;
      let h_i32 = pixmap_h as i32;
      x0 = x0.clamp(0, w_i32);
      y0 = y0.clamp(0, h_i32);
      x1 = x1.clamp(0, w_i32);
      y1 = y1.clamp(0, h_i32);
      if x1 <= x0 || y1 <= y0 {
        return Ok(Some(mask));
      }
      let region_w = (x1 - x0) as u32;
      let region_h = (y1 - y0) as u32;
      if region_w == 0 || region_h == 0 {
        return Ok(Some(mask));
      }

      let Some(mut tmp) = new_pixmap(region_w, region_h) else {
        return Ok(Some(mask));
      };
      tmp.data_mut().fill(0);

      let scale_x = rect.width() / image.width() as f32;
      let scale_y = rect.height() / image.height() as f32;
      if !scale_x.is_finite() || !scale_y.is_finite() {
        return Ok(Some(mask));
      }

      // Map source image pixel coordinates into the canvas's pre-transform coordinate space, then
      // apply the current canvas transform to land in pixmap space.
      let dest_map = Transform::from_row(scale_x, 0.0, 0.0, scale_y, rect.x(), rect.y());
      let full_transform = concat_transforms(transform, dest_map);
      // Translate so the temporary pixmap's origin aligns with (x0, y0) in the full pixmap.
      let tmp_transform = concat_transforms(
        Transform::from_translate(-(x0 as f32), -(y0 as f32)),
        full_transform,
      );

      let paint = PixmapPaint {
        opacity: 1.0,
        blend_mode: SkiaBlendMode::SourceOver,
        quality,
      };
      tmp.draw_pixmap(0, 0, image.as_ref(), &paint, tmp_transform, None);
      let region_mask = Mask::from_pixmap(tmp.as_ref(), MaskType::Alpha);

      check_active(RenderStage::Paint)?;
      let src = region_mask.data();
      let dst = mask.data_mut();
      let src_stride = region_w as usize;
      let dst_stride = pixmap_w as usize;
      let dst_x = x0 as usize;
      let dst_y = y0 as usize;
      let mut deadline_counter = 0usize;
      for row in 0..region_h as usize {
        check_active_periodic(&mut deadline_counter, 32, RenderStage::Paint)?;
        let src_off = row * src_stride;
        let dst_off = (dst_y + row) * dst_stride + dst_x;
        dst[dst_off..dst_off + src_stride].copy_from_slice(&src[src_off..src_off + src_stride]);
      }

      Ok(Some(mask))
    })()?;

    self.current_state.clip_mask = match (new_mask, self.current_state.clip_mask.take()) {
      (Some(mut next), Some(existing)) => {
        combine_masks(&mut next, existing.as_ref())?;
        Some(Rc::new(next))
      }
      (Some(mask), None) => Some(Rc::new(mask)),
      (None, existing) => existing,
    };

    Ok(())
  }

  /// Sets a clip mask that is the union of the provided text runs.
  ///
  /// This is used for `background-clip: text` / `-webkit-background-clip: text` by rasterizing
  /// glyphs into an alpha mask and intersecting with any existing clip.
  pub fn set_clip_text(&mut self, runs: &[TextItem]) -> Result<()> {
    // Parallel tiling renders each tile into a smaller pixmap with a translated canvas transform.
    // When glyph edges extend beyond the tile's pixmap bounds, tiny-skia clips the rasterization
    // to the surface extents, introducing subtle numerical differences compared to rasterizing the
    // same glyphs on the full surface. This can show up as seams at tile boundaries when the clip
    // edge is anti-aliased.
    //
    // Keep serial and tiled output pixel-identical by rasterizing text clip masks into a padded
    // scratch surface and then cropping back down to the target pixmap size, matching
    // `set_clip_path`.
    const CLIP_TEXT_MASK_PADDING_PX: u32 = 32;

    let pixmap_w = self.pixmap.width();
    let pixmap_h = self.pixmap.height();
    if pixmap_w == 0 || pixmap_h == 0 {
      self.current_state.clip_rect = Some(Rect::ZERO);
      self.current_state.clip_mask = None;
      return Ok(());
    }
    let pixmap_bounds = Rect::from_xywh(0.0, 0.0, pixmap_w as f32, pixmap_h as f32);

    let mut clip_bounds: Option<Rect> = None;
    let base_transform = self.current_state.transform;
    for run in runs {
      let mut run_bounds = crate::paint::display_list::text_bounds(run);
      let outline_size = run.font_size * run.scale;
      let overhang = outline_size.abs() * 0.5;
      let synthetic = run.synthetic_bold.abs() * 2.0;
      let pad = (overhang + synthetic).max(0.0);
      if pad > 0.0 {
        run_bounds = run_bounds.inflate(pad);
      }
      if run.scale != 1.0 {
        // Scale only affects glyph outlines, not positions. Inflate to keep bounds conservative.
        let extra = (run.font_size * (run.scale - 1.0).abs()).max(0.0);
        if extra > 0.0 {
          run_bounds = run_bounds.inflate(extra);
        }
      }
      let rotation = rotation_transform(run.rotation, run.origin.x, run.origin.y);
      let mut transform = rotation.unwrap_or_else(Transform::identity);
      transform = concat_transforms(base_transform, transform);
      let mapped = if transform == Transform::identity() {
        run_bounds
      } else {
        Self::transform_rect_aabb(run_bounds, transform)
      };
      clip_bounds = Some(match clip_bounds {
        Some(prev) => prev.union(mapped),
        None => mapped,
      });
    }

    let clip_bounds = clip_bounds.unwrap_or(Rect::ZERO);
    let clip_bounds = if runs.is_empty() {
      Rect::ZERO
    } else if clip_bounds.width() <= 0.0
      || clip_bounds.height() <= 0.0
      || !clip_bounds.x().is_finite()
      || !clip_bounds.y().is_finite()
      || !clip_bounds.width().is_finite()
      || !clip_bounds.height().is_finite()
    {
      pixmap_bounds
    } else {
      clip_bounds
    };

    if runs.is_empty() {
      self.current_state.clip_rect = Some(Rect::ZERO);
    } else if self.current_state.clip_rect.is_none() {
      self.current_state.clip_rect = Some(pixmap_bounds);
    }

    let mut render_mask = |size: IntSize, transform: Transform| -> Result<Mask> {
      let mut mask_pixmap = new_pixmap_with_context(size.width(), size.height(), "text clip mask")?;

      let state = TextRenderState {
        transform,
        clip_mask: None,
        opacity: 1.0,
        blend_mode: SkiaBlendMode::SourceOver,
        allow_subpixel_aa: false,
        font_smoothing: FontSmoothing::Auto,
      };

      for run in runs {
        if run.glyphs.is_empty() {
          continue;
        }
        let Some(font) = run.font.as_deref() else {
          continue;
        };

        let hb_variations = Self::hb_variations(&run.variations);
        let positions: Vec<GlyphPosition> = run
          .glyphs
          .iter()
          .map(|g| GlyphPosition {
            glyph_id: g.glyph_id,
            cluster: g.cluster,
            x_offset: g.x_offset,
            y_offset: g.y_offset,
            x_advance: g.x_advance,
            y_advance: g.y_advance,
          })
          .collect();
        let rotation = rotation_transform(run.rotation, run.origin.x, run.origin.y);
        self.text_rasterizer.render_glyph_run(
          &positions,
          font,
          run.font_size * run.scale,
          run.synthetic_bold,
          run.synthetic_oblique,
          run.palette_index,
          run.palette_overrides.as_slice(),
          run.palette_override_hash,
          &hb_variations,
          rotation,
          run.origin.x,
          run.origin.y,
          Rgba::WHITE,
          state,
          &mut mask_pixmap,
        )?;
      }

      Ok(Mask::from_pixmap(mask_pixmap.as_ref(), MaskType::Alpha))
    };

    let new_mask = if runs.is_empty() {
      let Some(mut mask) = Mask::new(pixmap_w, pixmap_h) else {
        return Ok(());
      };
      mask.data_mut().fill(0);
      Some(mask)
    } else if let Some(size) = IntSize::from_wh(pixmap_w, pixmap_h) {
      let needs_padding = CLIP_TEXT_MASK_PADDING_PX > 0
        && (clip_bounds.min_x() < 0.0
          || clip_bounds.min_y() < 0.0
          || clip_bounds.max_x() > pixmap_w as f32
          || clip_bounds.max_y() > pixmap_h as f32);

      if !needs_padding {
        Some(render_mask(size, self.current_state.transform)?)
      } else {
        let overflow_left = (0.0 - clip_bounds.min_x()).ceil().max(0.0) as u32;
        let overflow_top = (0.0 - clip_bounds.min_y()).ceil().max(0.0) as u32;
        let overflow_right = (clip_bounds.max_x() - pixmap_w as f32).ceil().max(0.0) as u32;
        let overflow_bottom = (clip_bounds.max_y() - pixmap_h as f32).ceil().max(0.0) as u32;

        let pad_left = overflow_left.saturating_add(CLIP_TEXT_MASK_PADDING_PX);
        let pad_top = overflow_top.saturating_add(CLIP_TEXT_MASK_PADDING_PX);
        let pad_right = overflow_right.saturating_add(CLIP_TEXT_MASK_PADDING_PX);
        let pad_bottom = overflow_bottom.saturating_add(CLIP_TEXT_MASK_PADDING_PX);

        let padded_w = pixmap_w
          .checked_add(pad_left)
          .and_then(|w| w.checked_add(pad_right));
        let padded_h = pixmap_h
          .checked_add(pad_top)
          .and_then(|h| h.checked_add(pad_bottom));
        let padded_size = padded_w.and_then(|w| padded_h.and_then(|h| IntSize::from_wh(w, h)));
        if let Some(padded_size) = padded_size {
          let padded_transform = self
            .current_state
            .transform
            .post_translate(pad_left as f32, pad_top as f32);
          let padded_mask = render_mask(padded_size, padded_transform)?;
          match crop_mask(&padded_mask, pad_left, pad_top, pixmap_w, pixmap_h)? {
            Some(mask) => Some(mask),
            None => Some(render_mask(size, self.current_state.transform)?),
          }
        } else {
          Some(render_mask(size, self.current_state.transform)?)
        }
      }
    } else {
      None
    };

    self.current_state.clip_mask = match (new_mask, self.current_state.clip_mask.take()) {
      (Some(mut next), Some(existing)) => {
        combine_masks(&mut next, existing.as_ref())?;
        Some(Rc::new(next))
      }
      (Some(mask), None) => Some(Rc::new(mask)),
      (None, existing) => existing,
    };
    Ok(())
  }

  /// Clears the clip rectangle
  pub fn clear_clip(&mut self) {
    self.current_state.clip_rect = None;
    self.current_state.clip_mask = None;
  }

  /// Returns the current clip bounds if any.
  pub(crate) fn clip_bounds(&self) -> Option<Rect> {
    self.current_state.clip_rect
  }

  /// Returns the current clip mask, including any rounded radii.
  pub(crate) fn clip_mask(&self) -> Option<&Mask> {
    self.current_state.clip_mask.as_deref()
  }

  /// Clones the reference-counted clip mask, if any.
  ///
  /// This is cheaper than cloning the underlying `Mask` and is useful when callers need to hold
  /// onto the clip mask while also mutating the canvas (e.g. `backdrop-filter` sampling).
  pub(crate) fn clip_mask_rc(&self) -> Option<Rc<Mask>> {
    self.current_state.clip_mask.clone()
  }

  fn current_text_state<'a>(&self, clip_mask: Option<&'a Mask>) -> TextRenderState<'a> {
    self.current_text_state_with_font_smoothing(clip_mask, FontSmoothing::Auto)
  }

  fn current_text_state_with_font_smoothing<'a>(
    &self,
    clip_mask: Option<&'a Mask>,
    font_smoothing: FontSmoothing,
  ) -> TextRenderState<'a> {
    TextRenderState {
      transform: self.current_state.transform,
      clip_mask,
      opacity: self.current_state.opacity,
      blend_mode: self.current_state.blend_mode,
      allow_subpixel_aa: true,
      font_smoothing,
    }
  }

  // ========================================================================
  // Drawing Operations
  // ========================================================================

  /// Draws a filled rectangle
  ///
  /// # Arguments
  ///
  /// * `rect` - Rectangle to fill
  /// * `color` - Fill color
  ///
  /// # Examples
  ///
  /// ```rust,ignore
  /// let rect = Rect::from_xywh(10.0, 10.0, 100.0, 50.0);
  /// canvas.draw_rect(rect, Rgba::rgb(255, 0, 0));
  /// ```
  pub fn draw_rect(&mut self, rect: Rect, color: Rgba) {
    self.mirror_to_source_alpha(|canvas| canvas.draw_rect_impl(rect, color));
  }

  fn snapped_device_rect_for_axis_aligned_fill(
    &self,
    rect: Rect,
    transform: Transform,
  ) -> Option<Rect> {
    // Snapping is only meaningful for axis-aligned transforms (no rotation/skew). For other
    // transforms we rely on the regular anti-aliased rasterization.
    if transform.kx.abs() > 1e-6 || transform.ky.abs() > 1e-6 {
      return None;
    }
    // This snapping path exists to:
    // - avoid seams between adjacent backgrounds whose edges land on fractional device pixels
    // - match Chrome/Skia's non-AA axis-aligned rect fill coverage for both opaque fills and
    //   semi-transparent hairlines (e.g. 0.5px dividers).
    //
    // It must not quantize animated CSS transforms (e.g. `transform: translateX(0.5px)`), otherwise
    // transitions appear to "stick" to integer pixels.
    //
    // Restrict snapping to translation-only transforms whose translation is already near an
    // integer device pixel.
    if (transform.sx - 1.0).abs() > 1e-6 || (transform.sy - 1.0).abs() > 1e-6 {
      return None;
    }
    let tx_round = transform.tx.round();
    let ty_round = transform.ty.round();
    if (transform.tx - tx_round).abs() > NEAR_INTEGER_EPSILON_PX
      || (transform.ty - ty_round).abs() > NEAR_INTEGER_EPSILON_PX
    {
      return None;
    }
    if !rect.x().is_finite()
      || !rect.y().is_finite()
      || !rect.width().is_finite()
      || !rect.height().is_finite()
    {
      return None;
    }

    // Snap in *device* space so the resulting fill is stable under translated canvases
    // (e.g. tile-based rendering).
    //
    // We intentionally do *not* require that the rect edges are already near integer pixels.
    // Chrome/Skia fill axis-aligned rectangles without anti-aliasing, so pixels are covered based
    // on their centers using an "open min / closed max" rule. Using the same pixel-center rule
    // avoids blended seams/border "bleed" when layout produces fractional edges like `left: 2.8px`
    // or `height: 51.6px`.
    let x0 = rect.min_x();
    let x1 = rect.max_x();
    let y0 = rect.min_y();
    let y1 = rect.max_y();

    // Use the rounded translation when snapping. This keeps tile-based rendering stable even when
    // the translation carries small floating-point error (we already ensured the error is tiny).
    let dx0 = x0 + tx_round;
    let dx1 = x1 + tx_round;
    let dy0 = y0 + ty_round;
    let dy1 = y1 + ty_round;
    if !dx0.is_finite() || !dx1.is_finite() || !dy0.is_finite() || !dy1.is_finite() {
      return None;
    }

    let min_x = dx0.min(dx1);
    let max_x = dx0.max(dx1);
    let min_y = dy0.min(dy1);
    let max_y = dy0.max(dy1);

    // Determine the covered pixel bounds in device space.
    //
    // Chrome/Skia's non-AA axis-aligned fills use a pixel-center rule:
    // - Min edge: "open" (pixel centers exactly on the min edge are outside)
    // - Max edge: "closed" (pixel centers exactly on the max edge are inside)
    //
    // In terms of integer pixel coordinates, that maps to:
    //   start = floor(min + 0.5)
    //   end   = floor(max + 0.5)
    //
    // This avoids 1px seams when adjacent opaque backgrounds share the same fractional boundary,
    // while still matching Chrome's rasterization for partially-covered edge pixels.
    let start_x = (min_x + 0.5).floor();
    let end_x = (max_x + 0.5).floor();
    let start_y = (min_y + 0.5).floor();
    let end_y = (max_y + 0.5).floor();

    Some(Rect::from_xywh(
      start_x,
      start_y,
      end_x - start_x,
      end_y - start_y,
    ))
  }

  /// Fast path for axis-aligned `source-over` rectangle fills.
  ///
  /// tiny-skia's compositing differs subtly from Chrome/Skia for semi-transparent fills (often by
  /// ±1 in each channel). For pageset comparisons this can account for hundreds of thousands of
  /// differing pixels on large translucent panels.
  ///
  /// When there is no complex clip, we can composite directly into the destination premultiplied
  /// buffer using the same truncating `mul/255` arithmetic as Chrome's Skia backend.
  ///
  /// This path also supports fractional device bounds by applying per-pixel coverage (like
  /// anti-aliasing) manually. This keeps fully covered interior pixels stable (no tiny-skia ±1
  /// bias) while still respecting subpixel edges.
  fn try_fill_rect_source_over_trunc(&mut self, rect: Rect, color: Rgba) -> bool {
    if self.current_state.blend_mode != SkiaBlendMode::SourceOver {
      return false;
    }
    if rect.width() <= 0.0 || rect.height() <= 0.0 {
      return true;
    }
    if !rect.x().is_finite()
      || !rect.y().is_finite()
      || !rect.width().is_finite()
      || !rect.height().is_finite()
    {
      return false;
    }

    let transform = self.current_state.transform;
    // Only support translation-only transforms (common case for pageset rendering and tiling).
    if (transform.sx - 1.0).abs() > 1e-6
      || (transform.sy - 1.0).abs() > 1e-6
      || transform.kx.abs() > 1e-6
      || transform.ky.abs() > 1e-6
      || !transform.tx.is_finite()
      || !transform.ty.is_finite()
    {
      return false;
    }

    let tx = transform.tx.round();
    let ty = transform.ty.round();
    // Allow small float noise so "should-be-integer" translations like `1199.9998` still take the
    // truncating path. Keep the epsilon tight so real subpixel translations remain anti-aliased.
    if (transform.tx - tx).abs() > NEAR_INTEGER_EPSILON_PX
      || (transform.ty - ty).abs() > NEAR_INTEGER_EPSILON_PX
    {
      return false;
    }

    let combined_alpha = (color.a * self.current_state.opacity).clamp(0.0, 1.0);
    let sa = (combined_alpha * 255.0).round().clamp(0.0, 255.0) as u8;
    if sa == 0 {
      return true;
    }

    // Keep existing code paths for opaque fills (they include snapping to avoid seams) unless a
    // clip mask is present.
    //
    // For opaque draws with an anti-aliased clip (e.g. `overflow:hidden` + `border-radius`), the
    // clip edge behaves like a per-pixel source alpha of `coverage`. tiny-skia's internal
    // compositing uses slightly different rounding than Chrome/Skia, often resulting in ±1 channel
    // differences along the edge. Using the truncating `mul/255` math here keeps clipped opaque
    // content consistent with `draw_rounded_rect` / `fill_rounded_rect` fast paths.
    let clip_mask = self.current_state.clip_mask.as_deref();
    let clip_mask_data = clip_mask.map(|mask| mask.data());
    let clip_mask_stride = clip_mask.map(|mask| mask.width() as usize).unwrap_or(0);
    if sa == 255 && clip_mask_data.is_none() {
      return false;
    }

    // Chrome/Skia treat axis-aligned rect fills as non-AA and determine covered pixels based on
    // pixel centers ("open min / closed max"), even for semi-transparent fills. This matters for
    // hairlines like `height: 0.5px` which should cover a full 1px scanline in Chrome.
    let Some(mut dev_rect) = self.snapped_device_rect_for_axis_aligned_fill(rect, transform) else {
      return false;
    };

    if let Some(clip) = self.current_state.clip_rect {
      dev_rect = match dev_rect.intersection(clip) {
        Some(r) => r,
        None => return true,
      };
    }

    let bounds = Rect::from_xywh(0.0, 0.0, self.width() as f32, self.height() as f32);
    dev_rect = match dev_rect.intersection(bounds) {
      Some(r) => r,
      None => return true,
    };

    let x0 = dev_rect.min_x();
    let y0 = dev_rect.min_y();
    let x1 = dev_rect.max_x();
    let y1 = dev_rect.max_y();

    if x0 >= x1 || y0 >= y1 {
      return true;
    }

    // Snap "should-be-integer" edges that are only fractional due to float noise.
    //
    // This preserves existing behavior from the integer-bounds fast path (regression tests in
    // `tests/paint/canvas_test.rs`) while still allowing meaningfully fractional edges.
    let mut x0 = x0;
    let mut y0 = y0;
    let mut x1 = x1;
    let mut y1 = y1;
    for v in [&mut x0, &mut y0, &mut x1, &mut y1] {
      let r = v.round();
      if (*v - r).abs() <= NEAR_INTEGER_EPSILON_PX {
        *v = r;
      }
    }

    // Compute pixel bounding box.
    let pix_w = self.pixmap.width() as i32;
    let pix_h = self.pixmap.height() as i32;

    let mut start_x = x0.floor() as i32;
    let mut start_y = y0.floor() as i32;
    let mut end_x = x1.ceil() as i32;
    let mut end_y = y1.ceil() as i32;

    start_x = start_x.clamp(0, pix_w);
    end_x = end_x.clamp(0, pix_w);
    start_y = start_y.clamp(0, pix_h);
    end_y = end_y.clamp(0, pix_h);
    if start_x >= end_x || start_y >= end_y {
      return true;
    }

    // Fully covered interior (coverage=1). These pixels dominate large translucent fills.
    let mut full_x0 = x0.ceil() as i32;
    let mut full_x1 = x1.floor() as i32;
    let mut full_y0 = y0.ceil() as i32;
    let mut full_y1 = y1.floor() as i32;
    full_x0 = full_x0.clamp(0, pix_w);
    full_x1 = full_x1.clamp(0, pix_w);
    full_y0 = full_y0.clamp(0, pix_h);
    full_y1 = full_y1.clamp(0, pix_h);
    let stride = self.pixmap.width() as usize * 4;
    let data = self.pixmap.data_mut();

    #[inline]
    fn blend_pixel(dst: &mut [u8], idx: usize, color: Rgba, sa_u8: u8) {
      if sa_u8 == 0 {
        return;
      }
      let sa = sa_u8 as u16;
      let inv_sa = 255u16 - sa;
      let sr = (color.r as u16 * sa) / 255u16;
      let sg = (color.g as u16 * sa) / 255u16;
      let sb = (color.b as u16 * sa) / 255u16;

      let dr = dst[idx] as u16;
      let dg = dst[idx + 1] as u16;
      let db = dst[idx + 2] as u16;
      let da = dst[idx + 3] as u16;

      let out_a = sa + (da * inv_sa) / 255u16;
      let out_r = sr + (dr * inv_sa) / 255u16;
      let out_g = sg + (dg * inv_sa) / 255u16;
      let out_b = sb + (db * inv_sa) / 255u16;

      // Clamp channels to the resulting alpha to preserve premultiplied invariants.
      let out_a_u8 = out_a.min(255) as u8;
      dst[idx + 3] = out_a_u8;
      let clamp = out_a_u8 as u16;
      dst[idx] = out_r.min(clamp).min(255) as u8;
      dst[idx + 1] = out_g.min(clamp).min(255) as u8;
      dst[idx + 2] = out_b.min(clamp).min(255) as u8;
    }

    // Fill interior with constant alpha using truncating `mul/255`.
    //
    // When a clip mask exists (rounded corners, clip-path, etc.) we still need to respect the mask
    // even for fully-covered pixels, so fall back to per-pixel alpha there.
    if full_x0 < full_x1 && full_y0 < full_y1 {
      let w = (full_x1 - full_x0) as usize;
      if let Some(src) = clip_mask_data {
        for y in full_y0..full_y1 {
          let mut idx = y as usize * stride + full_x0 as usize * 4;
          let mut midx = y as usize * clip_mask_stride + full_x0 as usize;
          for _ in 0..w {
            let mask_a = src[midx] as f32;
            let pix_sa = (combined_alpha * mask_a).round().clamp(0.0, 255.0) as u8;
            blend_pixel(data, idx, color, pix_sa);
            idx += 4;
            midx += 1;
          }
        }
      } else {
        let sa = sa as u16;
        let inv_sa = 255u16 - sa;
        let sr = (color.r as u16 * sa) / 255u16;
        let sg = (color.g as u16 * sa) / 255u16;
        let sb = (color.b as u16 * sa) / 255u16;

        for y in full_y0..full_y1 {
          let mut idx = y as usize * stride + full_x0 as usize * 4;
          for _ in 0..w {
            let dr = data[idx] as u16;
            let dg = data[idx + 1] as u16;
            let db = data[idx + 2] as u16;
            let da = data[idx + 3] as u16;

            let out_a = sa + (da * inv_sa) / 255u16;
            let out_r = sr + (dr * inv_sa) / 255u16;
            let out_g = sg + (dg * inv_sa) / 255u16;
            let out_b = sb + (db * inv_sa) / 255u16;

            // Clamp channels to the resulting alpha to preserve premultiplied invariants.
            let out_a_u8 = out_a.min(255) as u8;
            data[idx + 3] = out_a_u8;
            let clamp = out_a_u8 as u16;
            data[idx] = out_r.min(clamp).min(255) as u8;
            data[idx + 1] = out_g.min(clamp).min(255) as u8;
            data[idx + 2] = out_b.min(clamp).min(255) as u8;

            idx += 4;
          }
        }
      }
    }

    // Blend the surrounding edge pixels with per-pixel coverage.
    //
    // We only iterate the bounding box and skip the fully-covered interior to keep this fast.
    for y in start_y..end_y {
      let py0 = y as f32;
      let py1 = (y + 1) as f32;
      let cover_y = (y1.min(py1) - y0.max(py0)).clamp(0.0, 1.0);
      if cover_y <= 0.0 {
        continue;
      }

      let is_interior_row = y >= full_y0 && y < full_y1;
      let (left_x_end, right_x_start) = if is_interior_row {
        (full_x0.min(end_x), full_x1.max(start_x))
      } else {
        (end_x, end_x)
      };

      // Left band.
      for x in start_x..left_x_end {
        let px0 = x as f32;
        let px1 = (x + 1) as f32;
        let cover_x = (x1.min(px1) - x0.max(px0)).clamp(0.0, 1.0);
        let cover = cover_x * cover_y;
        if cover <= 0.0 {
          continue;
        }
        let mask_a = clip_mask_data
          .map(|src| src[y as usize * clip_mask_stride + x as usize] as f32)
          .unwrap_or(255.0);
        let pix_sa = (combined_alpha * cover * mask_a).round().clamp(0.0, 255.0) as u8;
        if pix_sa == 0 {
          continue;
        }
        let idx = y as usize * stride + x as usize * 4;
        blend_pixel(data, idx, color, pix_sa);
      }

      // Right band (interior rows only). For top/bottom rows we already processed the full row.
      if is_interior_row {
        for x in right_x_start..end_x {
          // Skip fully-covered interior columns.
          if x >= full_x0 && x < full_x1 {
            continue;
          }
          let px0 = x as f32;
          let px1 = (x + 1) as f32;
          let cover_x = (x1.min(px1) - x0.max(px0)).clamp(0.0, 1.0);
          let cover = cover_x * cover_y;
          if cover <= 0.0 {
            continue;
          }
          let mask_a = clip_mask_data
            .map(|src| src[y as usize * clip_mask_stride + x as usize] as f32)
            .unwrap_or(255.0);
          let pix_sa = (combined_alpha * cover * mask_a).round().clamp(0.0, 255.0) as u8;
          if pix_sa == 0 {
            continue;
          }
          let idx = y as usize * stride + x as usize * 4;
          blend_pixel(data, idx, color, pix_sa);
        }
      }
    }

    // For interior rows, the right-band loop above deliberately avoids iterating the fully-covered
    // columns. The left-band loop always covers the full `start_x..end_x` range for top/bottom
    // rows, so no pixels are missed.

    true
  }

  /// Fast path for axis-aligned `source-over` rounded-rectangle fills.
  ///
  /// Like [`Canvas::try_fill_rect_source_over_trunc`], tiny-skia's `fill_path` compositing differs
  /// subtly from Chrome/Skia for semi-transparent colors (often by ±1 in each channel). This
  /// affects both explicitly translucent fills and the anti-aliased edges of opaque rounded rects
  /// (which behave like `alpha = coverage`).
  ///
  /// Rounded-rectangle backgrounds show up frequently in UI-heavy pages, so these off-by-one
  /// errors can dominate page-loop diffs.
  ///
  /// When the transform is translation-only and there is no complex clip mask, we can rasterize
  /// the rounded-rect coverage into a temporary `Mask` and composite into the destination buffer
  /// using truncating `mul/255` arithmetic to match Chrome's Skia backend.
  fn try_fill_rounded_rect_source_over_trunc(
    &mut self,
    rect: Rect,
    radii: BorderRadii,
    color: Rgba,
  ) -> bool {
    if self.current_state.blend_mode != SkiaBlendMode::SourceOver {
      return false;
    }
    if rect.width() <= 0.0 || rect.height() <= 0.0 {
      return true;
    }
    if !rect.x().is_finite()
      || !rect.y().is_finite()
      || !rect.width().is_finite()
      || !rect.height().is_finite()
    {
      return false;
    }

    let mut transform = self.current_state.transform;
    // Only support translation-only transforms (common case for pageset rendering and tiling).
    if (transform.sx - 1.0).abs() > 1e-6
      || (transform.sy - 1.0).abs() > 1e-6
      || transform.kx.abs() > 1e-6
      || transform.ky.abs() > 1e-6
      || !transform.tx.is_finite()
      || !transform.ty.is_finite()
    {
      return false;
    }

    let tx = transform.tx.round();
    let ty = transform.ty.round();
    if (transform.tx - tx).abs() > NEAR_INTEGER_EPSILON_PX
      || (transform.ty - ty).abs() > NEAR_INTEGER_EPSILON_PX
    {
      return false;
    }

    let combined_alpha = (color.a * self.current_state.opacity).clamp(0.0, 1.0);
    let sa = (combined_alpha * 255.0).round().clamp(0.0, 255.0) as u8;
    if sa == 0 {
      return true;
    }

    let clip_mask = self.current_state.clip_mask.as_deref();

    // If we don't have a clip mask, we only handle rectangular clip bounds when they align to
    // integer device pixels. Fractional clip edges would require additional anti-aliasing support
    // to match tiny-skia's path clipping.
    //
    // When a clip mask exists, we can instead clamp the iteration bounds conservatively and apply
    // the clip coverage per pixel.
    let clip_int = if let Some(clip) = self.current_state.clip_rect {
      if clip.width() <= 0.0 || clip.height() <= 0.0 {
        return true;
      }
      let cx0 = clip.min_x();
      let cy0 = clip.min_y();
      let cx1 = clip.max_x();
      let cy1 = clip.max_y();
      if !cx0.is_finite() || !cy0.is_finite() || !cx1.is_finite() || !cy1.is_finite() {
        return false;
      }
      if clip_mask.is_none() {
        let cx0i = cx0.round();
        let cy0i = cy0.round();
        let cx1i = cx1.round();
        let cy1i = cy1.round();
        if (cx0 - cx0i).abs() > NEAR_INTEGER_EPSILON_PX
          || (cy0 - cy0i).abs() > NEAR_INTEGER_EPSILON_PX
          || (cx1 - cx1i).abs() > NEAR_INTEGER_EPSILON_PX
          || (cy1 - cy1i).abs() > NEAR_INTEGER_EPSILON_PX
        {
          return false;
        }
        Some((cx0i as i64, cy0i as i64, cx1i as i64, cy1i as i64))
      } else {
        Some((
          cx0.floor() as i64,
          cy0.floor() as i64,
          cx1.ceil() as i64,
          cy1.ceil() as i64,
        ))
      }
    } else {
      None
    };

    let Some(path) = self.build_rounded_rect_path(rect, radii) else {
      return true;
    };

    // Ensure the raster surface fully contains the transformed rounded-rect so coverage doesn't
    // depend on pixmap clipping (important for tile-based painting).
    const ROUNDED_RECT_COVERAGE_MARGIN_PX: i64 = 2;
    let bounds = Self::transform_rect_aabb(rect, transform);
    if bounds.width() <= 0.0
      || bounds.height() <= 0.0
      || !bounds.x().is_finite()
      || !bounds.y().is_finite()
      || !bounds.width().is_finite()
      || !bounds.height().is_finite()
    {
      return true;
    }

    let mut x0 = bounds.min_x().floor() as i64;
    let mut y0 = bounds.min_y().floor() as i64;
    let mut x1 = bounds.max_x().ceil() as i64;
    let mut y1 = bounds.max_y().ceil() as i64;

    x0 = x0.saturating_sub(ROUNDED_RECT_COVERAGE_MARGIN_PX);
    y0 = y0.saturating_sub(ROUNDED_RECT_COVERAGE_MARGIN_PX);
    x1 = x1.saturating_add(ROUNDED_RECT_COVERAGE_MARGIN_PX);
    y1 = y1.saturating_add(ROUNDED_RECT_COVERAGE_MARGIN_PX);

    let scratch_w_i64 = x1 - x0;
    let scratch_h_i64 = y1 - y0;
    let Ok(scratch_w) = u32::try_from(scratch_w_i64) else {
      return false;
    };
    let Ok(scratch_h) = u32::try_from(scratch_h_i64) else {
      return false;
    };
    if scratch_w == 0 || scratch_h == 0 {
      return true;
    }

    let mut scratch = ROUNDED_RECT_PAD_SCRATCH.with(|cell| std::mem::take(&mut *cell.borrow_mut()));
    let mut mask = match scratch.mask.take() {
      Some(existing) if existing.width() == scratch_w && existing.height() == scratch_h => existing,
      _ => match Mask::new(scratch_w, scratch_h) {
        Some(m) => m,
        None => {
          // Allocation failed; fall back to tiny-skia.
          scratch.mask = None;
          ROUNDED_RECT_PAD_SCRATCH.with(|cell| {
            *cell.borrow_mut() = scratch;
          });
          return false;
        }
      },
    };
    mask.data_mut().fill(0);
    mask.fill_path(
      &path,
      FillRule::Winding,
      true,
      transform.post_translate(-(x0 as f32), -(y0 as f32)),
    );

    let dest_w = self.width() as i64;
    let dest_h = self.height() as i64;
    let mut inter_x0 = x0.max(0);
    let mut inter_y0 = y0.max(0);
    let mut inter_x1 = x1.min(dest_w);
    let mut inter_y1 = y1.min(dest_h);
    if let Some((cx0, cy0, cx1, cy1)) = clip_int {
      inter_x0 = inter_x0.max(cx0);
      inter_y0 = inter_y0.max(cy0);
      inter_x1 = inter_x1.min(cx1);
      inter_y1 = inter_y1.min(cy1);
    }

    if inter_x1 <= inter_x0 || inter_y1 <= inter_y0 {
      scratch.mask = Some(mask);
      ROUNDED_RECT_PAD_SCRATCH.with(|cell| {
        *cell.borrow_mut() = scratch;
      });
      return true;
    }

    {
      let src = mask.data();
      let clip = clip_mask.map(|m| m.data());
      let clip_stride = clip_mask.map(|m| m.width() as usize).unwrap_or(0);
      let src_stride = scratch_w as usize;
      let dst_stride = self.width() as usize * 4;
      let dst = self.pixmap.data_mut();
      let copy_w = (inter_x1 - inter_x0) as usize;
      let copy_h = (inter_y1 - inter_y0) as usize;
      let src_x = (inter_x0 - x0) as usize;
      let src_y = (inter_y0 - y0) as usize;
      let dst_x = inter_x0 as usize * 4;
      let dst_y = inter_y0 as usize;
      let clip_x = inter_x0 as usize;

      let sa = sa as u16;
      let cr = color.r as u16;
      let cg = color.g as u16;
      let cb = color.b as u16;

      for row in 0..copy_h {
        let src_off = (src_y + row) * src_stride + src_x;
        let mut dst_off = (dst_y + row) * dst_stride + dst_x;
        let clip_off = (dst_y + row) * clip_stride + clip_x;
        for col in 0..copy_w {
          let mut coverage = src[src_off + col] as u16;
          if coverage == 0 {
            dst_off += 4;
            continue;
          }

          if let Some(clip) = clip {
            let clip_coverage = clip[clip_off + col] as u16;
            coverage = (coverage * clip_coverage) / 255u16;
            if coverage == 0 {
              dst_off += 4;
              continue;
            }
          }

          let pix_sa = (coverage * sa) / 255u16;
          if pix_sa == 0 {
            dst_off += 4;
            continue;
          }

          let inv_sa = 255u16 - pix_sa;
          let sr = (cr * pix_sa) / 255u16;
          let sg = (cg * pix_sa) / 255u16;
          let sb = (cb * pix_sa) / 255u16;

          let dr = dst[dst_off] as u16;
          let dg = dst[dst_off + 1] as u16;
          let db = dst[dst_off + 2] as u16;
          let da = dst[dst_off + 3] as u16;

          let out_a = pix_sa + (da * inv_sa) / 255u16;
          let out_r = sr + (dr * inv_sa) / 255u16;
          let out_g = sg + (dg * inv_sa) / 255u16;
          let out_b = sb + (db * inv_sa) / 255u16;

          // Clamp channels to the resulting alpha to preserve premultiplied invariants.
          let out_a_u8 = out_a.min(255) as u8;
          dst[dst_off + 3] = out_a_u8;
          let clamp = out_a_u8 as u16;
          dst[dst_off] = out_r.min(clamp).min(255) as u8;
          dst[dst_off + 1] = out_g.min(clamp).min(255) as u8;
          dst[dst_off + 2] = out_b.min(clamp).min(255) as u8;

          dst_off += 4;
        }
      }
    }

    scratch.mask = Some(mask);
    ROUNDED_RECT_PAD_SCRATCH.with(|cell| {
      *cell.borrow_mut() = scratch;
    });

    true
  }

  fn draw_rect_impl(&mut self, rect: Rect, color: Rgba) {
    // Skip fully transparent colors
    if color.a == 0.0 || self.current_state.opacity == 0.0 {
      return;
    }

    if self.try_fill_rect_source_over_trunc(rect, color) {
      return;
    }

    // Apply clip
    let rect = match self.apply_clip(rect) {
      Some(r) => r,
      None => return, // Fully clipped
    };

    let transform = self.current_state.transform;

    // Pixel-snap *effectively opaque* axis-aligned fills to avoid fractional-edge seams between
    // adjacent backgrounds (see `src/paint/tests/canvas_test.rs`).
    //
    // "Opaque" is determined after quantizing alpha using the same rounding logic as Skia/Chrome
    // (i.e. `round(alpha * 255)`), rather than requiring `color.a == 1.0` and
    // `current_state.opacity == 1.0` exactly. In practice computed opacity values can be extremely
    // close to 1.0 while still rounding to an 8-bit alpha of 255; treating those as opaque improves
    // border/background fidelity without affecting true semi-transparent content.
    //
    // Use a non-AA rasterization path (pixel-center rule) when the current transform is a
    // translation that's already near an integer device pixel. This avoids quantizing intentional
    // subpixel translations (e.g. CSS transform animations) while still producing crisp UI edges
    // for typical layout-derived fractional positions (e.g. `rem`/`pt`).
    if self.current_state.blend_mode == SkiaBlendMode::SourceOver {
      let alpha = (color.a * self.current_state.opacity).clamp(0.0, 1.0);
      let alpha_u8 = (alpha * 255.0).round().clamp(0.0, 255.0) as u8;
      if alpha_u8 == 255
        && (transform.sx - 1.0).abs() <= 1e-6
        && (transform.sy - 1.0).abs() <= 1e-6
        && transform.kx.abs() <= 1e-6
        && transform.ky.abs() <= 1e-6
        && transform.tx.is_finite()
        && transform.ty.is_finite()
      {
        let tx_round = transform.tx.round();
        let ty_round = transform.ty.round();
        if (transform.tx - tx_round).abs() <= 1e-3 && (transform.ty - ty_round).abs() <= 1e-3 {
          // First try the seam-avoidance snap that rounds near-integer rect edges in device space.
          if let Some(snapped) = self.snapped_device_rect_for_axis_aligned_fill(rect, transform) {
            if let Some(skia_rect) = self.to_skia_rect(snapped) {
              let path = PathBuilder::from_rect(skia_rect);
              let mut paint = self.current_state.create_paint(color);
              paint.anti_alias = false;
              self.pixmap.fill_path(
                &path,
                &paint,
                FillRule::Winding,
                Transform::identity(),
                self.current_state.clip_mask.as_deref(),
              );
              return;
            }
          }

          // Fall back to a non-AA fill at the original fractional geometry.
          if let Some(skia_rect) = self.to_skia_rect(rect) {
            let path = PathBuilder::from_rect(skia_rect);
            let mut paint = self.current_state.create_paint(color);
            paint.anti_alias = false;
            self.pixmap.fill_path(
              &path,
              &paint,
              FillRule::Winding,
              transform,
              self.current_state.clip_mask.as_deref(),
            );
            return;
          }
        }
      }
    }

    if let Some(skia_rect) = self.to_skia_rect(rect) {
      let path = PathBuilder::from_rect(skia_rect);
      let paint = self.current_state.create_paint(color);

      let needs_scratch = (transform.kx.abs() > 1e-6 || transform.ky.abs() > 1e-6)
        && transform != Transform::identity()
        && {
          let bounds = Self::transform_rect_aabb(rect, transform);
          bounds.min_x() < 0.0
            || bounds.min_y() < 0.0
            || bounds.max_x() > self.width() as f32
            || bounds.max_y() > self.height() as f32
        };

      if needs_scratch {
        const FILL_RECT_SCRATCH_MARGIN_PX: i64 = 2;

        let bounds = Self::transform_rect_aabb(rect, transform);
        if bounds.width() > 0.0
          && bounds.height() > 0.0
          && bounds.x().is_finite()
          && bounds.y().is_finite()
          && bounds.width().is_finite()
          && bounds.height().is_finite()
        {
          let mut x0 = bounds.min_x().floor() as i64;
          let mut y0 = bounds.min_y().floor() as i64;
          let mut x1 = bounds.max_x().ceil() as i64;
          let mut y1 = bounds.max_y().ceil() as i64;

          x0 = x0.saturating_sub(FILL_RECT_SCRATCH_MARGIN_PX);
          y0 = y0.saturating_sub(FILL_RECT_SCRATCH_MARGIN_PX);
          x1 = x1.saturating_add(FILL_RECT_SCRATCH_MARGIN_PX);
          y1 = y1.saturating_add(FILL_RECT_SCRATCH_MARGIN_PX);

          let scratch_w_i64 = x1 - x0;
          let scratch_h_i64 = y1 - y0;
          let Ok(scratch_w) = u32::try_from(scratch_w_i64) else {
            // Scratch would overflow; fall back to direct rasterization.
            self.pixmap.fill_path(
              &path,
              &paint,
              FillRule::Winding,
              transform,
              self.current_state.clip_mask.as_deref(),
            );
            return;
          };
          let Ok(scratch_h) = u32::try_from(scratch_h_i64) else {
            self.pixmap.fill_path(
              &path,
              &paint,
              FillRule::Winding,
              transform,
              self.current_state.clip_mask.as_deref(),
            );
            return;
          };
          if scratch_w == 0 || scratch_h == 0 {
            return;
          }

          let dest_w = self.width() as i64;
          let dest_h = self.height() as i64;
          let inter_x0 = x0.max(0);
          let inter_y0 = y0.max(0);
          let inter_x1 = x1.min(dest_w);
          let inter_y1 = y1.min(dest_h);
          if inter_x1 <= inter_x0 || inter_y1 <= inter_y0 {
            return;
          }

          let mut scratch = FILL_RECT_SCRATCH.with(|cell| std::mem::take(&mut *cell.borrow_mut()));
          let mut tmp = match scratch.pixmap.take() {
            Some(existing) if existing.width() == scratch_w && existing.height() == scratch_h => {
              existing
            }
            _ => match new_pixmap(scratch_w, scratch_h) {
              Some(pixmap) => pixmap,
              None => {
                // Allocation failed; fall back to direct rasterization.
                self.pixmap.fill_path(
                  &path,
                  &paint,
                  FillRule::Winding,
                  transform,
                  self.current_state.clip_mask.as_deref(),
                );
                scratch.pixmap = None;
                FILL_RECT_SCRATCH.with(|cell| {
                  *cell.borrow_mut() = scratch;
                });
                return;
              }
            },
          };

          tmp.data_mut().fill(0);

          // Seed the scratch pixmap with the current destination contents so we can reuse the
          // existing blend-mode implementation.
          {
            let src = self.pixmap.data();
            let dst = tmp.data_mut();
            let src_stride = self.width() as usize * 4;
            let dst_stride = scratch_w as usize * 4;
            let copy_w = (inter_x1 - inter_x0) as usize;
            let copy_h = (inter_y1 - inter_y0) as usize;
            let row_bytes = copy_w * 4;
            let src_x = inter_x0 as usize * 4;
            let dst_x = (inter_x0 - x0) as usize * 4;
            let src_y = inter_y0 as usize;
            let dst_y = (inter_y0 - y0) as usize;
            for row in 0..copy_h {
              let src_off = (src_y + row) * src_stride + src_x;
              let dst_off = (dst_y + row) * dst_stride + dst_x;
              dst[dst_off..dst_off + row_bytes].copy_from_slice(&src[src_off..src_off + row_bytes]);
            }
          }

          let clip_mask = self.current_state.clip_mask.as_deref();
          let mut scratch_mask: Option<Mask> = None;
          let scratch_clip: Option<&Mask> = if let Some(clip_mask) = clip_mask {
            let mut mask = match scratch.mask.take() {
              Some(existing) if existing.width() == scratch_w && existing.height() == scratch_h => {
                existing
              }
              _ => match Mask::new(scratch_w, scratch_h) {
                Some(m) => m,
                None => {
                  self.pixmap.fill_path(
                    &path,
                    &paint,
                    FillRule::Winding,
                    transform,
                    Some(clip_mask),
                  );
                  scratch.pixmap = Some(tmp);
                  scratch.mask = None;
                  FILL_RECT_SCRATCH.with(|cell| {
                    *cell.borrow_mut() = scratch;
                  });
                  return;
                }
              },
            };
            mask.data_mut().fill(0);
            {
              let src = clip_mask.data();
              let dst = mask.data_mut();
              let src_stride = clip_mask.width() as usize;
              let dst_stride = scratch_w as usize;
              let copy_w = (inter_x1 - inter_x0) as usize;
              let copy_h = (inter_y1 - inter_y0) as usize;
              let src_x = inter_x0 as usize;
              let dst_x = (inter_x0 - x0) as usize;
              let src_y = inter_y0 as usize;
              let dst_y = (inter_y0 - y0) as usize;
              for row in 0..copy_h {
                let src_off = (src_y + row) * src_stride + src_x;
                let dst_off = (dst_y + row) * dst_stride + dst_x;
                dst[dst_off..dst_off + copy_w].copy_from_slice(&src[src_off..src_off + copy_w]);
              }
            }
            scratch_mask = Some(mask);
            scratch_mask.as_ref()
          } else {
            None
          };

          let scratch_transform = concat_transforms(
            Transform::from_translate(-(x0 as f32), -(y0 as f32)),
            transform,
          );
          tmp.fill_path(
            &path,
            &paint,
            FillRule::Winding,
            scratch_transform,
            scratch_clip,
          );

          // Copy the updated destination contents back into the active pixmap.
          {
            let src = tmp.data();
            let width = self.width() as usize;
            let dst = self.pixmap.data_mut();
            let src_stride = scratch_w as usize * 4;
            let dst_stride = width * 4;
            let copy_w = (inter_x1 - inter_x0) as usize;
            let copy_h = (inter_y1 - inter_y0) as usize;
            let row_bytes = copy_w * 4;
            let src_x = (inter_x0 - x0) as usize * 4;
            let dst_x = inter_x0 as usize * 4;
            let src_y = (inter_y0 - y0) as usize;
            let dst_y = inter_y0 as usize;
            for row in 0..copy_h {
              let src_off = (src_y + row) * src_stride + src_x;
              let dst_off = (dst_y + row) * dst_stride + dst_x;
              dst[dst_off..dst_off + row_bytes].copy_from_slice(&src[src_off..src_off + row_bytes]);
            }
          }

          scratch.pixmap = Some(tmp);
          if scratch_mask.is_some() {
            scratch.mask = scratch_mask;
          }
          FILL_RECT_SCRATCH.with(|cell| {
            *cell.borrow_mut() = scratch;
          });
          return;
        }
      }

      self.pixmap.fill_path(
        &path,
        &paint,
        FillRule::Winding,
        transform,
        self.current_state.clip_mask.as_deref(),
      );
    }
  }

  /// Draws a stroked rectangle outline
  ///
  /// # Arguments
  ///
  /// * `rect` - Rectangle to stroke
  /// * `color` - Stroke color
  /// * `width` - Stroke width in pixels
  ///
  /// # Examples
  ///
  /// ```rust,ignore
  /// canvas.stroke_rect(rect, Rgba::BLACK, 2.0);
  /// ```
  pub fn stroke_rect(&mut self, rect: Rect, color: Rgba, width: f32) {
    self.mirror_to_source_alpha(|canvas| canvas.stroke_rect_impl(rect, color, width));
  }

  fn stroke_rect_impl(&mut self, rect: Rect, color: Rgba, width: f32) {
    if color.a == 0.0 || self.current_state.opacity == 0.0 {
      return;
    }

    if let Some(skia_rect) = self.to_skia_rect(rect) {
      let path = PathBuilder::from_rect(skia_rect);
      let paint = self.current_state.create_paint(color);
      let stroke = Stroke {
        width,
        ..Default::default()
      };
      self.pixmap.stroke_path(
        &path,
        &paint,
        &stroke,
        self.current_state.transform,
        self.current_state.clip_mask.as_deref(),
      );
    }
  }

  /// Draws a stroked rectangle outline using an explicit blend mode override.
  pub fn stroke_rect_with_blend(
    &mut self,
    rect: Rect,
    color: Rgba,
    width: f32,
    blend_mode: BlendMode,
  ) {
    self.mirror_to_source_alpha(|canvas| {
      canvas.stroke_rect_with_blend_impl(rect, color, width, blend_mode)
    });
  }

  fn stroke_rect_with_blend_impl(
    &mut self,
    rect: Rect,
    color: Rgba,
    width: f32,
    blend_mode: BlendMode,
  ) {
    if color.a == 0.0 || self.current_state.opacity == 0.0 {
      return;
    }

    if let Some(skia_rect) = self.to_skia_rect(rect) {
      let path = PathBuilder::from_rect(skia_rect);
      let paint = self
        .current_state
        .create_paint_with_blend(color, blend_mode.to_skia());
      let stroke = Stroke {
        width,
        ..Default::default()
      };
      self.pixmap.stroke_path(
        &path,
        &paint,
        &stroke,
        self.current_state.transform,
        self.current_state.clip_mask.as_deref(),
      );
    }
  }

  /// Draws a filled rounded rectangle
  ///
  /// # Arguments
  ///
  /// * `rect` - Rectangle bounds
  /// * `radii` - Corner radii
  /// * `color` - Fill color
  ///
  /// # Examples
  ///
  /// ```rust,ignore
  /// let radii = BorderRadii::uniform(10.0);
  /// canvas.draw_rounded_rect(rect, radii, Rgba::BLUE);
  /// ```
  pub fn draw_rounded_rect(&mut self, rect: Rect, radii: BorderRadii, color: Rgba) {
    self.mirror_to_source_alpha(|canvas| canvas.draw_rounded_rect_impl(rect, radii, color));
  }

  fn draw_rounded_rect_impl(&mut self, rect: Rect, radii: BorderRadii, color: Rgba) {
    if color.a == 0.0 || self.current_state.opacity == 0.0 {
      return;
    }

    // If no radius, use simple rect
    if !radii.has_radius() {
      return self.draw_rect(rect, color);
    }

    if let Some(clip) = self.current_state.clip_rect {
      if clip.width() <= 0.0 || clip.height() <= 0.0 {
        return;
      }

      let intersects = if self.current_state.transform == Transform::identity() {
        rect.intersection(clip).is_some()
      } else {
        Self::transform_rect_aabb(rect, self.current_state.transform)
          .intersection(clip)
          .is_some()
      };

      if !intersects {
        return;
      }
    }

    let transform = self.current_state.transform;

    // Chrome/Blink tends to snap box-decoration geometry (including semi-transparent skeleton
    // placeholders) to the device pixel grid when the effective transform is translation-only.
    //
    // This avoids 1px blended seams when layout produces quarter/half pixel edges (e.g. via
    // percentage padding like `padding-top: 56.25%` used for 16:9 aspect ratio boxes).
    //
    // Keep this restricted to translation-only transforms whose translation is already near an
    // integer device pixel so we don't quantize real subpixel animations (e.g. `transform:
    // translateY(0.5px)`).
    let rect = if self.current_state.blend_mode == SkiaBlendMode::SourceOver
      && transform.kx.abs() <= 1e-6
      && transform.ky.abs() <= 1e-6
      && (transform.sx - 1.0).abs() <= 1e-6
      && (transform.sy - 1.0).abs() <= 1e-6
      && (transform.tx - transform.tx.round()).abs() <= 1e-3
      && (transform.ty - transform.ty.round()).abs() <= 1e-3
    {
      let x0 = rect.min_x().round();
      let x1 = rect.max_x().round();
      let y0 = rect.min_y().round();
      let y1 = rect.max_y().round();
      let min_x = x0.min(x1);
      let max_x = x0.max(x1);
      let min_y = y0.min(y1);
      let max_y = y0.max(y1);
      let snapped = Rect::from_xywh(min_x, min_y, max_x - min_x, max_y - min_y);
      if snapped.width() > 0.0 && snapped.height() > 0.0 {
        snapped
      } else {
        rect
      }
    } else {
      rect
    };

    if self.try_fill_rounded_rect_source_over_trunc(rect, radii, color) {
      return;
    }

    // As with `draw_rect_impl`, pixel-snap opaque axis-aligned rounded-rect fills so UI
    // backgrounds don't produce 1px blended seams at fractional boundaries. For rounded rects we
    // keep anti-aliasing enabled so corners remain smooth.
    //
    // We only take this path for pure translation transforms (no scale/rotation) so radii remain
    // in the same coordinate space when we bake the transform into the rect.
    let (rect, transform) = if color.a == 1.0
      && self.current_state.opacity == 1.0
      && self.current_state.blend_mode == SkiaBlendMode::SourceOver
      && transform.kx.abs() <= 1e-6
      && transform.ky.abs() <= 1e-6
      && (transform.sx - 1.0).abs() <= 1e-6
      && (transform.sy - 1.0).abs() <= 1e-6
    {
      if let Some(snapped) = self.snapped_device_rect_for_axis_aligned_fill(rect, transform) {
        (snapped, Transform::identity())
      } else {
        (rect, transform)
      }
    } else {
      (rect, transform)
    };

    let Some(path) = self.build_rounded_rect_path(rect, radii) else {
      return;
    };

    // Parallel tiling translates the canvas so each tile renders a sub-region of the full
    // viewport. When a large rounded-rect extends beyond the (tile + halo) pixmap bounds,
    // tiny-skia clips the path during rasterization which can lead to seams between tiles.
    //
    // To keep serial and tiled output pixel-identical, rasterize into a scratch pixmap large
    // enough to fully contain the transformed rounded-rect (so no clipping occurs), then copy the
    // affected destination pixels back.
    const ROUNDED_RECT_SCRATCH_MARGIN_PX: i64 = 2;

    let bounds = Self::transform_rect_aabb(rect, transform);
    let needs_scratch = bounds.min_x() < 0.0
      || bounds.min_y() < 0.0
      || bounds.max_x() > self.width() as f32
      || bounds.max_y() > self.height() as f32;

    if needs_scratch {
      if bounds.width() <= 0.0
        || bounds.height() <= 0.0
        || !bounds.x().is_finite()
        || !bounds.y().is_finite()
        || !bounds.width().is_finite()
        || !bounds.height().is_finite()
      {
        return;
      }

      let mut x0 = bounds.min_x().floor() as i64;
      let mut y0 = bounds.min_y().floor() as i64;
      let mut x1 = bounds.max_x().ceil() as i64;
      let mut y1 = bounds.max_y().ceil() as i64;

      x0 = x0.saturating_sub(ROUNDED_RECT_SCRATCH_MARGIN_PX);
      y0 = y0.saturating_sub(ROUNDED_RECT_SCRATCH_MARGIN_PX);
      x1 = x1.saturating_add(ROUNDED_RECT_SCRATCH_MARGIN_PX);
      y1 = y1.saturating_add(ROUNDED_RECT_SCRATCH_MARGIN_PX);

      let scratch_w_i64 = x1 - x0;
      let scratch_h_i64 = y1 - y0;
      let Ok(scratch_w) = u32::try_from(scratch_w_i64) else {
        // Scratch would overflow; fall back to direct rasterization.
        let paint = self.current_state.create_paint(color);
        self.pixmap.fill_path(
          &path,
          &paint,
          FillRule::Winding,
          transform,
          self.current_state.clip_mask.as_deref(),
        );
        return;
      };
      let Ok(scratch_h) = u32::try_from(scratch_h_i64) else {
        let paint = self.current_state.create_paint(color);
        self.pixmap.fill_path(
          &path,
          &paint,
          FillRule::Winding,
          transform,
          self.current_state.clip_mask.as_deref(),
        );
        return;
      };
      if scratch_w == 0 || scratch_h == 0 {
        return;
      }

      let dest_w = self.width() as i64;
      let dest_h = self.height() as i64;
      let inter_x0 = x0.max(0);
      let inter_y0 = y0.max(0);
      let inter_x1 = x1.min(dest_w);
      let inter_y1 = y1.min(dest_h);
      if inter_x1 <= inter_x0 || inter_y1 <= inter_y0 {
        return;
      }

      let mut scratch =
        ROUNDED_RECT_PAD_SCRATCH.with(|cell| std::mem::take(&mut *cell.borrow_mut()));
      let mut tmp = match scratch.pixmap.take() {
        Some(existing) if existing.width() == scratch_w && existing.height() == scratch_h => {
          existing
        }
        _ => match new_pixmap(scratch_w, scratch_h) {
          Some(pixmap) => pixmap,
          None => {
            // Allocation failed; fall back to direct rasterization.
            let paint = self.current_state.create_paint(color);
            self.pixmap.fill_path(
              &path,
              &paint,
              FillRule::Winding,
              transform,
              self.current_state.clip_mask.as_deref(),
            );
            scratch.pixmap = None;
            ROUNDED_RECT_PAD_SCRATCH.with(|cell| {
              *cell.borrow_mut() = scratch;
            });
            return;
          }
        },
      };

      tmp.data_mut().fill(0);

      // Seed the scratch pixmap with the current destination contents so we can use the same
      // blending path as a direct `fill_path` call.
      {
        let src = self.pixmap.data();
        let dst = tmp.data_mut();
        let src_stride = self.width() as usize * 4;
        let dst_stride = scratch_w as usize * 4;
        let copy_w = (inter_x1 - inter_x0) as usize;
        let copy_h = (inter_y1 - inter_y0) as usize;
        let row_bytes = copy_w * 4;
        let src_x = inter_x0 as usize * 4;
        let dst_x = (inter_x0 - x0) as usize * 4;
        let src_y = inter_y0 as usize;
        let dst_y = (inter_y0 - y0) as usize;
        for row in 0..copy_h {
          let src_off = (src_y + row) * src_stride + src_x;
          let dst_off = (dst_y + row) * dst_stride + dst_x;
          dst[dst_off..dst_off + row_bytes].copy_from_slice(&src[src_off..src_off + row_bytes]);
        }
      }

      let clip_mask = self.current_state.clip_mask.as_deref();
      let mut scratch_mask: Option<Mask> = None;
      let scratch_clip: Option<&Mask> = if let Some(clip_mask) = clip_mask {
        let mut mask = match scratch.mask.take() {
          Some(existing) if existing.width() == scratch_w && existing.height() == scratch_h => {
            existing
          }
          _ => match Mask::new(scratch_w, scratch_h) {
            Some(m) => m,
            None => {
              let paint = self.current_state.create_paint(color);
              self
                .pixmap
                .fill_path(&path, &paint, FillRule::Winding, transform, Some(clip_mask));
              scratch.pixmap = Some(tmp);
              scratch.mask = None;
              ROUNDED_RECT_PAD_SCRATCH.with(|cell| {
                *cell.borrow_mut() = scratch;
              });
              return;
            }
          },
        };
        mask.data_mut().fill(0);
        {
          let src = clip_mask.data();
          let dst = mask.data_mut();
          let src_stride = clip_mask.width() as usize;
          let dst_stride = scratch_w as usize;
          let copy_w = (inter_x1 - inter_x0) as usize;
          let copy_h = (inter_y1 - inter_y0) as usize;
          let src_x = inter_x0 as usize;
          let dst_x = (inter_x0 - x0) as usize;
          let src_y = inter_y0 as usize;
          let dst_y = (inter_y0 - y0) as usize;
          for row in 0..copy_h {
            let src_off = (src_y + row) * src_stride + src_x;
            let dst_off = (dst_y + row) * dst_stride + dst_x;
            dst[dst_off..dst_off + copy_w].copy_from_slice(&src[src_off..src_off + copy_w]);
          }
        }
        scratch_mask = Some(mask);
        scratch_mask.as_ref()
      } else {
        None
      };

      let paint = self.current_state.create_paint(color);
      tmp.fill_path(
        &path,
        &paint,
        FillRule::Winding,
        transform.post_translate(-(x0 as f32), -(y0 as f32)),
        scratch_clip,
      );

      // Copy the updated destination contents back into the active pixmap.
      {
        let src = tmp.data();
        let width = self.width() as usize;
        let dst = self.pixmap.data_mut();
        let src_stride = scratch_w as usize * 4;
        let dst_stride = width * 4;
        let copy_w = (inter_x1 - inter_x0) as usize;
        let copy_h = (inter_y1 - inter_y0) as usize;
        let row_bytes = copy_w * 4;
        let src_x = (inter_x0 - x0) as usize * 4;
        let dst_x = inter_x0 as usize * 4;
        let src_y = (inter_y0 - y0) as usize;
        let dst_y = inter_y0 as usize;
        for row in 0..copy_h {
          let src_off = (src_y + row) * src_stride + src_x;
          let dst_off = (dst_y + row) * dst_stride + dst_x;
          dst[dst_off..dst_off + row_bytes].copy_from_slice(&src[src_off..src_off + row_bytes]);
        }
      }

      scratch.pixmap = Some(tmp);
      if scratch_mask.is_some() {
        scratch.mask = scratch_mask;
      }
      ROUNDED_RECT_PAD_SCRATCH.with(|cell| {
        *cell.borrow_mut() = scratch;
      });
      return;
    }

    let paint = self.current_state.create_paint(color);
    self.pixmap.fill_path(
      &path,
      &paint,
      FillRule::Winding,
      transform,
      self.current_state.clip_mask.as_deref(),
    );
  }

  /// Draws a stroked rounded rectangle outline
  pub fn stroke_rounded_rect(&mut self, rect: Rect, radii: BorderRadii, color: Rgba, width: f32) {
    self
      .mirror_to_source_alpha(|canvas| canvas.stroke_rounded_rect_impl(rect, radii, color, width));
  }

  fn stroke_rounded_rect_impl(&mut self, rect: Rect, radii: BorderRadii, color: Rgba, width: f32) {
    if color.a == 0.0 || self.current_state.opacity == 0.0 {
      return;
    }

    if !radii.has_radius() {
      return self.stroke_rect(rect, color, width);
    }

    if let Some(clip) = self.current_state.clip_rect {
      if clip.width() <= 0.0 || clip.height() <= 0.0 {
        return;
      }

      let intersects = if self.current_state.transform == Transform::identity() {
        rect.intersection(clip).is_some()
      } else {
        Self::transform_rect_aabb(rect, self.current_state.transform)
          .intersection(clip)
          .is_some()
      };

      if !intersects {
        return;
      }
    }

    if let Some(path) = self.build_rounded_rect_path(rect, radii) {
      let paint = self.current_state.create_paint(color);
      let stroke = Stroke {
        width,
        ..Default::default()
      };
      self.pixmap.stroke_path(
        &path,
        &paint,
        &stroke,
        self.current_state.transform,
        self.current_state.clip_mask.as_deref(),
      );
    }
  }

  fn hb_variations(variations: &[FontVariation]) -> Vec<HbVariation> {
    variations
      .iter()
      .map(|v| HbVariation {
        tag: v.tag,
        value: v.value(),
      })
      .collect()
  }

  pub(crate) fn glyph_paths(
    &mut self,
    position: Point,
    glyphs: &[GlyphPosition],
    _font: &LoadedFont,
    font_size: f32,
    _synthetic_oblique: f32,
    _variations: &[FontVariation],
    _rotation: Option<Transform>,
  ) -> Result<(Vec<tiny_skia::Path>, PathBounds)> {
    // Approximate bounds using glyph advances to avoid needing full outline extraction.
    let mut min_x = position.x;
    let mut max_x = position.x;
    for glyph in glyphs {
      let gx = position.x + glyph.x_offset;
      min_x = min_x.min(gx);
      max_x = max_x.max(gx + glyph.x_advance);
    }
    let ascent = font_size;
    let descent = font_size * 0.25;
    let mut bounds = PathBounds::new();
    let x = if min_x.is_finite() { min_x } else { 0.0 };
    let y = position.y - ascent;
    let y = if y.is_finite() { y } else { 0.0 };
    let w = (max_x - min_x).max(0.0);
    let w = if w.is_finite() { w } else { 0.0 };
    let h = ascent + descent;
    let h = if h.is_finite() { h } else { 0.0 };
    let rect = SkiaRect::from_xywh(x, y, w, h).or_else(|| SkiaRect::from_xywh(x, y, 1.0, 1.0));
    if let Some(rect) = rect {
      bounds.include(&rect);
    }
    Ok((Vec::new(), bounds))
  }

  /// Draws a shaped text run at the specified position.
  ///
  /// Applies the current canvas transform, clip, opacity, blend mode, palette
  /// index, and font variations to the provided [`ShapedRun`].
  ///
  /// # Examples
  ///
  /// ```rust,ignore
  /// let pipeline = ShapingPipeline::new();
  /// let style = ComputedStyle::default();
  /// let runs = pipeline.shape("Hello", &style, &font_context)?;
  /// let run = &runs[0];
  /// canvas.draw_shaped_run(run, Point::new(10.0, 50.0), Rgba::BLACK)?;
  /// ```
  pub fn draw_shaped_run(&mut self, run: &ShapedRun, position: Point, color: Rgba) -> Result<()> {
    self.mirror_to_source_alpha_result(|canvas| canvas.draw_shaped_run_impl(run, position, color))
  }

  fn draw_shaped_run_impl(&mut self, run: &ShapedRun, position: Point, color: Rgba) -> Result<()> {
    if run.glyphs.is_empty() || color.a == 0.0 || self.current_state.opacity == 0.0 {
      return Ok(());
    }

    let state = self.current_text_state(self.current_state.clip_mask.as_deref());
    let mut pixmap = self.pixmap.as_mut();
    self.text_rasterizer.render_shaped_run_with_state_pixmap_mut(
      run,
      position.x,
      position.y,
      color,
      &mut pixmap,
      state,
    )?;
    Ok(())
  }

  /// Draws text glyphs at the specified position.
  ///
  /// Renders positioned glyphs from the text shaping pipeline using an explicit
  /// palette index and variation list.
  ///
  /// # Arguments
  ///
  /// * `position` - Baseline origin for the text
  /// * `glyphs` - Positioned glyphs from text shaping
  /// * `font` - Font containing glyph outlines
  /// * `font_size` - Font size in pixels
  /// * `color` - Text color
  /// * `synthetic_bold` - Additional stroke width to simulate bold
  /// * `synthetic_oblique` - Shear factor to simulate italics
  /// * `palette_index` - Color font palette index
  /// * `variations` - Active variation settings
  ///
  /// # Examples
  ///
  /// ```rust,ignore
  /// let pipeline = ShapingPipeline::new();
  /// let style = ComputedStyle::default();
  /// let runs = pipeline.shape("Hello", &style, &font_context)?;
  /// let run = &runs[0];
  /// canvas.draw_text(
  ///   Point::new(10.0, 50.0),
  ///   &run.glyphs,
  ///   &run.font,
  ///   run.font_size,
  ///   Rgba::BLACK,
  ///   run.synthetic_bold,
  ///   run.synthetic_oblique,
  ///   run.palette_index,
  ///   run.palette_overrides.as_slice(),
  ///   run.palette_override_hash,
  ///   &[],
  /// )?;
  /// ```
  pub fn draw_text(
    &mut self,
    position: Point,
    glyphs: &[GlyphInstance],
    font: &LoadedFont,
    font_size: f32,
    color: Rgba,
    synthetic_bold: f32,
    synthetic_oblique: f32,
    palette_index: u16,
    palette_overrides: &[(u16, Rgba)],
    palette_override_hash: u64,
    variations: &[FontVariation],
  ) -> Result<()> {
    self.draw_text_run(
      position,
      glyphs,
      font,
      font_size,
      1.0,
      RunRotation::None,
      true,
      color,
      synthetic_bold,
      synthetic_oblique,
      palette_index,
      palette_overrides,
      palette_override_hash,
      variations,
    )
  }

  #[allow(clippy::too_many_arguments)]
  pub fn draw_text_run(
    &mut self,
    position: Point,
    glyphs: &[GlyphInstance],
    font: &LoadedFont,
    font_size: f32,
    run_scale: f32,
    rotation: RunRotation,
    allow_subpixel_aa: bool,
    color: Rgba,
    synthetic_bold: f32,
    synthetic_oblique: f32,
    palette_index: u16,
    palette_overrides: &[(u16, Rgba)],
    palette_override_hash: u64,
    variations: &[FontVariation],
  ) -> Result<()> {
    self.mirror_to_source_alpha_result(|canvas| {
      canvas.draw_text_run_impl(
        position,
        glyphs,
        font,
        font_size,
        run_scale,
        rotation,
        allow_subpixel_aa,
        color,
        None,
        synthetic_bold,
        synthetic_oblique,
        palette_index,
        palette_overrides,
        palette_override_hash,
        variations,
        FontSmoothing::Auto,
      )
    })
  }

  #[allow(clippy::too_many_arguments)]
  pub fn draw_text_run_with_stroke(
    &mut self,
    position: Point,
    glyphs: &[GlyphInstance],
    font: &LoadedFont,
    font_size: f32,
    run_scale: f32,
    rotation: RunRotation,
    allow_subpixel_aa: bool,
    color: Rgba,
    stroke_width: f32,
    stroke_color: Rgba,
    synthetic_bold: f32,
    synthetic_oblique: f32,
    palette_index: u16,
    palette_overrides: &[(u16, Rgba)],
    palette_override_hash: u64,
    variations: &[FontVariation],
  ) -> Result<()> {
    self.draw_text_run_with_stroke_and_font_smoothing(
      position,
      glyphs,
      font,
      font_size,
      run_scale,
      rotation,
      allow_subpixel_aa,
      color,
      stroke_width,
      stroke_color,
      synthetic_bold,
      synthetic_oblique,
      palette_index,
      palette_overrides,
      palette_override_hash,
      variations,
      FontSmoothing::Auto,
    )
  }

  #[allow(clippy::too_many_arguments)]
  pub fn draw_text_run_with_stroke_and_font_smoothing(
    &mut self,
    position: Point,
    glyphs: &[GlyphInstance],
    font: &LoadedFont,
    font_size: f32,
    run_scale: f32,
    rotation: RunRotation,
    allow_subpixel_aa: bool,
    color: Rgba,
    stroke_width: f32,
    stroke_color: Rgba,
    synthetic_bold: f32,
    synthetic_oblique: f32,
    palette_index: u16,
    palette_overrides: &[(u16, Rgba)],
    palette_override_hash: u64,
    variations: &[FontVariation],
    font_smoothing: FontSmoothing,
  ) -> Result<()> {
    let stroke = (stroke_width > 0.0 && stroke_color.a > 0.0).then_some(TextStroke {
      width: stroke_width,
      color: stroke_color,
    });
    self.mirror_to_source_alpha_result(|canvas| {
      canvas.draw_text_run_impl(
        position,
        glyphs,
        font,
        font_size,
        run_scale,
        rotation,
        allow_subpixel_aa,
        color,
        stroke,
        synthetic_bold,
        synthetic_oblique,
        palette_index,
        palette_overrides,
        palette_override_hash,
        variations,
        font_smoothing,
      )
    })
  }

  #[allow(clippy::too_many_arguments)]
  fn draw_text_run_impl(
    &mut self,
    position: Point,
    glyphs: &[GlyphInstance],
    font: &LoadedFont,
    font_size: f32,
    run_scale: f32,
    rotation: RunRotation,
    allow_subpixel_aa: bool,
    color: Rgba,
    stroke: Option<TextStroke>,
    synthetic_bold: f32,
    synthetic_oblique: f32,
    palette_index: u16,
    palette_overrides: &[(u16, Rgba)],
    palette_override_hash: u64,
    variations: &[FontVariation],
    font_smoothing: FontSmoothing,
  ) -> Result<()> {
    let has_stroke = stroke.is_some();
    if glyphs.is_empty() || (color.a == 0.0 && !has_stroke) || self.current_state.opacity == 0.0 {
      return Ok(());
    }

    self.materialize_rect_clip_mask_if_needed();

    let hb_variations = Self::hb_variations(variations);
    let mut state = self.current_text_state_with_font_smoothing(
      self.current_state.clip_mask.as_deref(),
      font_smoothing,
    );
    state.allow_subpixel_aa = allow_subpixel_aa;

    let positions: Vec<GlyphPosition> = glyphs
      .iter()
      .map(|g| GlyphPosition {
        glyph_id: g.glyph_id,
        cluster: g.cluster,
        x_offset: g.x_offset,
        y_offset: g.y_offset,
        x_advance: g.x_advance,
        y_advance: g.y_advance,
      })
      .collect();

    let rotation = rotation_transform(rotation, position.x, position.y);
    let mut pixmap = self.pixmap.as_mut();
    self
      .text_rasterizer
      .render_glyph_run_with_stroke_pixmap_mut(
      &positions,
      font,
      font_size * run_scale,
      synthetic_bold,
      synthetic_oblique,
      palette_index,
      palette_overrides,
      palette_override_hash,
      &hb_variations,
      rotation,
      position.x,
      position.y,
      color,
      stroke,
      state,
      &mut pixmap,
    )?;
    Ok(())
  }

  /// Draws a pre-rasterized color glyph pixmap.
  ///
  /// The provided `glyph_opacity` is multiplied by the current canvas state
  /// opacity so color glyphs participate in CSS opacity the same way outline
  /// fills do.
  pub fn draw_color_glyph(
    &mut self,
    position: Point,
    glyph: &ColorGlyphRaster,
    glyph_opacity: f32,
    glyph_transform: Option<Transform>,
  ) {
    self.mirror_to_source_alpha(|canvas| {
      canvas.draw_color_glyph_impl(position, glyph, glyph_opacity, glyph_transform);
    });
  }

  fn draw_color_glyph_impl(
    &mut self,
    position: Point,
    glyph: &ColorGlyphRaster,
    glyph_opacity: f32,
    glyph_transform: Option<Transform>,
  ) {
    let combined_opacity = (glyph_opacity * self.current_state.opacity).clamp(0.0, 1.0);
    if combined_opacity == 0.0 {
      return;
    }

    let mut paint = PixmapPaint::default();
    paint.opacity = combined_opacity;
    paint.blend_mode = self.current_state.blend_mode;
    let translation = Transform::from_translate(position.x + glyph.left, position.y + glyph.top);
    let mut transform = glyph_transform.unwrap_or_else(Transform::identity);
    transform = concat_transforms(transform, translation);
    transform = concat_transforms(self.current_state.transform, transform);
    let clip = self.current_state.clip_mask.as_deref();
    self
      .pixmap
      .draw_pixmap(0, 0, glyph.image.as_ref().as_ref(), &paint, transform, clip);
  }

  /// Draws a line between two points
  ///
  /// # Arguments
  ///
  /// * `start` - Starting point
  /// * `end` - Ending point
  /// * `color` - Line color
  /// * `width` - Line width in pixels
  pub fn draw_line(&mut self, start: Point, end: Point, color: Rgba, width: f32) {
    self.mirror_to_source_alpha(|canvas| canvas.draw_line_impl(start, end, color, width));
  }

  fn draw_line_impl(&mut self, start: Point, end: Point, color: Rgba, width: f32) {
    if color.a == 0.0 || self.current_state.opacity == 0.0 {
      return;
    }

    let mut pb = PathBuilder::new();
    pb.move_to(start.x, start.y);
    pb.line_to(end.x, end.y);

    if let Some(path) = pb.finish() {
      let paint = self.current_state.create_paint(color);
      let stroke = Stroke {
        width,
        ..Default::default()
      };
      self.pixmap.stroke_path(
        &path,
        &paint,
        &stroke,
        self.current_state.transform,
        self.current_state.clip_mask.as_deref(),
      );
    }
  }

  /// Draws a filled circle
  ///
  /// # Arguments
  ///
  /// * `center` - Center point of the circle
  /// * `radius` - Circle radius in pixels
  /// * `color` - Fill color
  pub fn draw_circle(&mut self, center: Point, radius: f32, color: Rgba) {
    self.mirror_to_source_alpha(|canvas| canvas.draw_circle_impl(center, radius, color));
  }

  fn draw_circle_impl(&mut self, center: Point, radius: f32, color: Rgba) {
    if color.a == 0.0 || radius <= 0.0 {
      return;
    }

    if let Some(path) = self.build_circle_path(center, radius) {
      let paint = self.current_state.create_paint(color);
      self.pixmap.fill_path(
        &path,
        &paint,
        FillRule::Winding,
        self.current_state.transform,
        self.current_state.clip_mask.as_deref(),
      );
    }
  }

  /// Strokes a circle outline
  pub fn stroke_circle(&mut self, center: Point, radius: f32, color: Rgba, width: f32) {
    self.mirror_to_source_alpha(|canvas| canvas.stroke_circle_impl(center, radius, color, width));
  }

  fn stroke_circle_impl(&mut self, center: Point, radius: f32, color: Rgba, width: f32) {
    if color.a == 0.0 || radius <= 0.0 {
      return;
    }

    if let Some(path) = self.build_circle_path(center, radius) {
      let paint = self.current_state.create_paint(color);
      let stroke = Stroke {
        width,
        ..Default::default()
      };
      self.pixmap.stroke_path(
        &path,
        &paint,
        &stroke,
        self.current_state.transform,
        self.current_state.clip_mask.as_deref(),
      );
    }
  }

  // ========================================================================
  // Path Building Helpers
  // ========================================================================

  /// Converts a geometry Rect to tiny-skia Rect
  fn to_skia_rect(&self, rect: Rect) -> Option<SkiaRect> {
    SkiaRect::from_xywh(rect.x(), rect.y(), rect.width(), rect.height())
  }

  /// Applies the current clip to a rectangle
  /// Applies the current clip to a rectangle.
  pub(crate) fn apply_clip(&self, rect: Rect) -> Option<Rect> {
    if let Some(clip) = self.current_state.clip_rect {
      if clip.width() <= 0.0 || clip.height() <= 0.0 {
        return None;
      }

      if self.current_state.transform == Transform::identity() {
        rect.intersection(clip)
      } else {
        let transformed_rect = Self::transform_rect_aabb(rect, self.current_state.transform);
        if transformed_rect.intersection(clip).is_some() {
          Some(rect)
        } else {
          None
        }
      }
    } else {
      Some(rect)
    }
  }

  #[inline]
  fn transform_point(transform: Transform, point: Point) -> Point {
    Point::new(
      point.x * transform.sx + point.y * transform.kx + transform.tx,
      point.x * transform.ky + point.y * transform.sy + transform.ty,
    )
  }

  #[inline]
  fn transform_rect_aabb(rect: Rect, transform: Transform) -> Rect {
    let p1 = Self::transform_point(transform, rect.origin);
    let p2 = Self::transform_point(transform, Point::new(rect.max_x(), rect.min_y()));
    let p3 = Self::transform_point(transform, Point::new(rect.min_x(), rect.max_y()));
    let p4 = Self::transform_point(transform, Point::new(rect.max_x(), rect.max_y()));

    let min_x = p1.x.min(p2.x).min(p3.x).min(p4.x);
    let max_x = p1.x.max(p2.x).max(p3.x).max(p4.x);
    let min_y = p1.y.min(p2.y).min(p3.y).min(p4.y);
    let max_y = p1.y.max(p2.y).max(p3.y).max(p4.y);

    Rect::from_xywh(min_x, min_y, max_x - min_x, max_y - min_y)
  }

  fn build_clip_mask(&self, rect: Rect, radii: BorderRadii) -> Option<Mask> {
    if rect.width() <= 0.0
      || rect.height() <= 0.0
      || self.width() == 0
      || self.height() == 0
      || !rect.x().is_finite()
      || !rect.y().is_finite()
      || !rect.width().is_finite()
      || !rect.height().is_finite()
    {
      return None;
    }

    // Fast path: axis-aligned rectangular clips don't need the full pixmap rasterization step.
    // Filling the mask directly avoids allocating a temporary RGBA pixmap (4 bytes/pixel) per
    // clip.
    if radii.is_zero() {
      if let Some(mask) = self.build_clip_mask_fast_rect(rect) {
        return Some(mask);
      }
    }

    self.build_clip_mask_slow_path(rect, radii)
  }

  fn build_clip_mask_fast_rect(&self, rect: Rect) -> Option<Mask> {
    let transform = self.current_state.transform;
    if transform.kx.abs() > 1e-6 || transform.ky.abs() > 1e-6 {
      return None;
    }
    if transform.sx.abs() < 1e-6 || transform.sy.abs() < 1e-6 {
      return None;
    }
    if !transform.sx.is_finite()
      || !transform.sy.is_finite()
      || !transform.tx.is_finite()
      || !transform.ty.is_finite()
    {
      return None;
    }

    // Rasterize the rect directly using a pixel-center rule so fractional edges do not expand into
    // adjacent pixels.
    let dx0 = rect.min_x() * transform.sx + transform.tx;
    let dx1 = rect.max_x() * transform.sx + transform.tx;
    let dy0 = rect.min_y() * transform.sy + transform.ty;
    let dy1 = rect.max_y() * transform.sy + transform.ty;
    if !dx0.is_finite() || !dx1.is_finite() || !dy0.is_finite() || !dy1.is_finite() {
      return None;
    }

    let min_x = dx0.min(dx1);
    let max_x = dx0.max(dx1);
    let min_y = dy0.min(dy1);
    let max_y = dy0.max(dy1);

    let mut mask = Mask::new(self.width(), self.height())?;
    mask.data_mut().fill(0);

    let w_i64 = self.width() as i64;
    let h_i64 = self.height() as i64;

    let x0 = (min_x - 0.5).ceil() as i64;
    let y0 = (min_y - 0.5).ceil() as i64;
    // Match tiny-skia's non-AA rasterizer, which treats pixel centers on the max edge as inside.
    // (Equivalently, it behaves like a `[min, max]` interval in pixel-center space.)
    let x1 = (max_x - 0.5).floor() as i64 + 1;
    let y1 = (max_y - 0.5).floor() as i64 + 1;

    let x0 = x0.clamp(0, w_i64);
    let y0 = y0.clamp(0, h_i64);
    let x1 = x1.clamp(0, w_i64);
    let y1 = y1.clamp(0, h_i64);
    if x1 <= x0 || y1 <= y0 {
      return Some(mask);
    }

    let stride = self.width() as usize;
    let data = mask.data_mut();
    let x0 = x0 as usize;
    let x1 = x1 as usize;
    for y in y0 as usize..y1 as usize {
      let start = y * stride + x0;
      data[start..start + (x1 - x0)].fill(255);
    }

    Some(mask)
  }

  fn build_clip_mask_slow_path(&self, rect: Rect, radii: BorderRadii) -> Option<Mask> {
    let mut mask = Mask::new(self.width(), self.height())?;
    mask.data_mut().fill(0);

    let path = self.build_rounded_rect_path(rect, radii)?;
    let transform = self.current_state.transform;
    // Match Skia's hard-edge `clipRect` behaviour for axis-aligned rectangular clips while still
    // anti-aliasing rounded corners and non-axis-aligned transforms.
    let anti_alias = !radii.is_zero() || transform.kx.abs() > 1e-6 || transform.ky.abs() > 1e-6;
    let bounds = Self::transform_rect_aabb(rect, transform);
    let needs_scratch = bounds.min_x() < 0.0
      || bounds.min_y() < 0.0
      || bounds.max_x() > self.width() as f32
      || bounds.max_y() > self.height() as f32;

    if !needs_scratch {
      mask.fill_path(&path, FillRule::Winding, anti_alias, transform);
      return Some(mask);
    }

    // When tiling, the clip path can extend far beyond the tile + halo pixmap. tiny-skia clips
    // paths to the raster surface before converting them into a coverage mask. The resulting mask
    // can differ compared to rasterizing the same clip on a larger surface, which shows up as
    // seams when the clipped content crosses tile boundaries.
    //
    // Fix this by rasterizing the clip into a scratch mask that fully contains the transformed
    // clip bounds, then copying the overlapping region back into the tile-sized mask.
    const CLIP_MASK_SCRATCH_MARGIN_PX: i64 = 2;

    if bounds.width() <= 0.0
      || bounds.height() <= 0.0
      || !bounds.x().is_finite()
      || !bounds.y().is_finite()
      || !bounds.width().is_finite()
      || !bounds.height().is_finite()
    {
      return Some(mask);
    }

    let mut x0 = bounds.min_x().floor() as i64;
    let mut y0 = bounds.min_y().floor() as i64;
    let mut x1 = bounds.max_x().ceil() as i64;
    let mut y1 = bounds.max_y().ceil() as i64;
    x0 = x0.saturating_sub(CLIP_MASK_SCRATCH_MARGIN_PX);
    y0 = y0.saturating_sub(CLIP_MASK_SCRATCH_MARGIN_PX);
    x1 = x1.saturating_add(CLIP_MASK_SCRATCH_MARGIN_PX);
    y1 = y1.saturating_add(CLIP_MASK_SCRATCH_MARGIN_PX);

    let scratch_w_i64 = x1 - x0;
    let scratch_h_i64 = y1 - y0;
    let Ok(scratch_w) = u32::try_from(scratch_w_i64) else {
      mask.fill_path(&path, FillRule::Winding, anti_alias, transform);
      return Some(mask);
    };
    let Ok(scratch_h) = u32::try_from(scratch_h_i64) else {
      mask.fill_path(&path, FillRule::Winding, anti_alias, transform);
      return Some(mask);
    };
    if scratch_w == 0 || scratch_h == 0 {
      return Some(mask);
    }

    let dest_w = self.width() as i64;
    let dest_h = self.height() as i64;
    let inter_x0 = x0.max(0);
    let inter_y0 = y0.max(0);
    let inter_x1 = x1.min(dest_w);
    let inter_y1 = y1.min(dest_h);
    if inter_x1 <= inter_x0 || inter_y1 <= inter_y0 {
      return Some(mask);
    }

    let mut scratch = ROUNDED_RECT_PAD_SCRATCH.with(|cell| std::mem::take(&mut *cell.borrow_mut()));
    let mut tmp = match scratch.mask.take() {
      Some(existing) if existing.width() == scratch_w && existing.height() == scratch_h => existing,
      _ => match Mask::new(scratch_w, scratch_h) {
        Some(m) => m,
        None => {
          mask.fill_path(&path, FillRule::Winding, anti_alias, transform);
          scratch.mask = None;
          ROUNDED_RECT_PAD_SCRATCH.with(|cell| {
            *cell.borrow_mut() = scratch;
          });
          return Some(mask);
        }
      },
    };
    tmp.data_mut().fill(0);
    tmp.fill_path(
      &path,
      FillRule::Winding,
      anti_alias,
      transform.post_translate(-(x0 as f32), -(y0 as f32)),
    );

    {
      let src = tmp.data();
      let dst = mask.data_mut();
      let src_stride = scratch_w as usize;
      let dst_stride = self.width() as usize;
      let copy_w = (inter_x1 - inter_x0) as usize;
      let copy_h = (inter_y1 - inter_y0) as usize;
      let src_x = (inter_x0 - x0) as usize;
      let dst_x = inter_x0 as usize;
      let src_y = (inter_y0 - y0) as usize;
      let dst_y = inter_y0 as usize;
      for row in 0..copy_h {
        let src_off = (src_y + row) * src_stride + src_x;
        let dst_off = (dst_y + row) * dst_stride + dst_x;
        dst[dst_off..dst_off + copy_w].copy_from_slice(&src[src_off..src_off + copy_w]);
      }
    }

    scratch.mask = Some(tmp);
    ROUNDED_RECT_PAD_SCRATCH.with(|cell| {
      *cell.borrow_mut() = scratch;
    });
    Some(mask)
  }

  /// Builds a path for a rounded rectangle
  fn build_rounded_rect_path(&self, rect: Rect, radii: BorderRadii) -> Option<tiny_skia::Path> {
    crate::paint::rasterize::build_rounded_rect_path(
      rect.x(),
      rect.y(),
      rect.width(),
      rect.height(),
      &radii,
    )
  }

  /// Builds a path for a circle using cubic bezier approximation
  fn build_circle_path(&self, center: Point, radius: f32) -> Option<tiny_skia::Path> {
    // Use the cubic bezier approximation for a circle
    // Magic number for circle approximation: 4/3 * tan(π/8) ≈ 0.5522847498
    const KAPPA: f32 = 0.552_284_8;
    let k = radius * KAPPA;

    let mut pb = PathBuilder::new();

    // Start at top
    pb.move_to(center.x, center.y - radius);

    // Top-right quadrant
    pb.cubic_to(
      center.x + k,
      center.y - radius,
      center.x + radius,
      center.y - k,
      center.x + radius,
      center.y,
    );

    // Bottom-right quadrant
    pb.cubic_to(
      center.x + radius,
      center.y + k,
      center.x + k,
      center.y + radius,
      center.x,
      center.y + radius,
    );

    // Bottom-left quadrant
    pb.cubic_to(
      center.x - k,
      center.y + radius,
      center.x - radius,
      center.y + k,
      center.x - radius,
      center.y,
    );

    // Top-left quadrant
    pb.cubic_to(
      center.x - radius,
      center.y - k,
      center.x - k,
      center.y - radius,
      center.x,
      center.y - radius,
    );

    pb.close();
    pb.finish()
  }
}

/// Convert a non-isolated group surface into an isolated source image.
///
/// For a non-isolated compositing group, the group surface is initialized with the group backdrop
/// (the already-painted pixels behind the group) and group content is rendered on top. This
/// produces a surface `out` where:
///
/// ```text
/// out = src ⊕ backdrop
/// ```
///
/// where `⊕` is source-over compositing and `src` is the group's "computed element" (its color and
/// alpha contribution excluding the backdrop).
///
/// This helper extracts `src` from `out` and `backdrop` in-place so callers can composite `src`
/// once (with the group's mix-blend-mode / opacity / filters) without the backdrop contributing
/// twice (CSS Compositing & Blending group invariance).
pub(crate) fn uncomposite_layer_source_over_backdrop(
  layer: &mut Pixmap,
  backdrop: PixmapRef<'_>,
  origin: (i32, i32),
  source_alpha: Option<(&Pixmap, (i32, i32))>,
) -> RenderResult<()> {
  #[inline]
  fn mul_div_255_round_u16(a: u16, b: u16) -> u16 {
    // Exact rounding division by 255 for u8 products.
    //
    // This implements `round((a * b) / 255)` for `a, b ∈ [0, 255]` using only
    // multiplies/shifts. The intermediate range fits in u32.
    let prod = (a as u32) * (b as u32);
    (((prod + 128) * 257) >> 16) as u16
  }

  let layer_w = layer.width() as i32;
  let layer_h = layer.height() as i32;
  if layer_w <= 0 || layer_h <= 0 {
    return Ok(());
  }

  let backdrop_w = backdrop.width() as i32;
  let backdrop_h = backdrop.height() as i32;
  if backdrop_w <= 0 || backdrop_h <= 0 {
    return Ok(());
  }

  let origin_x = origin.0;
  let origin_y = origin.1;

  let mut lx0 = 0i32;
  let mut ly0 = 0i32;
  let mut lx1 = layer_w;
  let mut ly1 = layer_h;

  if origin_x < 0 {
    lx0 = lx0.max(-origin_x);
  }
  if origin_y < 0 {
    ly0 = ly0.max(-origin_y);
  }
  if origin_x + layer_w > backdrop_w {
    lx1 = lx1.min(backdrop_w - origin_x);
  }
  if origin_y + layer_h > backdrop_h {
    ly1 = ly1.min(backdrop_h - origin_y);
  }

  if lx0 >= lx1 || ly0 >= ly1 {
    return Ok(());
  }

  let (alpha_pixels, alpha_stride, alpha_w, alpha_h, alpha_origin_x, alpha_origin_y) =
    if let Some((alpha_pixmap, alpha_origin)) = source_alpha {
      (
        Some(alpha_pixmap.pixels()),
        alpha_pixmap.width() as usize,
        alpha_pixmap.width() as i32,
        alpha_pixmap.height() as i32,
        alpha_origin.0,
        alpha_origin.1,
      )
    } else {
      (None, 0, 0, 0, 0, 0)
    };

  #[inline]
  fn ceil_div(numer: u32, denom: u32) -> u16 {
    if denom == 0 {
      return 0;
    }
    ((numer + denom - 1) / denom).min(255) as u16
  }

  let layer_stride = layer.width() as usize;
  let backdrop_stride = backdrop.width() as usize;
  let layer_pixels = layer.pixels_mut();
  let backdrop_pixels = backdrop.pixels();
  let width = (lx1 - lx0) as usize;

  let mut deadline_counter = 0usize;
  for ly in ly0..ly1 {
    check_active_periodic(&mut deadline_counter, 32, RenderStage::Paint)?;

    let layer_row = ly as usize * layer_stride + lx0 as usize;
    let backdrop_row = (ly + origin_y) as usize * backdrop_stride + (lx0 + origin_x) as usize;

    for offset in 0..width {
      let out_px = &mut layer_pixels[layer_row + offset];
      let back_px = backdrop_pixels[backdrop_row + offset];

      let ba = back_px.alpha() as u16;
      let oa = out_px.alpha() as u16;

      let sa = if let Some(alpha_pixels) = alpha_pixels {
        let ax = (lx0.saturating_add(offset as i32)).saturating_add(alpha_origin_x);
        let ay = ly.saturating_add(alpha_origin_y);
        if ax < 0 || ay < 0 || ax >= alpha_w || ay >= alpha_h {
          0
        } else {
          alpha_pixels[ay as usize * alpha_stride + ax as usize].alpha() as u16
        }
      } else if ba < 255 {
        let num = oa.saturating_sub(ba) as u32;
        if num == 0 {
          0
        } else {
          let denom = (255u32).saturating_sub(ba as u32);
          (((num * 255 + denom / 2) / denom).min(255)) as u16
        }
      } else {
        // When the backdrop is fully opaque, `oa` is always 255 under source-over compositing, so
        // alpha alone cannot recover the source alpha. Choose the smallest alpha that yields a
        // valid premultiplied source color for all channels.
        let (br, bg, bb) = (
          back_px.red() as u16,
          back_px.green() as u16,
          back_px.blue() as u16,
        );
        let (or, og, ob) = (
          out_px.red() as u16,
          out_px.green() as u16,
          out_px.blue() as u16,
        );

        let mut bound = 0u16;
        for (b, o) in [(br, or), (bg, og), (bb, ob)] {
          if b > 0 && b > o {
            bound = bound.max(ceil_div(u32::from(b - o) * 255, u32::from(b)));
          }
          if b < 255 && o > b {
            bound = bound.max(ceil_div(u32::from(o - b) * 255, u32::from(255 - b)));
          }
        }
        bound
      };

      if sa == 0 {
        *out_px = PremultipliedColorU8::TRANSPARENT;
        continue;
      }

      let inv_sa = 255u16.saturating_sub(sa);
      let sr =
        (out_px.red() as u16).saturating_sub(mul_div_255_round_u16(back_px.red() as u16, inv_sa));
      let sg = (out_px.green() as u16)
        .saturating_sub(mul_div_255_round_u16(back_px.green() as u16, inv_sa));
      let sb =
        (out_px.blue() as u16).saturating_sub(mul_div_255_round_u16(back_px.blue() as u16, inv_sa));

      let sr = sr.min(sa);
      let sg = sg.min(sa);
      let sb = sb.min(sa);

      *out_px = PremultipliedColorU8::from_rgba(sr as u8, sg as u8, sb as u8, sa as u8)
        .unwrap_or(PremultipliedColorU8::TRANSPARENT);
    }
  }

  Ok(())
}

pub(crate) fn composite_layer_into_pixmap(
  target: &mut Pixmap,
  layer: &Pixmap,
  opacity: f32,
  blend_mode: SkiaBlendMode,
  origin: (i32, i32),
  clip: Option<&Mask>,
) {
  let mut target = target.as_mut();
  composite_layer_into_pixmap_with_clip_rect(&mut target, layer, opacity, blend_mode, origin, clip, None)
}

pub(crate) fn composite_layer_into_pixmap_mut(
  target: &mut PixmapMut<'_>,
  layer: &Pixmap,
  opacity: f32,
  blend_mode: SkiaBlendMode,
  origin: (i32, i32),
  clip: Option<&Mask>,
) {
  composite_layer_into_pixmap_with_clip_rect(target, layer, opacity, blend_mode, origin, clip, None)
}

pub(crate) fn composite_layer_into_pixmap_with_clip_rect(
  target: &mut PixmapMut<'_>,
  layer: &Pixmap,
  opacity: f32,
  blend_mode: SkiaBlendMode,
  origin: (i32, i32),
  clip: Option<&Mask>,
  clip_rect: Option<Rect>,
) {
  fn composite_source_over(
    target: &mut PixmapMut<'_>,
    layer: &Pixmap,
    opacity: f32,
    origin: (i32, i32),
    clip: Option<&Mask>,
    clip_rect: Option<Rect>,
  ) {
    // Match Chrome/Skia: evaluate `opacity` using unbiased 8-bit math.
    //
    // Importantly, we do *not* apply ordered dithering here. Chrome's `opacity` compositing (e.g.
    // `opacity: 0.3` over white) produces uniform pixels, whereas ordered dither introduces a
    // checkerboard ±1 pattern that dominates strict page diffs.
    //
    // We do the blend in a 0..=256 alpha domain (instead of 0..=255) so 0.5 opacity maps to an
    // exact 128/256 rather than 128/255, avoiding systematic darkening bias (e.g. 50% red over
    // white should yield 128, not 127).
    #[inline]
    fn alpha255_to_256(a: u8) -> u16 {
      let a = a as u16;
      a + (a >> 7)
    }
    #[inline]
    fn mul_div_256_round_u16(a: u16, b: u16) -> u16 {
      // Computes `round((a * b) / 256)` for `a,b ∈ [0, 256]`.
      (((a as u32) * (b as u32) + 128) >> 8) as u16
    }

    let opacity_256 = (opacity * 256.0).round().clamp(0.0, 256.0) as u16;
    if opacity_256 == 0 {
      return;
    }

    let dst_w = target.width() as i32;
    let dst_h = target.height() as i32;
    if dst_w <= 0 || dst_h <= 0 {
      return;
    }

    let src_w = layer.width() as i32;
    let src_h = layer.height() as i32;
    if src_w <= 0 || src_h <= 0 {
      return;
    }

    let mut dst_x0 = origin.0.max(0);
    let mut dst_y0 = origin.1.max(0);
    let mut dst_x1 = origin.0.saturating_add(src_w).min(dst_w);
    let mut dst_y1 = origin.1.saturating_add(src_h).min(dst_h);
    if let Some(clip_rect) = clip_rect {
      // `clip_rect` is specified in device space and uses a non-AA pixel-center inclusion rule,
      // matching the mask construction in `materialize_rect_clip_mask_if_needed`.
      if clip_rect.width() <= 0.0 || clip_rect.height() <= 0.0 {
        return;
      }
      let min_x = clip_rect.min_x();
      let max_x = clip_rect.max_x();
      let min_y = clip_rect.min_y();
      let max_y = clip_rect.max_y();
      if min_x.is_finite() && max_x.is_finite() && min_y.is_finite() && max_y.is_finite() {
        let cx0 = (min_x - 0.5).ceil() as i64;
        let cy0 = (min_y - 0.5).ceil() as i64;
        let cx1 = (max_x - 0.5).floor() as i64 + 1;
        let cy1 = (max_y - 0.5).floor() as i64 + 1;
        let w = dst_w as i64;
        let h = dst_h as i64;
        let cx0 = cx0.clamp(0, w) as i32;
        let cy0 = cy0.clamp(0, h) as i32;
        let cx1 = cx1.clamp(0, w) as i32;
        let cy1 = cy1.clamp(0, h) as i32;
        dst_x0 = dst_x0.max(cx0);
        dst_y0 = dst_y0.max(cy0);
        dst_x1 = dst_x1.min(cx1);
        dst_y1 = dst_y1.min(cy1);
      }
    }
    if dst_x0 >= dst_x1 || dst_y0 >= dst_y1 {
      return;
    }

    let src_x0 = (dst_x0 - origin.0) as usize;
    let src_y0 = (dst_y0 - origin.1) as usize;
    let copy_w = (dst_x1 - dst_x0) as usize;
    let copy_h = (dst_y1 - dst_y0) as usize;
    if copy_w == 0 || copy_h == 0 {
      return;
    }

    let (clip_data, clip_stride) = match clip {
      Some(mask) => (Some(mask.data()), mask.width() as usize),
      None => (None, 0),
    };

    let dst_stride = target.width() as usize;
    let src_stride = layer.width() as usize;
    let dst_pixels = target.pixels_mut();
    let src_pixels = layer.pixels();

    for row in 0..copy_h {
      let dst_y = dst_y0 as usize + row;
      let src_y = src_y0 + row;

      let dst_row_start = dst_y * dst_stride + dst_x0 as usize;
      let src_row_start = src_y * src_stride + src_x0;
      let dst_row = &mut dst_pixels[dst_row_start..dst_row_start + copy_w];
      let src_row = &src_pixels[src_row_start..src_row_start + copy_w];

      for (col, (dst_px, src_px)) in dst_row.iter_mut().zip(src_row.iter()).enumerate() {
        let mut scale_256 = opacity_256;
        if let Some(clip_data) = clip_data {
          let m = clip_data[dst_y * clip_stride + dst_x0 as usize + col];
          if m == 0 {
            continue;
          }
          if m != 255 {
            scale_256 = mul_div_256_round_u16(scale_256, alpha255_to_256(m));
            if scale_256 == 0 {
              continue;
            }
          }
        }

        // Scale source premultiplied pixels by layer opacity / clip coverage.
        let sr = mul_div_256_round_u16(src_px.red() as u16, scale_256);
        let sg = mul_div_256_round_u16(src_px.green() as u16, scale_256);
        let sb = mul_div_256_round_u16(src_px.blue() as u16, scale_256);
        let sa_256 = mul_div_256_round_u16(alpha255_to_256(src_px.alpha()), scale_256);
        if sa_256 == 0 {
          continue;
        }

        let inv_sa_256 = 256u16.saturating_sub(sa_256);
        let dr = dst_px.red() as u16;
        let dg = dst_px.green() as u16;
        let db = dst_px.blue() as u16;
        let da_256 = alpha255_to_256(dst_px.alpha());

        let out_a_256 = sa_256 + mul_div_256_round_u16(da_256, inv_sa_256);
        let out_r = sr + mul_div_256_round_u16(dr, inv_sa_256);
        let out_g = sg + mul_div_256_round_u16(dg, inv_sa_256);
        let out_b = sb + mul_div_256_round_u16(db, inv_sa_256);

        // Map 0..=256 alpha back to 0..=255.
        let out_a_u8 = if out_a_256 >= 256 {
          255
        } else {
          out_a_256 as u8
        };
        let clamp = out_a_u8 as u16;
        let out_r_u8 = out_r.min(clamp).min(255) as u8;
        let out_g_u8 = out_g.min(clamp).min(255) as u8;
        let out_b_u8 = out_b.min(clamp).min(255) as u8;
        *dst_px = PremultipliedColorU8::from_rgba(out_r_u8, out_g_u8, out_b_u8, out_a_u8)
          .unwrap_or(PremultipliedColorU8::TRANSPARENT);
      }
    }
  }

  let opacity = opacity.clamp(0.0, 1.0);
  if opacity <= 0.0 || !opacity.is_finite() {
    return;
  }

  if blend_mode == SkiaBlendMode::SourceOver {
    composite_source_over(target, layer, opacity, origin, clip, clip_rect);
    return;
  }

  // tiny-skia's `draw_pixmap` only supports mask clipping. When callers have a rectangular clip
  // bounds but no materialized mask, build a temporary mask so non-source-over blend modes still
  // respect the clip.
  let mut rect_mask_storage: Option<Mask> = None;
  let clip = clip.or_else(|| {
    let rect = clip_rect?;
    if rect.width() <= 0.0 || rect.height() <= 0.0 {
      return None;
    }
    let w = target.width();
    let h = target.height();
    if w == 0 || h == 0 {
      return None;
    }
    let mut mask = Mask::new(w, h)?;
    mask.data_mut().fill(0);

    let w_i64 = w as i64;
    let h_i64 = h as i64;
    let min_x = rect.min_x();
    let max_x = rect.max_x();
    let min_y = rect.min_y();
    let max_y = rect.max_y();
    if !min_x.is_finite() || !max_x.is_finite() || !min_y.is_finite() || !max_y.is_finite() {
      return None;
    }
    let x0 = (min_x - 0.5).ceil() as i64;
    let y0 = (min_y - 0.5).ceil() as i64;
    let x1 = (max_x - 0.5).floor() as i64 + 1;
    let y1 = (max_y - 0.5).floor() as i64 + 1;
    let x0 = x0.clamp(0, w_i64) as usize;
    let y0 = y0.clamp(0, h_i64) as usize;
    let x1 = x1.clamp(0, w_i64) as usize;
    let y1 = y1.clamp(0, h_i64) as usize;
    if x1 <= x0 || y1 <= y0 {
      return None;
    }
    let stride = w as usize;
    for y in y0..y1 {
      let start = y * stride + x0;
      mask.data_mut()[start..start + (x1 - x0)].fill(255);
    }
    rect_mask_storage = Some(mask);
    rect_mask_storage.as_ref()
  });

  let mut paint = PixmapPaint::default();
  paint.opacity = opacity;
  paint.blend_mode = blend_mode;
  // `push_layer[_bounded]` keeps the current transform active while painting into the offscreen
  // pixmap, so the layer is already rasterized in the parent's device/pixmap coordinate space.
  // When compositing back, we must not re-apply the transform, otherwise non-identity transforms
  // (e.g. translated tile painters / bounded layers) would double-transform the content.
  let transform = Transform::identity();

  if paint.blend_mode == SkiaBlendMode::Plus {
    draw_pixmap_with_plus_blend_mut(
      target,
      origin.0,
      origin.1,
      layer.as_ref(),
      paint.opacity,
      paint.quality,
      transform,
      clip,
    );
  } else {
    target.draw_pixmap(origin.0, origin.1, layer.as_ref(), &paint, transform, clip);
  }
}

pub(crate) fn draw_pixmap_with_plus_blend(
  target: &mut Pixmap,
  x: i32,
  y: i32,
  src: PixmapRef<'_>,
  opacity: f32,
  quality: FilterQuality,
  transform: Transform,
  clip: Option<&Mask>,
) {
  let mut target_mut = target.as_mut();
  draw_pixmap_with_plus_blend_mut(
    &mut target_mut,
    x,
    y,
    src,
    opacity,
    quality,
    transform,
    clip,
  );
}

pub(crate) fn draw_pixmap_with_plus_blend_mut(
  target: &mut PixmapMut<'_>,
  x: i32,
  y: i32,
  src: PixmapRef<'_>,
  opacity: f32,
  quality: FilterQuality,
  transform: Transform,
  clip: Option<&Mask>,
) {
  let opacity = opacity.clamp(0.0, 1.0);
  if opacity <= 0.0 {
    return;
  }

  let src_w = src.width();
  let src_h = src.height();
  if src_w == 0 || src_h == 0 {
    return;
  }

  // Fast path: integer translation-only draws can just saturating-add premultiplied pixels.
  if clip.is_none()
    && (transform.sx - 1.0).abs() < 1e-6
    && (transform.sy - 1.0).abs() < 1e-6
    && transform.kx.abs() < 1e-6
    && transform.ky.abs() < 1e-6
  {
    let tx = transform.tx + x as f32;
    let ty = transform.ty + y as f32;
    if tx.is_finite() && ty.is_finite() {
      let tx_rounded = tx.round();
      let ty_rounded = ty.round();
      if (tx - tx_rounded).abs() < 1e-6 && (ty - ty_rounded).abs() < 1e-6 {
        let dst_x = tx_rounded as i32;
        let dst_y = ty_rounded as i32;
        let dst_w = target.width() as i32;
        let dst_h = target.height() as i32;
        let src_w_i = src_w as i32;
        let src_h_i = src_h as i32;

        let dst_start_x = dst_x.max(0);
        let dst_start_y = dst_y.max(0);
        let src_start_x = (-dst_x).max(0);
        let src_start_y = (-dst_y).max(0);
        let copy_w = (src_w_i - src_start_x).min(dst_w - dst_start_x);
        let copy_h = (src_h_i - src_start_y).min(dst_h - dst_start_y);
        if copy_w <= 0 || copy_h <= 0 {
          return;
        }

        let opaque = opacity >= 1.0 - 1e-6;
        let opacity_u16 = if opaque {
          256
        } else {
          (opacity * 256.0).round().clamp(0.0, 256.0) as u16
        };

        let dst_stride = target.width() as usize * 4;
        let src_stride = src_w as usize * 4;
        let dst_data = target.data_mut();
        let src_data = src.data();
        let row_bytes = copy_w as usize * 4;
        for row in 0..copy_h as usize {
          let dst_off = (dst_start_y as usize + row) * dst_stride + dst_start_x as usize * 4;
          let src_off = (src_start_y as usize + row) * src_stride + src_start_x as usize * 4;
          let dst_row = &mut dst_data[dst_off..dst_off + row_bytes];
          let src_row = &src_data[src_off..src_off + row_bytes];
          if opaque {
            for (dst_byte, src_byte) in dst_row.iter_mut().zip(src_row.iter()) {
              *dst_byte = dst_byte.saturating_add(*src_byte);
            }
          } else {
            for (dst_byte, src_byte) in dst_row.iter_mut().zip(src_row.iter()) {
              let scaled = ((*src_byte as u16 * opacity_u16 + 128) >> 8) as u8;
              *dst_byte = dst_byte.saturating_add(scaled);
            }
          }
        }
        return;
      }
    }
  }

  // Slow path: rasterize into a transparent scratch pixmap using SourceOver (honors transforms
  // and clip masks), then saturating-add the result into the destination.
  let Some(mut scratch) = new_pixmap(target.width(), target.height()) else {
    let mut paint = PixmapPaint::default();
    paint.opacity = opacity;
    paint.quality = quality;
    paint.blend_mode = SkiaBlendMode::SourceOver;
    target.draw_pixmap(x, y, src, &paint, transform, clip);
    return;
  };

  let mut paint = PixmapPaint::default();
  paint.opacity = opacity;
  paint.quality = quality;
  paint.blend_mode = SkiaBlendMode::SourceOver;
  scratch.draw_pixmap(x, y, src, &paint, transform, clip);

  let mut combined = transform;
  combined.tx += x as f32;
  combined.ty += y as f32;
  let w = src_w as f32;
  let h = src_h as f32;
  let map = |x: f32, y: f32| -> (f32, f32) {
    (
      x * combined.sx + y * combined.kx + combined.tx,
      x * combined.ky + y * combined.sy + combined.ty,
    )
  };
  let (x0, y0) = map(0.0, 0.0);
  let (x1, y1) = map(w, 0.0);
  let (x2, y2) = map(w, h);
  let (x3, y3) = map(0.0, h);
  let mut min_x = x0.min(x1).min(x2).min(x3);
  let mut max_x = x0.max(x1).max(x2).max(x3);
  let mut min_y = y0.min(y1).min(y2).min(y3);
  let mut max_y = y0.max(y1).max(y2).max(y3);
  if !(min_x.is_finite()
    && max_x.is_finite()
    && min_y.is_finite()
    && max_y.is_finite()
    && min_x <= max_x
    && min_y <= max_y)
  {
    min_x = 0.0;
    min_y = 0.0;
    max_x = target.width() as f32;
    max_y = target.height() as f32;
  }

  // Account for bilinear filtering bleeding slightly outside of the source quad.
  let pad = match quality {
    FilterQuality::Nearest => 0,
    _ => 1,
  };
  let left = (min_x.floor() as i32).saturating_sub(pad);
  let top = (min_y.floor() as i32).saturating_sub(pad);
  let right = (max_x.ceil() as i32).saturating_add(pad);
  let bottom = (max_y.ceil() as i32).saturating_add(pad);

  let dst_w = target.width() as i32;
  let dst_h = target.height() as i32;
  let left = left.clamp(0, dst_w);
  let top = top.clamp(0, dst_h);
  let right = right.clamp(0, dst_w);
  let bottom = bottom.clamp(0, dst_h);
  if right <= left || bottom <= top {
    return;
  }

  let dst_stride = target.width() as usize * 4;
  let dst_data = target.data_mut();
  let src_data = scratch.data();
  let row_bytes = (right - left) as usize * 4;
  for row in top..bottom {
    let off = row as usize * dst_stride + left as usize * 4;
    let dst_row = &mut dst_data[off..off + row_bytes];
    let src_row = &src_data[off..off + row_bytes];
    for (dst_byte, src_byte) in dst_row.iter_mut().zip(src_row.iter()) {
      *dst_byte = dst_byte.saturating_add(*src_byte);
    }
  }
}

fn combine_masks(into: &mut Mask, existing: &Mask) -> RenderResult<()> {
  if into.width() != existing.width() || into.height() != existing.height() {
    return Ok(());
  }

  #[inline]
  fn mul_div_255_round(value: u8, alpha: u8) -> u8 {
    // Match `tiny_skia::Pixmap::apply_mask` rounding behavior.
    let prod = value as u16 * alpha as u16;
    ((prod + 255) >> 8) as u8
  }

  check_active(RenderStage::Paint)?;
  let width = into.width() as usize;
  let height = into.height() as usize;
  let dst = into.data_mut();
  let src = existing.data();
  let mut deadline_counter = 0usize;
  for row in 0..height {
    check_active_periodic(&mut deadline_counter, 32, RenderStage::Paint)?;
    let offset = row * width;
    let dst_row = &mut dst[offset..offset + width];
    let src_row = &src[offset..offset + width];
    for (dst, src) in dst_row.iter_mut().zip(src_row.iter()) {
      *dst = mul_div_255_round(*dst, *src);
    }
  }
  Ok(())
}

fn scissor_mask_to_rect(mask: &mut Mask, rect: Rect) -> RenderResult<()> {
  let w = mask.width();
  let h = mask.height();
  if w == 0 || h == 0 {
    return Ok(());
  }
  if rect.width() <= 0.0
    || rect.height() <= 0.0
    || !rect.x().is_finite()
    || !rect.y().is_finite()
    || !rect.width().is_finite()
    || !rect.height().is_finite()
  {
    mask.data_mut().fill(0);
    return Ok(());
  }

  // Use a pixel-center rule consistent with `build_clip_mask_fast_rect`.
  let x0 = (rect.min_x() - 0.5).ceil() as i64;
  let y0 = (rect.min_y() - 0.5).ceil() as i64;
  let x1 = (rect.max_x() - 0.5).floor() as i64 + 1;
  let y1 = (rect.max_y() - 0.5).floor() as i64 + 1;

  let w_i64 = w as i64;
  let h_i64 = h as i64;
  let x0 = x0.clamp(0, w_i64);
  let y0 = y0.clamp(0, h_i64);
  let x1 = x1.clamp(0, w_i64);
  let y1 = y1.clamp(0, h_i64);
  if x1 <= x0 || y1 <= y0 {
    mask.data_mut().fill(0);
    return Ok(());
  }

  check_active(RenderStage::Paint)?;
  let stride = w as usize;
  let data = mask.data_mut();

  let y0 = y0 as usize;
  let y1 = y1 as usize;
  let x0 = x0 as usize;
  let x1 = x1 as usize;

  // Clear rows above/below the intersection. Use a single contiguous fill per region so this stays
  // on the hot `memset` path.
  if y0 > 0 {
    data[..y0 * stride].fill(0);
  }
  if y1 < h as usize {
    data[y1 * stride..].fill(0);
  }

  // Clear left/right segments where the clip rect is implicitly zero.
  let mut deadline_counter = 0usize;
  for row in y0..y1 {
    check_active_periodic(&mut deadline_counter, 32, RenderStage::Paint)?;
    let off = row * stride;
    if x0 > 0 {
      data[off..off + x0].fill(0);
    }
    if x1 < stride {
      data[off + x1..off + stride].fill(0);
    }
  }

  Ok(())
}

pub(crate) fn crop_mask(
  mask: &Mask,
  origin_x: u32,
  origin_y: u32,
  width: u32,
  height: u32,
) -> RenderResult<Option<Mask>> {
  if width == 0 || height == 0 {
    return Ok(None);
  }

  let mask_width = mask.width();
  let mask_height = mask.height();
  if origin_x >= mask_width || origin_y >= mask_height {
    return Ok(None);
  }

  let crop_w = width.min(mask_width.saturating_sub(origin_x));
  let crop_h = height.min(mask_height.saturating_sub(origin_y));
  if crop_w == 0 || crop_h == 0 {
    return Ok(None);
  }

  check_active(RenderStage::Paint)?;
  let Some(mut out) = Mask::new(crop_w, crop_h) else {
    return Ok(None);
  };
  let src = mask.data();
  let dst = out.data_mut();
  let src_stride = mask_width as usize;
  let dst_stride = crop_w as usize;
  let mut deadline_counter = 0usize;
  for row in 0..crop_h as usize {
    check_active_periodic(&mut deadline_counter, 32, RenderStage::Paint)?;
    let src_idx = (origin_y as usize + row) * src_stride + origin_x as usize;
    let dst_idx = row * dst_stride;
    dst[dst_idx..dst_idx + dst_stride].copy_from_slice(&src[src_idx..src_idx + dst_stride]);
  }

  Ok(Some(out))
}

pub(crate) fn crop_mask_i32(
  mask: &Mask,
  origin_x: i32,
  origin_y: i32,
  width: u32,
  height: u32,
) -> RenderResult<Option<Mask>> {
  if width == 0 || height == 0 {
    return Ok(None);
  }

  let mask_width = mask.width();
  let mask_height = mask.height();
  if mask_width == 0 || mask_height == 0 {
    return Ok(None);
  }

  // Fast path: fully contained non-negative region can reuse the unsigned crop implementation.
  if origin_x >= 0 && origin_y >= 0 {
    let ox = origin_x as u32;
    let oy = origin_y as u32;
    let max_x = ox.saturating_add(width);
    let max_y = oy.saturating_add(height);
    if ox < mask_width && oy < mask_height && max_x <= mask_width && max_y <= mask_height {
      return crop_mask(mask, ox, oy, width, height);
    }
  }

  let dst_x0 = origin_x as i64;
  let dst_y0 = origin_y as i64;
  let dst_x1 = dst_x0 + width as i64;
  let dst_y1 = dst_y0 + height as i64;

  let src_x0 = 0i64;
  let src_y0 = 0i64;
  let src_x1 = mask_width as i64;
  let src_y1 = mask_height as i64;

  let inter_x0 = dst_x0.max(src_x0);
  let inter_y0 = dst_y0.max(src_y0);
  let inter_x1 = dst_x1.min(src_x1);
  let inter_y1 = dst_y1.min(src_y1);
  if inter_x1 <= inter_x0 || inter_y1 <= inter_y0 {
    return Ok(None);
  }
  check_active(RenderStage::Paint)?;
  let Some(mut out) = Mask::new(width, height) else {
    return Ok(None);
  };

  // Out-of-bounds pixels in the source mask are treated as 0 (fully clipped).
  out.data_mut().fill(0);

  let src = mask.data();
  let dst = out.data_mut();
  let src_stride = mask_width as usize;
  let dst_stride = width as usize;

  let src_x = inter_x0 as usize;
  let src_y = inter_y0 as usize;
  let dst_x = (inter_x0 - dst_x0) as usize;
  let dst_y = (inter_y0 - dst_y0) as usize;
  let copy_w = (inter_x1 - inter_x0) as usize;
  let copy_h = (inter_y1 - inter_y0) as usize;

  let mut deadline_counter = 0usize;
  for row in 0..copy_h {
    check_active_periodic(&mut deadline_counter, 32, RenderStage::Paint)?;
    let src_idx = (src_y + row) * src_stride + src_x;
    let dst_idx = (dst_y + row) * dst_stride + dst_x;
    dst[dst_idx..dst_idx + copy_w].copy_from_slice(&src[src_idx..src_idx + copy_w]);
  }

  Ok(Some(out))
}

/// Applies a mask that is positioned in a larger coordinate space.
///
/// The provided `mask` is interpreted as covering the rectangle
/// `[mask_origin.x, mask_origin.y]..[mask_origin.x + mask.width, mask_origin.y + mask.height]`
/// in the same coordinate space as `pixmap_origin`.
///
/// Mask values outside of this rectangle are treated as `0` (fully transparent).
///
/// Returns `false` when the mask rectangle does not overlap the pixmap at all; callers can use
/// this to skip further work because the pixmap would become fully transparent.
pub(crate) fn apply_mask_with_offset(
  pixmap: &mut Pixmap,
  pixmap_origin: (i32, i32),
  mask: &Mask,
  mask_origin: (i32, i32),
) -> RenderResult<bool> {
  #[inline]
  fn mul_div_255_round(value: u8, alpha: u8) -> u8 {
    // Match `tiny_skia::Pixmap::apply_mask` rounding behavior.
    let prod = value as u16 * alpha as u16;
    ((prod + 255) >> 8) as u8
  }

  let pixmap_w = pixmap.width() as i32;
  let pixmap_h = pixmap.height() as i32;
  if pixmap_w <= 0 || pixmap_h <= 0 {
    return Ok(false);
  }

  let pix_x0 = pixmap_origin.0;
  let pix_y0 = pixmap_origin.1;
  let pix_x1 = pix_x0 + pixmap_w;
  let pix_y1 = pix_y0 + pixmap_h;

  let mask_x0 = mask_origin.0;
  let mask_y0 = mask_origin.1;
  let mask_x1 = mask_x0 + mask.width() as i32;
  let mask_y1 = mask_y0 + mask.height() as i32;

  let inter_x0 = pix_x0.max(mask_x0);
  let inter_y0 = pix_y0.max(mask_y0);
  let inter_x1 = pix_x1.min(mask_x1);
  let inter_y1 = pix_y1.min(mask_y1);

  if inter_x1 <= inter_x0 || inter_y1 <= inter_y0 {
    return Ok(false);
  }

  let local_x0 = (inter_x0 - pix_x0) as usize;
  let local_y0 = (inter_y0 - pix_y0) as usize;
  let local_x1 = (inter_x1 - pix_x0) as usize;
  let local_y1 = (inter_y1 - pix_y0) as usize;

  let mask_local_x0 = (inter_x0 - mask_x0) as usize;
  let mask_local_y0 = (inter_y0 - mask_y0) as usize;

  let pixmap_stride = pixmap.width() as usize * 4;
  let pixmap_height = pixmap.height() as usize;
  let pix_data = pixmap.data_mut();

  // Clear rows above/below the intersection. Use one contiguous fill per region rather than
  // per-row loops to keep the hot path in `memset`.
  if local_y0 > 0 {
    pix_data[..local_y0 * pixmap_stride].fill(0);
  }
  if local_y1 < pixmap_height {
    pix_data[local_y1 * pixmap_stride..].fill(0);
  }

  let mask_stride = mask.width() as usize;
  let mask_data = mask.data();
  check_active(RenderStage::Paint)?;
  let mut deadline_counter = 0usize;
  for row_offset in 0..(local_y1 - local_y0) {
    check_active_periodic(&mut deadline_counter, 32, RenderStage::Paint)?;
    let y = local_y0 + row_offset;
    let pix_row = &mut pix_data[y * pixmap_stride..(y + 1) * pixmap_stride];

    // Clear left/right segments where mask is implicitly zero.
    if local_x0 > 0 {
      pix_row[..local_x0 * 4].fill(0);
    }
    if local_x1 < pixmap_w as usize {
      pix_row[local_x1 * 4..].fill(0);
    }

    let mask_y = mask_local_y0 + row_offset;
    if mask_y >= mask.height() as usize {
      continue;
    }
    let mask_row = &mask_data[mask_y * mask_stride..(mask_y + 1) * mask_stride];
    let mask_slice = &mask_row[mask_local_x0..mask_local_x0 + (local_x1 - local_x0)];

    let mut base = local_x0 * 4;
    for m in mask_slice.iter().copied() {
      if m == 255 {
        base += 4;
        continue;
      }
      if m == 0 {
        pix_row[base..base + 4].fill(0);
        base += 4;
        continue;
      }
      pix_row[base] = mul_div_255_round(pix_row[base], m);
      pix_row[base + 1] = mul_div_255_round(pix_row[base + 1], m);
      pix_row[base + 2] = mul_div_255_round(pix_row[base + 2], m);
      pix_row[base + 3] = mul_div_255_round(pix_row[base + 3], m);
      base += 4;
    }
  }

  Ok(true)
}

// ============================================================================
// Blend Mode Conversion
// ============================================================================

/// Extension trait for converting BlendMode to tiny-skia
trait BlendModeExt {
  fn to_skia(self) -> SkiaBlendMode;
}

impl BlendModeExt for BlendMode {
  /// Converts to tiny-skia BlendMode
  fn to_skia(self) -> SkiaBlendMode {
    match self {
      BlendMode::Normal => SkiaBlendMode::SourceOver,
      BlendMode::Multiply => SkiaBlendMode::Multiply,
      BlendMode::Screen => SkiaBlendMode::Screen,
      BlendMode::Overlay => SkiaBlendMode::Overlay,
      BlendMode::Darken => SkiaBlendMode::Darken,
      BlendMode::Lighten => SkiaBlendMode::Lighten,
      BlendMode::ColorDodge => SkiaBlendMode::ColorDodge,
      BlendMode::ColorBurn => SkiaBlendMode::ColorBurn,
      BlendMode::HardLight => SkiaBlendMode::HardLight,
      BlendMode::SoftLight => SkiaBlendMode::SoftLight,
      BlendMode::Difference => SkiaBlendMode::Difference,
      BlendMode::Exclusion => SkiaBlendMode::Exclusion,
      BlendMode::Hue => SkiaBlendMode::Hue,
      BlendMode::Saturation => SkiaBlendMode::Saturation,
      BlendMode::Color => SkiaBlendMode::Color,
      BlendMode::Luminosity => SkiaBlendMode::Luminosity,
      BlendMode::PlusLighter => SkiaBlendMode::Plus,
      BlendMode::PlusDarker => SkiaBlendMode::Darken,
      BlendMode::HueHsv => SkiaBlendMode::Hue,
      BlendMode::SaturationHsv => SkiaBlendMode::Saturation,
      BlendMode::ColorHsv => SkiaBlendMode::Color,
      BlendMode::LuminosityHsv => SkiaBlendMode::Luminosity,
      BlendMode::HueOklch => SkiaBlendMode::Hue,
      BlendMode::ChromaOklch => SkiaBlendMode::Saturation,
      BlendMode::ColorOklch => SkiaBlendMode::Color,
      BlendMode::LuminosityOklch => SkiaBlendMode::Luminosity,
    }
  }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
  use super::*;
  use crate::paint::pixmap::{new_pixmap, NewPixmapAllocRecorder};
  use tiny_skia::{FillRule, Mask, MaskType, Transform};

  fn pixel(pixmap: &Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
    let width = pixmap.width();
    let idx = ((y * width + x) * 4) as usize;
    let data = pixmap.data();
    (data[idx], data[idx + 1], data[idx + 2], data[idx + 3])
  }

  fn dummy_font() -> LoadedFont {
    LoadedFont {
      id: None,
      data: std::sync::Arc::new(Vec::new()),
      index: 0,
      face_metrics_overrides: Default::default(),
      face_settings: Default::default(),
      family: "Dummy".to_string(),
      weight: Default::default(),
      style: Default::default(),
      stretch: Default::default(),
    }
  }

  #[test]
  fn canvas_glyph_paths_do_not_panic_on_empty_or_zero_advance() {
    let mut canvas = Canvas::new(10, 10, Rgba::WHITE).unwrap();
    let font = dummy_font();

    let (paths, bounds) = canvas
      .glyph_paths(Point::new(0.0, 0.0), &[], &font, 16.0, 0.0, &[], None)
      .unwrap();
    assert!(paths.is_empty());
    assert!(bounds.is_valid());

    let glyphs = [GlyphPosition {
      glyph_id: 0,
      cluster: 0,
      x_offset: 0.0,
      y_offset: 0.0,
      x_advance: 0.0,
      y_advance: 0.0,
    }];
    let (paths, bounds) = canvas
      .glyph_paths(Point::new(0.0, 0.0), &glyphs, &font, 16.0, 0.0, &[], None)
      .unwrap();
    assert!(paths.is_empty());
    assert!(bounds.is_valid());
  }

  #[test]
  fn test_canvas_creation() {
    let canvas = Canvas::new(100, 100, Rgba::WHITE);
    assert!(canvas.is_ok());

    let canvas = canvas.unwrap();
    assert_eq!(canvas.width(), 100);
    assert_eq!(canvas.height(), 100);
  }

  #[test]
  fn test_canvas_creation_transparent() {
    let canvas = Canvas::new_transparent(50, 50);
    assert!(canvas.is_ok());
  }

  #[test]
  fn test_canvas_bounds() {
    let canvas = Canvas::new(200, 150, Rgba::WHITE).unwrap();
    let bounds = canvas.bounds();

    assert_eq!(bounds.x(), 0.0);
    assert_eq!(bounds.y(), 0.0);
    assert_eq!(bounds.width(), 200.0);
    assert_eq!(bounds.height(), 150.0);
  }

  #[test]
  fn test_canvas_size() {
    let canvas = Canvas::new(300, 200, Rgba::WHITE).unwrap();
    let size = canvas.size();

    assert_eq!(size.width, 300.0);
    assert_eq!(size.height, 200.0);
  }

  #[test]
  fn test_canvas_clear() {
    let mut canvas = Canvas::new(10, 10, Rgba::WHITE).unwrap();
    canvas.clear(Rgba::rgb(255, 0, 0));

    // tiny-skia uses premultiplied RGBA format
    // Verify first pixel is red
    let data = canvas.pixmap().data();
    assert_eq!(data[0], 255); // R
    assert_eq!(data[1], 0); // G
    assert_eq!(data[2], 0); // B
    assert_eq!(data[3], 255); // A
  }

  #[test]
  fn test_canvas_alpha_rounding_matches_expected_blend() {
    // `rgba(0,0,0,0.3)` composited over opaque white should round to 178 rather than 179.
    // This is sensitive to how alpha floats are quantized to u8 before passing to tiny-skia.
    let mut canvas = Canvas::new(10, 10, Rgba::WHITE).unwrap();
    let rect = Rect::from_xywh(0.0, 0.0, 10.0, 10.0);
    canvas.draw_rect(rect, Rgba::new(0, 0, 0, 0.3));

    let pixmap = canvas.into_pixmap();
    assert_eq!(pixel(&pixmap, 5, 5), (178, 178, 178, 255));
  }

  #[test]
  fn test_canvas_draw_rect() {
    let mut canvas = Canvas::new(100, 100, Rgba::WHITE).unwrap();
    let rect = Rect::from_xywh(10.0, 10.0, 20.0, 20.0);
    canvas.draw_rect(rect, Rgba::rgb(255, 0, 0));

    // Verify the pixmap was modified
    let pixmap = canvas.into_pixmap();
    assert_eq!(pixmap.width(), 100);
  }

  #[test]
  fn test_canvas_state_save_restore() {
    let mut canvas = Canvas::new(100, 100, Rgba::WHITE).unwrap();

    assert_eq!(canvas.state_depth(), 0);

    canvas.save();
    assert_eq!(canvas.state_depth(), 1);

    canvas.set_opacity(0.5);
    assert_eq!(canvas.opacity(), 0.5);

    canvas.save();
    assert_eq!(canvas.state_depth(), 2);

    canvas.restore();
    assert_eq!(canvas.state_depth(), 1);
    assert_eq!(canvas.opacity(), 0.5);

    canvas.restore();
    assert_eq!(canvas.state_depth(), 0);
    assert_eq!(canvas.opacity(), 1.0);
  }

  #[test]
  fn test_canvas_opacity() {
    let mut canvas = Canvas::new(100, 100, Rgba::WHITE).unwrap();

    assert_eq!(canvas.opacity(), 1.0);

    canvas.set_opacity(0.5);
    assert_eq!(canvas.opacity(), 0.5);

    // Clamping
    canvas.set_opacity(1.5);
    assert_eq!(canvas.opacity(), 1.0);

    canvas.set_opacity(-0.5);
    assert_eq!(canvas.opacity(), 0.0);
  }

  #[test]
  fn test_canvas_transform() {
    let mut canvas = Canvas::new(100, 100, Rgba::WHITE).unwrap();

    // Default is identity
    let t = canvas.transform();
    assert_eq!(t, Transform::identity());

    // Translate
    canvas.translate(10.0, 20.0);
    let t = canvas.transform();
    // Verify translation
    assert!((t.tx - 10.0).abs() < 0.001);
    assert!((t.ty - 20.0).abs() < 0.001);
  }

  #[test]
  fn test_border_radii_zero() {
    let radii = BorderRadii::ZERO;
    assert!(!radii.has_radius());
    assert_eq!(radii.max_radius(), 0.0);
  }

  #[test]
  fn test_border_radii_uniform() {
    let radii = BorderRadii::uniform(10.0);
    assert!(radii.has_radius());
    assert!(radii.is_uniform());
    assert_eq!(radii.max_radius(), 10.0);
  }

  #[test]
  fn test_border_radii_different() {
    let radii = BorderRadii::new(
      BorderRadius::uniform(5.0),
      BorderRadius::uniform(10.0),
      BorderRadius::uniform(15.0),
      BorderRadius::uniform(20.0),
    );
    assert!(radii.has_radius());
    assert!(!radii.is_uniform());
    assert_eq!(radii.max_radius(), 20.0);
  }

  #[test]
  fn test_canvas_draw_rounded_rect() {
    let mut canvas = Canvas::new(100, 100, Rgba::WHITE).unwrap();
    let rect = Rect::from_xywh(10.0, 10.0, 50.0, 50.0);
    let radii = BorderRadii::uniform(5.0);
    canvas.draw_rounded_rect(rect, radii, Rgba::rgb(0, 0, 255));

    // Just verify it doesn't crash
    let _ = canvas.into_pixmap();
  }

  #[test]
  fn opaque_axis_aligned_rounded_rect_fills_are_pixel_snapped() {
    let mut canvas = Canvas::new(100, 60, Rgba::rgb(0, 38, 118)).unwrap();
    let rect = Rect::from_xywh(10.0, 4.8, 80.0, 42.0);
    let radii = BorderRadii::uniform(20.0);
    canvas.draw_rounded_rect(rect, radii, Rgba::WHITE);

    // Sample a pixel in the middle of the top edge (away from rounded corners). With snapping, the
    // row immediately above the rounded-rect should remain the background color.
    let above = canvas.pixmap().pixel(50, 4).unwrap();
    assert_eq!(
      (above.red(), above.green(), above.blue(), above.alpha()),
      (0, 38, 118, 255)
    );

    let inside = canvas.pixmap().pixel(50, 5).unwrap();
    assert_eq!(
      (inside.red(), inside.green(), inside.blue(), inside.alpha()),
      (255, 255, 255, 255)
    );
  }

  #[test]
  fn opaque_axis_aligned_rect_fills_do_not_cover_partial_max_edge_scanlines() {
    // Regression for openbsd.org: a table-cell background ended at x=201.06, and we previously
    // snapped opaque rect fills using `ceil(max)`, which painted one extra device pixel column into
    // the adjacent cell.
    //
    // Chrome/Skia use a pixel-center rule ("open min / closed max"): pixels whose centers lie
    // beyond the max edge must not be covered.
    let fill_color = Rgba::rgb(7, 28, 46);
    let mut canvas = Canvas::new(20, 20, Rgba::WHITE).unwrap();
    canvas.draw_rect(Rect::from_xywh(0.0, 0.0, 10.4, 10.4), fill_color);

    // Max edge at 10.4px should *not* include pixel 10 (center at 10.5).
    let px10 = canvas.pixmap().pixel(10, 0).unwrap();
    assert_eq!(
      (px10.red(), px10.green(), px10.blue(), px10.alpha()),
      (255, 255, 255, 255)
    );
    let px9 = canvas.pixmap().pixel(9, 0).unwrap();
    assert_eq!(
      (px9.red(), px9.green(), px9.blue(), px9.alpha()),
      (7, 28, 46, 255)
    );

    let py10 = canvas.pixmap().pixel(0, 10).unwrap();
    assert_eq!(
      (py10.red(), py10.green(), py10.blue(), py10.alpha()),
      (255, 255, 255, 255)
    );
    let py9 = canvas.pixmap().pixel(0, 9).unwrap();
    assert_eq!(
      (py9.red(), py9.green(), py9.blue(), py9.alpha()),
      (7, 28, 46, 255)
    );
  }

  #[test]
  fn semi_transparent_axis_aligned_rounded_rect_fills_are_pixel_snapped() {
    let mut canvas = Canvas::new(100, 60, Rgba::WHITE).unwrap();
    let rect = Rect::from_xywh(10.0, 4.8, 80.0, 42.0);
    let radii = BorderRadii::uniform(20.0);
    canvas.draw_rounded_rect(rect, radii, Rgba::new(0, 0, 0, 0.1));

    // Like `opaque_axis_aligned_rounded_rect_fills_are_pixel_snapped`, ensure fractional edges do
    // not produce a partially-blended scanline above the rounded-rect.
    let above = canvas.pixmap().pixel(50, 4).unwrap();
    assert_eq!(
      (above.red(), above.green(), above.blue(), above.alpha()),
      (255, 255, 255, 255)
    );

    // Inside the rounded-rect, `black@0.1` should blend over white to `229` per channel when alpha
    // is quantized via `round(alpha*255)` and composited with truncating `mul/255`.
    let inside = canvas.pixmap().pixel(50, 5).unwrap();
    assert_eq!(
      (inside.red(), inside.green(), inside.blue(), inside.alpha()),
      (229, 229, 229, 255)
    );
  }

  #[test]
  fn opaque_rounded_rect_edge_composites_with_truncating_mul_div_255() {
    let background = Rgba::rgb(0, 38, 118);
    let mut canvas = Canvas::new(40, 40, background).unwrap();
    let rect = Rect::from_xywh(5.0, 5.0, 30.0, 30.0);
    let radii = BorderRadii::uniform(15.0);
    canvas.draw_rounded_rect(rect, radii, Rgba::WHITE);

    let path = canvas
      .build_rounded_rect_path(rect, radii)
      .expect("rounded rect path");
    let mut mask = Mask::new(canvas.width(), canvas.height()).expect("mask");
    mask.data_mut().fill(0);
    mask.fill_path(&path, FillRule::Winding, true, Transform::identity());

    let width = canvas.width() as usize;
    let data = mask.data();
    let mut checked = 0usize;
    for (idx, coverage) in data.iter().enumerate() {
      let coverage = *coverage;
      if coverage == 0 || coverage == 255 {
        continue;
      }
      checked += 1;

      let x = (idx % width) as u32;
      let y = (idx / width) as u32;
      let pix = canvas.pixmap().pixel(x, y).expect("pixel in bounds");

      let pix_sa = coverage as u16;
      let inv_sa = 255u16 - pix_sa;

      // Source is solid white (r=g=b=a=255); per-pixel source alpha is the AA coverage.
      // Use truncating `mul/255` arithmetic to match Chrome/Skia.
      let expected_r = pix_sa + (background.r as u16 * inv_sa) / 255u16;
      let expected_g = pix_sa + (background.g as u16 * inv_sa) / 255u16;
      let expected_b = pix_sa + (background.b as u16 * inv_sa) / 255u16;
      assert_eq!(
        (pix.red(), pix.green(), pix.blue(), pix.alpha()),
        (expected_r as u8, expected_g as u8, expected_b as u8, 255),
        "pixel ({x}, {y}) coverage={coverage}"
      );
    }
    assert!(checked > 0, "expected some anti-aliased edge pixels");
  }

  #[test]
  fn test_canvas_stroke_rect() {
    let mut canvas = Canvas::new(100, 100, Rgba::WHITE).unwrap();
    let rect = Rect::from_xywh(10.0, 10.0, 50.0, 50.0);
    canvas.stroke_rect(rect, Rgba::BLACK, 2.0);

    let _ = canvas.into_pixmap();
  }

  #[test]
  fn test_canvas_draw_line() {
    let mut canvas = Canvas::new(100, 100, Rgba::WHITE).unwrap();
    canvas.draw_line(
      Point::new(10.0, 10.0),
      Point::new(90.0, 90.0),
      Rgba::BLACK,
      1.0,
    );

    let _ = canvas.into_pixmap();
  }

  #[test]
  fn test_canvas_draw_circle() {
    let mut canvas = Canvas::new(100, 100, Rgba::WHITE).unwrap();
    canvas.draw_circle(Point::new(50.0, 50.0), 20.0, Rgba::rgb(0, 255, 0));

    let _ = canvas.into_pixmap();
  }

  #[test]
  fn test_blend_mode_default() {
    assert_eq!(BlendMode::default(), BlendMode::Normal);
  }

  #[test]
  fn test_blend_mode_to_skia() {
    assert_eq!(BlendMode::Normal.to_skia(), SkiaBlendMode::SourceOver);
    assert_eq!(BlendMode::Multiply.to_skia(), SkiaBlendMode::Multiply);
    assert_eq!(BlendMode::Screen.to_skia(), SkiaBlendMode::Screen);
  }

  #[test]
  fn test_canvas_skip_transparent() {
    let mut canvas = Canvas::new(100, 100, Rgba::WHITE).unwrap();

    // Drawing with transparent color should not crash
    canvas.draw_rect(Rect::from_xywh(10.0, 10.0, 20.0, 20.0), Rgba::TRANSPARENT);

    let _ = canvas.into_pixmap();
  }

  #[test]
  fn transparent_text_run_skips_rasterization() {
    use crate::text::face_cache;
    use std::path::PathBuf;
    use std::sync::Arc;

    let font_path =
      PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fonts/DejaVuSans-subset.ttf");
    let data = Arc::new(std::fs::read(font_path).expect("read test font"));
    let font = LoadedFont {
      id: None,
      data,
      index: 0,
      face_metrics_overrides: crate::text::font_db::FontFaceMetricsOverrides::default(),
      face_settings: Default::default(),
      family: "DejaVu Sans Subset".to_string(),
      weight: crate::text::font_db::FontWeight::NORMAL,
      style: crate::text::font_db::FontStyle::Normal,
      stretch: crate::text::font_db::FontStretch::Normal,
    };

    let cached_face = face_cache::get_ttf_face(&font).expect("parse test font");
    let face = cached_face.face();
    let ch = ['W', 'O', 'F', '2']
      .iter()
      .copied()
      .find(|ch| face.glyph_index(*ch).is_some())
      .expect("expected test font to contain at least one ASCII glyph");
    let glyph_id = face.glyph_index(ch).expect("glyph present").0 as u32;

    let glyphs = [GlyphInstance {
      glyph_id,
      cluster: 0,
      x_offset: 0.0,
      y_offset: 0.0,
      x_advance: 0.0,
      y_advance: 0.0,
    }];
    let palette_overrides: &[(u16, Rgba)] = &[];
    let variations: &[FontVariation] = &[];

    let mut canvas = Canvas::new(64, 64, Rgba::WHITE).unwrap();
    let before = canvas.text_cache_stats();
    canvas
      .draw_text_run(
        Point::new(10.0, 20.0),
        &glyphs,
        &font,
        16.0,
        1.0,
        RunRotation::None,
        true,
        Rgba::TRANSPARENT,
        0.0,
        0.0,
        0,
        palette_overrides,
        0,
        variations,
      )
      .expect("draw transparent text");
    let after = canvas.text_cache_stats();
    assert_eq!(
      before, after,
      "transparent text should skip glyph rasterization and cache touches"
    );
  }

  #[test]
  fn test_canvas_clip() {
    let mut canvas = Canvas::new(100, 100, Rgba::WHITE).unwrap();

    canvas
      .set_clip(Rect::from_xywh(20.0, 20.0, 60.0, 60.0))
      .unwrap();

    // Draw a rectangle that extends beyond the clip
    canvas.draw_rect(
      Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
      Rgba::rgb(255, 0, 0),
    );

    canvas.clear_clip();

    let _ = canvas.into_pixmap();
  }

  #[test]
  fn clip_limits_rect_fill() {
    let mut canvas = Canvas::new(10, 10, Rgba::WHITE).unwrap();
    canvas
      .set_clip(Rect::from_xywh(2.0, 2.0, 4.0, 4.0))
      .unwrap();
    assert!(
      canvas.clip_mask().is_none(),
      "expected simple rect clip under identity transform to be represented by bounds only"
    );
    canvas.draw_rect(Rect::from_xywh(0.0, 0.0, 10.0, 10.0), Rgba::rgb(255, 0, 0));
    let pixmap = canvas.into_pixmap();

    assert_eq!(pixel(&pixmap, 3, 3), (255, 0, 0, 255));
    assert_eq!(pixel(&pixmap, 0, 0), (255, 255, 255, 255));
  }

  #[test]
  fn clip_force_mask_builds_mask() {
    let mut canvas = Canvas::new(10, 10, Rgba::WHITE).unwrap();
    canvas
      .set_clip_force_mask(Rect::from_xywh(2.0, 2.0, 4.0, 4.0))
      .unwrap();
    assert!(
      canvas.clip_mask().is_some(),
      "expected set_clip_force_mask to build a per-pixel mask"
    );
  }

  #[test]
  fn clip_image_mask_uses_alpha_channel() {
    let mut canvas = Canvas::new(8, 4, Rgba::WHITE).unwrap();
    let mut mask = Pixmap::new(4, 4).unwrap();
    mask.data_mut().fill(0);
    for y in 0..4u32 {
      for x in 0..2u32 {
        let idx = ((y * 4 + x) * 4) as usize;
        mask.data_mut()[idx + 3] = 255;
      }
    }

    canvas
      .set_clip_image_mask(
        &mask,
        Rect::from_xywh(2.0, 0.0, 4.0, 4.0),
        FilterQuality::Nearest,
      )
      .unwrap();
    canvas.draw_rect(Rect::from_xywh(0.0, 0.0, 8.0, 4.0), Rgba::RED);
    let pixmap = canvas.into_pixmap();

    assert_eq!(
      pixel(&pixmap, 2, 1),
      (255, 0, 0, 255),
      "expected left half of masked rect to draw"
    );
    assert_eq!(
      pixel(&pixmap, 5, 1),
      (255, 255, 255, 255),
      "expected right half of masked rect to be clipped out"
    );
  }

  #[test]
  fn rounded_clip_masks_corners() {
    let mut canvas = Canvas::new(12, 12, Rgba::WHITE).unwrap();
    canvas
      .set_clip_with_radii(
        Rect::from_xywh(2.0, 2.0, 8.0, 8.0),
        Some(BorderRadii::uniform(4.0)),
      )
      .unwrap();
    canvas.draw_rect(Rect::from_xywh(0.0, 0.0, 12.0, 12.0), Rgba::rgb(0, 0, 255));
    let pixmap = canvas.into_pixmap();

    assert_eq!(pixel(&pixmap, 6, 6), (0, 0, 255, 255));
    assert_eq!(pixel(&pixmap, 2, 2), (255, 255, 255, 255));
  }

  #[test]
  fn translated_clip_tracks_device_bounds() {
    let mut canvas = Canvas::new(10, 10, Rgba::WHITE).unwrap();
    canvas.translate(2.0, 1.0);

    canvas
      .set_clip(Rect::from_xywh(1.0, 1.0, 4.0, 4.0))
      .unwrap();

    if let Some(bounds) = canvas.clip_bounds() {
      assert_eq!(bounds, Rect::from_xywh(3.0, 2.0, 4.0, 4.0));
    }

    canvas.draw_rect(Rect::from_xywh(0.0, 0.0, 6.0, 6.0), Rgba::rgb(255, 0, 0));
    let pixmap = canvas.into_pixmap();

    // Inside the translated clip
    assert_eq!(pixel(&pixmap, 4, 3), (255, 0, 0, 255));
    // Inside the draw rect but outside the clip bounds
    assert_eq!(pixel(&pixmap, 2, 2), (255, 255, 255, 255));
  }

  #[test]
  fn translation_before_push_layer_matches_direct_drawing() {
    let rect = Rect::from_xywh(4.0, 5.0, 5.0, 4.0);
    let (dx, dy) = (3.0, 2.0);

    let mut direct = Canvas::new(16, 16, Rgba::WHITE).unwrap();
    direct.translate(dx, dy);
    direct.draw_rect(rect, Rgba::RED);
    let direct_pixmap = direct.into_pixmap();

    let mut layered = Canvas::new(16, 16, Rgba::WHITE).unwrap();
    layered.translate(dx, dy);
    layered.push_layer(1.0).unwrap();
    layered.draw_rect(rect, Rgba::RED);
    layered.pop_layer().unwrap();
    let layered_pixmap = layered.into_pixmap();

    assert_eq!(
      layered_pixmap.data(),
      direct_pixmap.data(),
      "push_layer/pop_layer should behave like direct drawing when a translation exists before push"
    );
  }

  #[test]
  fn translation_before_push_layer_bounded_matches_direct_drawing() {
    let rect = Rect::from_xywh(4.0, 5.0, 5.0, 4.0);
    let (dx, dy) = (3.0, 2.0);
    let bounds_in_device_space =
      Rect::from_xywh(rect.x() + dx, rect.y() + dy, rect.width(), rect.height());

    let mut direct = Canvas::new(16, 16, Rgba::WHITE).unwrap();
    direct.translate(dx, dy);
    direct.draw_rect(rect, Rgba::RED);
    let direct_pixmap = direct.into_pixmap();

    let mut layered = Canvas::new(16, 16, Rgba::WHITE).unwrap();
    layered.translate(dx, dy);
    layered
      .push_layer_bounded(1.0, None, bounds_in_device_space)
      .unwrap();
    layered.draw_rect(rect, Rgba::RED);
    layered.pop_layer().unwrap();
    let layered_pixmap = layered.into_pixmap();

    assert_eq!(
      layered_pixmap.data(),
      direct_pixmap.data(),
      "push_layer_bounded/pop_layer should behave like direct drawing when a translation exists before push"
    );
  }

  #[test]
  fn scale_before_push_layer_matches_direct_drawing() {
    let rect = Rect::from_xywh(1.0, 1.0, 3.0, 2.0);

    let mut direct = Canvas::new(16, 16, Rgba::WHITE).unwrap();
    direct.scale(2.0, 2.0);
    direct.draw_rect(rect, Rgba::RED);
    let direct_pixmap = direct.into_pixmap();

    let mut layered = Canvas::new(16, 16, Rgba::WHITE).unwrap();
    layered.scale(2.0, 2.0);
    layered.push_layer(1.0).unwrap();
    layered.draw_rect(rect, Rgba::RED);
    layered.pop_layer().unwrap();
    let layered_pixmap = layered.into_pixmap();

    assert_eq!(
      layered_pixmap.data(),
      direct_pixmap.data(),
      "push_layer/pop_layer should behave like direct drawing when a scale exists before push"
    );
  }

  #[test]
  fn layer_composite_respects_parent_transform_set_before_push() {
    let mut canvas = Canvas::new(8, 8, Rgba::WHITE).unwrap();
    canvas.translate(-2.0, -1.0);

    canvas.push_layer(1.0).unwrap();
    let rect = Rect::from_xywh(3.0, 2.0, 4.0, 4.0);
    canvas.draw_rect(rect, Rgba::RED);
    canvas.pop_layer().unwrap();

    let pixmap = canvas.into_pixmap();
    assert_eq!(pixel(&pixmap, 1, 1), (255, 0, 0, 255));
    assert_eq!(pixel(&pixmap, 0, 0), (255, 255, 255, 255));
    assert_eq!(pixel(&pixmap, 6, 5), (255, 255, 255, 255));
  }

  #[test]
  fn bounded_layer_composite_respects_parent_transform_set_before_push() {
    let rect = Rect::from_xywh(3.0, 2.0, 4.0, 4.0);

    let mut full = Canvas::new(8, 8, Rgba::WHITE).unwrap();
    full.translate(-2.0, -1.0);
    full.push_layer(1.0).unwrap();
    full.draw_rect(rect, Rgba::RED);
    full.pop_layer().unwrap();
    let full_pixmap = full.into_pixmap();

    let mut bounded = Canvas::new(8, 8, Rgba::WHITE).unwrap();
    bounded.translate(-2.0, -1.0);
    bounded
      .push_layer_bounded(1.0, None, Rect::from_xywh(1.0, 1.0, 4.0, 4.0))
      .unwrap();
    bounded.draw_rect(rect, Rgba::RED);
    bounded.pop_layer().unwrap();
    let bounded_pixmap = bounded.into_pixmap();

    assert_eq!(pixel(&bounded_pixmap, 1, 1), (255, 0, 0, 255));
    assert_eq!(pixel(&bounded_pixmap, 6, 5), (255, 255, 255, 255));
    assert_eq!(
      bounded_pixmap.data(),
      full_pixmap.data(),
      "bounded layer should match full layer rendering under parent transforms"
    );
  }

  #[test]
  fn bounded_layer_matches_full_layer_output() {
    let mut full = Canvas::new(8, 8, Rgba::WHITE).unwrap();
    full.push_layer(1.0).unwrap();
    let rect = Rect::from_xywh(2.0, 3.0, 3.0, 2.0);
    full.draw_rect(rect, Rgba::RED);
    full.pop_layer().unwrap();
    let full_pixmap = full.into_pixmap();

    let mut bounded = Canvas::new(8, 8, Rgba::WHITE).unwrap();
    bounded
      .push_layer_bounded(1.0, None, rect)
      .expect("bounded layer");
    bounded.draw_rect(rect, Rgba::RED);
    bounded.pop_layer().unwrap();
    let bounded_pixmap = bounded.into_pixmap();

    assert_eq!(
      full_pixmap.data(),
      bounded_pixmap.data(),
      "bounded layer should match full layer rendering"
    );
  }

  #[test]
  fn layer_composite_does_not_double_apply_transforms() {
    // Regression test: offscreen layers are painted with the current transform active. When
    // compositing the layer back onto the parent pixmap we must not apply that transform again,
    // otherwise translated canvases (used by parallel tiling) shift layer content twice.
    let mut canvas = Canvas::new(20, 10, Rgba::WHITE).unwrap();
    canvas.translate(-10.0, 0.0);

    canvas.push_layer(1.0).unwrap();
    canvas.draw_rect(Rect::from_xywh(25.0, 2.0, 4.0, 4.0), Rgba::BLUE);
    canvas.pop_layer().unwrap();

    let pixmap = canvas.into_pixmap();
    assert_eq!(pixel(&pixmap, 15, 2), (0, 0, 255, 255));
    assert_eq!(pixel(&pixmap, 5, 2), (255, 255, 255, 255));
  }

  #[test]
  fn rotated_clip_bounds_prevents_culling() {
    let mut canvas = Canvas::new(10, 10, Rgba::WHITE).unwrap();
    let rotate_90_about_center = Transform::from_row(0.0, 1.0, -1.0, 0.0, 10.0, 0.0);
    canvas.set_transform(rotate_90_about_center);

    let clip_rect = Rect::from_xywh(6.0, 2.0, 3.0, 5.0);
    canvas.set_clip(clip_rect).unwrap();

    if let Some(bounds) = canvas.clip_bounds() {
      assert_eq!(bounds, Rect::from_xywh(3.0, 6.0, 5.0, 3.0));
    }

    canvas.draw_rect(Rect::from_xywh(4.0, 0.0, 6.0, 10.0), Rgba::rgb(0, 255, 0));
    let pixmap = canvas.into_pixmap();

    // Inside the rotated clip area
    assert_eq!(pixel(&pixmap, 4, 7), (0, 255, 0, 255));
    // Within the drawn rect but outside the rotated clip
    assert_eq!(pixel(&pixmap, 2, 7), (255, 255, 255, 255));
  }

  #[test]
  fn clip_path_scales_with_device_pixels() {
    let circle = ResolvedClipPath::Circle {
      center: Point::new(5.0, 5.0),
      radius: 4.0,
    };

    let mut canvas = Canvas::new(20, 15, Rgba::WHITE).unwrap();
    canvas.set_clip_path(&circle, 1.0).unwrap();
    canvas.draw_rect(Rect::from_xywh(0.0, 0.0, 20.0, 15.0), Rgba::rgb(255, 0, 0));
    let pixmap = canvas.into_pixmap();

    assert_eq!(pixel(&pixmap, 5, 5), (255, 0, 0, 255));
    assert_eq!(pixel(&pixmap, 12, 5), (255, 255, 255, 255));

    let mut hidpi = Canvas::new(40, 30, Rgba::WHITE).unwrap();
    hidpi.set_clip_path(&circle, 2.0).unwrap();
    hidpi.draw_rect(Rect::from_xywh(0.0, 0.0, 40.0, 30.0), Rgba::rgb(0, 255, 0));
    let pixmap = hidpi.into_pixmap();

    assert_eq!(pixel(&pixmap, 15, 5), (0, 255, 0, 255));
    assert_eq!(pixel(&pixmap, 0, 0), (255, 255, 255, 255));
    assert_eq!(pixel(&pixmap, 20, 5), (255, 255, 255, 255));
  }

  #[test]
  fn clip_path_follows_transforms() {
    let triangle = ResolvedClipPath::Polygon {
      points: vec![
        Point::new(0.0, 0.0),
        Point::new(4.0, 0.0),
        Point::new(0.0, 4.0),
      ],
      fill_rule: FillRule::Winding,
    };

    let mut canvas = Canvas::new(20, 15, Rgba::WHITE).unwrap();
    let transform = Transform::from_rotate(90.0).post_concat(Transform::from_translate(10.0, 0.0));
    canvas.set_transform(transform);
    canvas.set_clip_path(&triangle, 1.0).unwrap();
    canvas.draw_rect(Rect::from_xywh(0.0, 0.0, 20.0, 15.0), Rgba::rgb(0, 0, 255));

    let pixmap = canvas.into_pixmap();

    assert_eq!(pixel(&pixmap, 9, 2), (0, 0, 255, 255));
    assert_eq!(pixel(&pixmap, 1, 1), (255, 255, 255, 255));
  }

  #[test]
  fn bounded_layer_matches_full_layer() {
    let mut full = Canvas::new(12, 12, Rgba::WHITE).unwrap();
    full.push_layer(1.0).unwrap();
    full.translate(1.0, 1.0);
    full.set_clip(Rect::from_xywh(2.0, 2.0, 6.0, 6.0)).unwrap();
    full.draw_rect(Rect::from_xywh(2.0, 2.0, 3.0, 3.0), Rgba::rgb(255, 0, 0));
    full.pop_layer().unwrap();
    let full_pixmap = full.into_pixmap();

    let mut bounded = Canvas::new(12, 12, Rgba::WHITE).unwrap();
    bounded
      .push_layer_bounded(1.0, None, Rect::from_xywh(1.0, 1.0, 8.0, 8.0))
      .unwrap();
    bounded.translate(1.0, 1.0);
    bounded
      .set_clip(Rect::from_xywh(2.0, 2.0, 6.0, 6.0))
      .unwrap();
    bounded.draw_rect(Rect::from_xywh(2.0, 2.0, 3.0, 3.0), Rgba::rgb(255, 0, 0));
    bounded.pop_layer().unwrap();
    let bounded_pixmap = bounded.into_pixmap();

    assert_eq!(bounded_pixmap.data(), full_pixmap.data());
  }

  #[test]
  fn save_restore_survives_layer_push_pop() {
    let mut canvas = Canvas::new(4, 4, Rgba::WHITE).unwrap();
    canvas.set_opacity(0.5);
    canvas.set_blend_mode(BlendMode::Screen);
    canvas.save();
    canvas.set_opacity(0.25);

    canvas.push_layer(0.8).unwrap();
    canvas.set_opacity(0.1);
    canvas.save();
    canvas.set_blend_mode(BlendMode::Multiply);
    canvas.restore();

    assert_eq!(canvas.state_depth(), 1);
    assert_eq!(canvas.opacity(), 0.1);

    canvas.pop_layer().unwrap();
    assert_eq!(canvas.state_depth(), 1);
    assert_eq!(canvas.opacity(), 0.25);
    assert_eq!(canvas.blend_mode(), SkiaBlendMode::Screen);

    canvas.restore();
    assert_eq!(canvas.state_depth(), 0);
    assert_eq!(canvas.opacity(), 0.5);
  }

  #[test]
  fn bounded_layer_preserves_parent_clip_mask() {
    let mut canvas = Canvas::new(8, 8, Rgba::WHITE).unwrap();
    canvas
      .set_clip_with_radii(
        Rect::from_xywh(1.0, 1.0, 6.0, 6.0),
        Some(BorderRadii::uniform(1.0)),
      )
      .unwrap();

    canvas
      .push_layer_bounded(1.0, None, Rect::from_xywh(1.0, 1.0, 6.0, 6.0))
      .unwrap();
    canvas.draw_rect(Rect::from_xywh(0.0, 0.0, 8.0, 8.0), Rgba::rgb(255, 0, 0));
    canvas.pop_layer().unwrap();

    // Clip mask should be restored and still apply to subsequent draws.
    canvas.draw_rect(Rect::from_xywh(0.0, 0.0, 8.0, 8.0), Rgba::rgb(0, 255, 0));
    let pixmap = canvas.into_pixmap();

    assert_eq!(pixel(&pixmap, 0, 0), (255, 255, 255, 255));
    assert_eq!(pixel(&pixmap, 7, 7), (255, 255, 255, 255));
    assert_eq!(pixel(&pixmap, 3, 3), (0, 255, 0, 255));
  }

  #[test]
  fn fast_rect_clip_mask_matches_slow_path() {
    let canvas = Canvas::new_transparent(24, 24).unwrap();
    let rects = [
      // Integer-aligned.
      Rect::from_xywh(2.0, 3.0, 8.0, 6.0),
      // Fractional coordinates.
      Rect::from_xywh(1.2, 2.8, 10.7, 5.3),
      // Very thin rectangles.
      Rect::from_xywh(5.0, 5.0, 0.3, 10.0),
      Rect::from_xywh(7.4, 9.1, 8.0, 0.4),
      // Edge-clamped rectangles.
      Rect::from_xywh(-3.0, -2.0, 6.0, 5.0),
      Rect::from_xywh(20.0, 20.0, 10.0, 10.0),
    ];

    for rect in rects {
      let fast = canvas
        .build_clip_mask(rect, BorderRadii::ZERO)
        .expect("fast mask");
      let slow = canvas
        .build_clip_mask_slow_path(rect, BorderRadii::ZERO)
        .expect("slow mask");
      assert_eq!(
        fast.data(),
        slow.data(),
        "fast-path mask differs from slow-path for {rect:?}"
      );
    }
  }

  #[test]
  fn rect_clip_fast_path_avoids_pixmap_allocation() {
    let mut canvas = Canvas::new_transparent(8, 8).unwrap();
    let recorder = NewPixmapAllocRecorder::start();
    canvas
      .set_clip_with_radii(Rect::from_xywh(1.0, 1.0, 6.0, 6.0), None)
      .unwrap();
    let allocations = recorder.take();
    assert!(
      allocations.is_empty(),
      "expected no new_pixmap allocations for rect clip fast-path, got {allocations:?}"
    );
  }

  fn build_clip_mask_slow_path_reference(
    canvas: &Canvas,
    rect: Rect,
    radii: BorderRadii,
  ) -> Option<Mask> {
    let mut mask_pixmap = new_pixmap(canvas.width(), canvas.height())?;
    let paint = {
      let mut p = Paint::default();
      p.set_color_rgba8(255, 255, 255, 255);
      p
    };

    let path = canvas.build_rounded_rect_path(rect, radii)?;
    mask_pixmap.fill_path(
      &path,
      &paint,
      FillRule::Winding,
      canvas.current_state.transform,
      None,
    );
    Some(Mask::from_pixmap(mask_pixmap.as_ref(), MaskType::Alpha))
  }

  #[test]
  fn slow_path_clip_mask_matches_reference_for_rounded_rect_with_transform() {
    let mut canvas = Canvas::new_transparent(32, 32).unwrap();
    canvas.set_transform(Transform::from_translate(1.25, -0.5));

    let rect = Rect::from_xywh(4.2, 3.7, 12.8, 9.5);
    let radii = BorderRadii::uniform(3.3);

    let optimized = canvas
      .build_clip_mask_slow_path(rect, radii)
      .expect("optimized mask");
    let reference =
      build_clip_mask_slow_path_reference(&canvas, rect, radii).expect("reference mask");
    assert_eq!(optimized.data(), reference.data());
  }

  #[test]
  fn rounded_rect_clip_slow_path_avoids_pixmap_allocation() {
    let mut canvas = Canvas::new_transparent(8, 8).unwrap();
    let recorder = NewPixmapAllocRecorder::start();
    canvas
      .set_clip_with_radii(
        Rect::from_xywh(1.0, 1.0, 6.0, 6.0),
        Some(BorderRadii::uniform(2.0)),
      )
      .unwrap();
    let allocations = recorder.take();
    assert!(
      allocations.is_empty(),
      "expected no new_pixmap allocations for rounded clip masks, got {allocations:?}"
    );
  }

  #[test]
  fn transformed_rect_clip_slow_path_avoids_pixmap_allocation() {
    let mut canvas = Canvas::new_transparent(8, 8).unwrap();
    canvas.translate(0.5, 0.25);
    let recorder = NewPixmapAllocRecorder::start();
    canvas
      .set_clip(Rect::from_xywh(1.0, 1.0, 6.0, 6.0))
      .unwrap();
    let allocations = recorder.take();
    assert!(
      allocations.is_empty(),
      "expected no new_pixmap allocations for transformed rect clip masks, got {allocations:?}"
    );
  }

  #[test]
  fn crop_mask_extracts_expected_bytes() {
    let mut mask = Mask::new(5, 4).unwrap();
    for (idx, dst) in mask.data_mut().iter_mut().enumerate() {
      *dst = (idx as u8).wrapping_mul(17);
    }

    let cropped = crop_mask(&mask, 1, 1, 10, 10).unwrap().unwrap();
    assert_eq!(cropped.width(), 4);
    assert_eq!(cropped.height(), 3);

    let mut expected = Vec::new();
    let src = mask.data();
    let src_stride = mask.width() as usize;
    for row in 0..3usize {
      let src_idx = (1 + row) * src_stride + 1;
      expected.extend_from_slice(&src[src_idx..src_idx + 4]);
    }

    assert_eq!(cropped.data(), expected.as_slice());
  }

  #[test]
  fn crop_mask_does_not_allocate_pixmaps() {
    let mask = Mask::new(16, 16).unwrap();

    let recorder = NewPixmapAllocRecorder::start();
    let _ = crop_mask(&mask, 0, 0, 8, 8).unwrap().unwrap();
    assert!(
      recorder.take().is_empty(),
      "crop_mask should not allocate Pixmaps"
    );
  }

  #[test]
  fn apply_mask_with_offset_matches_tiny_skia_when_aligned() {
    let mut base = new_pixmap(8, 8).expect("pixmap");
    for (idx, chunk) in base.data_mut().chunks_exact_mut(4).enumerate() {
      let v = (idx as u8).wrapping_mul(37).wrapping_add(11);
      chunk.copy_from_slice(&[v, v.rotate_left(1), v.rotate_left(2), v.rotate_left(3)]);
    }

    let mut mask = Mask::new(8, 8).expect("mask");
    for y in 0..8u32 {
      for x in 0..8u32 {
        mask.data_mut()[(y * 8 + x) as usize] = (x * 17 + y * 13) as u8;
      }
    }

    let mut expected = base.clone();
    expected.apply_mask(&mask);

    let mut actual = base.clone();
    assert!(apply_mask_with_offset(&mut actual, (0, 0), &mask, (0, 0)).unwrap());

    assert_eq!(actual.data(), expected.data());
  }

  #[test]
  fn composite_layer_source_over_matches_chrome_rounding_without_dither() {
    let mut dst = new_pixmap(4, 4).expect("pixmap");
    let dst_px = PremultipliedColorU8::from_rgba(255, 255, 255, 255).expect("premultiplied dst");
    dst.pixels_mut().fill(dst_px);

    let mut layer = new_pixmap(4, 4).expect("pixmap");
    let src_px = PremultipliedColorU8::from_rgba(0, 0, 0, 255).expect("premultiplied src");
    layer.pixels_mut().fill(src_px);

    composite_layer_into_pixmap(
      &mut dst,
      &layer,
      0.3,
      SkiaBlendMode::SourceOver,
      (0, 0),
      None,
    );

    for y in 0..4 {
      for x in 0..4 {
        let p = dst.pixel(x, y).expect("pixel");
        assert_eq!(
          (p.red(), p.green(), p.blue(), p.alpha()),
          (178, 178, 178, 255),
          "unexpected pixel at ({x}, {y})"
        );
      }
    }
  }

  #[test]
  fn composite_layer_respects_rect_clip_without_mask() {
    // Regression for pages with `filter` inside `overflow:hidden` ancestors (e.g. gitlab.io):
    // rectangular clips are often represented as bounds-only `clip_rect` for performance, but
    // layer compositing must still respect the clip even when no per-pixel mask was built.
    let mut canvas = Canvas::new(4, 4, Rgba::WHITE).unwrap();
    canvas
      .set_clip(Rect::from_xywh(0.0, 2.0, 4.0, 2.0))
      .expect("set_clip");
    assert!(
      canvas.clip_mask().is_none(),
      "expected bounds-only clip mask fast path"
    );

    let mut layer = new_pixmap(4, 4).expect("pixmap");
    layer
      .pixels_mut()
      .fill(PremultipliedColorU8::from_rgba(255, 0, 0, 255).expect("premultiplied"));

    canvas.composite_layer(&layer, 1.0, None, (0, 0));
    let pixmap = canvas.into_pixmap();

    // Top half should remain unclipped (white); bottom half should be red.
    for y in 0..2u32 {
      for x in 0..4u32 {
        assert_eq!(pixel(&pixmap, x, y), (255, 255, 255, 255), "pixel {x},{y}");
      }
    }
    for y in 2..4u32 {
      for x in 0..4u32 {
        assert_eq!(pixel(&pixmap, x, y), (255, 0, 0, 255), "pixel {x},{y}");
      }
    }
  }
}
