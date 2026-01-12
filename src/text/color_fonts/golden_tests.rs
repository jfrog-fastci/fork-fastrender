//! Golden image tests for COLR color fonts.
//!
//! These tests used to live as integration tests, but are unit tests now so they can directly
//! exercise the color-font rasterization stack.

mod colrv1_color_font_test {
  use crate::image_compare::{compare_images, decode_png, encode_png, CompareConfig};
  use crate::style::color::Rgba;
  use crate::text::color_fonts::{ColorFontRenderer, ColorGlyphRaster};
  use crate::text::font_db::{FontStretch, FontStyle, FontWeight, LoadedFont};
  use crate::text::font_instance::FontInstance;
  use image::RgbaImage;
  use rustybuzz::Variation;
  use std::fs;
  use std::path::PathBuf;
  use std::sync::Arc;
  use tiny_skia::Pixmap;
  use ttf_parser::Tag;

  fn fixtures_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
      .join("tests")
      .join("fixtures")
  }

  fn test_glyph_id(face: &ttf_parser::Face<'_>) -> ttf_parser::GlyphId {
    face
      .glyph_index('A')
      .or_else(|| face.glyph_index('G'))
      .expect("test glyph not found")
  }

  fn load_test_font() -> LoadedFont {
    let data = std::fs::read(fixtures_path().join("fonts/colrv1-test.ttf")).unwrap();
    LoadedFont {
      id: None,
      data: Arc::new(data),
      index: 0,
      family: "ColrV1Test".into(),
      weight: FontWeight::NORMAL,
      style: FontStyle::Normal,
      stretch: FontStretch::Normal,
      face_metrics_overrides: Default::default(),
      face_settings: Default::default(),
    }
  }

  fn render_glyph(
    font: &LoadedFont,
    palette_index: u16,
    text_color: Rgba,
    variations: &[Variation],
  ) -> ColorGlyphRaster {
    let face = font.as_ttf_face().unwrap();
    let gid = test_glyph_id(&face);
    let instance = FontInstance::new(font, variations).unwrap();

    ColorFontRenderer::new()
      .render(
        font,
        &instance,
        gid.0 as u32,
        64.0,
        palette_index,
        &[],
        0,
        text_color,
        0.0,
        variations,
        None,
      )
      .expect("expected color glyph")
  }

  fn save_or_compare(name: &str, raster: &ColorGlyphRaster) {
    let actual_image = pixmap_to_rgba_image(&raster.image);
    let actual_bytes = encode_png(&actual_image).expect("encode actual png");
    if let Ok(dir) = std::env::var("SAVE_ACTUAL") {
      let mut path = PathBuf::from(dir);
      let _ = fs::create_dir_all(&path);
      path.push(name);
      fs::write(&path, &actual_bytes).expect("failed to write actual render");
    }

    let path = fixtures_path().join("golden").join(name);
    if std::env::var("UPDATE_GOLDEN").is_ok() {
      fs::write(&path, &actual_bytes).expect("failed to write golden");
      return;
    }

    let golden_bytes =
      fs::read(&path).expect("missing golden image; set UPDATE_GOLDEN=1 to create");
    if golden_bytes == actual_bytes {
      return;
    }

    let golden = decode_png(&golden_bytes).expect("failed to decode golden png");
    let actual = decode_png(&actual_bytes).expect("failed to decode actual png");
    if std::env::var("SAVE_ACTUAL").is_ok() {
      if golden.width() == actual.width() && golden.height() == actual.height() {
        let differing = actual
          .as_raw()
          .iter()
          .zip(golden.as_raw().iter())
          .filter(|(a, b)| a != b)
          .count();
        println!("debug: comparing '{}' ({} bytes differ)", name, differing);
      }
    }
    let diff = compare_images(&actual, &golden, &CompareConfig::strict());
    assert!(
      diff.is_match(),
      "rendered image {name} did not match golden: {}",
      diff.summary()
    );
  }

  fn pixmap_to_rgba_image(pixmap: &tiny_skia::Pixmap) -> RgbaImage {
    let width = pixmap.width();
    let height = pixmap.height();
    let mut rgba = RgbaImage::new(width, height);

    for (dst, src) in rgba
      .as_mut()
      .chunks_exact_mut(4)
      .zip(pixmap.data().chunks_exact(4))
    {
      let r = src[0];
      let g = src[1];
      let b = src[2];
      let a = src[3];

      if a == 0 {
        dst.copy_from_slice(&[0, 0, 0, 0]);
        continue;
      }

      let alpha = a as f32 / 255.0;
      dst[0] = ((r as f32 / alpha).min(255.0)) as u8;
      dst[1] = ((g as f32 / alpha).min(255.0)) as u8;
      dst[2] = ((b as f32 / alpha).min(255.0)) as u8;
      dst[3] = a;
    }

    rgba
  }

  fn painted_bounds(pixmap: &Pixmap) -> Option<(u32, u32, u32, u32)> {
    let width = pixmap.width();
    let height = pixmap.height();
    let data = pixmap.data();
    let mut min_x = width;
    let mut min_y = height;
    let mut max_x = 0;
    let mut max_y = 0;
    let mut found = false;

    for y in 0..height {
      for x in 0..width {
        let idx = ((y * width + x) as usize) * 4 + 3;
        if data[idx] != 0 {
          found = true;
          min_x = min_x.min(x);
          min_y = min_y.min(y);
          max_x = max_x.max(x);
          max_y = max_y.max(y);
        }
      }
    }

    if !found {
      return None;
    }

    Some((min_x, min_y, max_x + 1, max_y + 1))
  }

  #[test]
  fn renders_colrv1_palette0_matches_golden() {
    let font = load_test_font();
    let raster = render_glyph(&font, 0, Rgba::from_rgba8(40, 60, 210, 255), &[]);
    save_or_compare("colrv1_palette0.png", &raster);
  }

  #[test]
  fn palette_selection_updates_colors() {
    let font = load_test_font();
    let raster = render_glyph(&font, 1, Rgba::from_rgba8(40, 60, 210, 255), &[]);
    save_or_compare("colrv1_palette1.png", &raster);
  }

  #[test]
  fn malformed_colrv1_table_falls_back() {
    let font = load_test_font();
    let mut data = (*font.data).clone();
    if let Some((offset, _)) = find_table(&data, b"COLR") {
      if offset + 2 <= data.len() {
        data[offset] = 0xFF;
        data[offset + 1] = 0xFF;
      }
    }
    let corrupted = LoadedFont {
      data: Arc::new(data),
      ..font
    };
    let instance =
      FontInstance::new(&corrupted, &[]).expect("corrupted COLR font should still parse instance");
    let face = corrupted.as_ttf_face().unwrap();
    let gid = test_glyph_id(&face);
    let rendered = ColorFontRenderer::new().render(
      &corrupted,
      &instance,
      gid.0 as u32,
      64.0,
      0,
      &[],
      0,
      Rgba::BLACK,
      0.0,
      &[],
      None,
    );
    assert!(rendered.is_none(), "malformed COLR should not render");
  }

  fn load_variable_test_font() -> LoadedFont {
    let data = std::fs::read(fixtures_path().join("fonts/colrv1-var-test.ttf")).unwrap();
    LoadedFont {
      id: None,
      data: Arc::new(data),
      index: 0,
      family: "ColrV1VarTest".into(),
      weight: FontWeight::NORMAL,
      style: FontStyle::Normal,
      stretch: FontStretch::Normal,
      face_metrics_overrides: Default::default(),
      face_settings: Default::default(),
    }
  }

  fn load_variable_clip_font() -> LoadedFont {
    let data = std::fs::read(fixtures_path().join("fonts/colrv1-var-clip-test.ttf")).unwrap();
    LoadedFont {
      id: None,
      data: Arc::new(data),
      index: 0,
      family: "ColrV1VarClipTest".into(),
      weight: FontWeight::NORMAL,
      style: FontStyle::Normal,
      stretch: FontStretch::Normal,
      face_metrics_overrides: Default::default(),
      face_settings: Default::default(),
    }
  }

  fn load_gvar_test_font() -> LoadedFont {
    let data = std::fs::read(fixtures_path().join("fonts/colrv1-gvar-test.ttf")).unwrap();
    LoadedFont {
      id: None,
      data: Arc::new(data),
      index: 0,
      family: "ColrV1GvarTest".into(),
      weight: FontWeight::NORMAL,
      style: FontStyle::Normal,
      stretch: FontStretch::Normal,
      face_metrics_overrides: Default::default(),
      face_settings: Default::default(),
    }
  }

  #[test]
  fn variable_colrv1_changes_with_variations() {
    let font = load_variable_test_font();
    let text_color = Rgba::from_rgba8(30, 40, 220, 255);
    let base = render_glyph(&font, 0, text_color, &[]);
    let varied = render_glyph(
      &font,
      0,
      text_color,
      &[Variation {
        tag: Tag::from_bytes(b"wght"),
        value: 1.0,
      }],
    );
    save_or_compare("colrv1_var_default.png", &base);
    save_or_compare("colrv1_var_wght1.png", &varied);

    let diff = compare_images(
      &pixmap_to_rgba_image(&base.image),
      &pixmap_to_rgba_image(&varied.image),
      &CompareConfig::strict(),
    );
    assert!(
      !diff.is_match(),
      "variable render should differ between instances"
    );
  }

  #[test]
  fn variable_clip_box_adjusts_bounds() {
    let font = load_variable_clip_font();
    let text_color = Rgba::from_rgba8(20, 40, 200, 255);
    let base = render_glyph(&font, 0, text_color, &[]);
    let varied = render_glyph(
      &font,
      0,
      text_color,
      &[Variation {
        tag: Tag::from_bytes(b"wght"),
        value: 1.0,
      }],
    );

    assert!(
      varied.image.height() > base.image.height(),
      "variable clip box should expand raster height: base={} varied={}",
      base.image.height(),
      varied.image.height()
    );

    let diff = compare_images(
      &pixmap_to_rgba_image(&base.image),
      &pixmap_to_rgba_image(&varied.image),
      &CompareConfig::strict(),
    );
    assert!(
      !diff.is_match(),
      "clip variation should change rendered pixels"
    );
  }

  #[test]
  fn variable_colrv1_applies_gvar_outlines() {
    let font = load_gvar_test_font();
    let text_color = Rgba::from_rgba8(25, 180, 230, 255);
    let base = render_glyph(&font, 0, text_color, &[]);
    let varied = render_glyph(
      &font,
      0,
      text_color,
      &[Variation {
        tag: Tag::from_bytes(b"wght"),
        value: 900.0,
      }],
    );
    let diff = compare_images(
      &pixmap_to_rgba_image(&base.image),
      &pixmap_to_rgba_image(&varied.image),
      &CompareConfig::strict(),
    );
    assert!(
      !diff.is_match(),
      "gvar-driven COLRv1 outlines should differ between variation instances"
    );

    let base_bounds = painted_bounds(&base.image).expect("base gvar glyph should paint");
    let varied_bounds = painted_bounds(&varied.image).expect("varied gvar glyph should paint");
    assert_eq!(base_bounds, (1, 1, 8, 46), "unexpected base glyph bounds");
    assert_eq!(
      varied_bounds,
      (1, 1, 24, 46),
      "unexpected varied glyph bounds"
    );
    assert_ne!(
      base_bounds, varied_bounds,
      "variation should affect outline bounds"
    );
  }

  fn find_table(data: &[u8], tag: &[u8; 4]) -> Option<(usize, usize)> {
    if data.len() < 12 {
      return None;
    }
    let num_tables = u16::from_be_bytes([data[4], data[5]]) as usize;
    let mut offset = 12;
    for _ in 0..num_tables {
      if offset + 16 > data.len() {
        return None;
      }
      let table_tag = &data[offset..offset + 4];
      let table_offset = u32::from_be_bytes([
        data[offset + 8],
        data[offset + 9],
        data[offset + 10],
        data[offset + 11],
      ]) as usize;
      let length = u32::from_be_bytes([
        data[offset + 12],
        data[offset + 13],
        data[offset + 14],
        data[offset + 15],
      ]) as usize;
      if table_tag == tag {
        return Some((table_offset, length));
      }
      offset += 16;
    }
    None
  }
}

