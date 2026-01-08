use fastrender::FontDatabase;

#[test]
fn bundled_emoji_fixture_covers_pageset_observed_codepoints() {
  let mut db = FontDatabase::empty();
  db.load_font_data(include_bytes!("../fixtures/fonts/FastRenderEmoji.ttf").to_vec())
    .expect("load FastRenderEmoji.ttf fixture");

  let emoji_face = db
    .faces()
    .next()
    .expect("fixture should expose a font face")
    .id;

  // Pageset-driven emoji regressions (see docs/notes/bundled-fonts.md).
  for codepoint in [
    0x1F34C, // 🍌
    0x26BE,  // ⚾
    0x1F48E, // 💎
    0x1F49C, // 💜
    0x1F4A9, // 💩
    0x1F534, // 🔴
    0x1F64F, // 🙏
    0x1F923, // 🤣
    0x1F92E, // 🤮
    0x1F973, // 🥳
    0x1F9F5, // 🧵
    0x1FAAC, // 🪬
  ] {
    let ch = char::from_u32(codepoint).expect("valid Unicode scalar");
    assert!(
      db.has_glyph(emoji_face, ch),
      "FastRenderEmoji.ttf should map U+{codepoint:04X} ({ch}) to a non-.notdef glyph"
    );
  }
}

