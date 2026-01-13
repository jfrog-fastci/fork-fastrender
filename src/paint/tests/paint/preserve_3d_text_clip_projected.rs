use crate::geometry::Point;
use crate::paint::display_list::{
  BlendMode, BorderRadii, ClipItem, ClipShape, DisplayItem, DisplayList, FillRectItem,
  GlyphInstance, StackingContextItem, TextItem, Transform3D,
};
use crate::paint::display_list_renderer::DisplayListRenderer;
use crate::style::color::Rgba;
use crate::style::types::{BackfaceVisibility, FontSmoothing, TextRendering, TransformStyle};
use crate::text::font_db::FontConfig;
use crate::text::font_loader::FontContext;
use crate::Rect;
use std::path::PathBuf;
use std::sync::Arc;
use tiny_skia::Pixmap;

fn pixel(pixmap: &Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let px = pixmap.pixel(x, y).expect("pixel in bounds");
  (px.red(), px.green(), px.blue(), px.alpha())
}

fn ctx(
  bounds: Rect,
  transform_style: TransformStyle,
  transform: Option<Transform3D>,
  child_perspective: Option<Transform3D>,
) -> StackingContextItem {
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
    transform,
    child_perspective,
    transform_style,
    backface_visibility: BackfaceVisibility::Visible,
    filters: Vec::new(),
    backdrop_filters: Vec::new(),
    radii: BorderRadii::ZERO,
    mask: None,
    mask_border: None,
    has_clip_path: false,
  }
}

fn glyph_instances_from_text_run(
  run: &crate::text::pipeline::ShapedRun,
) -> Vec<GlyphInstance> {
  run
    .glyphs
    .iter()
    .map(|glyph| GlyphInstance {
      glyph_id: glyph.glyph_id,
      cluster: glyph.cluster,
      x_offset: glyph.x_offset,
      // Display-list glyph offsets use a y-down coordinate system.
      y_offset: -glyph.y_offset,
      x_advance: glyph.x_advance,
      y_advance: glyph.y_advance,
    })
    .collect()
}

fn variations_from_run(
  run: &crate::text::pipeline::ShapedRun,
) -> Vec<crate::paint::display_list::FontVariation> {
  let mut variations: Vec<crate::paint::display_list::FontVariation> = run
    .variations
    .iter()
    .map(|v| crate::paint::display_list::FontVariation::new(v.tag, v.value))
    .collect();
  variations.sort_by_key(|v| v.tag);
  variations
}

