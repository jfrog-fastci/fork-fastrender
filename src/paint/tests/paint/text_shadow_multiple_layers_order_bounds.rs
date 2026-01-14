use super::util::create_stacking_context_bounds_renderer;
use crate::paint::display_list::TextShadowItem;
use crate::{
  DisplayItem, DisplayList, DisplayListOptimizer, GlyphInstance, PaintTextItem as TextItem, Point,
  Rect, RenderArtifactRequest, RenderArtifacts, RenderOptions, Rgba,
};

fn capture_display_list(html: &str, width: u32, height: u32) -> DisplayList {
  let mut renderer = create_stacking_context_bounds_renderer();
  let options = RenderOptions::new().with_viewport(width, height);
  let mut artifacts = RenderArtifacts::new(RenderArtifactRequest {
    display_list: true,
    ..RenderArtifactRequest::none()
  });
  renderer
    .render_html_with_options_and_artifacts(html, options, &mut artifacts)
    .expect("render html");
  artifacts.display_list.take().expect("display list captured")
}

fn pixmap_has_strong_blue_pixels(pixmap: &tiny_skia::Pixmap) -> bool {
  pixmap.pixels().iter().any(|px| {
    let a = px.alpha();
    if a == 0 {
      return false;
    }
    let r = px.red();
    let g = px.green();
    let b = px.blue();
    b.saturating_sub(r) > 32 && b.saturating_sub(g) > 32
  })
}

fn pixmap_has_strong_red_pixels(pixmap: &tiny_skia::Pixmap) -> bool {
  pixmap.pixels().iter().any(|px| {
    let a = px.alpha();
    if a == 0 {
      return false;
    }
    let r = px.red();
    let g = px.green();
    let b = px.blue();
    r.saturating_sub(g) > 32 && r.saturating_sub(b) > 32
  })
}

fn pixmap_has_opaque_near_black_pixels(pixmap: &tiny_skia::Pixmap) -> bool {
  pixmap.pixels().iter().any(|px| {
    if px.alpha() != 255 {
      return false;
    }
    let r = px.red();
    let g = px.green();
    let b = px.blue();
    r < 16 && g < 16 && b < 16
  })
}

#[test]
fn text_shadow_multiple_layers_preserves_css_order_in_display_list() {
  let html = r#"
    <!doctype html>
    <style>
      html, body { margin: 0; background: white; }
      #t {
        position: absolute;
        left: 10px;
        top: 10px;
        font: 40px/1 sans-serif;
        color: rgb(0, 0, 0);
        text-shadow:
          rgb(255, 0, 0) 2px 0 0,
          rgb(0, 0, 255) 4px 0 0;
      }
    </style>
    <div id="t">A</div>
  "#;

  let list = capture_display_list(html, 200, 100);
  let text = list
    .items()
    .iter()
    .find_map(|item| match item {
      DisplayItem::Text(text) if text.shadows.len() == 2 => Some(text),
      _ => None,
    })
    .expect("expected a Text item with 2 shadows");

  assert_eq!(text.color, Rgba::BLACK, "expected opaque text fill");
  assert_eq!(
    text.shadows[0].color,
    Rgba::RED,
    "expected first shadow to be red"
  );
  assert_eq!(
    text.shadows[1].color,
    Rgba::BLUE,
    "expected second shadow to be blue"
  );
  assert_eq!(
    text.shadows[0].offset,
    Point::new(2.0, 0.0),
    "expected first shadow offset to match CSS order"
  );
  assert_eq!(
    text.shadows[1].offset,
    Point::new(4.0, 0.0),
    "expected second shadow offset to match CSS order"
  );
}

