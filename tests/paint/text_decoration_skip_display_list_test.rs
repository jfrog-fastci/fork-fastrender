use fastrender::geometry::Rect;
use fastrender::paint::display_list::DisplayItem;
use fastrender::paint::display_list::DisplayList;
use fastrender::paint::display_list::TextDecorationItem;
use fastrender::text::font_db::FontConfig;
use fastrender::{
  FastRender, LayoutParallelism, PaintParallelism, RenderArtifactRequest, RenderArtifacts,
  RenderOptions, Rgba,
};

fn render_display_list(html: &str, width: u32, height: u32) -> DisplayList {
  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("renderer");
  let options = RenderOptions::new()
    .with_viewport(width, height)
    .with_paint_parallelism(PaintParallelism::disabled())
    .with_layout_parallelism(LayoutParallelism::disabled());
  let mut artifacts = RenderArtifacts::new(RenderArtifactRequest {
    display_list: true,
    ..RenderArtifactRequest::none()
  });
  renderer
    .render_html_with_options_and_artifacts(html, options, &mut artifacts)
    .expect("render html");
  artifacts.display_list.take().expect("display list")
}

fn find_unique_background_rect(list: &DisplayList, color: Rgba) -> Rect {
  let mut rect: Option<Rect> = None;
  for item in list.items() {
    let candidate = match item {
      DisplayItem::FillRect(fill) if fill.color == color => Some(fill.rect),
      DisplayItem::FillRoundedRect(fill) if fill.color == color => Some(fill.rect),
      _ => None,
    };
    if let Some(candidate) = candidate {
      assert!(
        rect.is_none(),
        "expected unique background rect for {color:?}, found multiple"
      );
      rect = Some(candidate);
    }
  }
  rect.expect("expected background rect not found")
}

fn find_background_rect_bounds(list: &DisplayList, color: Rgba) -> Rect {
  let mut min_x = f32::INFINITY;
  let mut min_y = f32::INFINITY;
  let mut max_x = f32::NEG_INFINITY;
  let mut max_y = f32::NEG_INFINITY;
  let mut found = false;

  for item in list.items() {
    let rect = match item {
      DisplayItem::FillRect(fill) if fill.color == color => Some(fill.rect),
      DisplayItem::FillRoundedRect(fill) if fill.color == color => Some(fill.rect),
      _ => None,
    };
    let Some(rect) = rect else {
      continue;
    };

    found = true;
    min_x = min_x.min(rect.min_x());
    min_y = min_y.min(rect.min_y());
    max_x = max_x.max(rect.max_x());
    max_y = max_y.max(rect.max_y());
  }

  assert!(found, "expected background rect not found for {color:?}");
  Rect::from_points(fastrender::geometry::Point::new(min_x, min_y), fastrender::geometry::Point::new(max_x, max_y))
}

fn underline_covers_inline_pos(item: &TextDecorationItem, inline_pos: f32) -> bool {
  if item.inline_vertical {
    return false;
  }
  let eps = 0.01;
  for deco in &item.decorations {
    let Some(stroke) = &deco.underline else {
      continue;
    };
    if let Some(segments) = &stroke.segments {
      for (start, end) in segments {
        let abs_start = item.line_start + *start;
        let abs_end = item.line_start + *end;
        if inline_pos + eps >= abs_start && inline_pos - eps <= abs_end {
          return true;
        }
      }
    } else {
      let abs_start = item.line_start;
      let abs_end = item.line_start + item.line_width;
      if inline_pos + eps >= abs_start && inline_pos - eps <= abs_end {
        return true;
      }
    }
  }
  false
}

fn any_underline_covers_inline_pos(list: &DisplayList, inline_pos: f32) -> bool {
  list.items().iter().any(|item| match item {
    DisplayItem::TextDecoration(decoration) => underline_covers_inline_pos(decoration, inline_pos),
    _ => false,
  })
}

