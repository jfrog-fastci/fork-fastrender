use crate::error::{Error, RenderError, Result};
use crate::geometry::Rect;
use crate::paint::display_list::BorderRadii;
use tiny_skia::{BlendMode, FillRule, FilterQuality, Mask, Pixmap, PixmapPaint, Transform};

/// Metadata describing how an out-of-process subframe surface should be embedded into a parent
/// frame.
///
/// All geometry fields are expressed in **CSS pixels**; the compositor converts to device pixels
/// using `dpr` when drawing into the parent pixmap.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SubframePlacement {
  /// Destination rectangle of the subframe content box, in CSS pixels, relative to the parent
  /// frame.
  pub rect_css: Rect,
  /// Device pixel ratio of the parent surface.
  pub dpr: f32,
  /// Optional content-box clip radii in CSS pixels.
  ///
  /// If provided, the subframe surface is clipped to a rounded rectangle that matches the
  /// content-box border radius semantics used by in-process iframe painting.
  ///
  /// Callers that want a hard rectangular clip can pass `Some(BorderRadii::ZERO)`.
  pub clip_radii_css: Option<BorderRadii>,
}

impl SubframePlacement {
  fn rect_px(self) -> Rect {
    Rect::from_xywh(
      self.rect_css.x() * self.dpr,
      self.rect_css.y() * self.dpr,
      self.rect_css.width() * self.dpr,
      self.rect_css.height() * self.dpr,
    )
  }

  fn clip_radii_px(self, dest_px: Rect) -> Option<BorderRadii> {
    let radii = self.clip_radii_css?;
    Some(scale_radii(radii, self.dpr).clamped(dest_px.width(), dest_px.height()))
  }
}

/// Composite child surfaces onto a base pixmap for presentation.
///
/// The base pixmap is modified in-place. Child pixmaps are assumed to be premultiplied RGBA
/// surfaces, compatible with tiny-skia's blending model.
pub fn composite(
  base: &mut Pixmap,
  overlays: impl IntoIterator<Item = (Pixmap, SubframePlacement)>,
) -> Result<()> {
  for (overlay, placement) in overlays {
    composite_one(base, &overlay, placement)?;
  }
  Ok(())
}

fn composite_one(base: &mut Pixmap, overlay: &Pixmap, placement: SubframePlacement) -> Result<()> {
  if base.width() == 0 || base.height() == 0 {
    return Ok(());
  }
  if overlay.width() == 0 || overlay.height() == 0 {
    return Ok(());
  }

  if !placement.dpr.is_finite() || placement.dpr <= 0.0 {
    return Err(Error::Render(RenderError::InvalidParameters {
      message: format!("invalid dpr {}", placement.dpr),
    }));
  }

  let dest_px = placement.rect_px();
  if !dest_px.x().is_finite()
    || !dest_px.y().is_finite()
    || !dest_px.width().is_finite()
    || !dest_px.height().is_finite()
  {
    return Err(Error::Render(RenderError::InvalidParameters {
      message: format!("non-finite destination rect {dest_px:?}"),
    }));
  }
  if dest_px.width() <= 0.0 || dest_px.height() <= 0.0 {
    return Ok(());
  }

  let scale_x = dest_px.width() / overlay.width() as f32;
  let scale_y = dest_px.height() / overlay.height() as f32;
  if !scale_x.is_finite() || !scale_y.is_finite() || scale_x == 0.0 || scale_y == 0.0 {
    return Err(Error::Render(RenderError::InvalidParameters {
      message: format!(
        "invalid scale factors (sx={scale_x}, sy={scale_y}) for dest={dest_px:?} src=({}x{})",
        overlay.width(),
        overlay.height()
      ),
    }));
  }

  let mut paint = PixmapPaint::default();
  paint.opacity = 1.0;
  paint.blend_mode = BlendMode::SourceOver;
  paint.quality = FilterQuality::Bilinear;

  let clip_mask = placement
    .clip_radii_px(dest_px)
    .map(|radii| build_clip_mask(base.width(), base.height(), dest_px, radii))
    .transpose()?;

  let transform = Transform::from_row(
    scale_x,
    0.0,
    0.0,
    scale_y,
    dest_px.x(),
    dest_px.y(),
  );

  base.draw_pixmap(0, 0, overlay.as_ref(), &paint, transform, clip_mask.as_ref());
  Ok(())
}

fn build_clip_mask(width: u32, height: u32, rect: Rect, radii: BorderRadii) -> Result<Mask> {
  let mut mask = Mask::new(width, height).ok_or_else(|| {
    Error::Render(RenderError::PaintFailed {
      operation: format!("failed to allocate clip mask ({width}x{height})"),
    })
  })?;
  mask.data_mut().fill(0);

  let Some(path) = crate::paint::rasterize::build_rounded_rect_path(
    rect.x(),
    rect.y(),
    rect.width(),
    rect.height(),
    &radii,
  ) else {
    return Err(Error::Render(RenderError::InvalidParameters {
      message: format!("failed to build rounded-rect path rect={rect:?} radii={radii:?}"),
    }));
  };

  // Always anti-alias rounded corners; this is needed for border-radius clip fidelity.
  mask.fill_path(&path, FillRule::Winding, true, Transform::identity());
  Ok(mask)
}

