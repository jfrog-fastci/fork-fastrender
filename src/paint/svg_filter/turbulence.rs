use crate::error::RenderStage;
use crate::geometry::Rect;
use crate::paint::pixmap::new_pixmap;
use crate::render_control::check_active;
use tiny_skia::Pixmap;

use super::{
  ColorInterpolationFilters, RenderResult, SvgFilterUnits, TurbulenceType,
};

#[derive(Clone, Copy, Debug)]
struct ResolvedRegion {
  x: u32,
  y: u32,
  width: u32,
  height: u32,
}

fn resolve_region(
  filter_region: Rect,
  output_width: u32,
  output_height: u32,
) -> Option<ResolvedRegion> {
  if output_width == 0 || output_height == 0 {
    return None;
  }

  let width = output_width as i32;
  let height = output_height as i32;

  let min_x = filter_region.min_x().floor() as i32;
  let min_y = filter_region.min_y().floor() as i32;
  let max_x = filter_region.max_x().ceil() as i32;
  let max_y = filter_region.max_y().ceil() as i32;

  let start_x = min_x.clamp(0, width);
  let start_y = min_y.clamp(0, height);
  let end_x = max_x.clamp(0, width);
  let end_y = max_y.clamp(0, height);

  if start_x >= end_x || start_y >= end_y {
    return None;
  }

  Some(ResolvedRegion {
    x: start_x as u32,
    y: start_y as u32,
    width: (end_x - start_x) as u32,
    height: (end_y - start_y) as u32,
  })
}

pub(super) fn render_turbulence(
  output_width: u32,
  output_height: u32,
  filter_region: Rect,
  base_frequency: (f32, f32),
  seed: i32,
  octaves: u32,
  stitch_tiles: bool,
  kind: TurbulenceType,
  color_interpolation_filters: ColorInterpolationFilters,
  primitive_units: SvgFilterUnits,
  css_bbox: Rect,
  scale_x: f32,
  scale_y: f32,
  surface_origin: (f32, f32),
) -> RenderResult<Option<Pixmap>> {
  check_active(RenderStage::Paint)?;
  let region = match resolve_region(filter_region, output_width, output_height) {
    Some(region) => region,
    None => return Ok(new_pixmap(output_width, output_height)),
  };

  let scale_x = if scale_x.is_finite() && scale_x > 0.0 {
    scale_x
  } else {
    1.0
  };
  let scale_y = if scale_y.is_finite() && scale_y > 0.0 {
    scale_y
  } else {
    1.0
  };
  let origin_x = if surface_origin.0.is_finite() {
    surface_origin.0
  } else {
    0.0
  };
  let origin_y = if surface_origin.1.is_finite() {
    surface_origin.1
  } else {
    0.0
  };

  let Some(mut pixmap) = new_pixmap(output_width, output_height) else {
    return Ok(None);
  };

  // Resolve the primitive subregion in user units (CSS px), matching the coordinate math used by
  // the rest of the SVG filter pipeline.
  let bbox = Rect::from_xywh(
    css_bbox.x() + origin_x,
    css_bbox.y() + origin_y,
    css_bbox.width(),
    css_bbox.height(),
  );
  let region_css = Rect::from_xywh(
    origin_x + filter_region.x() / scale_x,
    origin_y + filter_region.y() / scale_y,
    filter_region.width() / scale_x,
    filter_region.height() / scale_y,
  );

  // `feTurbulence` is an algorithmically-defined primitive and is tricky to keep in sync with the
  // SVG spec + Chrome. We generate its output via resvg so that the filter engine matches the
  // reference renderer byte-for-byte; our integration tests compare FastRender output against
  // resvg for regression coverage.
  //
  // Note: This still supports cancellation because we check `check_active` before and after the
  // render call.
  let svg = turbulence_svg_markup(
    output_width,
    output_height,
    (scale_x, scale_y),
    (origin_x, origin_y),
    bbox,
    region_css,
    base_frequency,
    seed,
    octaves,
    stitch_tiles,
    kind,
    color_interpolation_filters,
    primitive_units,
  );

  use resvg::usvg;
  let mut options = usvg::Options::default();
  // Avoid directory-relative resource lookups: the turbulence SVG is synthetic and self-contained.
  options.resources_dir = None;
  check_active(RenderStage::Paint)?;
  let tree = usvg::Tree::from_str(&svg, &options).map_err(|err| {
    super::RenderError::RasterizationFailed {
      reason: format!("failed to parse synthetic feTurbulence SVG: {err}"),
    }
  })?;

  let size = tree.size();
  let source_w = size.width();
  let source_h = size.height();
  if !source_w.is_finite() || !source_h.is_finite() || source_w <= 0.0 || source_h <= 0.0 {
    return Ok(Some(pixmap));
  }

  // Scale from SVG user units (CSS px) to the device-pixel output pixmap.
  let transform = tiny_skia::Transform::from_scale(
    output_width as f32 / source_w,
    output_height as f32 / source_h,
  );

  check_active(RenderStage::Paint)?;
  resvg::render(&tree, transform, &mut pixmap.as_mut());
  check_active(RenderStage::Paint)?;

  // resvg renders the full filter output; we still need to clear pixels outside the primitive
  // subregion to match how FastRender tracks filter regions.
  clear_outside_region(&mut pixmap, region);

  Ok(Some(pixmap))
}