#[test]
fn text_items_preserve_variable_font_wght_variations() {
  let html = r#"
    <!doctype html>
    <html>
      <head>
        <style>
          @font-face {
            font-family: "TestVar";
            src: url("tests/fixtures/fonts/TestVar.ttf") format("truetype");
          }
          body { margin: 0; background: white; }
          .sample {
            margin: 32px;
            font-family: "TestVar";
            font-size: 120px;
            line-height: 1.1;
          }
        </style>
      </head>
      <body>
        <div class="sample" style="font-weight: 100">A</div>
        <div class="sample" style="font-weight: 900">A</div>
      </body>
    </html>
  "#;

  let list = render_display_list(html, 460, 400);
  let all_text_runs: Vec<_> = list
    .items()
    .iter()
    .filter_map(|item| match item {
      DisplayItem::Text(text) => Some(text),
      _ => None,
    })
    .collect();
  let all_families: Vec<_> = all_text_runs
    .iter()
    .filter_map(|text| text.font.as_ref().map(|font| font.family.clone()))
    .collect();

  let mut runs: Vec<_> = all_text_runs
    .into_iter()
    .filter(|text| {
      text
        .font
        .as_ref()
        .is_some_and(|font| font.family == "TestVar" && !text.glyphs.is_empty())
    })
    .collect();

  assert!(
    runs.len() >= 2,
    "expected at least two TestVar text runs, found {} (all text families: {all_families:?})",
    runs.len(),
  );

  runs.sort_by(|a, b| a.origin.y.partial_cmp(&b.origin.y).unwrap_or(std::cmp::Ordering::Equal));
  let first = runs[0];
  let second = runs[1];

  let wght_tag = ttf_parser::Tag::from_bytes(b"wght");
  let wght_value = |run: &fastrender::paint::display_list::TextItem| -> Option<f32> {
    run
      .variations
      .iter()
      .find(|v| v.tag == wght_tag)
      .map(|v| v.value())
  };

  let first_wght = wght_value(first);
  let second_wght = wght_value(second);
  assert!(
    first_wght.is_some() && second_wght.is_some(),
    "expected both runs to carry wght variation (first={first_wght:?}, second={second_wght:?})"
  );
  assert!(
    second_wght.unwrap() > first_wght.unwrap(),
    "expected second run to have higher wght than first (first={first_wght:?}, second={second_wght:?})"
  );
}

#[test]
fn text_decoration_skip_objects_default_skips_atomic_inlines_and_none_draws_across() {
  let obj_color = Rgba::new(1, 2, 3, 1.0);
  let base_html = r#"
    <!doctype html>
    <html>
      <head>
        <style>
          body { margin: 0; background: white; font-size: 20px; }
          .container {
            text-decoration: underline;
            text-decoration-skip-ink: none;
            white-space: nowrap;
          }
          .obj {
            display: inline-block;
            width: 60px;
            height: 20px;
            background: rgb(1, 2, 3);
          }
        </style>
      </head>
      <body>
        <div class="container">hello<span class="obj"></span>world</div>
      </body>
    </html>
  "#;

  let list = render_display_list(base_html, 240, 80);
  let obj_rect = find_unique_background_rect(&list, obj_color);
  let obj_center = obj_rect.x() + obj_rect.width() * 0.5;
  assert!(
    !any_underline_covers_inline_pos(&list, obj_center),
    "expected underline to skip object center (x={obj_center}), but it was decorated"
  );

  let noskip_html = r#"
    <!doctype html>
    <html>
      <head>
        <style>
          body { margin: 0; background: white; font-size: 20px; }
          .container {
            text-decoration: underline;
            text-decoration-skip-ink: none;
            white-space: nowrap;
          }
          .obj {
            display: inline-block;
            width: 60px;
            height: 20px;
            background: rgb(1, 2, 3);
            -webkit-text-decoration-skip: none;
          }
        </style>
      </head>
      <body>
        <div class="container">hello<span class="obj"></span>world</div>
      </body>
    </html>
  "#;

  let list = render_display_list(noskip_html, 240, 80);
  let obj_rect = find_unique_background_rect(&list, obj_color);
  let obj_center = obj_rect.x() + obj_rect.width() * 0.5;
  assert!(
    any_underline_covers_inline_pos(&list, obj_center),
    "expected underline to draw across object center (x={obj_center}) when skipping disabled"
  );
}