mod colrv1_linear_gradient_test {
  use crate::image_compare::{compare_images, decode_png, encode_png, CompareConfig};
  use crate::style::color::Rgba;
  use crate::text::color_fonts::{ColorFontRenderer, ColorGlyphRaster};
  use crate::text::font_db::{FontStretch, FontStyle, FontWeight, LoadedFont};
  use crate::text::font_instance::FontInstance;
  use image::RgbaImage;
  use std::path::PathBuf;
  use std::sync::Arc;
  use tiny_skia::Pixmap;

  fn fixtures_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
      .join("tests")
      .join("fixtures")
  }

  fn load_sheared_font() -> LoadedFont {
    let data = std::fs::read(fixtures_path().join("fonts/colrv1-linear-shear.ttf")).unwrap();
    LoadedFont {
      id: None,
      data: Arc::new(data),
      index: 0,
      family: "ColrV1LinearShear".into(),
      weight: FontWeight::NORMAL,
      style: FontStyle::Normal,
      stretch: FontStretch::Normal,
      face_metrics_overrides: Default::default(),
      face_settings: Default::default(),
    }
  }

  fn render_sheared_glyph(font: &LoadedFont) -> ColorGlyphRaster {
    let face = font.as_ttf_face().unwrap();
    let gid = face.glyph_index('G').unwrap();
    let instance = FontInstance::new(font, &[]).unwrap();

    ColorFontRenderer::new()
      .render(
        font,
        &instance,
        gid.0 as u32,
        64.0,
        0,
        &[],
        0,
        Rgba::BLACK,
        0.0,
        &[],
        None,
      )
      .expect("expected color glyph")
  }

  fn save_or_compare(name: &str, raster: &ColorGlyphRaster) {
    let path = fixtures_path().join("golden").join(name);
    if std::env::var("UPDATE_GOLDEN").is_ok() {
      let encoded =
        encode_png(&pixmap_to_rgba_image(&raster.image)).expect("failed to encode golden");
      std::fs::write(&path, encoded).expect("failed to write golden");
    }

    let golden =
      decode_png(&std::fs::read(&path).expect("missing golden image; set UPDATE_GOLDEN=1 to create"))
        .expect("failed to decode golden png");
    let actual = decode_png(
      &encode_png(&pixmap_to_rgba_image(&raster.image)).expect("failed to encode render to PNG"),
    )
    .expect("failed to decode encoded render");
    let byte_mismatches = actual
      .as_raw()
      .iter()
      .zip(golden.as_raw())
      .filter(|(a, b)| a != b)
      .count();
    let diff = compare_images(&actual, &golden, &CompareConfig::strict());
    assert!(
      diff.is_match(),
      "rendered image {name} did not match golden ({byte_mismatches} byte mismatches): {:?}\n{}",
      diff.statistics,
      sample_pixels(&actual, &golden),
    );
  }

  fn pixmap_to_rgba_image(pixmap: &Pixmap) -> RgbaImage {
    let width = pixmap.width();
    let height = pixmap.height();
    let mut rgba = RgbaImage::new(width, height);

    for (dst, src) in rgba
      .as_mut()
      .chunks_exact_mut(4)
      .zip(pixmap.data().chunks_exact(4))
    {
      let r = src[0];
      let g = src[1];
      let b = src[2];
      let a = src[3];

      if a == 0 {
        dst.copy_from_slice(&[0, 0, 0, 0]);
        continue;
      }

      let alpha = a as f32 / 255.0;
      dst[0] = ((r as f32 / alpha).min(255.0)) as u8;
      dst[1] = ((g as f32 / alpha).min(255.0)) as u8;
      dst[2] = ((b as f32 / alpha).min(255.0)) as u8;
      dst[3] = a;
    }

    rgba
  }

  fn sample_pixels(actual: &RgbaImage, expected: &RgbaImage) -> String {
    let coords = [
      (0, 0),
      (actual.width() / 2, actual.height() / 2),
      (
        actual.width().saturating_sub(1),
        actual.height().saturating_sub(1),
      ),
    ];
    let mut lines = Vec::new();
    for (x, y) in coords {
      let idx = ((y as usize * actual.width() as usize) + x as usize) * 4;
      let actual_px = [
        actual.as_raw()[idx],
        actual.as_raw()[idx + 1],
        actual.as_raw()[idx + 2],
        actual.as_raw()[idx + 3],
      ];
      let expected_px = [
        expected.as_raw()[idx],
        expected.as_raw()[idx + 1],
        expected.as_raw()[idx + 2],
        expected.as_raw()[idx + 3],
      ];
      lines.push(format!(
        "sample ({}, {}): actual {:?}, expected {:?}",
        x, y, actual_px, expected_px
      ));
    }

    let mut mismatches = Vec::new();
    for y in 0..actual.height() {
      for x in 0..actual.width() {
        let idx = ((y as usize * actual.width() as usize) + x as usize) * 4;
        let actual_px = [
          actual.as_raw()[idx],
          actual.as_raw()[idx + 1],
          actual.as_raw()[idx + 2],
          actual.as_raw()[idx + 3],
        ];
        let expected_px = [
          expected.as_raw()[idx],
          expected.as_raw()[idx + 1],
          expected.as_raw()[idx + 2],
          expected.as_raw()[idx + 3],
        ];
        if actual_px != expected_px {
          mismatches.push(format!(
            "diff ({}, {}): actual {:?}, expected {:?}",
            x, y, actual_px, expected_px
          ));
          if mismatches.len() >= 5 {
            break;
          }
        }
      }
      if mismatches.len() >= 5 {
        break;
      }
    }

    lines.extend(mismatches);
    lines.join("\n")
  }

  #[test]
  fn sheared_linear_gradient_respects_third_point() {
    let font = load_sheared_font();
    let raster = render_sheared_glyph(&font);
    save_or_compare("colrv1_linear_shear.png", &raster);
  }
}

