//! Shape resolution for `shape-outside`.
//!
//! Converts CSS `shape-outside` values into per-row horizontal spans that the
//! float context can use to shorten line boxes. Shapes are expanded by
//! `shape-margin` and support basic shapes as well as simple image/gradient
//! masks.

use crate::css::types::{ColorStop, RadialGradientShape, RadialGradientSize};
use crate::error::{RenderError, RenderStage};
use crate::geometry::{Point, Rect, Size};
use crate::image_loader::ImageCache;
use crate::layout::formatting_context::LayoutError;
use crate::paint::clip_path::{resolve_basic_shape, ResolvedClipPath};
use crate::paint::pixmap::{new_pixmap, reserve_buffer};
use crate::style::color::Rgba;
use crate::style::types::{
  BackgroundImage, BackgroundPosition, ReferenceBox, ShapeOutside,
};
use crate::style::values::Length;
use crate::style::ComputedStyle;
use crate::text::font_loader::FontContext;
use std::f32::consts::PI;
use tiny_skia::{Mask, PathBuilder, Pixmap, SpreadMode, Transform};

/// Horizontal coverage for a float shape sampled at 1 CSS px increments.
#[derive(Debug, Clone, PartialEq)]
pub struct FloatShape {
  start_y: f32,
  spans: Vec<Option<(f32, f32)>>,
  /// For each sampled row `i`, stores the next row index `j > i` where the span differs,
  /// or `spans.len()` when the span does not change again.
  ///
  /// This is precomputed so `next_change_after` is O(1) instead of scanning runs.
  next_change_idx: Vec<usize>,
  /// Index of the first row whose span is `Some(..)`, or `spans.len()` if none.
  ///
  /// This supports the `y < start_y` semantics in `next_change_after`, where the "current"
  /// span is treated as `None` and we return the first row where the shape becomes non-empty.
  first_non_none_idx: usize,
}

impl FloatShape {
  fn from_spans(start_y: f32, spans: Vec<Option<(f32, f32)>>) -> Self {
    let len = spans.len();
    let first_non_none_idx = spans.iter().position(|span| span.is_some()).unwrap_or(len);

    let mut next_change_idx = Vec::new();
    next_change_idx.resize(len, len);
    if len > 0 {
      next_change_idx[len - 1] = len;
      for i in (0..len.saturating_sub(1)).rev() {
        next_change_idx[i] = if spans[i] == spans[i + 1] {
          next_change_idx[i + 1]
        } else {
          i + 1
        };
      }
    }

    Self {
      start_y,
      spans,
      next_change_idx,
      first_non_none_idx,
    }
  }

  #[cfg(test)]
  pub(crate) fn from_spans_for_test(start_y: f32, spans: Vec<Option<(f32, f32)>>) -> Self {
    Self::from_spans(start_y, spans)
  }

  /// Horizontal span at a particular y coordinate.
  pub fn span_at(&self, y: f32) -> Option<(f32, f32)> {
    let idx = ((y - self.start_y).floor()) as isize;
    if idx < 0 || idx as usize >= self.spans.len() {
      return None;
    }
    self.spans[idx as usize]
  }

  /// Combined span covering any shape pixels in the given range.
  pub fn span_in_range(&self, y_start: f32, y_end: f32) -> Option<(f32, f32)> {
    if y_end <= self.start_y || y_start >= self.bottom() {
      return None;
    }
    let len = self.spans.len();
    let start_idx = ((y_start - self.start_y).floor()).max(0.0) as usize;
    let end_idx = ((y_end - self.start_y).ceil()).max(0.0) as usize;
    let start_idx = start_idx.min(len);
    let end_idx = end_idx.min(len);
    if start_idx >= end_idx {
      return None;
    }
    let mut min_x = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    for span in &self.spans[start_idx..end_idx] {
      if let Some((l, r)) = span {
        min_x = min_x.min(*l);
        max_x = max_x.max(*r);
      }
    }
    if max_x > min_x {
      Some((min_x, max_x))
    } else {
      None
    }
  }

  pub fn top(&self) -> f32 {
    self.start_y
  }

  pub fn bottom(&self) -> f32 {
    self.start_y + self.spans.len() as f32
  }

  /// Returns the next Y coordinate after `y` where the sampled span changes.
  ///
  /// This is used by float layout to find the next vertical boundary that could
  /// alter available inline space when a `shape-outside` float is active.
  pub fn next_change_after(&self, y: f32) -> Option<f32> {
    let len = self.spans.len();
    if len == 0 {
      return None;
    }

    // Preserve legacy semantics:
    // - y < start_y: treat current span as `None` and return the first `Some(..)` row.
    // - y >= bottom: no further changes.
    if y < self.start_y {
      if self.first_non_none_idx < len {
        return Some(self.start_y + self.first_non_none_idx as f32);
      }
      return None;
    }
    if y >= self.bottom() {
      return None;
    }

    let row = ((y - self.start_y).floor()) as isize;
    let row = row.clamp(0, len.saturating_sub(1) as isize) as usize;
    let next_idx = self.next_change_idx[row];
    if next_idx < len {
      Some(self.start_y + next_idx as f32)
    } else {
      None
    }
  }
}

