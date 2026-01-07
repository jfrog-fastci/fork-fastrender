use crate::error::{RenderError, RenderStage};
use crate::render_control::check_active_periodic;
use tiny_skia::Path;
use tiny_skia::PathBuilder;

const SVG_PATH_DEADLINE_STRIDE: usize = 256;

/// Parse SVG path data into a [`tiny_skia::Path`].
///
/// This is shared by:
/// - SVG image decoding (`<path d="...">`)
/// - CSS features that reuse SVG path syntax (e.g. `clip-path: path("...")`)
///
/// Returns `Ok(None)` when the path data is syntactically invalid.
pub(crate) fn build_tiny_skia_path_from_svg_path_data(
  data: &str,
  deadline_counter: &mut usize,
  deadline_stride: usize,
  stage: RenderStage,
) -> std::result::Result<Option<Path>, RenderError> {
  use svgtypes::PathParser;
  use svgtypes::PathSegment;

  let mut pb = PathBuilder::new();
  let mut current = (0.0f32, 0.0f32);
  let mut subpath_start = (0.0f32, 0.0f32);
  let mut last_cubic_ctrl: Option<(f32, f32)> = None;
  let mut last_quad_ctrl: Option<(f32, f32)> = None;

  for segment in PathParser::from(data) {
    check_active_periodic(deadline_counter, deadline_stride, stage)?;
    let seg = match segment {
      Ok(seg) => seg,
      Err(_) => return Ok(None),
    };
    match seg {
      PathSegment::MoveTo { abs, x, y } => {
        let (nx, ny) = if abs {
          (x as f32, y as f32)
        } else {
          (current.0 + x as f32, current.1 + y as f32)
        };
        pb.move_to(nx, ny);
        current = (nx, ny);
        subpath_start = current;
        last_cubic_ctrl = None;
        last_quad_ctrl = None;
      }
      PathSegment::LineTo { abs, x, y } => {
        let (nx, ny) = if abs {
          (x as f32, y as f32)
        } else {
          (current.0 + x as f32, current.1 + y as f32)
        };
        pb.line_to(nx, ny);
        current = (nx, ny);
        last_cubic_ctrl = None;
        last_quad_ctrl = None;
      }
      PathSegment::HorizontalLineTo { abs, x } => {
        let nx = if abs { x as f32 } else { current.0 + x as f32 };
        pb.line_to(nx, current.1);
        current.0 = nx;
        last_cubic_ctrl = None;
        last_quad_ctrl = None;
      }
      PathSegment::VerticalLineTo { abs, y } => {
        let ny = if abs { y as f32 } else { current.1 + y as f32 };
        pb.line_to(current.0, ny);
        current.1 = ny;
        last_cubic_ctrl = None;
        last_quad_ctrl = None;
      }
      PathSegment::CurveTo {
        abs,
        x1,
        y1,
        x2,
        y2,
        x,
        y,
      } => {
        let (cx1, cy1) = if abs {
          (x1 as f32, y1 as f32)
        } else {
          (current.0 + x1 as f32, current.1 + y1 as f32)
        };
        let (cx2, cy2) = if abs {
          (x2 as f32, y2 as f32)
        } else {
          (current.0 + x2 as f32, current.1 + y2 as f32)
        };
        let (nx, ny) = if abs {
          (x as f32, y as f32)
        } else {
          (current.0 + x as f32, current.1 + y as f32)
        };
        pb.cubic_to(cx1, cy1, cx2, cy2, nx, ny);
        current = (nx, ny);
        last_cubic_ctrl = Some((cx2, cy2));
        last_quad_ctrl = None;
      }
      PathSegment::SmoothCurveTo { abs, x2, y2, x, y } => {
        let (cx1, cy1) = match last_cubic_ctrl {
          Some((px, py)) => (2.0 * current.0 - px, 2.0 * current.1 - py),
          None => current,
        };
        let (cx2, cy2) = if abs {
          (x2 as f32, y2 as f32)
        } else {
          (current.0 + x2 as f32, current.1 + y2 as f32)
        };
        let (nx, ny) = if abs {
          (x as f32, y as f32)
        } else {
          (current.0 + x as f32, current.1 + y as f32)
        };
        pb.cubic_to(cx1, cy1, cx2, cy2, nx, ny);
        current = (nx, ny);
        last_cubic_ctrl = Some((cx2, cy2));
        last_quad_ctrl = None;
      }
      PathSegment::Quadratic { abs, x1, y1, x, y } => {
        let (cx1, cy1) = if abs {
          (x1 as f32, y1 as f32)
        } else {
          (current.0 + x1 as f32, current.1 + y1 as f32)
        };
        let (nx, ny) = if abs {
          (x as f32, y as f32)
        } else {
          (current.0 + x as f32, current.1 + y as f32)
        };
        pb.quad_to(cx1, cy1, nx, ny);
        current = (nx, ny);
        last_quad_ctrl = Some((cx1, cy1));
        last_cubic_ctrl = None;
      }
      PathSegment::SmoothQuadratic { abs, x, y } => {
        let (cx1, cy1) = match last_quad_ctrl {
          Some((px, py)) => (2.0 * current.0 - px, 2.0 * current.1 - py),
          None => current,
        };
        let (nx, ny) = if abs {
          (x as f32, y as f32)
        } else {
          (current.0 + x as f32, current.1 + y as f32)
        };
        pb.quad_to(cx1, cy1, nx, ny);
        current = (nx, ny);
        last_quad_ctrl = Some((cx1, cy1));
        last_cubic_ctrl = None;
      }
      PathSegment::EllipticalArc {
        abs,
        rx,
        ry,
        x_axis_rotation,
        large_arc,
        sweep,
        x,
        y,
      } => {
        let (nx, ny) = if abs {
          (x as f32, y as f32)
        } else {
          (current.0 + x as f32, current.1 + y as f32)
        };

        if !arc_to_cubic_beziers(
          &mut pb,
          current,
          (rx as f32).abs(),
          (ry as f32).abs(),
          x_axis_rotation as f32,
          large_arc,
          sweep,
          (nx, ny),
        ) {
          pb.line_to(nx, ny);
        }

        current = (nx, ny);
        last_cubic_ctrl = None;
        last_quad_ctrl = None;
      }
      PathSegment::ClosePath { .. } => {
        pb.close();
        current = subpath_start;
        last_cubic_ctrl = None;
        last_quad_ctrl = None;
      }
    }
  }

  Ok(pb.finish())
}