#[test]
fn text_shadow_is_painted_behind_opaque_text_fill() {
  // Regression coverage: even when a blurred shadow overlaps the glyph interior, the opaque text
  // fill must be painted on top (CSS text-shadow semantics).
  //
  // This test compares a baseline render (no shadow) to a shadowed render and asserts that at least
  // one fully opaque "near black" pixel is unchanged, while the shadowed render still contains
  // visible red pixels.
  let html_base = r#"
    <!doctype html>
    <style>
      html, body { margin: 0; background: white; }
      #t {
        position: absolute;
        left: 40px;
        top: 10px;
        font-family: "DejaVu Sans";
        font-size: 120px;
        font-weight: 900;
        line-height: 1;
        -webkit-font-smoothing: antialiased;
        color: rgb(0, 0, 0);
      }
    </style>
    <div id="t">MMM</div>
  "#;

  let html_shadow = r#"
    <!doctype html>
    <style>
      html, body { margin: 0; background: white; }
      #t {
        position: absolute;
        left: 40px;
        top: 10px;
        font-family: "DejaVu Sans";
        font-size: 120px;
        font-weight: 900;
        line-height: 1;
        -webkit-font-smoothing: antialiased;
        color: rgb(0, 0, 0);
        text-shadow: rgb(255, 0, 0) 0 0 12px;
      }
    </style>
    <div id="t">MMM</div>
  "#;

  let width = 500;
  let height = 220;

  let mut renderer = create_stacking_context_bounds_renderer();
  let base = renderer
    .render_html(html_base, width, height)
    .expect("render baseline html");
  let mut renderer = create_stacking_context_bounds_renderer();
  let shadowed = renderer
    .render_html(html_shadow, width, height)
    .expect("render shadow html");

  assert!(
    pixmap_has_opaque_near_black_pixels(&base),
    "expected baseline render to contain opaque black glyph pixels"
  );
  assert!(
    !pixmap_has_strong_red_pixels(&base),
    "baseline render should not contain strong red pixels (no text-shadow)"
  );
  assert!(
    pixmap_has_strong_red_pixels(&shadowed),
    "shadowed render should contain strong red pixels from the text-shadow"
  );

  let mut candidates = 0usize;
  let mut unchanged = 0usize;
  for (a, b) in base.pixels().iter().zip(shadowed.pixels().iter()) {
    if a.alpha() != 255 {
      continue;
    }
    if a.red() >= 16 || a.green() >= 16 || a.blue() >= 16 {
      continue;
    }
    candidates += 1;
    if a.red() == b.red() && a.green() == b.green() && a.blue() == b.blue() && a.alpha() == b.alpha()
    {
      unchanged += 1;
      break;
    }
  }

  assert!(
    candidates > 0,
    "expected to find at least one opaque near-black baseline pixel to compare"
  );
  assert!(
    unchanged > 0,
    "expected at least one opaque near-black pixel to remain unchanged with text-shadow; this implies the fill is painted over the shadow (not vice versa). candidates={candidates}"
  );
}

#[test]
fn text_item_bounds_include_furthest_text_shadow_extent_for_culling() {
  // Use cached bounds so the test doesn't depend on font shaping or glyph metrics. The goal is to
  // ensure display-list bounds (used by viewport culling and stacking-context unioning) reflect the
  // furthest shadow extent, including blur.
  let base_bounds = Rect::from_xywh(-100.0, 20.0, 40.0, 20.0);
  let text = TextItem {
    origin: Point::new(-100.0, 40.0),
    cached_bounds: Some(base_bounds),
    glyphs: vec![GlyphInstance {
      glyph_id: 1,
      cluster: 0,
      x_offset: 0.0,
      y_offset: 0.0,
      x_advance: 40.0,
      y_advance: 0.0,
    }],
    color: Rgba::BLACK,
    font_size: 16.0,
    advance_width: 40.0,
    shadows: vec![
      TextShadowItem {
        offset: Point::new(40.0, 0.0),
        blur_radius: 0.0,
        color: Rgba::RED,
      },
      TextShadowItem {
        offset: Point::new(55.0, 0.0),
        // `TextShadowItem.blur_radius` is a gaussian sigma. 5px sigma corresponds to a ~15px
        // 3-sigma blur halo, which should bring an otherwise-offscreen shadow into view.
        blur_radius: 5.0,
        color: Rgba::BLUE,
      },
    ],
    ..Default::default()
  };

  let expected_bounds = base_bounds
    .union(base_bounds.translate(Point::new(40.0, 0.0)))
    .union(base_bounds.translate(Point::new(55.0, 0.0)).inflate(5.0 * 3.0));

  let mut list = DisplayList::new();
  list.push(DisplayItem::Text(text));

  let bounds = list.items()[0].bounds().expect("text bounds");
  assert_eq!(
    bounds, expected_bounds,
    "expected Text bounds to include shadow offsets + blur halo"
  );

  // The base glyph bounds and the first shadow are entirely offscreen (< 0). Only the second
  // shadow's blur halo intersects the viewport.
  let viewport = Rect::from_xywh(0.0, 0.0, 100.0, 100.0);
  let optimizer = DisplayListOptimizer::new();
  let culled = optimizer.intersect(&list, viewport);
  assert!(
    culled.items().iter().any(|item| matches!(item, DisplayItem::Text(_))),
    "expected viewport culling to keep the text item because its blurred shadow intersects the viewport"
  );
}

