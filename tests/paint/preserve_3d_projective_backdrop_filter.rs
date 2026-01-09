use fastrender::debug::runtime::{with_thread_runtime_toggles, RuntimeToggles};
use fastrender::paint::display_list::{
  BlendMode, BorderRadii, DisplayItem, DisplayList, FillRectItem, ResolvedFilter, StackingContextItem,
  Transform3D,
};
use fastrender::paint::display_list_renderer::DisplayListRenderer;
use fastrender::style::color::Rgba;
use fastrender::style::types::{BackfaceVisibility, TransformStyle};
use fastrender::text::font_loader::FontContext;
use fastrender::Rect;
use std::collections::HashMap;
use std::sync::Arc;

fn context(bounds: Rect, transform_style: TransformStyle) -> StackingContextItem {
  StackingContextItem {
    z_index: 0,
    creates_stacking_context: true,
    is_root: false,
    establishes_backdrop_root: false,
    has_backdrop_sensitive_descendants: false,
    bounds,
    plane_rect: bounds,
    mix_blend_mode: BlendMode::Normal,
    opacity: 1.0,
    is_isolated: false,
    transform: None,
    child_perspective: None,
    transform_style,
    backface_visibility: BackfaceVisibility::Visible,
    filters: Vec::new(),
    backdrop_filters: Vec::new(),
    radii: BorderRadii::ZERO,
    mask: None,
    has_clip_path: false,
  }
}

fn signed_area(quad: &[(f32, f32); 4]) -> f32 {
  let mut area = 0.0;
  for i in 0..4 {
    let (x0, y0) = quad[i];
    let (x1, y1) = quad[(i + 1) % 4];
    area += x0 * y1 - x1 * y0;
  }
  area * 0.5
}

fn point_in_convex_quad(pt: (f32, f32), quad: &[(f32, f32); 4]) -> bool {
  let ccw = signed_area(quad) >= 0.0;
  const EPS: f32 = 1e-3;
  for i in 0..4 {
    let (x0, y0) = quad[i];
    let (x1, y1) = quad[(i + 1) % 4];
    let cross = (x1 - x0) * (pt.1 - y0) - (y1 - y0) * (pt.0 - x0);
    if ccw {
      if cross < -EPS {
        return false;
      }
    } else if cross > EPS {
      return false;
    }
  }
  true
}

fn signed_distance_to_edge(pt: (f32, f32), quad: &[(f32, f32); 4], edge: usize) -> f32 {
  let ccw = signed_area(quad) >= 0.0;
  let (x0, y0) = quad[edge];
  let (x1, y1) = quad[(edge + 1) % 4];
  let vx = x1 - x0;
  let vy = y1 - y0;
  let len = (vx * vx + vy * vy).sqrt();
  if !len.is_finite() || len <= 1e-6 {
    return f32::NAN;
  }
  let cross = vx * (pt.1 - y0) - vy * (pt.0 - x0);
  if ccw {
    cross / len
  } else {
    -cross / len
  }
}

fn signed_min_distance(pt: (f32, f32), quad: &[(f32, f32); 4]) -> f32 {
  (0..4)
    .map(|edge| signed_distance_to_edge(pt, quad, edge))
    .fold(f32::INFINITY, |a, b| a.min(b))
}

