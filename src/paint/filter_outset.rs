//! Filter outset computation helpers.
//!
//! These utilities estimate how far a chain of CSS filters can extend content
//! beyond its original bounds. Calculations are conservative to avoid culling
//! or clipping visible pixels when filters are applied.

use crate::geometry::Rect;
use crate::paint::display_list::ResolvedFilter;
use crate::paint::svg_filter::{FilterPrimitive, SvgFilterUnits, SvgLength};

/// Outset distances on each side of a rectangle.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FilterOutset {
  pub left: f32,
  pub top: f32,
  pub right: f32,
  pub bottom: f32,
}

impl FilterOutset {
  pub const ZERO: Self = Self {
    left: 0.0,
    top: 0.0,
    right: 0.0,
    bottom: 0.0,
  };

  pub fn max_side(&self) -> f32 {
    self.left.max(self.top).max(self.right).max(self.bottom)
  }

  pub fn as_tuple(self) -> (f32, f32, f32, f32) {
    (self.left, self.top, self.right, self.bottom)
  }
}

/// Computes a conservative *dependency* outset (in CSS pixels) required to evaluate SVG filter
/// primitives when rendering in tiles.
///
/// Even if an SVG filter's declared filter region matches the element bbox (meaning the *visible*
/// output cannot extend beyond the bbox), primitives like `feGaussianBlur` still sample neighboring
/// pixels to produce each output pixel. In the tiled renderer each tile is rendered in isolation;
/// without enough "halo" pixels around tile boundaries, the kernel will see transparent pixels
/// outside the tile and produce seams that do not appear in serial rendering.
///
/// This function intentionally overestimates and is used to size that halo. Multiple primitives are
/// accumulated to safely cover worst-case chaining within the filter graph.
///
/// It covers primitives that can read beyond the "current" pixel location, including:
/// - kernel filters (`feGaussianBlur`, `feMorphology`, `feConvolveMatrix`)
/// - displacement (`feDisplacementMap`)
/// - translations (`feOffset`, and `feDropShadow` dx/dy)
/// - tiling (`feTile`)
/// - lighting normals (`feDiffuseLighting`, `feSpecularLighting` via `kernelUnitLength`)
fn svg_filter_kernel_outset_css(
  filter: &crate::paint::svg_filter::SvgFilter,
  bbox: Rect,
) -> FilterOutset {
  let mut pad_x = 0.0f32;
  let mut pad_y = 0.0f32;
  let filter_region = filter.resolve_region(bbox);
  let filter_region_w = filter_region.width().abs();
  let filter_region_h = filter_region.height().abs();
  let filter_region_w = if filter_region_w.is_finite() {
    filter_region_w
  } else {
    0.0
  };
  let filter_region_h = if filter_region_h.is_finite() {
    filter_region_h
  } else {
    0.0
  };
  let bbox_w = bbox.width().abs();
  let bbox_h = bbox.height().abs();
  let bbox_w = if bbox_w.is_finite() { bbox_w } else { 0.0 };
  let bbox_h = if bbox_h.is_finite() { bbox_h } else { 0.0 };

  let resolve_pair = |values: (f32, f32)| -> (f32, f32) {
    match filter.primitive_units {
      SvgFilterUnits::UserSpaceOnUse => values,
      SvgFilterUnits::ObjectBoundingBox => (values.0 * bbox_w, values.1 * bbox_h),
    }
  };

  let resolve_length = |len: SvgLength, basis: f32| -> f32 {
    match (filter.primitive_units, len) {
      (SvgFilterUnits::UserSpaceOnUse, SvgLength::Number(v)) => v,
      (SvgFilterUnits::UserSpaceOnUse, SvgLength::Percent(frac)) => frac * basis,
      (SvgFilterUnits::ObjectBoundingBox, SvgLength::Number(v)) => v * basis,
      (SvgFilterUnits::ObjectBoundingBox, SvgLength::Percent(frac)) => frac * basis,
    }
  };

  for step in &filter.steps {
    match &step.primitive {
      FilterPrimitive::GaussianBlur { std_dev, .. } => {
        let (sx, sy) = resolve_pair(*std_dev);
        let sx = if sx.is_finite() { sx.abs() } else { 0.0 };
        let sy = if sy.is_finite() { sy.abs() } else { 0.0 };
        let dx = sx * 3.0;
        let dy = sy * 3.0;
        if dx.is_finite() {
          pad_x += dx;
        }
        if dy.is_finite() {
          pad_y += dy;
        }
      }
      FilterPrimitive::DropShadow {
        dx,
        dy,
        std_dev,
        ..
      } => {
        let (sx, sy) = resolve_pair(*std_dev);
        let sx = if sx.is_finite() { sx.abs() } else { 0.0 };
        let sy = if sy.is_finite() { sy.abs() } else { 0.0 };
        let blur_x = sx * 3.0;
        let blur_y = sy * 3.0;
        if blur_x.is_finite() {
          pad_x += blur_x;
        }
        if blur_y.is_finite() {
          pad_y += blur_y;
        }

        let offset_x = resolve_length(*dx, bbox_w);
        let offset_y = resolve_length(*dy, bbox_h);
        let offset_x = if offset_x.is_finite() {
          offset_x.abs()
        } else {
          0.0
        };
        let offset_y = if offset_y.is_finite() {
          offset_y.abs()
        } else {
          0.0
        };
        pad_x += offset_x;
        pad_y += offset_y;
      }
      FilterPrimitive::Offset { dx, dy, .. } => {
        let dx = resolve_length(*dx, bbox_w);
        let dy = resolve_length(*dy, bbox_h);
        let dx = if dx.is_finite() { dx.abs() } else { 0.0 };
        let dy = if dy.is_finite() { dy.abs() } else { 0.0 };
        pad_x += dx;
        pad_y += dy;
      }
      FilterPrimitive::Morphology { radius, .. } => {
        let (rx, ry) = resolve_pair(*radius);
        let rx = if rx.is_finite() { rx.abs() } else { 0.0 };
        let ry = if ry.is_finite() { ry.abs() } else { 0.0 };
        if rx.is_finite() {
          pad_x += rx;
        }
        if ry.is_finite() {
          pad_y += ry;
        }
      }
      FilterPrimitive::ConvolveMatrix {
        order_x,
        order_y,
        target_x,
        target_y,
        ..
      } => {
        let order_x = i64::try_from(*order_x).unwrap_or(i64::MAX);
        let order_y = i64::try_from(*order_y).unwrap_or(i64::MAX);
        let max_x = order_x.saturating_sub(1);
        let max_y = order_y.saturating_sub(1);
        let tx = i64::from(*target_x);
        let ty = i64::from(*target_y);
        let reach_x = tx.max(max_x.saturating_sub(tx)).max(0);
        let reach_y = ty.max(max_y.saturating_sub(ty)).max(0);
        let reach_x = reach_x as f32;
        let reach_y = reach_y as f32;
        if reach_x.is_finite() {
          pad_x += reach_x;
        }
        if reach_y.is_finite() {
          pad_y += reach_y;
        }
      }
      FilterPrimitive::DisplacementMap { scale, .. } => {
        let resolved = match filter.primitive_units {
          SvgFilterUnits::UserSpaceOnUse => *scale,
          // NOTE: This intentionally resolves against bbox width for *both* axes to match
          // `SvgFilter::resolve_primitive_x` usage in `svg_filter.rs`.
          SvgFilterUnits::ObjectBoundingBox => *scale * bbox_w,
        };
        let resolved = if resolved.is_finite() { resolved } else { 0.0 };
        let margin = 0.5 * resolved.abs();
        if margin.is_finite() {
          pad_x += margin;
          pad_y += margin;
        }
      }
      FilterPrimitive::Tile { .. } => {
        // `feTile` can sample from any pixel within its input region (wrapping as needed). When
        // rendering tiles in isolation this means pixels near a tile edge can depend on pixels far
        // away (potentially on the other side of the filter region). Conservatively request a halo
        // large enough to cover the full filter region so each tile sees a complete wrap source.
        pad_x = pad_x.max(filter_region_w);
        pad_y = pad_y.max(filter_region_h);
      }
      FilterPrimitive::DiffuseLighting {
        kernel_unit_length,
        ..
      }
      | FilterPrimitive::SpecularLighting {
        kernel_unit_length,
        ..
      } => {
        // Lighting primitives sample the surface alpha at +/- kernelUnitLength (see
        // `surface_normal` in `svg_filter.rs`).
        let kernel = kernel_unit_length.as_ref().copied().unwrap_or((1.0, 1.0));
        let (kx, ky) = resolve_pair(kernel);
        let kx = if kx.is_finite() { kx.abs() } else { 0.0 };
        let ky = if ky.is_finite() { ky.abs() } else { 0.0 };
        if kx.is_finite() {
          pad_x += kx;
        }
        if ky.is_finite() {
          pad_y += ky;
        }
      }
      _ => {}
    }
  }

  FilterOutset {
    left: pad_x.max(0.0),
    top: pad_y.max(0.0),
    right: pad_x.max(0.0),
    bottom: pad_y.max(0.0),
  }
}

