//! Shared CSS filter utilities.
//!
//! These helpers mirror the legacy painter backend implementations and operate
//! on unpremultiplied channel values to avoid precision loss on semi-transparent
//! pixels. They are used by both the legacy painter and the display list
//! renderer to keep filter behavior in sync.

use crate::error::{RenderError, RenderStage};
use crate::paint::blur::{alpha_bounds, apply_gaussian_blur};
use crate::paint::pixmap::new_pixmap;
use crate::render_control::{active_deadline, check_active, check_active_periodic, with_deadline};
use crate::style::color::Rgba;
use rayon::prelude::*;
use std::cell::RefCell;
use std::collections::VecDeque;
use tiny_skia::BlendMode as SkiaBlendMode;
use tiny_skia::{Pixmap, PixmapPaint, PremultipliedColorU8, Transform};

const COLOR_FILTER_PARALLEL_THRESHOLD: usize = 2048;
const COLOR_FILTER_DEADLINE_STRIDE: usize = 1024;
const COLOR_FILTER_CHUNK_SIZE: usize = 1024;
const DROP_SHADOW_DEADLINE_STRIDE: usize = 4096;

pub(crate) fn apply_color_filter<F>(pixmap: &mut Pixmap, f: F) -> Result<(), RenderError>
where
  F: Fn([f32; 3], f32) -> ([f32; 3], f32) + Send + Sync,
{
  check_active(RenderStage::Paint)?;
  let pixels = pixmap.pixels_mut();
  if pixels.len() > COLOR_FILTER_PARALLEL_THRESHOLD {
    let deadline = active_deadline();
    pixels
      .par_chunks_mut(COLOR_FILTER_CHUNK_SIZE)
      .try_for_each(|chunk| {
        with_deadline(deadline.as_ref(), || -> Result<(), RenderError> {
          check_active(RenderStage::Paint)?;
          for px in chunk.iter_mut() {
            apply_color_filter_to_pixel(px, &f);
          }
          Ok(())
        })
      })?;
  } else {
    for (idx, px) in pixels.iter_mut().enumerate() {
      if idx % COLOR_FILTER_DEADLINE_STRIDE == 0 {
        check_active(RenderStage::Paint)?;
      }
      if std::env::var("DEBUG_FILTER_PIXEL").is_ok() {
        eprintln!(
          "filter in: r={} g={} b={} a={}",
          px.red(),
          px.green(),
          px.blue(),
          px.alpha()
        );
      }
      apply_color_filter_to_pixel(px, &f);
      if std::env::var("DEBUG_FILTER_PIXEL").is_ok() {
        eprintln!(
          "filter out: r={} g={} b={} a={}",
          px.red(),
          px.green(),
          px.blue(),
          px.alpha()
        );
      }
    }
  }
  Ok(())
}

fn apply_color_filter_to_pixel<F>(px: &mut PremultipliedColorU8, f: &F)
where
  F: Fn([f32; 3], f32) -> ([f32; 3], f32),
{
  let alpha = px.alpha() as f32 / 255.0;
  let base = if alpha > 0.0 {
    [
      (px.red() as f32 / 255.0) / alpha,
      (px.green() as f32 / 255.0) / alpha,
      (px.blue() as f32 / 255.0) / alpha,
    ]
  } else {
    [0.0, 0.0, 0.0]
  };
  let (mut color, mut new_alpha) = f(base, alpha);
  new_alpha = new_alpha.clamp(0.0, 1.0);
  color[0] = color[0].clamp(0.0, 1.0);
  color[1] = color[1].clamp(0.0, 1.0);
  color[2] = color[2].clamp(0.0, 1.0);

  let r = (color[0] * new_alpha * 255.0).round().clamp(0.0, 255.0) as u8;
  let g = (color[1] * new_alpha * 255.0).round().clamp(0.0, 255.0) as u8;
  let b = (color[2] * new_alpha * 255.0).round().clamp(0.0, 255.0) as u8;
  let a = (new_alpha * 255.0).round().clamp(0.0, 255.0) as u8;

  *px = PremultipliedColorU8::from_rgba(r, g, b, a).unwrap_or(PremultipliedColorU8::TRANSPARENT);
}

pub(crate) fn scale_color(color: [f32; 3], factor: f32) -> [f32; 3] {
  [color[0] * factor, color[1] * factor, color[2] * factor]
}