mod colrv1_radial_gradient_test {
  use crate::image_compare::{compare_images, decode_png, encode_png, CompareConfig};
  use crate::style::color::Rgba;
  use crate::text::color_fonts::{ColorFontRenderer, ColorGlyphRaster};
  use crate::text::font_db::{FontStretch, FontStyle, FontWeight, LoadedFont};
  use crate::text::font_instance::FontInstance;
  use image::RgbaImage;
  use std::path::PathBuf;
  use std::sync::Arc;
  use tiny_skia::Pixmap;

  fn fixtures_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
      .join("tests")
      .join("fixtures")
  }

  fn load_radial_font() -> LoadedFont {
    let data = std::fs::read(fixtures_path().join("fonts/colrv1-radial-two-circle.ttf"))
      .expect("failed to read radial gradient font");
    LoadedFont {
      id: None,
      data: Arc::new(data),
      index: 0,
      family: "ColrV1RadialTwoCircle".into(),
      weight: FontWeight::NORMAL,
      style: FontStyle::Normal,
      stretch: FontStretch::Normal,
      face_metrics_overrides: Default::default(),
      face_settings: Default::default(),
    }
  }

  fn render_glyph(font: &LoadedFont) -> ColorGlyphRaster {
    let face = font.as_ttf_face().unwrap();
    let gid = face.glyph_index('R').unwrap();
    let instance = FontInstance::new(font, &[]).unwrap();

    ColorFontRenderer::new()
      .render(
        font,
        &instance,
        gid.0 as u32,
        64.0,
        0,
        &[],
        0,
        Rgba::BLACK,
        0.0,
        &[],
        None,
      )
      .expect("expected color glyph")
  }

  fn pixmap_to_rgba_image(pixmap: &Pixmap) -> RgbaImage {
    let width = pixmap.width();
    let height = pixmap.height();
    let mut rgba = RgbaImage::new(width, height);

    for (dst, src) in rgba
      .as_mut()
      .chunks_exact_mut(4)
      .zip(pixmap.data().chunks_exact(4))
    {
      let r = src[0];
      let g = src[1];
      let b = src[2];
      let a = src[3];

      if a == 0 {
        dst.copy_from_slice(&[0, 0, 0, 0]);
        continue;
      }

      let alpha = a as f32 / 255.0;
      dst[0] = ((r as f32 / alpha).min(255.0)) as u8;
      dst[1] = ((g as f32 / alpha).min(255.0)) as u8;
      dst[2] = ((b as f32 / alpha).min(255.0)) as u8;
      dst[3] = a;
    }

    rgba
  }

  fn sample_pixels(actual: &RgbaImage, expected: &RgbaImage) -> String {
    let coords = [
      (0, 0),
      (actual.width() / 2, actual.height() / 2),
      (
        actual.width().saturating_sub(1),
        actual.height().saturating_sub(1),
      ),
    ];
    let mut lines = Vec::new();
    for (x, y) in coords {
      let idx = ((y as usize * actual.width() as usize) + x as usize) * 4;
      let actual_px = [
        actual.as_raw()[idx],
        actual.as_raw()[idx + 1],
        actual.as_raw()[idx + 2],
        actual.as_raw()[idx + 3],
      ];
      let expected_px = [
        expected.as_raw()[idx],
        expected.as_raw()[idx + 1],
        expected.as_raw()[idx + 2],
        expected.as_raw()[idx + 3],
      ];
      lines.push(format!(
        "sample ({}, {}): actual {:?}, expected {:?}",
        x, y, actual_px, expected_px
      ));
    }

    let mut mismatches = Vec::new();
    for y in 0..actual.height() {
      for x in 0..actual.width() {
        let idx = ((y as usize * actual.width() as usize) + x as usize) * 4;
        let actual_px = [
          actual.as_raw()[idx],
          actual.as_raw()[idx + 1],
          actual.as_raw()[idx + 2],
          actual.as_raw()[idx + 3],
        ];
        let expected_px = [
          expected.as_raw()[idx],
          expected.as_raw()[idx + 1],
          expected.as_raw()[idx + 2],
          expected.as_raw()[idx + 3],
        ];
        if actual_px != expected_px {
          mismatches.push(format!(
            "diff ({}, {}): actual {:?}, expected {:?}",
            x, y, actual_px, expected_px
          ));
          if mismatches.len() >= 5 {
            break;
          }
        }
      }
      if mismatches.len() >= 5 {
        break;
      }
    }

    lines.extend(mismatches);
    lines.join("\n")
  }

  fn save_or_compare(name: &str, raster: &ColorGlyphRaster) {
    let path = fixtures_path().join("golden").join(name);
    if std::env::var("UPDATE_GOLDEN").is_ok() {
      let encoded =
        encode_png(&pixmap_to_rgba_image(&raster.image)).expect("failed to encode golden");
      std::fs::write(&path, encoded).expect("failed to write golden");
    }

    let golden =
      decode_png(&std::fs::read(&path).expect("missing golden image; set UPDATE_GOLDEN=1 to create"))
        .expect("failed to decode golden png");
    let actual = decode_png(
      &encode_png(&pixmap_to_rgba_image(&raster.image)).expect("failed to encode render to PNG"),
    )
    .expect("failed to decode encoded render");
    let byte_mismatches = actual
      .as_raw()
      .iter()
      .zip(golden.as_raw())
      .filter(|(a, b)| a != b)
      .count();
    let diff = compare_images(&actual, &golden, &CompareConfig::strict());
    assert!(
      diff.is_match(),
      "rendered image {name} did not match golden ({byte_mismatches} byte mismatches): {:?}\n{}",
      diff.statistics,
      sample_pixels(&actual, &golden),
    );
  }

  #[test]
  fn radial_gradient_respects_two_circle_definition() {
    let font = load_radial_font();
    let raster = render_glyph(&font);
    save_or_compare("colrv1_radial_two_circle.png", &raster);
  }
}