/// Resolve the float shape for `shape-outside`.
///
/// Returns `None` when no special wrapping shape applies (fallback to the float's
/// margin box rectangle).
pub fn build_float_shape(
  style: &ComputedStyle,
  margin_box: Rect,
  border_box: Rect,
  containing_block: Size,
  viewport: Size,
  font_ctx: &FontContext,
  image_cache: &ImageCache,
) -> Result<Option<FloatShape>, LayoutError> {
  let shape_margin = resolve_shape_margin_px(style.shape_margin, style, viewport);
  let reference_boxes = compute_reference_boxes(
    style,
    margin_box,
    border_box,
    containing_block,
    viewport,
    font_ctx,
  );

  match &style.shape_outside {
    ShapeOutside::None => Ok(None),
    ShapeOutside::Box(reference) => {
      let rect = select_reference_box(reference_boxes, *reference);
      Ok(rect_span(rect).and_then(|base| expand_spans(base, shape_margin)))
    }
    ShapeOutside::BasicShape(basic, reference_override) => {
      let reference = reference_override.unwrap_or(ReferenceBox::MarginBox);
      let reference_rect = select_reference_box(reference_boxes, reference);
      let resolved = resolve_basic_shape(
        basic,
        reference_rect,
        style,
        (viewport.width, viewport.height),
        font_ctx,
        RenderStage::Layout,
      )
      .map_err(|err| match err {
        RenderError::Timeout { elapsed, .. } => LayoutError::Timeout { elapsed },
        other => LayoutError::MissingContext(other.to_string()),
      })?;
      let Some(resolved) = resolved else {
        return Ok(None);
      };
      let Some((mask, origin)) = rasterize_clip_shape(&resolved) else {
        return Ok(None);
      };
      let Some(base) = spans_from_mask(&mask, origin, 0.0) else {
        return Ok(None);
      };
      Ok(expand_spans(base, shape_margin))
    }
    ShapeOutside::Image(image) => {
      let reference_rect = reference_boxes.margin;
      let Some(bitmap) = image_mask(image, reference_rect, style, viewport, image_cache) else {
        return Ok(None);
      };
      let Some(base) = spans_from_alpha_pixels(
        bitmap.width,
        bitmap.height,
        &bitmap.data,
        Point::new(reference_rect.x(), reference_rect.y()),
        style.shape_image_threshold,
      ) else {
        return Ok(None);
      };
      Ok(expand_spans(base, shape_margin))
    }
  }
}

fn resolve_shape_margin_px(margin: Length, style: &ComputedStyle, viewport: Size) -> f32 {
  margin
    .resolve_with_context(
      None,
      viewport.width,
      viewport.height,
      style.font_size,
      style.root_font_size,
    )
    .unwrap_or(margin.value)
    .max(0.0)
}

#[derive(Clone, Copy)]
struct ReferenceRects {
  border: Rect,
  padding: Rect,
  content: Rect,
  margin: Rect,
}

fn compute_reference_boxes(
  style: &ComputedStyle,
  margin_box: Rect,
  border_box: Rect,
  containing_block: Size,
  viewport: Size,
  font_ctx: &FontContext,
) -> ReferenceRects {
  let resolve = |len: Length| -> f32 {
    crate::layout::utils::resolve_length_with_percentage_metrics(
      len,
      Some(containing_block.width),
      viewport,
      style.font_size,
      style.root_font_size,
      Some(style),
      Some(font_ctx),
    )
    .unwrap_or(len.value)
    .max(0.0)
  };

  let border_left = resolve(style.used_border_left_width());
  let border_right = resolve(style.used_border_right_width());
  let border_top = resolve(style.used_border_top_width());
  let border_bottom = resolve(style.used_border_bottom_width());

  let padding_left = resolve(style.padding_left);
  let padding_right = resolve(style.padding_right);
  let padding_top = resolve(style.padding_top);
  let padding_bottom = resolve(style.padding_bottom);

  let padding_rect = inset_rect(
    border_box,
    border_left,
    border_top,
    border_right,
    border_bottom,
  );
  let content_rect = inset_rect(
    padding_rect,
    padding_left,
    padding_top,
    padding_right,
    padding_bottom,
  );

  ReferenceRects {
    border: border_box,
    padding: padding_rect,
    content: content_rect,
    margin: margin_box,
  }
}

fn select_reference_box(boxes: ReferenceRects, reference: ReferenceBox) -> Rect {
  match reference {
    ReferenceBox::BorderBox
    | ReferenceBox::FillBox
    | ReferenceBox::StrokeBox
    | ReferenceBox::ViewBox => boxes.border,
    ReferenceBox::PaddingBox => boxes.padding,
    ReferenceBox::ContentBox => boxes.content,
    ReferenceBox::MarginBox => boxes.margin,
  }
}

fn inset_rect(rect: Rect, left: f32, top: f32, right: f32, bottom: f32) -> Rect {
  Rect::from_xywh(
    rect.x() + left,
    rect.y() + top,
    (rect.width() - left - right).max(0.0),
    (rect.height() - top - bottom).max(0.0),
  )
}

fn rect_span(rect: Rect) -> Option<SpanBuffer> {
  let start_y = rect.y();
  let height = rect.height().ceil().max(0.0) as usize;
  let mut spans = Vec::new();
  spans.try_reserve_exact(height).ok()?;
  spans.resize(height, None);
  for (idx, span) in spans.iter_mut().take(height).enumerate() {
    let y = start_y + idx as f32;
    if y >= rect.y() && y < rect.max_y() {
      *span = Some((rect.x(), rect.max_x()));
    }
  }
  Some(SpanBuffer { start_y, spans })
}

fn expand_spans(base: SpanBuffer, margin: f32) -> Option<FloatShape> {
  if margin <= 0.0 {
    return Some(FloatShape::from_spans(base.start_y, base.spans));
  }

  let start_y = base.start_y - margin;
  let end_y = base.start_y + base.spans.len() as f32 + margin;
  let out_len = (end_y - start_y).ceil().max(0.0) as usize;
  let mut spans = Vec::new();
  spans.try_reserve_exact(out_len).ok()?;
  spans.resize(out_len, None);

  for (row_idx, span) in base.spans.iter().enumerate() {
    let Some((min_x, max_x)) = span else { continue };
    let base_center_y = base.start_y + row_idx as f32 + 0.5;
    let out_start = ((base_center_y - margin - start_y - 0.5).floor()).max(0.0) as usize;
    let out_end = ((base_center_y + margin - start_y - 0.5).ceil()).max(0.0) as usize;
    let out_start = out_start.min(out_len);
    let out_end = out_end.min(out_len);

    for out_idx in out_start..out_end {
      let center_y = start_y + out_idx as f32 + 0.5;
      let dy = (center_y - base_center_y).abs();
      if dy > margin {
        continue;
      }
      let dx = (margin * margin - dy * dy).max(0.0).sqrt();
      let entry: &mut Option<(f32, f32)> = &mut spans[out_idx];
      match entry {
        Some((l, r)) => {
          *l = l.min(min_x - dx);
          *r = r.max(max_x + dx);
        }
        None => *entry = Some((min_x - dx, max_x + dx)),
      }
    }
  }

  Some(FloatShape::from_spans(start_y, spans))
}