pub(crate) fn apply_contrast(color: [f32; 3], factor: f32) -> [f32; 3] {
  [
    (color[0] - 0.5) * factor + 0.5,
    (color[1] - 0.5) * factor + 0.5,
    (color[2] - 0.5) * factor + 0.5,
  ]
}

pub(crate) fn grayscale(color: [f32; 3], amount: f32) -> [f32; 3] {
  let gray = color[0] * 0.2126 + color[1] * 0.7152 + color[2] * 0.0722;
  [
    color[0] + (gray - color[0]) * amount,
    color[1] + (gray - color[1]) * amount,
    color[2] + (gray - color[2]) * amount,
  ]
}

pub(crate) fn sepia(color: [f32; 3], amount: f32) -> [f32; 3] {
  let sepia_r = color[0] * 0.393 + color[1] * 0.769 + color[2] * 0.189;
  let sepia_g = color[0] * 0.349 + color[1] * 0.686 + color[2] * 0.168;
  let sepia_b = color[0] * 0.272 + color[1] * 0.534 + color[2] * 0.131;
  [
    color[0] + (sepia_r - color[0]) * amount,
    color[1] + (sepia_g - color[1]) * amount,
    color[2] + (sepia_b - color[2]) * amount,
  ]
}

pub(crate) fn saturate(color: [f32; 3], factor: f32) -> [f32; 3] {
  let rw = 0.213;
  let gw = 0.715;
  let bw = 0.072;
  [
    (rw + (1.0 - rw) * factor) * color[0]
      + (gw - gw * factor) * color[1]
      + (bw - bw * factor) * color[2],
    (rw - rw * factor) * color[0]
      + (gw + (1.0 - gw) * factor) * color[1]
      + (bw - bw * factor) * color[2],
    (rw - rw * factor) * color[0]
      + (gw - gw * factor) * color[1]
      + (bw + (1.0 - bw) * factor) * color[2],
  ]
}

pub(crate) fn hue_rotate(color: [f32; 3], degrees: f32) -> [f32; 3] {
  let angle = degrees.to_radians();
  let cos = angle.cos();
  let sin = angle.sin();

  let r = color[0];
  let g = color[1];
  let b = color[2];

  [
    r * (0.213 + cos * 0.787 - sin * 0.213)
      + g * (0.715 - 0.715 * cos - 0.715 * sin)
      + b * (0.072 - 0.072 * cos + 0.928 * sin),
    r * (0.213 - 0.213 * cos + 0.143 * sin)
      + g * (0.715 + 0.285 * cos + 0.140 * sin)
      + b * (0.072 - 0.072 * cos - 0.283 * sin),
    r * (0.213 - 0.213 * cos - 0.787 * sin)
      + g * (0.715 - 0.715 * cos + 0.715 * sin)
      + b * (0.072 + 0.928 * cos + 0.072 * sin),
  ]
}

pub(crate) fn invert(color: [f32; 3], amount: f32) -> [f32; 3] {
  [
    color[0] + (1.0 - color[0] - color[0]) * amount,
    color[1] + (1.0 - color[1] - color[1]) * amount,
    color[2] + (1.0 - color[2] - color[2]) * amount,
  ]
}

#[derive(Default)]
struct DropShadowSpreadScratch {
  alpha0: Vec<u8>,
  alpha1: Vec<u8>,
}

thread_local! {
  static DROP_SHADOW_SPREAD_SCRATCH: RefCell<DropShadowSpreadScratch> =
    RefCell::new(DropShadowSpreadScratch::default());
}

