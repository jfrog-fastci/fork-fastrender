use fastrender::css::types::{FontDisplay, FontFaceRule, FontFaceSource};
use fastrender::debug::runtime::{self, RuntimeToggles};
use fastrender::text::font_db::{FontDatabase, FontStyle};
use fastrender::text::font_loader::FontContext;
use fastrender::dom::DomNodeType;
use fastrender::style::cascade::StyledNode;
use fastrender::style::media::MediaType;
use fastrender::{BoxNode, FastRender, FastRenderConfig, FontConfig, FragmentContent, FragmentNode};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use url::Url;

fn find_styled_node_id_for_dom_id(node: &StyledNode, id_value: &str) -> Option<usize> {
  if let DomNodeType::Element { attributes, .. } = &node.node.node_type {
    if attributes
      .iter()
      .any(|(k, v)| k.eq_ignore_ascii_case("id") && v == id_value)
    {
      return Some(node.node_id);
    }
  }

  for child in node.children.iter() {
    if let Some(found) = find_styled_node_id_for_dom_id(child, id_value) {
      return Some(found);
    }
  }

  None
}

fn find_box_id_for_styled_node_id(node: &BoxNode, styled_node_id: usize) -> Option<usize> {
  if node.generated_pseudo.is_none() && node.styled_node_id == Some(styled_node_id) {
    return Some(node.id);
  }
  for child in node.children.iter() {
    if let Some(found) = find_box_id_for_styled_node_id(child, styled_node_id) {
      return Some(found);
    }
  }
  if let Some(footnote_body) = node.footnote_body.as_deref() {
    if let Some(found) = find_box_id_for_styled_node_id(footnote_body, styled_node_id) {
      return Some(found);
    }
  }
  None
}

fn find_fragment_height_for_box_id(node: &FragmentNode, box_id: usize) -> Option<f32> {
  let matches_box = match &node.content {
    FragmentContent::Block { box_id: Some(id) }
    | FragmentContent::Inline { box_id: Some(id), .. }
    | FragmentContent::Text { box_id: Some(id), .. }
    | FragmentContent::Replaced { box_id: Some(id), .. } => *id == box_id,
    _ => false,
  };
  if matches_box {
    return Some(node.bounds.height());
  }

  for child in node.children.iter() {
    if let Some(found) = find_fragment_height_for_box_id(child, box_id) {
      return Some(found);
    }
  }

  None
}

#[test]
fn font_display_swap_web_font_participates_in_layout() {
  let font_path = std::path::Path::new("tests/fixtures/fonts/NotoSansMono-subset.ttf");
  if !font_path.exists() {
    return;
  }
  let abs = std::fs::canonicalize(font_path).expect("canonicalize font fixture");
  let font_url = Url::from_file_path(&abs)
    .map_err(|()| ())
    .expect("file url for font fixture")
    .to_string();

  let text = "i".repeat(100);
  let html = format!(
    r#"<!doctype html>
    <html>
      <head>
        <style>
          @font-face {{
            font-family: TestWebMono;
            src: url("{font_url}");
            font-display: swap;
          }}
          body {{ margin: 0; }}
          #box {{
            width: 100px;
            font-size: 16px;
            line-height: 16px;
            word-break: break-all;
            font-family: TestWebMono, sans-serif;
          }}
        </style>
      </head>
      <body>
        <div id="box">{text}</div>
      </body>
    </html>"#
  );

  // The font source is a local `file:` URL, so `font-display: swap` should still participate in
  // layout immediately (matching the "cached on first paint" browser behavior).
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_WEB_FONT_WAIT_MS".to_string(),
    "0".to_string(),
  )]));
  let config = FastRenderConfig::default()
    .with_font_sources(FontConfig::bundled_only())
    .with_runtime_toggles(toggles);
  let mut renderer = FastRender::with_config(config).expect("renderer");
  let dom = renderer.parse_html(&html).expect("parse");
  let intermediates = renderer
    .layout_document_for_media_intermediates(&dom, 200, 200, MediaType::Screen)
    .expect("layout intermediates");

  let styled_node_id =
    find_styled_node_id_for_dom_id(&intermediates.styled_tree, "box").expect("box styled id");
  let box_id = find_box_id_for_styled_node_id(&intermediates.box_tree.root, styled_node_id)
    .expect("box id");
  let height = find_fragment_height_for_box_id(&intermediates.fragment_tree.root, box_id)
    .expect("box fragment height");

  // `TestWebMono` should load quickly from the local file and then be activated before layout.
  // If the layout uses the fallback `sans-serif` face instead, the narrow `i` glyphs should wrap
  // into far fewer lines; the monospace face produces a much taller block.
  assert!(
    height > 120.0,
    "expected swap web font to affect wrapping; got height {height:.2}px"
  );
}

#[test]
fn local_web_fonts_are_not_capped_by_max_web_fonts() {
  let font_path = std::path::Path::new("tests/fixtures/fonts/NotoSansMono-subset.ttf");
  if !font_path.exists() {
    return;
  }
  let abs = std::fs::canonicalize(font_path).expect("canonicalize font fixture");
  let font_url = Url::from_file_path(&abs)
    .map_err(|()| ())
    .expect("file url for font fixture")
    .to_string();

  let mut toggles = HashMap::new();
  toggles.insert("FASTR_MAX_WEB_FONTS".to_string(), "1".to_string());
  let toggles = Arc::new(RuntimeToggles::from_map(toggles));

  runtime::with_thread_runtime_toggles(toggles, || {
    let ctx = FontContext::with_database(Arc::new(FontDatabase::empty()));
    let faces: Vec<FontFaceRule> = (0..10)
      .map(|i| FontFaceRule {
        family: Some(format!("LocalWebFamily{}", i)),
        sources: vec![FontFaceSource::url(font_url.clone())],
        display: Some(FontDisplay::Swap),
        ..Default::default()
      })
      .collect();

    ctx
      .load_web_fonts(&faces, None, None)
      .expect("schedule web font loads");
    assert!(
      ctx.wait_for_pending_web_fonts(Duration::from_secs(1)),
      "expected local font loads to settle"
    );

    // If we mistakenly cap non-HTTP/HTTPS faces with FASTR_MAX_WEB_FONTS, the later families would
    // be declared but never loaded, and `get_font_simple` would return `None`.
    assert!(
      ctx
        .get_font_simple("LocalWebFamily9", 400, FontStyle::Normal)
        .is_some(),
      "expected local web fonts to load past FASTR_MAX_WEB_FONTS cap"
    );
  });
}