fn scale_radii(radii: BorderRadii, scale: f32) -> BorderRadii {
  BorderRadii {
    top_left: radii.top_left * scale,
    top_right: radii.top_right * scale,
    bottom_right: radii.bottom_right * scale,
    bottom_left: radii.bottom_left * scale,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use tiny_skia::PremultipliedColorU8;

  fn rgba(pixmap: &Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
    let px = pixmap.pixel(x, y).unwrap();
    (px.red(), px.green(), px.blue(), px.alpha())
  }

  fn fill_rect(
    pixmap: &mut Pixmap,
    x0: u32,
    y0: u32,
    w: u32,
    h: u32,
    color: PremultipliedColorU8,
  ) {
    let stride = pixmap.width() as usize;
    let pixels = pixmap.pixels_mut();
    for y in y0..(y0 + h) {
      let row = y as usize * stride;
      pixels[row + x0 as usize..row + (x0 + w) as usize].fill(color);
    }
  }

  #[test]
  fn compositor_scales_clips_and_blends() {
    let blue = PremultipliedColorU8::from_rgba(0, 0, 255, 255).unwrap();
    let red_half = PremultipliedColorU8::from_rgba(255, 0, 0, 128).unwrap();

    let mut base = Pixmap::new(50, 50).unwrap();
    base.pixels_mut().fill(blue);

    // Source pixmap is smaller than destination to force scaling.
    let mut child = Pixmap::new(10, 10).unwrap();
    child.pixels_mut().fill(red_half);

    let placement = SubframePlacement {
      rect_css: Rect::from_xywh(10.0, 10.0, 20.0, 20.0),
      dpr: 1.0,
      clip_radii_css: Some(BorderRadii::uniform(8.0)),
    };

    composite(&mut base, [(child, placement)]).unwrap();

    assert_eq!(rgba(&base, 0, 0), (0, 0, 255, 255), "base outside overlay");
    assert_eq!(
      rgba(&base, 20, 20),
      (128, 0, 127, 255),
      "expected premultiplied source-over blend inside clip"
    );
    assert_eq!(
      rgba(&base, 10, 10),
      (0, 0, 255, 255),
      "expected corner to be clipped out"
    );
  }

  #[test]
  fn compositor_iframe_content_box_clip_matches_legacy_semantics() {
    // Mirrors `src/paint/tests/legacy/iframe_content_box_clip.rs` but exercises the compositor
    // directly, modelling how an out-of-process iframe surface is embedded into the parent frame.
    //
    // The outer iframe element paints its border + background; the inner document is composited
    // into the content box and must be clipped to the *content-box* radius.
    let black = PremultipliedColorU8::from_rgba(0, 0, 0, 255).unwrap();
    let border = PremultipliedColorU8::from_rgba(255, 200, 0, 255).unwrap();
    let background = PremultipliedColorU8::from_rgba(0, 150, 0, 255).unwrap();
    let red = PremultipliedColorU8::from_rgba(255, 0, 0, 255).unwrap();

    let mut base = Pixmap::new(200, 200).unwrap();
    base.pixels_mut().fill(black);

    // Border box (content 100 + padding 40 + border 40 = 180).
    fill_rect(&mut base, 0, 0, 180, 180, border);
    // Padding box (inside border).
    fill_rect(&mut base, 20, 20, 140, 140, background);

    // Inner document surface (content box).
    let mut child = Pixmap::new(100, 100).unwrap();
    child.pixels_mut().fill(red);

    let placement = SubframePlacement {
      rect_css: Rect::from_xywh(40.0, 40.0, 100.0, 100.0),
      dpr: 1.0,
      // border-radius: 80px on border box, shrink by border+padding (40px) -> content box radius 40px.
      clip_radii_css: Some(BorderRadii::uniform(40.0)),
    };

    composite(&mut base, [(child, placement)]).unwrap();

    assert_eq!(
      rgba(&base, 90, 10),
      (255, 200, 0, 255),
      "expected border color at (90,10)"
    );
    assert_eq!(
      rgba(&base, 90, 30),
      (0, 150, 0, 255),
      "expected iframe background in padding at (90,30)"
    );
    assert_eq!(
      rgba(&base, 90, 90),
      (255, 0, 0, 255),
      "expected iframe content in content box at (90,90)"
    );
    assert_eq!(
      rgba(&base, 45, 45),
      (0, 150, 0, 255),
      "expected iframe content to be clipped at rounded corner (45,45)"
    );
  }
}