/// Compute a conservative outset for a chain of resolved filters without needing a bbox.
///
/// The calculation accumulates blurs across the chain (radius * 3 per blur) and unions drop
/// shadows with the current bounds including existing outsets. SVG filters are treated as
/// in-bounds; callers that need region awareness should provide a bbox to
/// `filter_outset_with_bounds`.
pub fn filter_outset(filters: &[ResolvedFilter], scale: f32) -> FilterOutset {
  filter_outset_with_bounds(filters, scale, None)
}

/// Compute a conservative outset for a chain of resolved filters using an optional bounding box.
///
/// When `bbox` is provided, SVG filters expand according to their resolved filter region in CSS
/// pixels.
pub fn filter_outset_with_bounds(
  filters: &[ResolvedFilter],
  scale: f32,
  bbox: Option<Rect>,
) -> FilterOutset {
  let mut outset = FilterOutset::ZERO;
  let scale = if scale.is_finite() { scale.abs() } else { 0.0 };

  for filter in filters {
    match filter {
      ResolvedFilter::Blur(radius) => {
        let delta = (*radius * scale).abs() * 3.0;
        outset.left += delta;
        outset.top += delta;
        outset.right += delta;
        outset.bottom += delta;
      }
      ResolvedFilter::DropShadow {
        offset_x,
        offset_y,
        blur_radius,
        spread,
        ..
      } => {
        let dx = offset_x * scale;
        let dy = offset_y * scale;
        let blur = blur_radius * scale;
        let spread = spread * scale;
        let delta = (blur.abs() * 3.0 + spread).max(0.0);

        // Union the existing bounds with the new shadow bounds, taking the
        // current outset as the base rectangle.
        let shadow_left = outset.left + delta - dx;
        let shadow_right = outset.right + delta + dx;
        let shadow_top = outset.top + delta - dy;
        let shadow_bottom = outset.bottom + delta + dy;

        outset.left = outset.left.max(shadow_left);
        outset.top = outset.top.max(shadow_top);
        outset.right = outset.right.max(shadow_right);
        outset.bottom = outset.bottom.max(shadow_bottom);
      }
      ResolvedFilter::SvgFilter(filter) => {
        if let Some(bbox) = bbox {
          // SVG filters have two distinct padding needs:
          // (1) The declared filter region may extend beyond the bbox, expanding the visible output.
          // (2) Kernel-based primitives (e.g. feGaussianBlur) require neighboring pixels even when
          //     the filter region matches the bbox, otherwise the tiled renderer will treat the tile
          //     edge as transparent and produce seams.
          let region = filter.resolve_region(bbox);
          let delta_left = (bbox.min_x() - region.min_x()).max(0.0) * scale;
          let delta_top = (bbox.min_y() - region.min_y()).max(0.0) * scale;
          let delta_right = (region.max_x() - bbox.max_x()).max(0.0) * scale;
          let delta_bottom = (region.max_y() - bbox.max_y()).max(0.0) * scale;

          let kernel = svg_filter_kernel_outset_css(filter.as_ref(), bbox);
          let kernel_left = kernel.left * scale;
          let kernel_top = kernel.top * scale;
          let kernel_right = kernel.right * scale;
          let kernel_bottom = kernel.bottom * scale;

          // Treat kernel padding like blur: it accumulates across the filter chain.
          if kernel_left.is_finite() {
            outset.left += kernel_left;
          }
          if kernel_top.is_finite() {
            outset.top += kernel_top;
          }
          if kernel_right.is_finite() {
            outset.right += kernel_right;
          }
          if kernel_bottom.is_finite() {
            outset.bottom += kernel_bottom;
          }

          // Visible output can still expand due to the declared filter region.
          outset.left = outset.left.max(delta_left);
          outset.top = outset.top.max(delta_top);
          outset.right = outset.right.max(delta_right);
          outset.bottom = outset.bottom.max(delta_bottom);
        }
      }
      _ => {}
    }
  }

  FilterOutset {
    left: outset.left.max(0.0),
    top: outset.top.max(0.0),
    right: outset.right.max(0.0),
    bottom: outset.bottom.max(0.0),
  }
}