#[test]
fn preserve_3d_text_clip_is_projected_in_clip_override() {
  let width = 200u32;
  let height = 120u32;
  let bounds = Rect::from_xywh(0.0, 0.0, width as f32, height as f32);

  let font_config = FontConfig::default()
    .with_system_fonts(false)
    .with_bundled_fonts(false)
    .add_font_dir(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fonts"));
  let font_ctx = FontContext::with_config(font_config);
  let font = font_ctx
    .get_sans_serif()
    .expect("test sans-serif font should load from tests/fonts");

  let shaped = super::color_font_helpers::shaped_run(&font, 'F', 80.0, 0);
  let run = TextItem {
    origin: Point::new(60.0, 90.0),
    cached_bounds: None,
    glyphs: glyph_instances_from_text_run(&shaped),
    color: Rgba::BLACK,
    allow_subpixel_aa: true,
    stroke_width: 0.0,
    stroke_color: Rgba::TRANSPARENT,
    font_smoothing: FontSmoothing::Auto,
    text_rendering: TextRendering::Auto,
    palette_index: shaped.palette_index,
    palette_overrides: shaped.palette_overrides.clone(),
    palette_override_hash: shaped.palette_override_hash,
    rotation: shaped.rotation,
    scale: shaped.scale,
    shadows: Vec::new(),
    font_size: shaped.font_size,
    advance_width: shaped.advance,
    font: Some(shaped.font.clone()),
    font_id: None,
    variations: variations_from_run(&shaped),
    synthetic_bold: shaped.synthetic_bold,
    synthetic_oblique: shaped.synthetic_oblique,
    emphasis: None,
    decorations: Vec::new(),
  };
  let runs: Arc<[TextItem]> = Arc::from(vec![run].into_boxed_slice());

  let mut baseline = DisplayList::new();
  baseline.push(DisplayItem::PushClip(ClipItem {
    shape: ClipShape::Text { runs: runs.clone() },
  }));
  baseline.push(DisplayItem::FillRect(FillRectItem {
    rect: bounds,
    color: Rgba::RED,
  }));
  baseline.push(DisplayItem::PopClip);
  let baseline_pixmap =
    DisplayListRenderer::new(width, height, Rgba::TRANSPARENT, font_ctx.clone())
      .unwrap()
      .render(&baseline)
      .unwrap();

  // Root perspective and a projective rotate around the canvas center.
  let perspective = Transform3D::perspective(250.0);
  let center = (bounds.width() * 0.5, bounds.height() * 0.5);
  let rotate = Transform3D::translate(center.0, center.1, 0.0)
    .multiply(&Transform3D::rotate_y(70_f32.to_radians()))
    .multiply(&Transform3D::translate(-center.0, -center.1, 0.0));
  let clip_transform = perspective.multiply(&rotate);

  // Pick an interior red pixel `p` whose projected position `p'` lands outside the untransformed
  // clip mask. This guarantees the test fails on main (which applies the text clip unprojected).
  const BASELINE_INTERIOR_RADIUS: i32 = 1;
  const PROJECTED_NEIGHBORHOOD_RADIUS: i32 = 3;
  let (width_i32, height_i32) = (width as i32, height as i32);

  let mut chosen: Option<((i32, i32), (i32, i32))> = None;
  'outer: for y in BASELINE_INTERIOR_RADIUS..(height_i32 - BASELINE_INTERIOR_RADIUS) {
    for x in BASELINE_INTERIOR_RADIUS..(width_i32 - BASELINE_INTERIOR_RADIUS) {
      // Require a small interior neighborhood to be fully inside the baseline clip so the
      // projected location is less likely to land near an edge due to rounding.
      let mut interior_ok = true;
      for dy in -BASELINE_INTERIOR_RADIUS..=BASELINE_INTERIOR_RADIUS {
        for dx in -BASELINE_INTERIOR_RADIUS..=BASELINE_INTERIOR_RADIUS {
          let (r, g, b, a) = pixel(&baseline_pixmap, (x + dx) as u32, (y + dy) as u32);
          if !(r == 255 && g == 0 && b == 0 && a == 255) {
            interior_ok = false;
            break;
          }
        }
        if !interior_ok {
          break;
        }
      }
      if !interior_ok {
        continue;
      }

      let p_center = Point::new(x as f32 + 0.5, y as f32 + 0.5);
      let Some(projected) = clip_transform.project_point_2d(p_center.x, p_center.y) else {
        continue;
      };
      let x_p = projected.x.round() as i32;
      let y_p = projected.y.round() as i32;
      if x_p < PROJECTED_NEIGHBORHOOD_RADIUS
        || y_p < PROJECTED_NEIGHBORHOOD_RADIUS
        || x_p >= width_i32 - PROJECTED_NEIGHBORHOOD_RADIUS
        || y_p >= height_i32 - PROJECTED_NEIGHBORHOOD_RADIUS
      {
        continue;
      }

      let displaced = (projected.x - p_center.x)
        .abs()
        .max((projected.y - p_center.y).abs());
      if displaced <= 2.0 {
        continue;
      }

      // Ensure the neighborhood around p' is completely outside the untransformed clip so the
      // test still fails on the main-branch behavior.
      let mut outside_baseline = true;
      for dy in -PROJECTED_NEIGHBORHOOD_RADIUS..=PROJECTED_NEIGHBORHOOD_RADIUS {
        for dx in -PROJECTED_NEIGHBORHOOD_RADIUS..=PROJECTED_NEIGHBORHOOD_RADIUS {
          let (_, _, _, a) = pixel(&baseline_pixmap, (x_p + dx) as u32, (y_p + dy) as u32);
          if a != 0 {
            outside_baseline = false;
            break;
          }
        }
        if !outside_baseline {
          break;
        }
      }
      if !outside_baseline {
        continue;
      }

      chosen = Some(((x, y), (x_p, y_p)));
      break 'outer;
    }
  }
  let ((_p_x, _p_y), (p_prime_x, p_prime_y)) =
    chosen.expect("expected a movable interior clip pixel");

  let mut projected_list = DisplayList::new();
  projected_list.push(DisplayItem::PushStackingContext(ctx(
    bounds,
    TransformStyle::Preserve3d,
    None,
    Some(perspective),
  )));
  projected_list.push(DisplayItem::PushStackingContext(ctx(
    bounds,
    TransformStyle::Preserve3d,
    Some(rotate),
    None,
  )));
  projected_list.push(DisplayItem::PushClip(ClipItem {
    shape: ClipShape::Text { runs: runs.clone() },
  }));

  // Ensure the text clip is inherited by a descendant preserve-3d plane so that preserve-3d clip
  // override machinery is exercised (rather than rasterizing the text clip inside the plane).
  projected_list.push(DisplayItem::PushStackingContext(ctx(
    bounds,
    TransformStyle::Flat,
    None,
    None,
  )));
  projected_list.push(DisplayItem::FillRect(FillRectItem {
    rect: bounds,
    color: Rgba::RED,
  }));
  projected_list.push(DisplayItem::PopStackingContext);

  projected_list.push(DisplayItem::PopClip);
  projected_list.push(DisplayItem::PopStackingContext);
  projected_list.push(DisplayItem::PopStackingContext);

  let projected_pixmap = DisplayListRenderer::new(width, height, Rgba::TRANSPARENT, font_ctx)
    .unwrap()
    .render(&projected_list)
    .unwrap();

  let mut found = false;
  for dy in -PROJECTED_NEIGHBORHOOD_RADIUS..=PROJECTED_NEIGHBORHOOD_RADIUS {
    for dx in -PROJECTED_NEIGHBORHOOD_RADIUS..=PROJECTED_NEIGHBORHOOD_RADIUS {
      let x = (p_prime_x + dx) as u32;
      let y = (p_prime_y + dy) as u32;
      // All candidates should already be outside the baseline clip, but keep this guard so the
      // assertion is self-contained and easier to diagnose.
      if pixel(&baseline_pixmap, x, y).3 != 0 {
        continue;
      }
      let (r, g, b, a) = pixel(&projected_pixmap, x, y);
      if a > 200 && r > 200 && g < 50 && b < 50 {
        found = true;
        break;
      }
    }
    if found {
      break;
    }
  }
  assert!(
    found,
    "expected projected clip to produce red pixels near projected position (p'=({}, {}))",
    p_prime_x, p_prime_y
  );
}