pub(crate) fn apply_spread(pixmap: &mut Pixmap, spread: f32) -> Result<(), RenderError> {
  // Run a separable square dilation/erosion with sliding-window extrema to avoid
  // the quadratic neighborhood scan.
  let radius = spread.abs().ceil() as i32;
  if radius <= 0 || spread == 0.0 {
    return Ok(());
  }
  let width = pixmap.width() as usize;
  let height = pixmap.height() as usize;
  if width == 0 || height == 0 {
    return Ok(());
  }
  check_active(RenderStage::Paint)?;
  let expand = spread > 0.0;
  let radius = radius as usize;
  // `apply_spread_slow_reference` uses clamp-to-edge addressing. Once the radius exceeds the
  // image dimensions, the effective neighborhood saturates to the full row/column, so clamp the
  // radius to avoid pathological work or integer overflow.
  let radius_x = radius.min(width.saturating_sub(1));
  let radius_y = radius.min(height.saturating_sub(1));

  let len = width
    .checked_mul(height)
    .ok_or(RenderError::InvalidParameters {
      message: format!("drop shadow spread: buffer size overflow ({width}x{height})"),
    })?;

  // Preserve the same "fallback ratio" behaviour as the legacy spread implementation: pixels that
  // were originally fully transparent but become opaque after dilation inherit the premultiplied
  // RGB/alpha ratio from the first non-transparent pixel we can find.
  let mut base_ratio = (0.0, 0.0, 0.0);
  for px in pixmap.pixels().iter() {
    let alpha = px.alpha();
    if alpha > 0 {
      let a = alpha as f32;
      base_ratio = (
        px.red() as f32 / a,
        px.green() as f32 / a,
        px.blue() as f32 / a,
      );
      break;
    }
  }

  let mut scratch = DROP_SHADOW_SPREAD_SCRATCH.with(|cell| std::mem::take(&mut *cell.borrow_mut()));
  let result = (|| -> Result<(), RenderError> {
    scratch
      .alpha0
      .try_reserve_exact(len.saturating_sub(scratch.alpha0.len()))
      .map_err(|err| RenderError::InvalidParameters {
        message: format!("drop shadow spread: alpha scratch allocation failed: {err}"),
      })?;
    scratch.alpha0.resize(len, 0);

    scratch
      .alpha1
      .try_reserve_exact(len.saturating_sub(scratch.alpha1.len()))
      .map_err(|err| RenderError::InvalidParameters {
        message: format!("drop shadow spread: alpha scratch allocation failed: {err}"),
      })?;
    scratch.alpha1.resize(len, 0);

    let mut checked = 0usize;
    for (src, dst) in pixmap.pixels().iter().zip(scratch.alpha0.iter_mut()) {
      *dst = src.alpha();
      checked = checked.wrapping_add(1);
      if checked % DROP_SHADOW_DEADLINE_STRIDE == 0 {
        check_active(RenderStage::Paint)?;
      }
    }

    apply_spread_alpha_horizontal(
      &scratch.alpha0,
      &mut scratch.alpha1,
      width,
      height,
      radius_x,
      expand,
    )?;

    apply_spread_alpha_vertical(
      &scratch.alpha1,
      &mut scratch.alpha0,
      width,
      height,
      radius_y,
      expand,
    )?;

    // Apply the updated alpha back onto the pixmap while preserving the per-pixel premultiplied
    // color ratios used by the legacy spread implementation.
    let dst_pixels = pixmap.pixels_mut();
    let mut checked = 0usize;
    for (idx, px) in dst_pixels.iter_mut().enumerate() {
      checked = checked.wrapping_add(1);
      if checked % DROP_SHADOW_DEADLINE_STRIDE == 0 {
        check_active(RenderStage::Paint)?;
      }
      let agg_alpha = scratch.alpha0[idx];
      if agg_alpha == 0 {
        *px = PremultipliedColorU8::TRANSPARENT;
        continue;
      }

      let orig = *px;
      let orig_alpha = orig.alpha();
      if orig_alpha > 0 {
        let factor = (agg_alpha as f32) / (orig_alpha as f32);
        let r = (orig.red() as f32 * factor).round().clamp(0.0, 255.0) as u8;
        let g = (orig.green() as f32 * factor).round().clamp(0.0, 255.0) as u8;
        let b = (orig.blue() as f32 * factor).round().clamp(0.0, 255.0) as u8;
        *px = PremultipliedColorU8::from_rgba(r, g, b, agg_alpha)
          .unwrap_or(PremultipliedColorU8::TRANSPARENT);
      } else {
        let r = (base_ratio.0 * agg_alpha as f32).round().clamp(0.0, 255.0) as u8;
        let g = (base_ratio.1 * agg_alpha as f32).round().clamp(0.0, 255.0) as u8;
        let b = (base_ratio.2 * agg_alpha as f32).round().clamp(0.0, 255.0) as u8;
        *px = PremultipliedColorU8::from_rgba(r, g, b, agg_alpha)
          .unwrap_or(PremultipliedColorU8::TRANSPARENT);
      }
    }

    Ok(())
  })();

  DROP_SHADOW_SPREAD_SCRATCH.with(|cell| {
    *cell.borrow_mut() = scratch;
  });

  result
}