fn rasterize_clip_shape(shape: &ResolvedClipPath) -> Option<(Mask, Point)> {
  let bounds = shape.bounds();
  if bounds.width() <= 0.0 || bounds.height() <= 0.0 {
    return None;
  }
  let origin = Point::new(bounds.x().floor(), bounds.y().floor());
  let width = (bounds.max_x() - origin.x).ceil().max(0.0) as u32;
  let height = (bounds.max_y() - origin.y).ceil().max(0.0) as u32;
  let translated = shape.translate(-origin.x, -origin.y);
  translated
    .mask(
      1.0,
      tiny_skia::IntSize::from_wh(width, height)?,
      Transform::identity(),
    )
    .map(|m| (m, origin))
}

fn spans_from_mask(mask: &Mask, origin: Point, threshold: f32) -> Option<SpanBuffer> {
  let width = mask.width();
  let height = mask.height();
  let data = mask.data();
  let height_usize = usize::try_from(height).ok()?;
  let mut spans = Vec::new();
  spans.try_reserve_exact(height_usize).ok()?;
  let start_y = origin.y;
  let row_stride = if height > 0 {
    data.len() / height as usize
  } else {
    0
  };
  let threshold_u8 = (threshold.clamp(0.0, 1.0) * 255.0) as u8;

  let mut row_start = 0usize;
  for _ in 0..height {
    let mut min_x = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    for x in 0..width {
      let alpha = data[row_start + x as usize];
      if alpha > threshold_u8 {
        min_x = min_x.min(origin.x + x as f32);
        max_x = max_x.max(origin.x + x as f32 + 1.0);
      }
    }
    if max_x > min_x {
      spans.push(Some((min_x, max_x)));
    } else {
      spans.push(None);
    }
    row_start += row_stride;
  }

  Some(SpanBuffer { start_y, spans })
}

struct SpanBuffer {
  start_y: f32,
  spans: Vec<Option<(f32, f32)>>,
}

struct AlphaBitmap {
  width: u32,
  height: u32,
  data: Vec<u8>,
}

fn spans_from_alpha_pixels(
  width: u32,
  height: u32,
  data: &[u8],
  origin: Point,
  threshold: f32,
) -> Option<SpanBuffer> {
  let width_usize = usize::try_from(width).ok()?;
  let height_usize = usize::try_from(height).ok()?;
  let expected_len = width_usize.checked_mul(height_usize)?;
  if data.len() < expected_len {
    return None;
  }

  let mut spans = Vec::new();
  spans.try_reserve_exact(height_usize).ok()?;
  let threshold_u8 = (threshold.clamp(0.0, 1.0) * 255.0) as u8;
  for row in 0..height_usize {
    let row_start = row.checked_mul(width_usize)?;
    let row_data = data.get(row_start..row_start + width_usize)?;
    let mut min_x = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    for (col_idx, alpha) in row_data.iter().enumerate() {
      if *alpha > threshold_u8 {
        let col = col_idx as f32;
        min_x = min_x.min(origin.x + col);
        max_x = max_x.max(origin.x + col + 1.0);
      }
    }
    if max_x > min_x {
      spans.push(Some((min_x, max_x)));
    } else {
      spans.push(None);
    }
  }

  Some(SpanBuffer {
    start_y: origin.y,
    spans,
  })
}

