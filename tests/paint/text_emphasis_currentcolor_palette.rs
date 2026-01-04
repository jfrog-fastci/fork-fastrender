use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use fastrender::css::parser::parse_stylesheet;
use fastrender::geometry::Rect;
use fastrender::paint::display_list_builder::DisplayListBuilder;
use fastrender::paint::display_list_renderer::DisplayListRenderer;
use fastrender::style::color::Rgba;
use fastrender::style::font_palette::FontPaletteRegistry;
use fastrender::style::media::MediaContext;
use fastrender::style::types::{FontPalette, TextEmphasisStyle};
use fastrender::text::font_db::FontDatabase;
use fastrender::text::font_loader::FontContext;
use fastrender::text::pipeline::ShapingPipeline;
use fastrender::tree::fragment_tree::FragmentNode;
use fastrender::ComputedStyle;

fn load_test_font() -> (FontContext, String) {
  let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fonts/ColorTestCOLR.ttf");
  let bytes = std::fs::read(path).expect("test color font should load");

  let mut db = FontDatabase::empty();
  db.load_font_data(bytes).expect("font should parse");
  let family = db
    .first_font()
    .expect("font should be present")
    .family
    .clone();

  (FontContext::with_database(Arc::new(db)), family)
}

fn build_current_color_palette_registry(family: &str) -> Arc<FontPaletteRegistry> {
  let css = format!(
    "@font-palette-values --emph {{ font-family: {family}; base-palette: 0; override-colors: 1 currentColor; }}"
  );
  let sheet = parse_stylesheet(&css).expect("palette rule should parse");
  let mut collected = sheet.collect_font_palette_rules(&MediaContext::default());
  assert_eq!(collected.len(), 1, "expected a single palette rule");

  let mut registry = FontPaletteRegistry::default();
  registry.register(collected.pop().unwrap().rule.clone());
  Arc::new(registry)
}

fn unpremultiply(channel: u8, alpha: u8) -> u8 {
  if alpha == 0 {
    return 0;
  }
  (((channel as u16) * 255 + (alpha as u16 / 2)) / alpha as u16).min(255) as u8
}

fn diff_histogram(before: &[u8], after: &[u8]) -> HashMap<[u8; 3], usize> {
  assert_eq!(before.len(), after.len(), "pixmaps must be same size");
  let mut hist = HashMap::new();

  for (b, a) in before.chunks_exact(4).zip(after.chunks_exact(4)) {
    if b == a {
      continue;
    }
    let alpha = a[3];
    if alpha == 0 {
      continue;
    }

    let rgb = [
      unpremultiply(a[2], alpha),
      unpremultiply(a[1], alpha),
      unpremultiply(a[0], alpha),
    ];
    *hist.entry(rgb).or_insert(0) += 1;
  }

  hist
}

#[test]
fn text_emphasis_string_palette_overrides_resolve_current_color_from_text_emphasis_color() {
  let (font_context, family) = load_test_font();
  let palette_registry = build_current_color_palette_registry(&family);

  let purple = Rgba::from_rgba8(128, 0, 128, 255);
  let green = Rgba::from_rgba8(0, 255, 0, 255);

  let mut base_style = ComputedStyle::default();
  base_style.font_family = vec![family.clone()].into();
  base_style.font_size = 64.0;
  base_style.font_palettes = palette_registry;
  base_style.font_palette = FontPalette::Named("--emph".into());
  base_style.color = purple;
  base_style.text_emphasis_color = Some(green);
  base_style.text_emphasis_style = TextEmphasisStyle::None;

  let mut emphasis_style = base_style.clone();
  emphasis_style.text_emphasis_style = TextEmphasisStyle::String("A".into());

  let shaper = ShapingPipeline::new();
  let runs = shaper
    .shape("A", &base_style, &font_context)
    .expect("shaping base text should succeed");
  assert!(!runs.is_empty(), "expected shaped runs");

  let width = 200;
  let height = 200;
  let bounds = Rect::from_xywh(0.0, 0.0, width as f32, height as f32);
  let baseline_offset = 150.0;

  let base_fragment = FragmentNode::new_text_shaped(
    bounds,
    "A",
    baseline_offset,
    Arc::new(runs.clone()),
    Arc::new(base_style),
  );
  let emph_fragment = FragmentNode::new_text_shaped(
    bounds,
    "A",
    baseline_offset,
    Arc::new(runs),
    Arc::new(emphasis_style),
  );

  let base_list = DisplayListBuilder::new()
    .with_font_context(font_context.clone())
    .build(&base_fragment);
  let emph_list = DisplayListBuilder::new()
    .with_font_context(font_context.clone())
    .build(&emph_fragment);

  let base_pixmap =
    DisplayListRenderer::new(width, height, Rgba::TRANSPARENT, font_context.clone())
      .expect("display list renderer should construct")
      .render(&base_list)
      .expect("render without emphasis");
  let emph_pixmap = DisplayListRenderer::new(width, height, Rgba::TRANSPARENT, font_context)
    .expect("display list renderer should construct")
    .render(&emph_list)
    .expect("render with emphasis");

  let hist = diff_histogram(base_pixmap.data(), emph_pixmap.data());
  assert!(
    !hist.is_empty(),
    "expected emphasis marks to change rendered pixels"
  );

  let emph_green = [0, 255, 0];
  let base_purple = [128, 0, 128];

  assert!(
    hist.contains_key(&emph_green),
    "expected emphasis diff pixels to contain emphasis currentColor {emph_green:?}, got {hist:?}"
  );
  assert!(
    !hist.contains_key(&base_purple),
    "expected emphasis diff pixels not to contain base text color {base_purple:?}, got {hist:?}"
  );
}