/// Compute a conservative outset for tiled rendering.
///
/// This is similar to [`filter_outset_with_bounds`], but it accounts for filter *kernel
/// dependencies* that may require extra pixels even when the visible output does not expand.
///
/// In particular, `drop-shadow()` with a negative spread performs an erosion pass. The filter's
/// output might shrink (so the visible outset is reduced), but the erosion still needs to sample
/// transparent neighbors up to `abs(spread)` away. When rendering tiles in isolation, failing to
/// include that halo can cause tile edges to be treated as transparent, producing seams and
/// serial-vs-parallel divergence.
pub(crate) fn filter_halo_outset_with_bounds(
  filters: &[ResolvedFilter],
  scale: f32,
  bbox: Option<Rect>,
) -> FilterOutset {
  let mut outset = FilterOutset::ZERO;
  let scale = if scale.is_finite() { scale.abs() } else { 0.0 };

  for filter in filters {
    match filter {
      ResolvedFilter::Blur(radius) => {
        let delta = (*radius * scale).abs() * 3.0;
        outset.left += delta;
        outset.top += delta;
        outset.right += delta;
        outset.bottom += delta;
      }
      ResolvedFilter::DropShadow {
        offset_x,
        offset_y,
        blur_radius,
        spread,
        ..
      } => {
        let dx = offset_x * scale;
        let dy = offset_y * scale;
        let blur = blur_radius * scale;
        let spread = spread * scale;
        // Unlike `filter_outset_with_bounds`, use `abs(spread)` so negative spreads still request
        // a halo large enough for the erosion kernel.
        let delta = blur.abs() * 3.0 + spread.abs();

        let shadow_left = outset.left + delta - dx;
        let shadow_right = outset.right + delta + dx;
        let shadow_top = outset.top + delta - dy;
        let shadow_bottom = outset.bottom + delta + dy;

        outset.left = outset.left.max(shadow_left);
        outset.top = outset.top.max(shadow_top);
        outset.right = outset.right.max(shadow_right);
        outset.bottom = outset.bottom.max(shadow_bottom);
      }
      ResolvedFilter::SvgFilter(filter) => {
        if let Some(bbox) = bbox {
          let region = filter.resolve_region(bbox);
          let delta_left = (bbox.min_x() - region.min_x()).max(0.0) * scale;
          let delta_top = (bbox.min_y() - region.min_y()).max(0.0) * scale;
          let delta_right = (region.max_x() - bbox.max_x()).max(0.0) * scale;
          let delta_bottom = (region.max_y() - bbox.max_y()).max(0.0) * scale;

          let kernel = svg_filter_kernel_outset_css(filter.as_ref(), bbox);
          let kernel_left = kernel.left * scale;
          let kernel_top = kernel.top * scale;
          let kernel_right = kernel.right * scale;
          let kernel_bottom = kernel.bottom * scale;

          if kernel_left.is_finite() {
            outset.left += kernel_left;
          }
          if kernel_top.is_finite() {
            outset.top += kernel_top;
          }
          if kernel_right.is_finite() {
            outset.right += kernel_right;
          }
          if kernel_bottom.is_finite() {
            outset.bottom += kernel_bottom;
          }

          outset.left = outset.left.max(delta_left);
          outset.top = outset.top.max(delta_top);
          outset.right = outset.right.max(delta_right);
          outset.bottom = outset.bottom.max(delta_bottom);
        }
      }
      _ => {}
    }
  }

  FilterOutset {
    left: outset.left.max(0.0),
    top: outset.top.max(0.0),
    right: outset.right.max(0.0),
    bottom: outset.bottom.max(0.0),
  }
}