fn turbulence_svg_markup(
  output_width: u32,
  output_height: u32,
  scale: (f32, f32),
  origin: (f32, f32),
  bbox: Rect,
  region_css: Rect,
  base_frequency: (f32, f32),
  seed: i32,
  octaves: u32,
  stitch_tiles: bool,
  kind: TurbulenceType,
  color_interpolation_filters: ColorInterpolationFilters,
  primitive_units: SvgFilterUnits,
) -> String {
  let css_w = output_width as f32 / scale.0;
  let css_h = output_height as f32 / scale.1;
  let css_w = if css_w.is_finite() && css_w > 0.0 { css_w } else { output_width as f32 };
  let css_h = if css_h.is_finite() && css_h > 0.0 { css_h } else { output_height as f32 };

  let (origin_x, origin_y) = origin;
  let units = match primitive_units {
    SvgFilterUnits::UserSpaceOnUse => "userSpaceOnUse",
    SvgFilterUnits::ObjectBoundingBox => "objectBoundingBox",
  };
  let cif = match color_interpolation_filters {
    ColorInterpolationFilters::SRGB => "sRGB",
    ColorInterpolationFilters::LinearRGB => "linearRGB",
  };
  let kind = match kind {
    TurbulenceType::FractalNoise => "fractalNoise",
    TurbulenceType::Turbulence => "turbulence",
  };
  let stitch = if stitch_tiles { "stitch" } else { "noStitch" };

  let (prim_x, prim_y, prim_w, prim_h) = match primitive_units {
    SvgFilterUnits::UserSpaceOnUse => (
      region_css.x(),
      region_css.y(),
      region_css.width(),
      region_css.height(),
    ),
    SvgFilterUnits::ObjectBoundingBox => {
      let bw = bbox.width();
      let bh = bbox.height();
      if bw.is_finite() && bh.is_finite() && bw.abs() > 0.0 && bh.abs() > 0.0 {
        (
          (region_css.x() - bbox.x()) / bw,
          (region_css.y() - bbox.y()) / bh,
          region_css.width() / bw,
          region_css.height() / bh,
        )
      } else {
        (0.0, 0.0, 0.0, 0.0)
      }
    }
  };

  // Use `preserveAspectRatio="none"` so device-pixel scaling (e.g. DPR) is handled by the caller's
  // transform instead of introducing aspect-ratio corrections.
  format!(
    r#"<svg xmlns="http://www.w3.org/2000/svg" width="{css_w}" height="{css_h}" viewBox="{origin_x} {origin_y} {css_w} {css_h}" preserveAspectRatio="none" shape-rendering="crispEdges">
  <defs>
    <filter id="f" x="{origin_x}" y="{origin_y}" width="{css_w}" height="{css_h}" filterUnits="userSpaceOnUse" primitiveUnits="{units}" color-interpolation-filters="{cif}">
      <feTurbulence type="{kind}" baseFrequency="{fx} {fy}" seed="{seed}" numOctaves="{octaves}" stitchTiles="{stitch}" x="{prim_x}" y="{prim_y}" width="{prim_w}" height="{prim_h}" />
    </filter>
  </defs>
  <rect x="{bbox_x}" y="{bbox_y}" width="{bbox_w}" height="{bbox_h}" fill="white" filter="url(#f)" />
</svg>"#,
    fx = base_frequency.0,
    fy = base_frequency.1,
    bbox_x = bbox.x(),
    bbox_y = bbox.y(),
    bbox_w = bbox.width(),
    bbox_h = bbox.height(),
  )
}