fn image_mask(
  image: &BackgroundImage,
  reference_rect: Rect,
  style: &ComputedStyle,
  viewport: Size,
  image_cache: &ImageCache,
) -> Option<AlphaBitmap> {
  match image {
    BackgroundImage::Url(url) => {
      let cached = image_cache.load(&url.url).ok()?;
      let transform = style.image_orientation.resolve(cached.orientation, true);
      let (dst_w, dst_h, _) = image_size(reference_rect);
      if dst_w == 0 || dst_h == 0 {
        return None;
      }

      let Some(bytes) = u64::from(dst_w).checked_mul(u64::from(dst_h)) else {
        eprintln!(
          "shape-outside mask dimensions overflow ({}x{})",
          dst_w, dst_h
        );
        return None;
      };
      let mut alpha = match reserve_buffer(bytes, "shape-outside image alpha") {
        Ok(buf) => buf,
        Err(err) => {
          eprintln!(
            "shape-outside mask {}x{} ({} bytes) skipped: {}",
            dst_w, dst_h, bytes, err
          );
          return None;
        }
      };

      if cached.is_vector {
        // SVG preserveAspectRatio depends on the viewport size. Rendering an SVG at its intrinsic
        // dimensions and then resizing the raster output can distort letterboxing. Rasterize the
        // SVG directly at the reference box size so the shape mask matches how the SVG would be
        // painted into the reference box.
        let svg = cached.svg_content.as_deref()?;
        let (render_w, render_h) = if transform.swaps_axes() {
          (dst_h, dst_w)
        } else {
          (dst_w, dst_h)
        };
        let pixmap = image_cache
          .render_svg_pixmap_at_size(svg, render_w, render_h, &url.url, 1.0)
          .ok()?;
        let w0 = pixmap.width();
        let h0 = pixmap.height();
        if w0 == 0 || h0 == 0 {
          return None;
        }
        debug_assert_eq!(w0, render_w);
        debug_assert_eq!(h0, render_h);

        let (w1, h1) = transform.oriented_dimensions(w0, h0);
        debug_assert_eq!(w1, dst_w);
        debug_assert_eq!(h1, dst_h);

        let data = pixmap.data();
        let w0_usize = usize::try_from(w0).ok()?;
        let h0_usize = usize::try_from(h0).ok()?;
        let row_stride = w0_usize.checked_mul(4)?;
        if data.len() < row_stride.checked_mul(h0_usize)? {
          return None;
        }

        for y in 0..dst_h {
          for x in 0..dst_w {
            let mut xr = x;
            let yr = y;
            if transform.flip_x {
              xr = w1.saturating_sub(1).saturating_sub(xr);
            }

            let (x0, y0) = match transform.quarter_turns % 4 {
              0 => (xr, yr),
              1 => (yr, h0.saturating_sub(1).saturating_sub(xr)),
              2 => (
                w0.saturating_sub(1).saturating_sub(xr),
                h0.saturating_sub(1).saturating_sub(yr),
              ),
              3 => (w0.saturating_sub(1).saturating_sub(yr), xr),
              _ => (xr, yr),
            };

            let x0 = usize::try_from(x0).ok()?;
            let y0 = usize::try_from(y0).ok()?;
            let idx = y0
              .checked_mul(row_stride)?
              .checked_add(x0.checked_mul(4)?)?
              .checked_add(3)?;
            alpha.push(*data.get(idx)?);
          }
        }
      } else {
        let rgba = cached.to_oriented_rgba(transform);
        let (src_w, src_h) = rgba.dimensions();
        if src_w == 0 || src_h == 0 {
          return None;
        }
        let raw = rgba.as_raw();

        if src_w == dst_w && src_h == dst_h {
          for chunk in raw.chunks_exact(4) {
            alpha.push(chunk[3]);
          }
        } else {
          let src_w_usize = usize::try_from(src_w).ok()?;
          let src_h_usize = usize::try_from(src_h).ok()?;
          let row_stride = src_w_usize.checked_mul(4)?;
          if raw.len() < row_stride.checked_mul(src_h_usize)? {
            return None;
          }

          let src_w_u64 = u64::from(src_w);
          let src_h_u64 = u64::from(src_h);
          let dst_w_u64 = u64::from(dst_w);
          let dst_h_u64 = u64::from(dst_h);
          for y in 0..dst_h {
            let src_y = (u64::from(y).saturating_mul(src_h_u64) / dst_h_u64) as usize;
            let row_start = src_y.checked_mul(row_stride)?;
            for x in 0..dst_w {
              let src_x = (u64::from(x).saturating_mul(src_w_u64) / dst_w_u64) as usize;
              let idx = row_start.checked_add(src_x.checked_mul(4)?)?.checked_add(3)?;
              alpha.push(*raw.get(idx)?);
            }
          }
        }
      }

      Some(AlphaBitmap {
        width: dst_w,
        height: dst_h,
        data: alpha,
      })
    }
    BackgroundImage::LinearGradient { angle, stops } => {
      let (width, height, origin) = image_size(reference_rect);
      let pixmap = render_linear_gradient(
        *angle,
        stops,
        style.color,
        style.used_dark_color_scheme,
        style.forced_colors,
        width,
        height,
        style.font_size,
        style.root_font_size,
        viewport,
      )?;
      Some(alpha_from_pixmap(pixmap, origin))
    }
    BackgroundImage::RepeatingLinearGradient { angle, stops } => {
      let (width, height, origin) = image_size(reference_rect);
      let pixmap = render_linear_gradient_repeat(
        *angle,
        stops,
        style.color,
        style.used_dark_color_scheme,
        style.forced_colors,
        width,
        height,
        style.font_size,
        style.root_font_size,
        viewport,
      )?;
      Some(alpha_from_pixmap(pixmap, origin))
    }
    BackgroundImage::RadialGradient {
      shape,
      size,
      position,
      stops,
    } => {
      let (width, height, origin) = image_size(reference_rect);
      let pixmap = render_radial_gradient_image(
        *shape,
        size,
        position,
        stops,
        style,
        viewport,
        reference_rect,
        width,
        height,
        false,
      )?;
      Some(alpha_from_pixmap(pixmap, origin))
    }
    BackgroundImage::RepeatingRadialGradient {
      shape,
      size,
      position,
      stops,
    } => {
      let (width, height, origin) = image_size(reference_rect);
      let pixmap = render_radial_gradient_image(
        *shape,
        size,
        position,
        stops,
        style,
        viewport,
        reference_rect,
        width,
        height,
        true,
      )?;
      Some(alpha_from_pixmap(pixmap, origin))
    }
    BackgroundImage::ConicGradient {
      from_angle,
      position,
      stops,
    } => render_conic_gradient_alpha(
      *from_angle,
      position,
      stops,
      style,
      viewport,
      reference_rect,
      false,
    ),
    BackgroundImage::RepeatingConicGradient {
      from_angle,
      position,
      stops,
    } => render_conic_gradient_alpha(
      *from_angle,
      position,
      stops,
      style,
      viewport,
      reference_rect,
      true,
    ),
    _ => None,
  }
}

fn image_size(rect: Rect) -> (u32, u32, Point) {
  let origin = Point::new(rect.x(), rect.y());
  let width = rect.width().ceil().max(1.0) as u32;
  let height = rect.height().ceil().max(1.0) as u32;
  (width, height, origin)
}