/// Compute the maximum outset needed to accommodate the provided filters, returning a tuple.
///
/// `bbox` is the bounding box of the filtered content in CSS pixels. `scale`
/// is the device scale to convert CSS lengths to the target coordinate space.
pub fn compute_filter_outset(
  filters: &[impl FilterOutsetExt],
  bbox: Rect,
  scale: f32,
) -> (f32, f32, f32, f32) {
  let mut out = (0.0, 0.0, 0.0, 0.0);
  let scale = if scale.is_finite() { scale.abs() } else { 0.0 };

  for filter in filters {
    filter.expand_outset(bbox, scale, &mut out);
  }

  (
    out.0.max(0.0),
    out.1.max(0.0),
    out.2.max(0.0),
    out.3.max(0.0),
  )
}

/// Trait implemented by filters that can contribute to an outset.
pub trait FilterOutsetExt {
  fn expand_outset(&self, bbox: Rect, scale: f32, out: &mut (f32, f32, f32, f32));
}

impl FilterOutsetExt for ResolvedFilter {
  fn expand_outset(&self, bbox: Rect, scale: f32, out: &mut (f32, f32, f32, f32)) {
    match self {
      ResolvedFilter::Blur(radius) => {
        let delta = (radius * scale).abs() * 3.0;
        out.0 += delta;
        out.1 += delta;
        out.2 += delta;
        out.3 += delta;
      }
      ResolvedFilter::DropShadow {
        offset_x,
        offset_y,
        blur_radius,
        spread,
        ..
      } => {
        let dx = offset_x * scale;
        let dy = offset_y * scale;
        let blur = blur_radius * scale;
        let spread = spread * scale;
        let delta = (blur.abs() * 3.0 + spread).max(0.0);
        let shadow_left = out.0 + delta - dx;
        let shadow_right = out.2 + delta + dx;
        let shadow_top = out.1 + delta - dy;
        let shadow_bottom = out.3 + delta + dy;
        out.0 = out.0.max(shadow_left);
        out.2 = out.2.max(shadow_right);
        out.1 = out.1.max(shadow_top);
        out.3 = out.3.max(shadow_bottom);
      }
      ResolvedFilter::SvgFilter(filter) => {
        let region = filter.resolve_region(bbox);
        let delta_left = (bbox.min_x() - region.min_x()).max(0.0) * scale;
        let delta_top = (bbox.min_y() - region.min_y()).max(0.0) * scale;
        let delta_right = (region.max_x() - bbox.max_x()).max(0.0) * scale;
        let delta_bottom = (region.max_y() - bbox.max_y()).max(0.0) * scale;

        let kernel = svg_filter_kernel_outset_css(filter.as_ref(), bbox);
        let kernel_left = kernel.left * scale;
        let kernel_top = kernel.top * scale;
        let kernel_right = kernel.right * scale;
        let kernel_bottom = kernel.bottom * scale;

        if kernel_left.is_finite() {
          out.0 += kernel_left;
        }
        if kernel_top.is_finite() {
          out.1 += kernel_top;
        }
        if kernel_right.is_finite() {
          out.2 += kernel_right;
        }
        if kernel_bottom.is_finite() {
          out.3 += kernel_bottom;
        }

        out.0 = out.0.max(delta_left);
        out.1 = out.1.max(delta_top);
        out.2 = out.2.max(delta_right);
        out.3 = out.3.max(delta_bottom);
      }
      _ => {}
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::paint::svg_filter::{
    ChannelSelector, ColorInterpolationFilters, EdgeMode, FilterInput, FilterPrimitive, FilterStep,
    LightSource, SvgFilter, SvgFilterRegion, SvgFilterUnits, SvgLength,
  };
  use std::sync::Arc;

  #[test]
  fn blur_filters_accumulate() {
    let filters = vec![ResolvedFilter::Blur(2.0), ResolvedFilter::Blur(3.0)];
    let bbox = Rect::from_xywh(0.0, 0.0, 10.0, 10.0);
    let (l, t, r, b) = compute_filter_outset(&filters, bbox, 1.0);
    assert!(
      (l - 15.0).abs() < 0.01
        && (t - 15.0).abs() < 0.01
        && (r - 15.0).abs() < 0.01
        && (b - 15.0).abs() < 0.01,
      "blur outsets should accumulate across filters"
    );
  }

  #[test]
  fn svg_filter_region_expands_outset() {
    let mut filter = SvgFilter {
      color_interpolation_filters: ColorInterpolationFilters::LinearRGB,
      steps: Vec::new(),
      region: SvgFilterRegion {
        x: SvgLength::Percent(-0.1),
        y: SvgLength::Percent(-0.1),
        width: SvgLength::Percent(1.2),
        height: SvgLength::Percent(1.2),
        units: SvgFilterUnits::ObjectBoundingBox,
      },
      filter_res: None,
      primitive_units: SvgFilterUnits::ObjectBoundingBox,
      fingerprint: 0,
    };
    filter.refresh_fingerprint();
    let filters = vec![ResolvedFilter::SvgFilter(Arc::new(filter))];
    let bbox = Rect::from_xywh(10.0, 20.0, 100.0, 50.0);
    let (l, t, r, b) = compute_filter_outset(&filters, bbox, 1.0);
    assert!(
      (l - 10.0).abs() < 0.01
        && (r - 10.0).abs() < 0.01
        && (t - 5.0).abs() < 0.01
        && (b - 5.0).abs() < 0.01,
      "expected outset of 10,5,10,5 but got {l},{t},{r},{b}"
    );
  }

  #[test]
  fn svg_filter_gaussian_blur_expands_outset_even_when_region_matches_bbox() {
    let mut filter = SvgFilter {
      color_interpolation_filters: ColorInterpolationFilters::LinearRGB,
      steps: vec![FilterStep {
        result: None,
        color_interpolation_filters: None,
        primitive: FilterPrimitive::GaussianBlur {
          input: FilterInput::SourceGraphic,
          std_dev: (3.0, 3.0),
          edge_mode: EdgeMode::Duplicate,
        },
        region: None,
      }],
      // Filter region = bbox.
      region: SvgFilterRegion {
        x: SvgLength::Number(0.0),
        y: SvgLength::Number(0.0),
        width: SvgLength::Number(1.0),
        height: SvgLength::Number(1.0),
        units: SvgFilterUnits::ObjectBoundingBox,
      },
      filter_res: None,
      primitive_units: SvgFilterUnits::UserSpaceOnUse,
      fingerprint: 0,
    };
    filter.refresh_fingerprint();
    let filters = vec![ResolvedFilter::SvgFilter(Arc::new(filter))];
    let bbox = Rect::from_xywh(10.0, 20.0, 100.0, 50.0);
    let (l, t, r, b) = compute_filter_outset(&filters, bbox, 1.0);
    assert!(
      (l - 9.0).abs() < 0.01
        && (r - 9.0).abs() < 0.01
        && (t - 9.0).abs() < 0.01
        && (b - 9.0).abs() < 0.01,
      "expected gaussian blur to contribute 9px padding, got {l},{t},{r},{b}"
    );
  }

  #[test]
  fn svg_filter_convolve_matrix_expands_outset() {
    let mut kernel = vec![0.0f32; 25];
    kernel[24] = 1.0;
    let mut filter = SvgFilter {
      color_interpolation_filters: ColorInterpolationFilters::LinearRGB,
      steps: vec![FilterStep {
        result: None,
        color_interpolation_filters: None,
        primitive: FilterPrimitive::ConvolveMatrix {
          input: FilterInput::SourceGraphic,
          order_x: 25,
          order_y: 1,
          kernel,
          divisor: None,
          bias: 0.0,
          target_x: 12,
          target_y: 0,
          edge_mode: EdgeMode::None,
          preserve_alpha: false,
        },
        region: None,
      }],
      // Filter region = bbox.
      region: SvgFilterRegion {
        x: SvgLength::Number(0.0),
        y: SvgLength::Number(0.0),
        width: SvgLength::Number(1.0),
        height: SvgLength::Number(1.0),
        units: SvgFilterUnits::ObjectBoundingBox,
      },
      filter_res: None,
      primitive_units: SvgFilterUnits::UserSpaceOnUse,
      fingerprint: 0,
    };
    filter.refresh_fingerprint();
    let filters = vec![ResolvedFilter::SvgFilter(Arc::new(filter))];
    let bbox = Rect::from_xywh(10.0, 20.0, 100.0, 50.0);
    let (l, t, r, b) = compute_filter_outset(&filters, bbox, 1.0);
    assert!(
      l >= 12.0 - 0.01 && r >= 12.0 - 0.01 && t >= 0.0 && b >= 0.0,
      "expected convolve matrix to contribute >=12px x padding, got {l},{t},{r},{b}"
    );
  }

  #[test]
  fn svg_filter_displacement_map_expands_outset() {
    let mut filter = SvgFilter {
      color_interpolation_filters: ColorInterpolationFilters::LinearRGB,
      steps: vec![FilterStep {
        result: None,
        color_interpolation_filters: None,
        primitive: FilterPrimitive::DisplacementMap {
          in1: FilterInput::SourceGraphic,
          in2: FilterInput::SourceGraphic,
          scale: 40.0,
          x_channel: ChannelSelector::A,
          y_channel: ChannelSelector::A,
        },
        region: None,
      }],
      // Filter region = bbox.
      region: SvgFilterRegion {
        x: SvgLength::Number(0.0),
        y: SvgLength::Number(0.0),
        width: SvgLength::Number(1.0),
        height: SvgLength::Number(1.0),
        units: SvgFilterUnits::ObjectBoundingBox,
      },
      filter_res: None,
      primitive_units: SvgFilterUnits::UserSpaceOnUse,
      fingerprint: 0,
    };
    filter.refresh_fingerprint();
    let filters = vec![ResolvedFilter::SvgFilter(Arc::new(filter))];
    let bbox = Rect::from_xywh(10.0, 20.0, 100.0, 50.0);
    let (l, t, r, b) = compute_filter_outset(&filters, bbox, 1.0);
    assert!(
      l >= 20.0 - 0.01
        && r >= 20.0 - 0.01
        && t >= 20.0 - 0.01
        && b >= 20.0 - 0.01,
      "expected displacement map to contribute >=20px padding, got {l},{t},{r},{b}"
    );
  }

  #[test]
  fn svg_filter_diffuse_lighting_expands_outset() {
    let mut filter = SvgFilter {
      color_interpolation_filters: ColorInterpolationFilters::LinearRGB,
      steps: vec![FilterStep {
        result: None,
        color_interpolation_filters: None,
        primitive: FilterPrimitive::DiffuseLighting {
          input: FilterInput::SourceGraphic,
          surface_scale: 1.0,
          diffuse_constant: 1.0,
          kernel_unit_length: Some((20.0, 0.0)),
          light: LightSource::Distant {
            azimuth: 0.0,
            elevation: 45.0,
          },
          lighting_color: crate::Rgba::WHITE,
        },
        region: None,
      }],
      // Filter region = bbox.
      region: SvgFilterRegion {
        x: SvgLength::Number(0.0),
        y: SvgLength::Number(0.0),
        width: SvgLength::Number(1.0),
        height: SvgLength::Number(1.0),
        units: SvgFilterUnits::ObjectBoundingBox,
      },
      filter_res: None,
      primitive_units: SvgFilterUnits::UserSpaceOnUse,
      fingerprint: 0,
    };
    filter.refresh_fingerprint();
    let filters = vec![ResolvedFilter::SvgFilter(Arc::new(filter))];
    let bbox = Rect::from_xywh(10.0, 20.0, 100.0, 50.0);
    let (l, t, r, b) = compute_filter_outset(&filters, bbox, 1.0);
    assert!(
      l >= 20.0 - 0.01 && r >= 20.0 - 0.01,
      "expected diffuse lighting to contribute >=20px x padding, got {l},{t},{r},{b}"
    );
  }
}
