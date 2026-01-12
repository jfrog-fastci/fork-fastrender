#![no_main]

use fastrender::style::color::Rgba;
use fastrender::text::color_fonts::{sanitize_preprocess_parse_svg_glyph_for_fuzzing, MAX_SVG_GLYPH_BYTES};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
  if data.is_empty() {
    return;
  }

  let (color_bytes, svg_bytes) = if data.len() >= 4 {
    data.split_at(4)
  } else {
    (&data[..], &[][..])
  };

  let color = match color_bytes {
    [r, g, b, a] => Rgba::from_rgba8(*r, *g, *b, *a),
    _ => Rgba::rgb(0, 0, 0),
  };

  let svg_bytes = if svg_bytes.len() > MAX_SVG_GLYPH_BYTES {
    &svg_bytes[..MAX_SVG_GLYPH_BYTES]
  } else {
    svg_bytes
  };

  let _ = sanitize_preprocess_parse_svg_glyph_for_fuzzing(svg_bytes, color);
});