#[test]
fn text_decoration_skip_spaces_clips_leading_and_trailing_spacers() {
  let html = r#"
    <!doctype html>
    <html>
      <head>
        <style>
          body { margin: 0; background: white; font-size: 20px; }
          .sample {
            white-space: pre;
            text-decoration: underline;
            text-decoration-skip-ink: none;
          }
        </style>
      </head>
      <body>
        <div class="sample">  hi  </div>
      </body>
    </html>
  "#;

  let list = render_display_list(html, 240, 80);
  let underline_segments = list.items().iter().find_map(|item| match item {
    DisplayItem::TextDecoration(decoration) => decoration
      .decorations
      .iter()
      .find_map(|deco| deco.underline.as_ref().and_then(|s| s.segments.clone()))
      .map(|segments| (decoration.line_width, segments)),
    _ => None,
  });
  let (line_width, segments) = underline_segments.expect("expected underline segments");
  assert!(
    segments.iter().any(|(start, end)| *start > 0.5 && *end < line_width - 0.5),
    "expected clipped underline segments due to leading/trailing spaces, got {segments:?} (line_width={line_width})"
  );

  let html = r#"
    <!doctype html>
    <html>
      <head>
        <style>
          body { margin: 0; background: white; font-size: 20px; }
          .sample {
            white-space: pre;
            text-decoration: underline;
            text-decoration-skip-ink: none;
            text-decoration-skip-spaces: none;
          }
        </style>
      </head>
      <body>
        <div class="sample">  hi  </div>
      </body>
    </html>
  "#;

  let list = render_display_list(html, 240, 80);
  let underline = list.items().iter().find_map(|item| match item {
    DisplayItem::TextDecoration(decoration) => decoration
      .decorations
      .iter()
      .find_map(|deco| deco.underline.as_ref().map(|stroke| (decoration.line_width, stroke))),
    _ => None,
  });
  let (line_width, stroke) = underline.expect("expected underline");
  let (min_start, max_end) = match stroke.segments.as_ref() {
    Some(segments) if !segments.is_empty() => segments
      .iter()
      .fold((f32::INFINITY, f32::NEG_INFINITY), |(min_start, max_end), (start, end)| {
        (min_start.min(*start), max_end.max(*end))
      }),
    _ => (0.0, line_width),
  };
  assert!(
    min_start <= 0.5 && max_end >= line_width - 0.5,
    "expected underline to cover the line edges when text-decoration-skip-spaces is none, got min_start={min_start} max_end={max_end} (line_width={line_width})"
  );
}

#[test]
fn text_decoration_skip_box_all_skips_inline_padding_for_ancestor_decorations() {
  let box_color = Rgba::new(8, 7, 6, 1.0);
  let base_html = r#"
    <!doctype html>
    <html>
      <head>
        <style>
          body { margin: 0; background: white; font-size: 20px; }
          .container {
            text-decoration: underline;
            text-decoration-skip-ink: none;
            white-space: nowrap;
          }
          .boxed {
            padding: 0 20px;
            background: rgb(8, 7, 6);
          }
        </style>
      </head>
      <body>
        <div class="container">hello<span class="boxed">X</span>world</div>
      </body>
    </html>
  "#;

  let list = render_display_list(base_html, 320, 80);
  let box_rect = find_background_rect_bounds(&list, box_color);
  let padding_probe = box_rect.x() + 10.0;
  assert!(
    any_underline_covers_inline_pos(&list, padding_probe),
    "expected underline to cover inline padding when text-decoration-skip-box is none (x={padding_probe})"
  );

  let skip_html = r#"
    <!doctype html>
    <html>
      <head>
        <style>
          body { margin: 0; background: white; font-size: 20px; }
          .container {
            text-decoration: underline;
            text-decoration-skip-ink: none;
            white-space: nowrap;
          }
          .boxed {
            padding: 0 20px;
            background: rgb(8, 7, 6);
            text-decoration-skip-box: all;
          }
        </style>
      </head>
      <body>
        <div class="container">hello<span class="boxed">X</span>world</div>
      </body>
    </html>
  "#;

  let list = render_display_list(skip_html, 320, 80);
  let box_rect = find_background_rect_bounds(&list, box_color);
  let padding_probe = box_rect.x() + 10.0;
  assert!(
    !any_underline_covers_inline_pos(&list, padding_probe),
    "expected underline to skip inline padding when text-decoration-skip-box is all (x={padding_probe})"
  );
}