mod colrv1_sweep_gradient_test {
  use crate::image_compare::{compare_images, decode_png, CompareConfig};
  use crate::style::color::Rgba;
  use crate::text::color_fonts::{ColorFontRenderer, ColorGlyphRaster};
  use crate::text::font_db::{FontStretch, FontStyle, FontWeight, LoadedFont};
  use crate::text::font_instance::FontInstance;
  use image::RgbaImage;
  use std::path::PathBuf;
  use std::sync::Arc;
  use tiny_skia::Pixmap;

  fn fixtures_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
      .join("tests")
      .join("fixtures")
  }

  fn load_sweep_font() -> LoadedFont {
    let data = std::fs::read(fixtures_path().join("fonts/colrv1-sweep-test.ttf")).unwrap();
    LoadedFont {
      id: None,
      data: Arc::new(data),
      index: 0,
      family: "ColrV1Sweep".into(),
      weight: FontWeight::NORMAL,
      style: FontStyle::Normal,
      stretch: FontStretch::Normal,
      face_metrics_overrides: Default::default(),
      face_settings: Default::default(),
    }
  }

  fn render_glyph(font: &LoadedFont, ch: char) -> ColorGlyphRaster {
    let face = font.as_ttf_face().unwrap();
    let gid = face.glyph_index(ch).unwrap();
    let instance = FontInstance::new(font, &[]).unwrap();

    ColorFontRenderer::new()
      .render(
        font,
        &instance,
        gid.0 as u32,
        64.0,
        0,
        &[],
        0,
        Rgba::BLACK,
        0.0,
        &[],
        None,
      )
      .expect("expected color glyph")
  }

  fn to_straight_rgba(pixmap: &Pixmap) -> RgbaImage {
    let mut rgba = RgbaImage::new(pixmap.width(), pixmap.height());

    for (dst, src) in rgba
      .as_mut()
      .chunks_exact_mut(4)
      .zip(pixmap.data().chunks_exact(4))
    {
      let a = src[3];
      if a == 0 {
        dst.copy_from_slice(&[0, 0, 0, 0]);
        continue;
      }

      let alpha = a as f32 / 255.0;
      dst[0] = ((src[0] as f32 / alpha).min(255.0)) as u8;
      dst[1] = ((src[1] as f32 / alpha).min(255.0)) as u8;
      dst[2] = ((src[2] as f32 / alpha).min(255.0)) as u8;
      dst[3] = a;
    }

    rgba
  }

  fn save_unpremultiplied_png(path: &std::path::Path, raster: &ColorGlyphRaster) {
    to_straight_rgba(raster.image.as_ref())
      .save(path)
      .expect("failed to write png");
  }

  fn save_or_compare(name: &str, raster: &ColorGlyphRaster) {
    let path = fixtures_path().join("golden").join(name);
    if std::env::var("UPDATE_GOLDEN").is_ok() {
      save_unpremultiplied_png(&path, raster);
      return;
    }

    if std::env::var("SAVE_ACTUAL").is_ok() {
      let actual_path = path.with_extension("actual.png");
      save_unpremultiplied_png(&actual_path, raster);
    }

    let golden_bytes =
      std::fs::read(&path).expect("missing golden image; set UPDATE_GOLDEN=1 to create");
    let golden = decode_png(&golden_bytes).expect("failed to decode golden png");
    let actual = to_straight_rgba(&raster.image);
    let diff = compare_images(&actual, &golden, &CompareConfig::strict());
    assert!(
      diff.is_match(),
      "rendered image {} did not match golden: {}",
      name,
      diff.summary()
    );
  }

  #[test]
  fn sweep_gradient_respects_transforms() {
    let font = load_sweep_font();
    let pad = render_glyph(&font, 'G');
    let pad_repeat = render_glyph(&font, 'G');
    let self_diff = compare_images(
      &to_straight_rgba(&pad.image),
      &to_straight_rgba(&pad_repeat.image),
      &CompareConfig::strict(),
    );
    assert!(
      self_diff.is_match(),
      "pad glyph rendering should be deterministic: {:?}",
      self_diff.statistics
    );
    // Glyph `J` wraps the sweep gradient in translate/scale/transform paints to exercise transform
    // accumulation in the sweep shader path.
    let transformed = render_glyph(&font, 'J');
    save_or_compare("colrv1_sweep_pad.png", &pad);
    save_or_compare("colrv1_sweep_transformed.png", &transformed);

    let diff = compare_images(
      &to_straight_rgba(&pad.image),
      &to_straight_rgba(&transformed.image),
      &CompareConfig::strict(),
    );
    assert!(
      !diff.is_match(),
      "transformed sweep gradient should differ from untransformed: {:?}",
      diff.statistics
    );
  }
}