fn find_inside_sample(
  quad: &[(f32, f32); 4],
  aabb: (i32, i32, i32, i32),
  w: u32,
  h: u32,
) -> (u32, u32) {
  let cx = quad.iter().map(|(x, _)| x).sum::<f32>() / 4.0;
  let cy = quad.iter().map(|(_, y)| y).sum::<f32>() / 4.0;
  let base_x = cx.round() as i32;
  let base_y = cy.round() as i32;

  let mut best: Option<(u32, u32, f32)> = None;
  for dy in -12..=12 {
    for dx in -12..=12 {
      let x = base_x + dx;
      let y = base_y + dy;
      if x < 0 || y < 0 || x >= w as i32 || y >= h as i32 {
        continue;
      }
      if x < aabb.0 || x > aabb.2 || y < aabb.1 || y > aabb.3 {
        continue;
      }
      let pt = (x as f32 + 0.5, y as f32 + 0.5);
      if !point_in_convex_quad(pt, quad) {
        continue;
      }
      let dist = signed_min_distance(pt, quad);
      if !dist.is_finite() {
        continue;
      }
      let replace = best.as_ref().map_or(true, |(_, _, best_dist)| dist > *best_dist);
      if replace {
        best = Some((x as u32, y as u32, dist));
      }
    }
  }

  let Some((x, y, dist)) = best else {
    panic!("failed to find a pixel inside projected quad");
  };
  assert!(
    dist >= 2.0,
    "expected to find a point sufficiently inside the projected quad (min dist {dist:.2}px)"
  );
  (x, y)
}

fn find_outside_sample_for_edge(
  quad: &[(f32, f32); 4],
  edge: usize,
  aabb: (i32, i32, i32, i32),
  w: u32,
  h: u32,
) -> (u32, u32) {
  let ccw = signed_area(quad) >= 0.0;
  let (x0, y0) = quad[edge];
  let (x1, y1) = quad[(edge + 1) % 4];
  let mx = (x0 + x1) * 0.5;
  let my = (y0 + y1) * 0.5;
  let vx = x1 - x0;
  let vy = y1 - y0;
  let (mut nx, mut ny) = if ccw { (vy, -vx) } else { (-vy, vx) };
  let nlen = (nx * nx + ny * ny).sqrt();
  if !nlen.is_finite() || nlen <= 1e-6 {
    panic!("degenerate quad edge");
  }
  nx /= nlen;
  ny /= nlen;

  for delta in [10.0, 8.0, 6.0, 4.0, 3.0, 2.0, 1.0] {
    let base_x = (mx + nx * delta).round() as i32;
    let base_y = (my + ny * delta).round() as i32;

    for dy in -3..=3 {
      for dx in -3..=3 {
        let x = base_x + dx;
        let y = base_y + dy;
        if x < 0 || y < 0 || x >= w as i32 || y >= h as i32 {
          continue;
        }
        // Ensure the sample is inside the quad's AABB, but allow it to be close to the edge we are
        // testing.
        if x < aabb.0 || x > aabb.2 || y < aabb.1 || y > aabb.3 {
          continue;
        }
        let pt = (x as f32 + 0.5, y as f32 + 0.5);
        let edge_dist = signed_distance_to_edge(pt, quad, edge);
        if edge_dist.is_finite() && edge_dist <= -2.0 {
          return (x as u32, y as u32);
        }
      }
    }
  }

  panic!("failed to find sample pixel outside projected quad edge {edge}");
}