/// Variant of [`build_tiny_skia_path_from_svg_path_data`] for call sites that don't have (or don't
/// want) deadline accounting, but still need deadline checks to be effective.
pub(crate) fn build_tiny_skia_path_from_svg_path_data_checked(
  data: &str,
  stage: RenderStage,
) -> std::result::Result<Option<Path>, RenderError> {
  let mut counter = 0usize;
  build_tiny_skia_path_from_svg_path_data(data, &mut counter, SVG_PATH_DEADLINE_STRIDE, stage)
}

/// Variant of [`build_tiny_skia_path_from_svg_path_data`] for call sites that only care about
/// syntax validity.
///
/// Deadline checks are disabled. This function must not be used from renderer hot paths where
/// `RenderOptions::timeout` must remain effective.
pub(crate) fn build_tiny_skia_path_from_svg_path_data_unchecked(data: &str) -> Option<Path> {
  let mut counter = 0usize;
  match build_tiny_skia_path_from_svg_path_data(data, &mut counter, 0, RenderStage::Paint) {
    Ok(path) => path,
    Err(err) => panic!("unexpected render error with deadline checks disabled: {err:?}"),
  }
}

fn arc_to_cubic_beziers(
  pb: &mut PathBuilder,
  start: (f32, f32),
  rx: f32,
  ry: f32,
  x_axis_rotation_deg: f32,
  large_arc: bool,
  sweep: bool,
  end: (f32, f32),
) -> bool {
  if !start.0.is_finite()
    || !start.1.is_finite()
    || !end.0.is_finite()
    || !end.1.is_finite()
    || !rx.is_finite()
    || !ry.is_finite()
    || !x_axis_rotation_deg.is_finite()
  {
    return false;
  }
  if (start.0 - end.0).abs() < f32::EPSILON && (start.1 - end.1).abs() < f32::EPSILON {
    return true;
  }
  if rx <= 0.0 || ry <= 0.0 {
    return false;
  }

  let x0 = start.0 as f64;
  let y0 = start.1 as f64;
  let x1 = end.0 as f64;
  let y1 = end.1 as f64;
  let mut rx = rx as f64;
  let mut ry = ry as f64;

  let phi = (x_axis_rotation_deg as f64).to_radians();
  let (sin_phi, cos_phi) = phi.sin_cos();

  let dx2 = (x0 - x1) * 0.5;
  let dy2 = (y0 - y1) * 0.5;
  let x1p = cos_phi * dx2 + sin_phi * dy2;
  let y1p = -sin_phi * dx2 + cos_phi * dy2;

  let rx_sq = rx * rx;
  let ry_sq = ry * ry;
  let x1p_sq = x1p * x1p;
  let y1p_sq = y1p * y1p;

  let lambda = (x1p_sq / rx_sq) + (y1p_sq / ry_sq);
  if lambda > 1.0 {
    let scale = lambda.sqrt();
    rx *= scale;
    ry *= scale;
  }

  let rx_sq = rx * rx;
  let ry_sq = ry * ry;
  let denom = rx_sq * y1p_sq + ry_sq * x1p_sq;
  if denom.abs() < f64::EPSILON {
    return false;
  }
  let mut num = rx_sq * ry_sq - rx_sq * y1p_sq - ry_sq * x1p_sq;
  if num < 0.0 {
    num = 0.0;
  }
  let sign = if large_arc == sweep { -1.0 } else { 1.0 };
  let coef = sign * (num / denom).sqrt();
  let cxp = coef * (rx * y1p) / ry;
  let cyp = coef * (-ry * x1p) / rx;

  let cx = cos_phi * cxp - sin_phi * cyp + (x0 + x1) * 0.5;
  let cy = sin_phi * cxp + cos_phi * cyp + (y0 + y1) * 0.5;

  let v1x = (x1p - cxp) / rx;
  let v1y = (y1p - cyp) / ry;
  let v2x = (-x1p - cxp) / rx;
  let v2y = (-y1p - cyp) / ry;

  fn vector_angle(ux: f64, uy: f64, vx: f64, vy: f64) -> f64 {
    let dot = ux * vx + uy * vy;
    let det = ux * vy - uy * vx;
    det.atan2(dot)
  }

  let mut start_angle = vector_angle(1.0, 0.0, v1x, v1y);
  let mut delta_angle = vector_angle(v1x, v1y, v2x, v2y);

  if !sweep && delta_angle > 0.0 {
    delta_angle -= std::f64::consts::TAU;
  } else if sweep && delta_angle < 0.0 {
    delta_angle += std::f64::consts::TAU;
  }

  // Split the arc into segments no larger than 90 degrees.
  let segments = (delta_angle.abs() / (std::f64::consts::FRAC_PI_2))
    .ceil()
    .max(1.0) as usize;
  let seg_angle = delta_angle / segments as f64;

  for _ in 0..segments {
    let theta1 = start_angle;
    let theta2 = theta1 + seg_angle;
    start_angle = theta2;

    let (sin_t1, cos_t1) = theta1.sin_cos();
    let (sin_t2, cos_t2) = theta2.sin_cos();

    let alpha = (4.0 / 3.0) * ((theta2 - theta1) * 0.25).tan();

    let x1 = cos_t1;
    let y1 = sin_t1;
    let x2 = cos_t2;
    let y2 = sin_t2;

    let cp1x = x1 - alpha * y1;
    let cp1y = y1 + alpha * x1;
    let cp2x = x2 + alpha * y2;
    let cp2y = y2 - alpha * x2;

    let map = |x: f64, y: f64| -> (f64, f64) {
      (
        cx + cos_phi * rx * x - sin_phi * ry * y,
        cy + sin_phi * rx * x + cos_phi * ry * y,
      )
    };

    let (c1x, c1y) = map(cp1x, cp1y);
    let (c2x, c2y) = map(cp2x, cp2y);
    let (ex, ey) = map(x2, y2);
    pb.cubic_to(
      c1x as f32, c1y as f32, c2x as f32, c2y as f32, ex as f32, ey as f32,
    );
  }

  true
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::render_control::{with_deadline, RenderDeadline};
  use std::sync::atomic::{AtomicUsize, Ordering};
  use std::sync::Arc;

  #[test]
  fn svg_path_parser_respects_cancel_callback() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_cb = Arc::clone(&calls);
    let cancel = Arc::new(move || calls_cb.fetch_add(1, Ordering::SeqCst) >= 1);
    let deadline = RenderDeadline::new(None, Some(cancel));

    let mut data = String::from("M0 0");
    for _ in 0..2048 {
      data.push_str(" L1 1");
    }

    let result = with_deadline(Some(&deadline), || {
      build_tiny_skia_path_from_svg_path_data_checked(&data, RenderStage::Paint)
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
}