#[test]
fn emoji_text_shadow_multiple_layers_survive_culling_and_paint_in_order() {
  let html = r#"
    <!doctype html>
    <style>
      html, body { margin: 0; background: white; }
      .emoji {
        position: absolute;
        left: -200px;
        top: 20px;
        font-family: "FastRender Emoji", emoji, sans-serif;
        font-size: 96px;
        line-height: 1;
        color: transparent;
        /* Two identical-offset shadows so order is observable without knowing glyph geometry. */
        text-shadow:
          rgb(255, 0, 0) 220px 0 0,
          rgb(0, 0, 255) 220px 0 0;
      }
    </style>
    <div class="emoji">😀</div>
  "#;

  let viewport_width = 240;
  let viewport_height = 160;
  let list = capture_display_list(html, viewport_width, viewport_height);

  let text = list.items().iter().find_map(|item| match item {
    DisplayItem::Text(text) if text.shadows.len() == 2 => Some(text),
    _ => None,
  });
  let Some(text) = text else {
    let shadow_counts: Vec<usize> = list
      .items()
      .iter()
      .filter_map(|item| match item {
        DisplayItem::Text(text) => Some(text.shadows.len()),
        _ => None,
      })
      .collect();
    let first_kinds: Vec<&'static str> = list
      .items()
      .iter()
      .take(16)
      .map(|item| match item {
        DisplayItem::FillRect(_) => "FillRect",
        DisplayItem::StrokeRect(_) => "StrokeRect",
        DisplayItem::Outline(_) => "Outline",
        DisplayItem::FillRoundedRect(_) => "FillRoundedRect",
        DisplayItem::StrokeRoundedRect(_) => "StrokeRoundedRect",
        DisplayItem::Text(_) => "Text",
        DisplayItem::Image(_) => "Image",
        DisplayItem::ImagePattern(_) => "ImagePattern",
        DisplayItem::BoxShadow(_) => "BoxShadow",
        DisplayItem::RemoteFrameSlot(_) => "RemoteFrameSlot",
        DisplayItem::ListMarker(_) => "ListMarker",
        DisplayItem::LinearGradient(_) => "LinearGradient",
        DisplayItem::LinearGradientPattern(_) => "LinearGradientPattern",
        DisplayItem::RadialGradient(_) => "RadialGradient",
        DisplayItem::RadialGradientPattern(_) => "RadialGradientPattern",
        DisplayItem::ConicGradient(_) => "ConicGradient",
        DisplayItem::ConicGradientPattern(_) => "ConicGradientPattern",
        DisplayItem::Border(_) => "Border",
        DisplayItem::TableCollapsedBorders(_) => "TableCollapsedBorders",
        DisplayItem::TextDecoration(_) => "TextDecoration",
        DisplayItem::PushClip(_) => "PushClip",
        DisplayItem::PopClip => "PopClip",
        DisplayItem::PushOpacity(_) => "PushOpacity",
        DisplayItem::PopOpacity => "PopOpacity",
        DisplayItem::PushTransform(_) => "PushTransform",
        DisplayItem::PopTransform => "PopTransform",
        DisplayItem::PushBlendMode(_) => "PushBlendMode",
        DisplayItem::PopBlendMode => "PopBlendMode",
        DisplayItem::PushStackingContext(_) => "PushStackingContext",
        DisplayItem::PopStackingContext => "PopStackingContext",
        DisplayItem::PushBackfaceVisibility(_) => "PushBackfaceVisibility",
        DisplayItem::PopBackfaceVisibility => "PopBackfaceVisibility",
        _ => "Other",
      })
      .collect();
    panic!(
      "expected emoji to produce a Text item with 2 shadows; list_len={} first_items={first_kinds:?} text_shadow_counts={shadow_counts:?}",
      list.items().len()
    );
  };

  assert_eq!(
    text.shadows[0].color,
    Rgba::RED,
    "expected first emoji shadow to be red"
  );
  assert_eq!(
    text.shadows[1].color,
    Rgba::BLUE,
    "expected second emoji shadow to be blue"
  );
  assert_eq!(
    text.shadows[0].offset,
    Point::new(220.0, 0.0),
    "expected first emoji shadow offset to match CSS"
  );
  assert_eq!(
    text.shadows[1].offset,
    Point::new(220.0, 0.0),
    "expected second emoji shadow offset to match CSS"
  );

  // Exercise viewport culling explicitly: the emoji itself is offscreen, so the text item is only
  // kept if its bounds include the shadow offsets.
  let optimizer = DisplayListOptimizer::new();
  let viewport = Rect::from_xywh(0.0, 0.0, viewport_width as f32, viewport_height as f32);
  let culled = optimizer.intersect(&list, viewport);
  assert!(
    culled.items().iter().any(|item| matches!(item, DisplayItem::Text(t) if t.shadows.len() == 2)),
    "expected viewport culling to preserve the shadowed emoji text item"
  );

  // Render to ensure the shadow pipeline applies to color emoji glyphs too. The two shadows overlap
  // exactly; if the CSS order is respected (red painted first, blue last), the visible silhouette
  // should be overwhelmingly blue.
  let mut renderer = create_stacking_context_bounds_renderer();
  let pixmap = renderer
    .render_html(html, viewport_width, viewport_height)
    .expect("render emoji html");
  assert!(
    pixmap_has_strong_blue_pixels(&pixmap),
    "expected emoji text-shadow output to contain strong blue pixels from the last shadow"
  );
  assert!(
    !pixmap_has_strong_red_pixels(&pixmap),
    "expected emoji text-shadow output to not contain strong red pixels when blue shadow is painted last"
  );
}