fn apply_spread_alpha_horizontal(
  src: &[u8],
  dst: &mut [u8],
  width: usize,
  height: usize,
  radius: usize,
  expand: bool,
) -> Result<(), RenderError> {
  debug_assert_eq!(src.len(), width * height);
  debug_assert_eq!(dst.len(), width * height);
  if radius == 0 {
    let mut checked = 0usize;
    for (s, d) in src.iter().zip(dst.iter_mut()) {
      *d = *s;
      checked = checked.wrapping_add(1);
      if checked % DROP_SHADOW_DEADLINE_STRIDE == 0 {
        check_active(RenderStage::Paint)?;
      }
    }
    return Ok(());
  }
  let window_size = radius
    .checked_mul(2)
    .and_then(|size| size.checked_add(1))
    .ok_or(RenderError::InvalidParameters {
      message: format!("drop shadow spread: window size overflow (radius={radius})"),
    })?;
  let extended_len = width
    .checked_add(
      radius
        .checked_mul(2)
        .ok_or(RenderError::InvalidParameters {
          message: format!("drop shadow spread: buffer size overflow (radius={radius})"),
        })?,
    )
    .ok_or(RenderError::InvalidParameters {
      message: format!("drop shadow spread: buffer size overflow (width={width}, radius={radius})"),
    })?;

  let queue_capacity = window_size
    .checked_add(1)
    .ok_or(RenderError::InvalidParameters {
      message: format!("drop shadow spread: window size overflow (radius={radius})"),
    })?;
  let mut queue: VecDeque<(usize, u8)> = VecDeque::new();
  queue
    .try_reserve_exact(queue_capacity)
    .map_err(|err| RenderError::InvalidParameters {
      message: format!("drop shadow spread: window buffer allocation failed: {err}"),
    })?;

  let mut deadline_counter = 0usize;
  for y in 0..height {
    queue.clear();
    let row_start = y * width;
    for j in 0..extended_len {
      check_active_periodic(
        &mut deadline_counter,
        DROP_SHADOW_DEADLINE_STRIDE,
        RenderStage::Paint,
      )?;
      let src_x = if j < radius {
        0
      } else if j >= radius + width {
        width - 1
      } else {
        j - radius
      };
      let value = src[row_start + src_x];

      if expand {
        while let Some(&(_, v)) = queue.back() {
          if v >= value {
            break;
          }
          queue.pop_back();
        }
      } else {
        while let Some(&(_, v)) = queue.back() {
          if v <= value {
            break;
          }
          queue.pop_back();
        }
      }
      queue.push_back((j, value));

      if j >= window_size {
        let expire = j - window_size;
        while let Some(&(idx, _)) = queue.front() {
          if idx <= expire {
            queue.pop_front();
          } else {
            break;
          }
        }
      }

      if j + 1 >= window_size {
        let out_x = j + 1 - window_size;
        dst[row_start + out_x] = queue.front().map(|(_, v)| *v).unwrap_or(0);
      }
    }
  }
  Ok(())
}

