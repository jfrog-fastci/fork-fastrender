use egui::{ColorImage, TextureHandle, TextureId, TextureOptions, Vec2};

/// A reusable egui texture for displaying a rendered page pixmap.
///
/// `tiny_skia::Pixmap` stores pixels as **premultiplied** RGBA8, while egui expects image inputs
/// to be **unmultiplied** (straight) RGBA8. This type handles the conversion and ensures we reuse
/// the existing `TextureHandle` when possible (resizing forces recreation).
pub struct PageTexture {
  texture: TextureHandle,
  size_px: [usize; 2],
  options: TextureOptions,
}

impl PageTexture {
  pub fn new(ctx: &egui::Context, pixmap: &tiny_skia::Pixmap, options: TextureOptions) -> Self {
    let size_px = [pixmap.width() as usize, pixmap.height() as usize];
    let image = pixmap_to_color_image(pixmap);
    let texture = ctx.load_texture("page", image, options);

    Self {
      texture,
      size_px,
      options,
    }
  }

  /// Updates the texture contents from `pixmap`.
  ///
  /// If `pixmap` has the same dimensions as the currently allocated texture, we update the
  /// existing texture in-place. Otherwise we recreate the texture to match the new size.
  pub fn update(&mut self, ctx: &egui::Context, pixmap: &tiny_skia::Pixmap) {
    let new_size_px = [pixmap.width() as usize, pixmap.height() as usize];
    let image = pixmap_to_color_image(pixmap);

    if new_size_px == self.size_px {
      self.texture.set(image, self.options);
    } else {
      self.texture = ctx.load_texture("page", image, self.options);
      self.size_px = new_size_px;
    }
  }

  pub fn texture_id(&self) -> TextureId {
    self.texture.id()
  }

  /// Returns the on-screen size (in egui "points") required to draw the texture at 1:1 physical
  /// pixel mapping.
  ///
  /// That is: `points * pixels_per_point = pixels`.
  pub fn logical_size_points(&self, pixels_per_point: f32) -> Vec2 {
    debug_assert!(pixels_per_point > 0.0);
    Vec2::new(
      self.size_px[0] as f32 / pixels_per_point,
      self.size_px[1] as f32 / pixels_per_point,
    )
  }
}

fn pixmap_to_color_image(pixmap: &tiny_skia::Pixmap) -> ColorImage {
  let size = [pixmap.width() as usize, pixmap.height() as usize];

  let data = pixmap.data();
  debug_assert_eq!(data.len(), size[0] * size[1] * 4);

  let mut pixels = Vec::with_capacity(size[0] * size[1]);
  for rgba in data.chunks_exact(4) {
    let r_premul = rgba[0];
    let g_premul = rgba[1];
    let b_premul = rgba[2];
    let a = rgba[3];

    let r = unpremultiply_channel(r_premul, a);
    let g = unpremultiply_channel(g_premul, a);
    let b = unpremultiply_channel(b_premul, a);

    pixels.push(egui::Color32::from_rgba_unmultiplied(r, g, b, a));
  }

  ColorImage { size, pixels }
}

#[inline]
fn unpremultiply_channel(c_premul: u8, a: u8) -> u8 {
  match a {
    0 => 0,
    255 => c_premul,
    a => {
      let a = u32::from(a);
      let c = u32::from(c_premul);
      let unmultiplied = (c * 255 + a / 2) / a;
      unmultiplied.min(255) as u8
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn pixmap_to_color_image_unpremultiplies_rgba() {
    let mut pixmap = tiny_skia::Pixmap::new(2, 2).expect("pixmap");
    pixmap.data_mut().copy_from_slice(&[
      // (0, 0): opaque red
      255, 0, 0, 255, //
      // (1, 0): 50% green (premultiplied)
      0, 128, 0, 128, //
      // (0, 1): fully transparent (rgb should be 0 in premultiplied)
      0, 0, 0, 0, //
      // (1, 1): partial alpha with rounding
      64, 32, 16, 128,
    ]);

    let image = pixmap_to_color_image(&pixmap);
    assert_eq!(image.size, [2, 2]);

    let expected = vec![
      egui::Color32::from_rgba_unmultiplied(255, 0, 0, 255),
      egui::Color32::from_rgba_unmultiplied(0, 255, 0, 128),
      egui::Color32::from_rgba_unmultiplied(0, 0, 0, 0),
      egui::Color32::from_rgba_unmultiplied(128, 64, 32, 128),
    ];
    assert_eq!(image.pixels, expected);
  }

  #[test]
  fn logical_size_points_maps_pixels_to_points() {
    let ctx = egui::Context::default();
    let pixmap = tiny_skia::Pixmap::new(4, 2).expect("pixmap");

    let tex = PageTexture::new(&ctx, &pixmap, TextureOptions::default());
    assert_eq!(tex.logical_size_points(2.0), Vec2::new(2.0, 1.0));
  }
}