mod colrv1_variable_outline_test {
  use crate::image_compare::{compare_images, decode_png, CompareConfig};
  use crate::style::color::Rgba;
  use crate::text::color_fonts::ColorFontRenderer;
  use crate::text::font_db::{FontStretch, FontStyle, FontWeight, LoadedFont};
  use crate::text::font_instance::FontInstance;
  use image::RgbaImage;
  use rustybuzz::Variation;
  use std::path::PathBuf;
  use std::sync::Arc;
  use tiny_skia::Pixmap;
  use ttf_parser::Tag;

  fn fixtures_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
      .join("tests")
      .join("fixtures")
  }

  fn load_variable_outline_font() -> LoadedFont {
    let data = std::fs::read(fixtures_path().join("fonts/colrv1-var-outline-test.ttf")).unwrap();
    LoadedFont {
      id: None,
      data: Arc::new(data),
      index: 0,
      family: "ColrV1VarOutlineTest".into(),
      weight: FontWeight::NORMAL,
      style: FontStyle::Normal,
      stretch: FontStretch::Normal,
      face_metrics_overrides: Default::default(),
      face_settings: Default::default(),
    }
  }

  fn render_glyph(
    font: &LoadedFont,
    text_color: Rgba,
    variations: &[Variation],
  ) -> crate::text::color_fonts::ColorGlyphRaster {
    let face = font.as_ttf_face().unwrap();
    let gid = face.glyph_index('A').unwrap();
    let instance = FontInstance::new(font, variations).unwrap();

    ColorFontRenderer::new()
      .render(
        font,
        &instance,
        gid.0 as u32,
        64.0,
        0,
        &[],
        0,
        text_color,
        0.0,
        variations,
        None,
      )
      .expect("expected color glyph")
  }

  fn pixmap_to_rgba_image(pixmap: &Pixmap) -> RgbaImage {
    let width = pixmap.width();
    let height = pixmap.height();
    let mut rgba = RgbaImage::new(width, height);

    for (dst, src) in rgba
      .as_mut()
      .chunks_exact_mut(4)
      .zip(pixmap.data().chunks_exact(4))
    {
      let r = src[0];
      let g = src[1];
      let b = src[2];
      let a = src[3];

      if a == 0 {
        dst.copy_from_slice(&[0, 0, 0, 0]);
        continue;
      }

      let alpha = a as f32 / 255.0;
      dst[0] = ((r as f32 / alpha).min(255.0)) as u8;
      dst[1] = ((g as f32 / alpha).min(255.0)) as u8;
      dst[2] = ((b as f32 / alpha).min(255.0)) as u8;
      dst[3] = a;
    }

    rgba
  }

  fn save_or_compare(name: &str, raster: &crate::text::color_fonts::ColorGlyphRaster) {
    let path = fixtures_path().join("golden").join(name);
    if std::env::var("UPDATE_GOLDEN").is_ok() {
      raster
        .image
        .save_png(&path)
        .expect("failed to write golden");
    }

    // Round-trip the rendered pixmap through PNG encoding/decoding so the comparison matches the
    // golden load path (unpremultiply/premultiply) rather than raw in-memory pixels.
    let current_path = path.with_extension("actual.png");
    raster
      .image
      .save_png(&current_path)
      .expect("failed to write actual render");
    let golden_bytes =
      std::fs::read(&path).expect("missing golden image; set UPDATE_GOLDEN=1 to create");
    let actual_bytes = std::fs::read(&current_path).expect("failed to load actual render");
    let golden = decode_png(&golden_bytes).expect("failed to decode golden png");
    let actual = decode_png(&actual_bytes).expect("failed to decode actual png");
    let diff = compare_images(&actual, &golden, &CompareConfig::strict());
    let _ = std::fs::remove_file(&current_path);
    assert!(
      diff.is_match(),
      "rendered image {} did not match golden: {}",
      name,
      diff.summary()
    );
  }

  #[test]
  fn variable_outline_changes_with_axis() {
    let font = load_variable_outline_font();
    let text_color = Rgba::from_rgba8(40, 80, 210, 255);

    let base = render_glyph(&font, text_color, &[]);
    let varied = render_glyph(
      &font,
      text_color,
      &[Variation {
        tag: Tag::from_bytes(b"wght"),
        value: 1.0,
      }],
    );

    save_or_compare("colrv1_var_outline_wght0.png", &base);
    save_or_compare("colrv1_var_outline_wght1.png", &varied);

    let diff = compare_images(
      &pixmap_to_rgba_image(&base.image),
      &pixmap_to_rgba_image(&varied.image),
      &CompareConfig::strict(),
    );
    assert!(
      !diff.is_match(),
      "variable render should differ between outline instances"
    );
  }
}