fn apply_spread_alpha_vertical(
  src: &[u8],
  dst: &mut [u8],
  width: usize,
  height: usize,
  radius: usize,
  expand: bool,
) -> Result<(), RenderError> {
  debug_assert_eq!(src.len(), width * height);
  debug_assert_eq!(dst.len(), width * height);
  if radius == 0 {
    let mut checked = 0usize;
    for (s, d) in src.iter().zip(dst.iter_mut()) {
      *d = *s;
      checked = checked.wrapping_add(1);
      if checked % DROP_SHADOW_DEADLINE_STRIDE == 0 {
        check_active(RenderStage::Paint)?;
      }
    }
    return Ok(());
  }
  let window_size = radius
    .checked_mul(2)
    .and_then(|size| size.checked_add(1))
    .ok_or(RenderError::InvalidParameters {
      message: format!("drop shadow spread: window size overflow (radius={radius})"),
    })?;
  let extended_len = height
    .checked_add(
      radius
        .checked_mul(2)
        .ok_or(RenderError::InvalidParameters {
          message: format!("drop shadow spread: buffer size overflow (radius={radius})"),
        })?,
    )
    .ok_or(RenderError::InvalidParameters {
      message: format!(
        "drop shadow spread: buffer size overflow (height={height}, radius={radius})"
      ),
    })?;

  let queue_capacity = window_size
    .checked_add(1)
    .ok_or(RenderError::InvalidParameters {
      message: format!("drop shadow spread: window size overflow (radius={radius})"),
    })?;
  let mut queue: VecDeque<(usize, u8)> = VecDeque::new();
  queue
    .try_reserve_exact(queue_capacity)
    .map_err(|err| RenderError::InvalidParameters {
      message: format!("drop shadow spread: window buffer allocation failed: {err}"),
    })?;

  let mut deadline_counter = 0usize;
  for x in 0..width {
    queue.clear();
    for j in 0..extended_len {
      check_active_periodic(
        &mut deadline_counter,
        DROP_SHADOW_DEADLINE_STRIDE,
        RenderStage::Paint,
      )?;
      let src_y = if j < radius {
        0
      } else if j >= radius + height {
        height - 1
      } else {
        j - radius
      };
      let value = src[src_y * width + x];

      if expand {
        while let Some(&(_, v)) = queue.back() {
          if v >= value {
            break;
          }
          queue.pop_back();
        }
      } else {
        while let Some(&(_, v)) = queue.back() {
          if v <= value {
            break;
          }
          queue.pop_back();
        }
      }
      queue.push_back((j, value));

      if j >= window_size {
        let expire = j - window_size;
        while let Some(&(idx, _)) = queue.front() {
          if idx <= expire {
            queue.pop_front();
          } else {
            break;
          }
        }
      }

      if j + 1 >= window_size {
        let out_y = j + 1 - window_size;
        dst[out_y * width + x] = queue.front().map(|(_, v)| *v).unwrap_or(0);
      }
    }
  }
  Ok(())
}

pub(crate) fn apply_drop_shadow(
  pixmap: &mut Pixmap,
  offset_x: f32,
  offset_y: f32,
  blur_radius: f32,
  spread: f32,
  color: Rgba,
) -> Result<(), RenderError> {
  check_active(RenderStage::Paint)?;
  let width = pixmap.width();
  let height = pixmap.height();
  if width == 0 || height == 0 {
    return Ok(());
  }

  let Some((min_x, min_y, bounds_w, bounds_h)) = alpha_bounds(pixmap) else {
    return Ok(());
  };
  let blur_pad = (blur_radius.abs() * 3.0).ceil() as u64;
  // Negative spread is an erosion pass. Even though it shrinks the shadow, we still need a
  // transparent margin so edge pixels can observe transparent neighbors; otherwise a tight
  // alpha-bounds crop would clamp-to-edge and prevent the erosion from taking effect.
  //
  // Positive spread already needs padding to avoid clipping the dilation.
  let spread_pad = spread.abs().ceil() as u64;
  let pad = match blur_pad.checked_add(spread_pad) {
    Some(pad) => match u32::try_from(pad) {
      Ok(pad) => pad,
      Err(_) => return Ok(()),
    },
    None => return Ok(()),
  };
  let pad_i32 = match i32::try_from(pad) {
    Ok(pad) => pad,
    Err(_) => return Ok(()),
  };
  let pad2 = match pad.checked_mul(2) {
    Some(pad2) => pad2,
    None => return Ok(()),
  };
  let shadow_w = match bounds_w.checked_add(pad2) {
    Some(w) => w,
    None => return Ok(()),
  };
  let shadow_h = match bounds_h.checked_add(pad2) {
    Some(h) => h,
    None => return Ok(()),
  };

  let mut shadow = match new_pixmap(shadow_w, shadow_h) {
    Some(p) => p,
    None => return Ok(()),
  };

  {
    let src = pixmap.pixels();
    let src_stride = width as usize;
    let dst_stride = shadow.width() as usize;
    let dst = shadow.pixels_mut();
    let mut deadline_counter = 0usize;
    for y in 0..bounds_h as usize {
      let src_row = (min_y as usize + y) * src_stride;
      let dst_row = (pad as usize + y) * dst_stride;
      for x in 0..bounds_w as usize {
        deadline_counter = deadline_counter.wrapping_add(1);
        if deadline_counter % DROP_SHADOW_DEADLINE_STRIDE == 0 {
          check_active(RenderStage::Paint)?;
        }
        let src_px = src[src_row + min_x as usize + x];
        let alpha = src_px.alpha() as f32 / 255.0;
        let dst_idx = dst_row + pad as usize + x;
        if alpha == 0.0 {
          dst[dst_idx] = PremultipliedColorU8::TRANSPARENT;
          continue;
        }
        let total_alpha = (color.a * alpha).clamp(0.0, 1.0);
        let r = (color.r as f32 / 255.0) * total_alpha;
        let g = (color.g as f32 / 255.0) * total_alpha;
        let b = (color.b as f32 / 255.0) * total_alpha;
        let a = total_alpha * 255.0;
        dst[dst_idx] = PremultipliedColorU8::from_rgba(
          (r * 255.0).round() as u8,
          (g * 255.0).round() as u8,
          (b * 255.0).round() as u8,
          a.round().clamp(0.0, 255.0) as u8,
        )
        .unwrap_or(PremultipliedColorU8::TRANSPARENT);
      }
    }
  }

  if spread != 0.0 {
    apply_spread(&mut shadow, spread)?;
  }

  if blur_radius > 0.0 {
    apply_gaussian_blur(&mut shadow, blur_radius)?;
  }

  let dest_x = match i32::try_from(min_x) {
    Ok(min_x) => min_x - pad_i32,
    Err(_) => return Ok(()),
  };
  let dest_y = match i32::try_from(min_y) {
    Ok(min_y) => min_y - pad_i32,
    Err(_) => return Ok(()),
  };
  let mut paint = PixmapPaint::default();
  paint.blend_mode = SkiaBlendMode::DestinationOver;
  pixmap.draw_pixmap(
    dest_x,
    dest_y,
    shadow.as_ref(),
    &paint,
    Transform::from_translate(offset_x, offset_y),
    None,
  );
  Ok(())
}