#[test]
fn preserve_3d_projective_plane_backdrop_filter_is_clipped_to_projected_quad() {
  let plane = Rect::from_xywh(20.0, 20.0, 60.0, 40.0);
  let root_bounds = Rect::from_xywh(0.0, 0.0, 120.0, 100.0);
  let perspective = Transform3D::perspective(200.0);
  let center = (
    plane.x() + plane.width() * 0.5,
    plane.y() + plane.height() * 0.5,
  );
  let rotate = Transform3D::translate(center.0, center.1, 0.0)
    .multiply(&Transform3D::rotate_y(70_f32.to_radians()))
    .multiply(&Transform3D::rotate_x(25_f32.to_radians()))
    .multiply(&Transform3D::translate(-center.0, -center.1, 0.0));

  let mut list = DisplayList::new();
  // Page background.
  list.push(DisplayItem::FillRect(FillRectItem {
    rect: root_bounds,
    color: Rgba::GREEN,
  }));

  let mut preserve_root = context(root_bounds, TransformStyle::Preserve3d);
  preserve_root.child_perspective = Some(perspective);
  list.push(DisplayItem::PushStackingContext(preserve_root));

  // Flat plane with a projective transform and a backdrop-filter.
  let mut filtered_plane = context(plane, TransformStyle::Flat);
  filtered_plane.z_index = 1;
  filtered_plane.transform = Some(rotate);
  filtered_plane.backdrop_filters = vec![ResolvedFilter::Invert(1.0)];
  filtered_plane.is_isolated = true;
  filtered_plane.establishes_backdrop_root = true;
  filtered_plane.has_backdrop_sensitive_descendants = true;
  list.push(DisplayItem::PushStackingContext(filtered_plane));
  list.push(DisplayItem::PopStackingContext);

  list.push(DisplayItem::PopStackingContext); // preserve-3d root

  let toggles = Arc::new(RuntimeToggles::from_map(HashMap::from([
    (
      "FASTR_PRESERVE3D_DISABLE_SCENE".to_string(),
      "0".to_string(),
    ),
    (
      "FASTR_PRESERVE3D_DISABLE_WARP".to_string(),
      "0".to_string(),
    ),
  ])));

  let pixmap = with_thread_runtime_toggles(toggles, || {
    DisplayListRenderer::new(
      root_bounds.width() as u32,
      root_bounds.height() as u32,
      Rgba::TRANSPARENT,
      FontContext::new(),
    )
    .unwrap()
    .render(&list)
    .unwrap()
  });

  let adjusted = perspective
    .multiply(&rotate)
    .multiply(&Transform3D::translate(plane.x(), plane.y(), 0.0));
  let corners = [
    (0.0, 0.0),
    (plane.width(), 0.0),
    (plane.width(), plane.height()),
    (0.0, plane.height()),
  ];
  let mut quad = [(0.0f32, 0.0f32); 4];
  for (idx, (x, y)) in corners.iter().enumerate() {
    let (tx, ty, _tz, tw) = adjusted.transform_point(*x, *y, 0.0);
    assert!(
      tw.is_finite() && tw.abs() >= Transform3D::MIN_PROJECTIVE_W,
      "expected stable projection"
    );
    quad[idx] = (tx / tw, ty / tw);
  }

  let (mut min_x, mut min_y, mut max_x, mut max_y) = (
    f32::INFINITY,
    f32::INFINITY,
    f32::NEG_INFINITY,
    f32::NEG_INFINITY,
  );
  for (x, y) in quad {
    min_x = min_x.min(x);
    min_y = min_y.min(y);
    max_x = max_x.max(x);
    max_y = max_y.max(y);
  }

  let w = pixmap.width();
  let h = pixmap.height();
  let clamp = |v: i32, max: i32| v.clamp(0, max);
  let aabb = (
    clamp(min_x.floor() as i32, w as i32 - 1),
    clamp(min_y.floor() as i32, h as i32 - 1),
    clamp(max_x.ceil() as i32 - 1, w as i32 - 1),
    clamp(max_y.ceil() as i32 - 1, h as i32 - 1),
  );

  let inside = find_inside_sample(&quad, aabb, w, h);
  let inside_px = pixmap.pixel(inside.0, inside.1).expect("pixel in-bounds");
  assert_eq!(
    (inside_px.red(), inside_px.green(), inside_px.blue(), inside_px.alpha()),
    (255, 0, 255, 255),
    "expected inverted green (magenta) inside the projected quad"
  );

  for edge in 0..4 {
    let pt = find_outside_sample_for_edge(&quad, edge, aabb, w, h);
    let px = pixmap.pixel(pt.0, pt.1).expect("pixel in-bounds");
    assert_eq!(
      (px.red(), px.green(), px.blue(), px.alpha()),
      (0, 255, 0, 255),
      "expected backdrop-filter output to be clipped to the projected quad (edge {edge})"
    );
  }
}