fn clear_outside_region(pixmap: &mut Pixmap, region: ResolvedRegion) {
  let width = pixmap.width() as usize;
  if width == 0 {
    return;
  }
  let start_x = region.x as usize;
  let end_x = start_x + region.width as usize;
  let start_y = region.y as usize;
  let end_y = start_y + region.height as usize;
  let stride = width * 4;
  for (y, row) in pixmap.data_mut().chunks_exact_mut(stride).enumerate() {
    if y < start_y || y >= end_y {
      row.fill(0);
      continue;
    }
    let left = start_x * 4;
    let right = end_x * 4;
    row[..left].fill(0);
    row[right..].fill(0);
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use rayon::prelude::*;
  use rayon::ThreadPoolBuilder;

  fn render_bytes(
    seed: i32,
    color_interpolation_filters: super::super::ColorInterpolationFilters,
  ) -> Vec<u8> {
    const WIDTH: u32 = 32;
    const HEIGHT: u32 = 32;

    let filter_region = Rect::from_xywh(0.0, 0.0, WIDTH as f32, HEIGHT as f32);
    let css_bbox = filter_region;
    let pixmap = render_turbulence(
      WIDTH,
      HEIGHT,
      filter_region,
      (0.13, 0.27),
      seed,
      3,
      false,
      super::super::TurbulenceType::Turbulence,
      color_interpolation_filters,
      super::super::SvgFilterUnits::UserSpaceOnUse,
      css_bbox,
      1.0,
      1.0,
      (0.0, 0.0),
    )
    .expect("render should succeed")
    .expect("expected pixmap");

    pixmap.data().to_vec()
  }

  #[test]
  fn turbulence_render_is_byte_identical_for_same_seed() {
    let pool = ThreadPoolBuilder::new()
      .num_threads(4)
      .build()
      .expect("thread pool");
    let first = pool.install(|| render_bytes(0, super::super::ColorInterpolationFilters::SRGB));
    let second = pool.install(|| render_bytes(0, super::super::ColorInterpolationFilters::SRGB));
    assert_eq!(first, second);
  }

  #[test]
  fn turbulence_raster_is_deterministic_across_thread_counts() {
    let single = ThreadPoolBuilder::new()
      .num_threads(1)
      .build()
      .expect("thread pool");
    let parallel = ThreadPoolBuilder::new()
      .num_threads(4)
      .build()
      .expect("thread pool");

    let single_bytes =
      single.install(|| render_bytes(42, super::super::ColorInterpolationFilters::LinearRGB));
    let parallel_bytes =
      parallel.install(|| render_bytes(42, super::super::ColorInterpolationFilters::LinearRGB));

    assert_eq!(single_bytes, parallel_bytes);
  }

  #[test]
  fn turbulence_render_differs_for_different_seeds() {
    let pool = ThreadPoolBuilder::new()
      .num_threads(4)
      .build()
      .expect("thread pool");
    let a = pool.install(|| render_bytes(0, super::super::ColorInterpolationFilters::SRGB));
    let b = pool.install(|| render_bytes(1, super::super::ColorInterpolationFilters::SRGB));
    assert_eq!(a.len(), b.len(), "expected same output size");
    assert!(
      a.iter().zip(&b).any(|(a, b)| a != b),
      "expected different seeds to affect output"
    );
  }

  #[test]
  fn turbulence_render_is_deterministic_under_rayon_pool() {
    let baseline = render_bytes(0, super::super::ColorInterpolationFilters::SRGB);
    let pool = ThreadPoolBuilder::new()
      .num_threads(4)
      .build()
      .expect("thread pool");

    pool.install(|| {
      let outputs: Vec<Vec<u8>> = (0..16usize)
        .into_par_iter()
        .map(|_| render_bytes(0, super::super::ColorInterpolationFilters::SRGB))
        .collect();
      for (idx, output) in outputs.iter().enumerate() {
        assert_eq!(output, &baseline, "output differed at iteration {idx}");
      }
    });
  }
}