mod colrv1_variable_transform_test {
  use crate::image_compare::{compare_images, decode_png, CompareConfig};
  use crate::style::color::Rgba;
  use crate::text::color_fonts::ColorFontRenderer;
  use crate::text::font_db::{FontStretch, FontStyle, FontWeight, LoadedFont};
  use crate::text::font_instance::FontInstance;
  use image::RgbaImage;
  use rustybuzz::Variation;
  use std::path::PathBuf;
  use std::sync::Arc;
  use tiny_skia::Pixmap;
  use ttf_parser::Tag;

  fn fixtures_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
      .join("tests")
      .join("fixtures")
  }

  fn load_variable_transform_font() -> LoadedFont {
    let data = std::fs::read(fixtures_path().join("fonts/colrv1-var-transform-test.ttf")).unwrap();
    LoadedFont {
      id: None,
      data: Arc::new(data),
      index: 0,
      family: "ColrV1VarTransformTest".into(),
      weight: FontWeight::NORMAL,
      style: FontStyle::Normal,
      stretch: FontStretch::Normal,
      face_metrics_overrides: Default::default(),
      face_settings: Default::default(),
    }
  }

  fn render_glyph(
    font: &LoadedFont,
    text_color: Rgba,
    variations: &[Variation],
  ) -> crate::text::color_fonts::ColorGlyphRaster {
    let face = font.as_ttf_face().unwrap();
    let gid = face.glyph_index('A').unwrap();
    let instance = FontInstance::new(font, variations).unwrap();

    ColorFontRenderer::new()
      .render(
        font,
        &instance,
        gid.0 as u32,
        64.0,
        0,
        &[],
        0,
        text_color,
        0.0,
        variations,
        None,
      )
      .expect("expected color glyph")
  }

  fn pixmap_to_rgba_image(pixmap: &Pixmap) -> RgbaImage {
    let width = pixmap.width();
    let height = pixmap.height();
    let mut rgba = RgbaImage::new(width, height);

    for (dst, src) in rgba
      .as_mut()
      .chunks_exact_mut(4)
      .zip(pixmap.data().chunks_exact(4))
    {
      let r = src[0];
      let g = src[1];
      let b = src[2];
      let a = src[3];

      if a == 0 {
        dst.copy_from_slice(&[0, 0, 0, 0]);
        continue;
      }

      let alpha = a as f32 / 255.0;
      dst[0] = ((r as f32 / alpha).min(255.0)) as u8;
      dst[1] = ((g as f32 / alpha).min(255.0)) as u8;
      dst[2] = ((b as f32 / alpha).min(255.0)) as u8;
      dst[3] = a;
    }

    rgba
  }

  fn save_or_compare(name: &str, raster: &crate::text::color_fonts::ColorGlyphRaster) {
    let path = fixtures_path().join("golden").join(name);
    if std::env::var("UPDATE_GOLDEN").is_ok() {
      raster
        .image
        .save_png(&path)
        .expect("failed to write golden");
    }

    // Round-trip the rendered pixmap through PNG encoding/decoding so the comparison matches the
    // golden load path (unpremultiply/premultiply) rather than raw in-memory pixels.
    let current_path = path.with_extension("actual.png");
    raster
      .image
      .save_png(&current_path)
      .expect("failed to write actual render");
    let golden_bytes =
      std::fs::read(&path).expect("missing golden image; set UPDATE_GOLDEN=1 to create");
    let actual_bytes = std::fs::read(&current_path).expect("failed to load actual render");
    let golden = decode_png(&golden_bytes).expect("failed to decode golden png");
    let actual = decode_png(&actual_bytes).expect("failed to decode actual png");
    let diff = compare_images(&actual, &golden, &CompareConfig::strict());
    let _ = std::fs::remove_file(&current_path);
    assert!(
      diff.is_match(),
      "rendered image {} did not match golden: {}",
      name,
      diff.summary()
    );
  }

  #[test]
  fn variable_transforms_change_with_axis() {
    let font = load_variable_transform_font();
    let text_color = Rgba::from_rgba8(35, 75, 205, 255);

    let base = render_glyph(&font, text_color, &[]);
    let varied = render_glyph(
      &font,
      text_color,
      &[Variation {
        tag: Tag::from_bytes(b"wght"),
        value: 1.0,
      }],
    );

    save_or_compare("colrv1_var_transform_wght0.png", &base);
    save_or_compare("colrv1_var_transform_wght1.png", &varied);

    let diff = compare_images(
      &pixmap_to_rgba_image(&base.image),
      &pixmap_to_rgba_image(&varied.image),
      &CompareConfig::strict(),
    );
    assert!(
      !diff.is_match(),
      "variable render should differ between transform instances"
    );
  }
}