fn alpha_from_pixmap(pixmap: Pixmap, _origin: Point) -> AlphaBitmap {
  let Some(bytes) = u64::from(pixmap.width()).checked_mul(u64::from(pixmap.height())) else {
    eprintln!(
      "shape-outside alpha overflow ({}x{})",
      pixmap.width(),
      pixmap.height()
    );
    return AlphaBitmap {
      width: pixmap.width(),
      height: pixmap.height(),
      data: Vec::new(),
    };
  };
  let mut alpha = match reserve_buffer(bytes, "shape-outside alpha from pixmap") {
    Ok(buf) => buf,
    Err(err) => {
      eprintln!(
        "shape-outside alpha {}x{} ({} bytes) skipped: {}",
        pixmap.width(),
        pixmap.height(),
        bytes,
        err
      );
      return AlphaBitmap {
        width: pixmap.width(),
        height: pixmap.height(),
        data: Vec::new(),
      };
    }
  };
  for chunk in pixmap.data().chunks_exact(4) {
    alpha.push(chunk[3]);
  }
  AlphaBitmap {
    width: pixmap.width(),
    height: pixmap.height(),
    data: alpha,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::style::types::BackgroundImageUrl;
  use crate::style::values::LengthUnit;
  use base64::engine::general_purpose::STANDARD;
  use base64::Engine;
  use image::{DynamicImage, Rgba};
  use std::io::Cursor;

  #[test]
  fn shape_outside_resolve_length_for_paint_requires_viewport_for_vw() {
    let len = Length::new(10.0, LengthUnit::Vw);
    let resolved = resolve_length_for_paint(&len, 16.0, 16.0, 0.0, Some((200.0, 100.0)));
    assert!((resolved - 20.0).abs() < 1e-6);

    let unresolved = resolve_length_for_paint(&len, 16.0, 16.0, 0.0, None);
    assert_eq!(unresolved, 0.0);
  }

  #[test]
  fn shape_outside_image_is_scaled_to_reference_box() {
    let svg = "data:image/svg+xml,<svg xmlns='http://www.w3.org/2000/svg' width='2' height='2'><rect width='2' height='2' fill='black'/></svg>";

    let mut style = ComputedStyle::default();
    style.shape_outside = ShapeOutside::Image(BackgroundImage::Url(BackgroundImageUrl::new(
      svg.to_string(),
    )));

    let margin_box = Rect::from_xywh(0.0, 0.0, 10.0, 10.0);
    let border_box = margin_box;
    let containing_block = Size::new(10.0, 10.0);
    let viewport = Size::new(100.0, 100.0);
    let font_ctx = FontContext::new();
    let image_cache = ImageCache::new();

    let shape = build_float_shape(
      &style,
      margin_box,
      border_box,
      containing_block,
      viewport,
      &font_ctx,
      &image_cache,
    )
    .expect("expected shape-outside image resolution to succeed")
    .expect("expected shape-outside image to produce a float shape");

    assert_eq!(shape.top(), 0.0);
    assert_eq!(shape.bottom(), 10.0);
    assert_eq!(shape.span_at(0.0), Some((0.0, 10.0)));
    assert_eq!(shape.span_at(9.0), Some((0.0, 10.0)));
    assert_eq!(shape.next_change_after(0.0), None);
  }

  fn make_png_data_url(pixels: &[[u8; 4]], width: u32, height: u32) -> String {
    let mut buf = image::RgbaImage::new(width, height);
    for y in 0..height {
      for x in 0..width {
        let idx = (y * width + x) as usize;
        let px = pixels.get(idx).copied().unwrap_or([0, 0, 0, 0]);
        buf.put_pixel(x, y, Rgba(px));
      }
    }

    let dynimg = DynamicImage::ImageRgba8(buf);
    let mut bytes = Vec::new();
    dynimg
      .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
      .expect("PNG encode");
    format!("data:image/png;base64,{}", STANDARD.encode(bytes))
  }

  #[test]
  fn shape_outside_image_is_scaled_in_reference_box_coordinates() {
    // 2×2 image with a single opaque pixel in the top-left corner.
    // When scaled into a 10×10 reference box (nearest sampling), it should occupy a 5×5 region.
    let png = make_png_data_url(
      &[[0, 0, 0, 255], [0, 0, 0, 0], [0, 0, 0, 0], [0, 0, 0, 0]],
      2,
      2,
    );

    let mut style = ComputedStyle::default();
    style.shape_outside = ShapeOutside::Image(BackgroundImage::Url(BackgroundImageUrl::new(png)));

    let margin_box = Rect::from_xywh(0.0, 0.0, 10.0, 10.0);
    let border_box = margin_box;
    let containing_block = Size::new(10.0, 10.0);
    let viewport = Size::new(100.0, 100.0);
    let font_ctx = FontContext::new();
    let image_cache = ImageCache::new();

    let shape = build_float_shape(
      &style,
      margin_box,
      border_box,
      containing_block,
      viewport,
      &font_ctx,
      &image_cache,
    )
    .expect("expected shape-outside image resolution to succeed")
    .expect("expected shape-outside image to produce a float shape");

    assert_eq!(shape.top(), 0.0);
    assert_eq!(shape.bottom(), 10.0);
    assert_eq!(shape.span_at(0.0), Some((0.0, 5.0)));
    assert_eq!(shape.span_at(4.0), Some((0.0, 5.0)));
    assert_eq!(shape.span_at(5.0), None);
    assert_eq!(shape.next_change_after(0.0), Some(5.0));
  }

  #[test]
  fn shape_outside_svg_is_rasterized_at_reference_box_size() {
    // SVG without explicit width/height uses a default intrinsic size (300x150), but shape-outside
    // should rasterize it into the reference box so preserveAspectRatio letterboxing is resolved in
    // the correct viewport.
    let svg = "data:image/svg+xml,<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 2 1'><rect width='2' height='1' fill='black'/></svg>";

    let mut style = ComputedStyle::default();
    style.shape_outside = ShapeOutside::Image(BackgroundImage::Url(BackgroundImageUrl::new(
      svg.to_string(),
    )));

    let margin_box = Rect::from_xywh(0.0, 0.0, 8.0, 8.0);
    let border_box = margin_box;
    let containing_block = Size::new(8.0, 8.0);
    let viewport = Size::new(100.0, 100.0);
    let font_ctx = FontContext::new();
    let image_cache = ImageCache::new();

    let shape = build_float_shape(
      &style,
      margin_box,
      border_box,
      containing_block,
      viewport,
      &font_ctx,
      &image_cache,
    )
    .expect("expected shape-outside image resolution to succeed")
    .expect("expected shape-outside image to produce a float shape");

    assert_eq!(shape.top(), 0.0);
    assert_eq!(shape.bottom(), 8.0);
    // viewBox 2:1 rendered into 8x8 should letterbox vertically, leaving 2px padding above and
    // below the content.
    assert_eq!(shape.span_at(0.0), None);
    assert_eq!(shape.span_at(1.0), None);
    assert_eq!(shape.span_at(2.0), Some((0.0, 8.0)));
    assert_eq!(shape.span_at(5.0), Some((0.0, 8.0)));
    assert_eq!(shape.span_at(6.0), None);
    assert_eq!(shape.next_change_after(0.0), Some(2.0));
    assert_eq!(shape.next_change_after(2.0), Some(6.0));
  }
}

fn render_linear_gradient(
  angle: f32,
  stops: &[ColorStop],
  current_color: Rgba,
  is_dark: bool,
  forced_colors: bool,
  width: u32,
  height: u32,
  font_size: f32,
  root_font_size: f32,
  viewport: Size,
) -> Option<Pixmap> {
  let rect = Rect::from_xywh(0.0, 0.0, width as f32, height as f32);
  let rad = angle.to_radians();
  let dx = rad.sin();
  let dy = -rad.cos();
  let resolved = normalize_color_stops(
    stops,
    current_color,
    is_dark,
    rect.width() * dx.abs() + rect.height() * dy.abs(),
    font_size,
    root_font_size,
    Some((viewport.width, viewport.height)),
    forced_colors,
  );
  if resolved.is_empty() {
    return None;
  }
  let sk_stops = gradient_stops(&resolved);
  let len = 0.5 * (rect.width() * dx.abs() + rect.height() * dy.abs());
  let cx = rect.x() + rect.width() / 2.0;
  let cy = rect.y() + rect.height() / 2.0;

  let start = tiny_skia::Point::from_xy(cx - dx * len, cy - dy * len);
  let end = tiny_skia::Point::from_xy(cx + dx * len, cy + dy * len);
  let shader =
    tiny_skia::LinearGradient::new(start, end, sk_stops, SpreadMode::Pad, Transform::identity())?;

  let mut pixmap = new_pixmap(width, height)?;
  let sk_rect = tiny_skia::Rect::from_xywh(0.0, 0.0, width as f32, height as f32)?;
  let path = PathBuilder::from_rect(sk_rect);
  let mut paint = tiny_skia::Paint::default();
  paint.shader = shader;
  paint.anti_alias = true;
  pixmap.fill_path(
    &path,
    &paint,
    tiny_skia::FillRule::Winding,
    Transform::identity(),
    None,
  );
  Some(pixmap)
}

fn render_linear_gradient_repeat(
  angle: f32,
  stops: &[ColorStop],
  current_color: Rgba,
  is_dark: bool,
  forced_colors: bool,
  width: u32,
  height: u32,
  font_size: f32,
  root_font_size: f32,
  viewport: Size,
) -> Option<Pixmap> {
  let rect = Rect::from_xywh(0.0, 0.0, width as f32, height as f32);
  let rad = angle.to_radians();
  let dx = rad.sin();
  let dy = -rad.cos();
  let resolved = normalize_color_stops(
    stops,
    current_color,
    is_dark,
    rect.width() * dx.abs() + rect.height() * dy.abs(),
    font_size,
    root_font_size,
    Some((viewport.width, viewport.height)),
    forced_colors,
  );
  if resolved.is_empty() {
    return None;
  }
  let sk_stops = gradient_stops(&resolved);
  let len = 0.5 * (rect.width() * dx.abs() + rect.height() * dy.abs());
  let cx = rect.x() + rect.width() / 2.0;
  let cy = rect.y() + rect.height() / 2.0;

  let start = tiny_skia::Point::from_xy(cx - dx * len, cy - dy * len);
  let end = tiny_skia::Point::from_xy(cx + dx * len, cy + dy * len);
  let shader = tiny_skia::LinearGradient::new(
    start,
    end,
    sk_stops,
    SpreadMode::Repeat,
    Transform::identity(),
  )?;

  let mut pixmap = new_pixmap(width, height)?;
  let sk_rect = tiny_skia::Rect::from_xywh(0.0, 0.0, width as f32, height as f32)?;
  let path = PathBuilder::from_rect(sk_rect);
  let mut paint = tiny_skia::Paint::default();
  paint.shader = shader;
  paint.anti_alias = true;
  pixmap.fill_path(
    &path,
    &paint,
    tiny_skia::FillRule::Winding,
    Transform::identity(),
    None,
  );
  Some(pixmap)
}

fn render_radial_gradient_image(
  shape: RadialGradientShape,
  size: &RadialGradientSize,
  position: &BackgroundPosition,
  stops: &[ColorStop],
  style: &ComputedStyle,
  viewport: Size,
  reference_rect: Rect,
  width: u32,
  height: u32,
  repeat: bool,
) -> Option<Pixmap> {
  let spread = if repeat {
    SpreadMode::Repeat
  } else {
    SpreadMode::Pad
  };

  let (cx, cy, radius_x, radius_y) = radial_geometry(
    Rect::from_xywh(0.0, 0.0, reference_rect.width(), reference_rect.height()),
    position,
    size,
    shape,
    style.font_size,
    style.root_font_size,
    Some((viewport.width, viewport.height)),
  );
  let resolved = normalize_color_stops(
    stops,
    style.color,
    style.used_dark_color_scheme,
    radius_x.max(radius_y),
    style.font_size,
    style.root_font_size,
    Some((viewport.width, viewport.height)),
    style.forced_colors,
  );
  if resolved.is_empty() {
    return None;
  }
  let sk_stops = gradient_stops(&resolved);
  if radius_x <= 0.0 || radius_y <= 0.0 {
    return None;
  }

  let Some(path_rect) = tiny_skia::Rect::from_xywh(0.0, 0.0, width as f32, height as f32) else {
    return None;
  };

  let Some(shader) = tiny_skia::RadialGradient::new(
    tiny_skia::Point::from_xy(0.0, 0.0),
    tiny_skia::Point::from_xy(0.0, 0.0),
    1.0,
    sk_stops,
    spread,
    Transform::from_translate(cx, cy).pre_scale(radius_x, radius_y),
  ) else {
    return None;
  };

  let mut pixmap = new_pixmap(width, height)?;
  let path = PathBuilder::from_rect(path_rect);
  let mut paint = tiny_skia::Paint::default();
  paint.shader = shader;
  paint.anti_alias = true;
  pixmap.fill_path(
    &path,
    &paint,
    tiny_skia::FillRule::Winding,
    Transform::identity(),
    None,
  );
  Some(pixmap)
}

fn render_conic_gradient_alpha(
  from_angle: f32,
  position: &BackgroundPosition,
  stops: &[ColorStop],
  style: &ComputedStyle,
  viewport: Size,
  reference_rect: Rect,
  repeating: bool,
) -> Option<AlphaBitmap> {
  let resolved = normalize_color_stops_unclamped(
    stops,
    style.color,
    style.used_dark_color_scheme,
    1.0,
    style.font_size,
    style.root_font_size,
    Some((viewport.width, viewport.height)),
    style.forced_colors,
  );
  if resolved.is_empty() {
    return None;
  }

  let width = reference_rect.width().ceil().max(1.0) as u32;
  let height = reference_rect.height().ceil().max(1.0) as u32;
  if width == 0 || height == 0 {
    return None;
  }

  let center = resolve_gradient_center(
    Rect::from_xywh(0.0, 0.0, reference_rect.width(), reference_rect.height()),
    position,
    style.font_size,
    style.root_font_size,
    Some((viewport.width, viewport.height)),
  );

  let start_angle = from_angle.to_radians();
  let period = if repeating {
    resolved.last().map(|s| s.0).unwrap_or(1.0).max(1e-6)
  } else {
    1.0
  };

  let Some(bytes) = u64::from(width).checked_mul(u64::from(height)) else {
    eprintln!("conic gradient mask overflow for {}x{}", width, height);
    return None;
  };
  let mut data = match reserve_buffer(bytes, "shape-outside conic gradient") {
    Ok(buf) => buf,
    Err(err) => {
      eprintln!(
        "conic gradient mask {}x{} ({} bytes) skipped: {}",
        width, height, bytes, err
      );
      return None;
    }
  };
  for y in 0..height {
    for x in 0..width {
      let dx = x as f32 + 0.5 - center.x;
      let dy = y as f32 + 0.5 - center.y;
      let angle = dx.atan2(-dy) + start_angle;
      let mut t = (angle / (2.0 * PI)).rem_euclid(1.0);
      t *= period;
      let color = sample_conic_stops(&resolved, t, repeating, period);
      data.push((color.a * 255.0).round().clamp(0.0, 255.0) as u8);
    }
  }

  Some(AlphaBitmap {
    width,
    height,
    data,
  })
}

fn normalize_color_stops(
  stops: &[ColorStop],
  current_color: Rgba,
  is_dark: bool,
  gradient_length: f32,
  font_size: f32,
  root_font_size: f32,
  viewport: Option<(f32, f32)>,
  forced_colors: bool,
) -> Vec<(f32, Rgba)> {
  // CSS Images 3: Gradient color stop “fixup”.
  //
  // https://www.w3.org/TR/css-images-3/#color-stop-fixup
  //
  // Stop positions are not clamped to the [0%, 100%] range; they may appear anywhere on the
  // infinite gradient line (e.g. `-50%`, `150%`).
  if stops.is_empty() {
    return Vec::new();
  }

  let gradient_length = if gradient_length.is_finite() && gradient_length > 0.0 {
    gradient_length
  } else {
    0.0
  };
  let (vw, vh) = viewport.unwrap_or((0.0, 0.0));

  let mut positions: Vec<Option<f32>> = stops
    .iter()
    .map(|stop| match stop.position {
      Some(crate::css::types::ColorStopPosition::Fraction(v)) => Some(v),
      Some(crate::css::types::ColorStopPosition::Length(len)) => {
        if gradient_length <= 0.0 {
          None
        } else {
          len
            .resolve_with_context(Some(gradient_length), vw, vh, font_size, root_font_size)
            .map(|px| px / gradient_length)
        }
      }
      None => None,
    })
    .collect();

  // If no stops had positions at all, evenly distribute them from 0%..100%.
  if positions.iter().all(|p| p.is_none()) {
    if stops.len() == 1 {
      return vec![(
        0.0,
        stops[0]
          .color
          .to_rgba_with_scheme_and_forced_colors(current_color, is_dark, forced_colors),
      )];
    }
    let denom = (stops.len() - 1) as f32;
    return stops
      .iter()
      .enumerate()
      .map(|(i, stop)| {
        (
          i as f32 / denom,
          stop
            .color
            .to_rgba_with_scheme_and_forced_colors(current_color, is_dark, forced_colors),
        )
      })
      .collect();
  }

  // Step 1: If the first stop has no position, set it to 0%.
  if positions.first().and_then(|p| *p).is_none() {
    positions[0] = Some(0.0);
  }
  // Step 2: If the last stop has no position, set it to 100%.
  if positions.last().and_then(|p| *p).is_none() {
    if let Some(last) = positions.last_mut() {
      *last = Some(1.0);
    }
  }

  // Step 3: Ensure positioned stops are non-decreasing.
  let mut max_specified = positions[0].unwrap_or(0.0);
  for pos in positions.iter_mut().skip(1) {
    if let Some(value) = *pos {
      if value < max_specified {
        *pos = Some(max_specified);
      } else {
        max_specified = value;
      }
    }
  }

  // Step 4: Distribute runs of missing stops between the nearest positioned stops.
  let mut idx = 0usize;
  while idx < positions.len() {
    if positions[idx].is_some() {
      idx += 1;
      continue;
    }

    // Safe because step 1 guarantees the first entry is positioned.
    let start_idx = idx.saturating_sub(1);
    let start_pos = positions[start_idx].unwrap_or(0.0);

    let mut end_idx = idx;
    while end_idx < positions.len() && positions[end_idx].is_none() {
      end_idx += 1;
    }
    // Safe because step 2 guarantees the last entry is positioned.
    if end_idx >= positions.len() {
      break;
    }
    let end_pos = positions[end_idx].unwrap_or(start_pos);

    let span = (end_idx - start_idx) as f32;
    for offset in 1..(end_idx - start_idx) {
      let t = offset as f32 / span;
      positions[start_idx + offset] = Some(start_pos + (end_pos - start_pos) * t);
    }

    idx = end_idx + 1;
  }

  // Pair the resolved positions with colors, keeping the result monotonic.
  let mut output = Vec::with_capacity(stops.len());
  let mut prev = f32::NEG_INFINITY;
  for (stop, pos_opt) in stops.iter().zip(positions.iter()) {
    let pos = pos_opt.unwrap_or(prev);
    let used = if pos < prev { prev } else { pos };
    prev = used;
    output.push((
      used,
      stop
        .color
        .to_rgba_with_scheme_and_forced_colors(current_color, is_dark, forced_colors),
    ));
  }

  output
}

fn gradient_stops(stops: &[(f32, Rgba)]) -> Vec<tiny_skia::GradientStop> {
  stops
    .iter()
    .map(|(pos, color)| {
      tiny_skia::GradientStop::new(
        *pos,
        tiny_skia::Color::from_rgba8(color.r, color.g, color.b, (color.a * 255.0).round() as u8),
      )
    })
    .collect()
}

fn normalize_color_stops_unclamped(
  stops: &[ColorStop],
  current_color: Rgba,
  is_dark: bool,
  gradient_length: f32,
  font_size: f32,
  root_font_size: f32,
  viewport: Option<(f32, f32)>,
  forced_colors: bool,
) -> Vec<(f32, Rgba)> {
  normalize_color_stops(
    stops,
    current_color,
    is_dark,
    gradient_length,
    font_size,
    root_font_size,
    viewport,
    forced_colors,
  )
}

fn resolve_length_for_paint(
  len: &Length,
  font_size: f32,
  root_font_size: f32,
  percentage_base: f32,
  viewport: Option<(f32, f32)>,
) -> f32 {
  crate::paint::paint_bounds::resolve_length_for_paint(
    len,
    font_size,
    root_font_size,
    percentage_base,
    viewport,
  )
}

fn radial_geometry(
  rect: Rect,
  position: &BackgroundPosition,
  size: &RadialGradientSize,
  shape: RadialGradientShape,
  font_size: f32,
  root_font_size: f32,
  viewport: Option<(f32, f32)>,
) -> (f32, f32, f32, f32) {
  let (align_x, off_x, align_y, off_y) = match position {
    BackgroundPosition::Position { x, y } => {
      let ox =
        resolve_length_for_paint(&x.offset, font_size, root_font_size, rect.width(), viewport);
      let oy = resolve_length_for_paint(
        &y.offset,
        font_size,
        root_font_size,
        rect.height(),
        viewport,
      );
      (x.alignment, ox, y.alignment, oy)
    }
  };
  let cx = rect.x() + align_x * rect.width() + off_x;
  let cy = rect.y() + align_y * rect.height() + off_y;

  let dx_left = (cx - rect.x()).max(0.0);
  let dx_right = (rect.x() + rect.width() - cx).max(0.0);
  let dy_top = (cy - rect.y()).max(0.0);
  let dy_bottom = (rect.y() + rect.height() - cy).max(0.0);

  let (mut rx, mut ry) = match size {
    RadialGradientSize::ClosestSide => (dx_left.min(dx_right), dy_top.min(dy_bottom)),
    RadialGradientSize::FarthestSide => (dx_left.max(dx_right), dy_top.max(dy_bottom)),
    RadialGradientSize::ClosestCorner => {
      let corners = [
        (dx_left, dy_top),
        (dx_left, dy_bottom),
        (dx_right, dy_top),
        (dx_right, dy_bottom),
      ];
      let mut best = f32::INFINITY;
      let mut best_pair = (0.0, 0.0);
      for (dx, dy) in corners {
        let dist = (dx * dx + dy * dy).sqrt();
        if dist < best {
          best = dist;
          best_pair = (dx, dy);
        }
      }
      (
        best_pair.0 * std::f32::consts::SQRT_2,
        best_pair.1 * std::f32::consts::SQRT_2,
      )
    }
    RadialGradientSize::FarthestCorner => {
      let corners = [
        (dx_left, dy_top),
        (dx_left, dy_bottom),
        (dx_right, dy_top),
        (dx_right, dy_bottom),
      ];
      let mut best = -f32::INFINITY;
      let mut best_pair = (0.0, 0.0);
      for (dx, dy) in corners {
        let dist = (dx * dx + dy * dy).sqrt();
        if dist > best {
          best = dist;
          best_pair = (dx, dy);
        }
      }
      (
        best_pair.0 * std::f32::consts::SQRT_2,
        best_pair.1 * std::f32::consts::SQRT_2,
      )
    }
    RadialGradientSize::Explicit { x, y } => {
      let rx =
        resolve_length_for_paint(x, font_size, root_font_size, rect.width(), viewport).max(0.0);
      let ry = y
        .as_ref()
        .map(|yy| {
          resolve_length_for_paint(yy, font_size, root_font_size, rect.height(), viewport).max(0.0)
        })
        .unwrap_or(rx);
      (rx, ry)
    }
  };

  if matches!(shape, RadialGradientShape::Circle) {
    let r = if matches!(
      size,
      RadialGradientSize::ClosestCorner | RadialGradientSize::FarthestCorner
    ) {
      let avg = (rx * rx + ry * ry) / 2.0;
      avg.sqrt()
    } else {
      rx.min(ry)
    };
    rx = r;
    ry = r;
  }

  (cx, cy, rx, ry)
}

fn resolve_gradient_center(
  rect: Rect,
  position: &BackgroundPosition,
  font_size: f32,
  root_font_size: f32,
  viewport: Option<(f32, f32)>,
) -> Point {
  let (align_x, off_x, align_y, off_y) = match position {
    BackgroundPosition::Position { x, y } => {
      let ox =
        resolve_length_for_paint(&x.offset, font_size, root_font_size, rect.width(), viewport);
      let oy = resolve_length_for_paint(
        &y.offset,
        font_size,
        root_font_size,
        rect.height(),
        viewport,
      );
      (x.alignment, ox, y.alignment, oy)
    }
  };
  Point::new(
    rect.x() + align_x * rect.width() + off_x,
    rect.y() + align_y * rect.height() + off_y,
  )
}

fn sample_conic_stops(stops: &[(f32, Rgba)], t: f32, repeating: bool, period: f32) -> Rgba {
  if stops.is_empty() {
    return Rgba::TRANSPARENT;
  }
  if stops.len() == 1 {
    return stops[0].1;
  }
  let total = if repeating {
    period
  } else {
    stops.last().map(|s| s.0).unwrap_or(1.0)
  };
  let mut pos = t;
  if repeating && total > 0.0 {
    pos = pos.rem_euclid(total);
  }
  if pos <= stops[0].0 {
    return stops[0].1;
  }
  let Some(&(last_pos, last_color)) = stops.last() else {
    return Rgba::TRANSPARENT;
  };
  if pos >= last_pos && !repeating {
    return last_color;
  }
  for window in stops.windows(2) {
    let (p0, c0) = window[0];
    let (p1, c1) = window[1];
    if pos < p0 {
      return c0;
    }
    if pos <= p1 || (repeating && (p1 - p0).abs() < f32::EPSILON) {
      let span = (p1 - p0).max(1e-6);
      let frac = ((pos - p0) / span).clamp(0.0, 1.0);
      return Rgba {
        r: ((1.0 - frac) * c0.r as f32 + frac * c1.r as f32)
          .round()
          .clamp(0.0, 255.0) as u8,
        g: ((1.0 - frac) * c0.g as f32 + frac * c1.g as f32)
          .round()
          .clamp(0.0, 255.0) as u8,
        b: ((1.0 - frac) * c0.b as f32 + frac * c1.b as f32)
          .round()
          .clamp(0.0, 255.0) as u8,
        a: (1.0 - frac) * c0.a + frac * c1.a,
      };
    }
  }

  stops.last().map(|(_, c)| *c).unwrap_or(Rgba::TRANSPARENT)
}