#[cfg(test)]
fn apply_spread_slow_reference(pixmap: &mut Pixmap, spread: f32) {
  let radius = spread.abs().ceil() as i32;
  if radius <= 0 || spread == 0.0 {
    return;
  }
  let expand = spread > 0.0;
  let width = pixmap.width() as i32;
  let height = pixmap.height() as i32;
  let original = pixmap.clone();
  let src = original.pixels();
  let dst = pixmap.pixels_mut();

  let mut base_ratio = (0.0, 0.0, 0.0);
  for px in src.iter() {
    let alpha = px.alpha();
    if alpha > 0 {
      let a = alpha as f32;
      base_ratio = (
        px.red() as f32 / a,
        px.green() as f32 / a,
        px.blue() as f32 / a,
      );
      break;
    }
  }

  for y in 0..height {
    for x in 0..width {
      let mut agg_alpha = if expand { 0u8 } else { 255u8 };
      for dy in -radius..=radius {
        for dx in -radius..=radius {
          let ny = (y + dy).clamp(0, height - 1);
          let nx = (x + dx).clamp(0, width - 1);
          let idx = (ny as usize) * (width as usize) + nx as usize;
          let px = src[idx];
          if expand {
            agg_alpha = agg_alpha.max(px.alpha());
          } else {
            agg_alpha = agg_alpha.min(px.alpha());
          }
        }
      }
      let idx = (y as usize) * (width as usize) + x as usize;
      if agg_alpha == 0 {
        dst[idx] = PremultipliedColorU8::TRANSPARENT;
        continue;
      }

      let orig = src[idx];
      let orig_alpha = orig.alpha();
      if orig_alpha > 0 {
        let factor = (agg_alpha as f32) / (orig_alpha as f32);
        let r = (orig.red() as f32 * factor).round().clamp(0.0, 255.0) as u8;
        let g = (orig.green() as f32 * factor).round().clamp(0.0, 255.0) as u8;
        let b = (orig.blue() as f32 * factor).round().clamp(0.0, 255.0) as u8;
        dst[idx] = PremultipliedColorU8::from_rgba(r, g, b, agg_alpha)
          .unwrap_or(PremultipliedColorU8::TRANSPARENT);
      } else {
        let r = (base_ratio.0 * agg_alpha as f32).round().clamp(0.0, 255.0) as u8;
        let g = (base_ratio.1 * agg_alpha as f32).round().clamp(0.0, 255.0) as u8;
        let b = (base_ratio.2 * agg_alpha as f32).round().clamp(0.0, 255.0) as u8;
        dst[idx] = PremultipliedColorU8::from_rgba(r, g, b, agg_alpha)
          .unwrap_or(PremultipliedColorU8::TRANSPARENT);
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::render_control::{with_deadline, RenderDeadline};
  use std::sync::atomic::{AtomicUsize, Ordering};
  use std::sync::Arc;

  fn bounding_box_for_color(
    pixmap: &Pixmap,
    predicate: impl Fn((u8, u8, u8, u8)) -> bool,
  ) -> Option<(u32, u32, u32, u32)> {
    let mut min_x = u32::MAX;
    let mut min_y = u32::MAX;
    let mut max_x = 0u32;
    let mut max_y = 0u32;
    let width = pixmap.width() as usize;

    for (idx, px) in pixmap.pixels().iter().enumerate() {
      if predicate((px.red(), px.green(), px.blue(), px.alpha())) {
        let x = (idx % width) as u32;
        let y = (idx / width) as u32;
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
      }
    }

    if min_x == u32::MAX {
      None
    } else {
      Some((min_x, min_y, max_x, max_y))
    }
  }

  #[test]
  fn drop_shadow_negative_spread_erodes_shadow() {
    let mut pixmap = new_pixmap(60, 40).expect("pixmap");
    pixmap.data_mut().fill(0);

    // Use a semi-transparent source so the shadow is visible even where it overlaps the source.
    // This lets us measure the actual shadow width (otherwise destination-over would drop the
    // shadow wherever the source is fully opaque).
    let fill = PremultipliedColorU8::from_rgba(0, 0, 0, 128).expect("premultiplied");
    let stride = pixmap.width();
    {
      let pixels = pixmap.pixels_mut();
      for y in 10..20u32 {
        let row = y * stride;
        for x in 10..30u32 {
          pixels[(row + x) as usize] = fill;
        }
      }
    }

    apply_drop_shadow(
      &mut pixmap,
      0.0,
      0.0,
      0.0,
      -2.0,
      Rgba::from_rgba8(255, 0, 0, 255),
    )
    .expect("drop shadow");

    let shadow_bbox =
      bounding_box_for_color(&pixmap, |(r, g, b, a)| a > 0 && r > g && r > b).expect("shadow");
    let width = shadow_bbox.2 - shadow_bbox.0 + 1;
    assert!(
      width < 20,
      "negative spread should shrink shadow width (got width {width})"
    );
  }

  #[test]
  fn apply_color_filter_respects_cancel_callback() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_cb = Arc::clone(&calls);
    let cancel = Arc::new(move || calls_cb.fetch_add(1, Ordering::SeqCst) >= 1);
    let deadline = RenderDeadline::new(None, Some(cancel));

    let mut pixmap = new_pixmap((COLOR_FILTER_PARALLEL_THRESHOLD + 1) as u32, 1).unwrap();
    let result = with_deadline(Some(&deadline), || {
      apply_color_filter(&mut pixmap, |c, a| (c, a))
    });

    assert!(
      matches!(
        result,
        Err(RenderError::Timeout {
          stage: RenderStage::Paint,
          ..
        })
      ),
      "expected timeout, got {result:?}"
    );
    assert!(calls.load(Ordering::SeqCst) >= 2);
  }

  #[test]
  fn apply_spread_matches_reference_for_small_radii() {
    fn run(spread: f32) {
      let mut pixmap = new_pixmap(9, 7).expect("pixmap");
      pixmap.data_mut().fill(0);
      let stride = pixmap.width() as usize;
      {
        let pixels = pixmap.pixels_mut();
        pixels[1 * stride + 1] = PremultipliedColorU8::from_rgba(255, 0, 0, 255).expect("pixel");
        pixels[2 * stride + 4] = PremultipliedColorU8::from_rgba(128, 0, 0, 128).expect("pixel");
        pixels[5 * stride + 2] = PremultipliedColorU8::from_rgba(0, 128, 0, 128).expect("pixel");
        pixels[3 * stride + 7] = PremultipliedColorU8::from_rgba(0, 0, 64, 64).expect("pixel");
      }

      let mut fast = pixmap.clone();
      let mut slow = pixmap.clone();
      apply_spread(&mut fast, spread).expect("fast spread");
      apply_spread_slow_reference(&mut slow, spread);
      assert_eq!(fast.data(), slow.data(), "spread mismatch for {spread}");
    }

    run(2.0);
    run(-2.0);
    // Radii larger than the image dimensions should saturate (clamp-to-edge neighborhood covers
    // the full image). The fast path clamps the radius to `width-1`/`height-1`, so verify that
    // behaviour matches the reference implementation.
    run(10.0);
    run(-10.0);
  }

  #[test]
  fn drop_shadow_spread_respects_cancel_callback() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_cb = Arc::clone(&calls);
    // Cancel on the 4th deadline check so we exercise the spread loops (copy + horizontal pass).
    let cancel = Arc::new(move || calls_cb.fetch_add(1, Ordering::SeqCst) >= 3);
    let deadline = RenderDeadline::new(None, Some(cancel));

    // Choose a spread large enough to trigger at least one `DROP_SHADOW_DEADLINE_STRIDE` check,
    // but small enough that we don't hit a second check while copying the alpha buffer.
    let stride = (DROP_SHADOW_DEADLINE_STRIDE as f32).max(1.0);
    let mut size = stride.sqrt().ceil() as u32;
    if size < 3 {
      size = 3;
    }
    if size % 2 == 0 {
      size += 1;
    }
    let pad = (size - 1) / 2;

    let mut pixmap = new_pixmap(10, 10).expect("pixmap");
    pixmap.data_mut().fill(0);
    pixmap.pixels_mut()[0] = PremultipliedColorU8::from_rgba(0, 0, 0, 255).expect("pixel");

    let spread = pad as f32;
    let result = with_deadline(Some(&deadline), || {
      apply_drop_shadow(
        &mut pixmap,
        0.0,
        0.0,
        0.0,
        spread,
        Rgba::from_rgba8(0, 0, 0, 255),
      )
    });

    assert!(
      matches!(
        result,
        Err(RenderError::Timeout {
          stage: RenderStage::Paint,
          ..
        })
      ),
      "expected timeout, got {result:?}"
    );
    assert!(calls.load(Ordering::SeqCst) >= 4);
  }

  #[test]
  fn drop_shadow_negative_spread_respects_cancel_callback() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_cb = Arc::clone(&calls);
    // Cancel on the 4th deadline check so we exercise the erosion pass as well.
    let cancel = Arc::new(move || calls_cb.fetch_add(1, Ordering::SeqCst) >= 3);
    let deadline = RenderDeadline::new(None, Some(cancel));

    let stride = (DROP_SHADOW_DEADLINE_STRIDE as f32).max(1.0);
    let mut size = stride.sqrt().ceil() as u32;
    if size < 3 {
      size = 3;
    }
    if size % 2 == 0 {
      size += 1;
    }
    let pad = (size - 1) / 2;

    let mut pixmap = new_pixmap(10, 10).expect("pixmap");
    pixmap.data_mut().fill(0);
    pixmap.pixels_mut()[0] = PremultipliedColorU8::from_rgba(0, 0, 0, 255).expect("pixel");

    let spread = -(pad as f32);
    let result = with_deadline(Some(&deadline), || {
      apply_drop_shadow(
        &mut pixmap,
        0.0,
        0.0,
        0.0,
        spread,
        Rgba::from_rgba8(0, 0, 0, 255),
      )
    });

    assert!(
      matches!(
        result,
        Err(RenderError::Timeout {
          stage: RenderStage::Paint,
          ..
        })
      ),
      "expected timeout, got {result:?}"
    );
    assert!(calls.load(Ordering::SeqCst) >= 4);
  }

  #[test]
  fn drop_shadow_huge_spread_does_not_panic() {
    use std::panic::{catch_unwind, AssertUnwindSafe};

    let mut pixmap = new_pixmap(10, 10).expect("pixmap");
    pixmap.data_mut().fill(0);
    pixmap.pixels_mut()[0] = PremultipliedColorU8::from_rgba(0, 0, 0, 255).expect("pixel");

    let result = catch_unwind(AssertUnwindSafe(|| {
      apply_drop_shadow(
        &mut pixmap,
        0.0,
        0.0,
        0.0,
        1.0e30,
        Rgba::from_rgba8(0, 0, 0, 255),
      )
    }));
    assert!(result.is_ok(), "expected no panic, got {result:?}");
    assert!(result.unwrap().is_ok());
  }
}
