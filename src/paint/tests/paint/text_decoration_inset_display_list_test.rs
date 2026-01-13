use crate::geometry::Rect;
use crate::paint::display_list::DisplayItem;
use crate::paint::display_list::DisplayList;
use crate::paint::display_list::TextDecorationItem;
use crate::text::font_db::FontConfig;
use crate::{
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
fn text_decoration_inset_trims_adjacent_underlines() {
  let left_bg = Rgba::rgb(255, 0, 0);
  let right_bg = Rgba::rgb(0, 255, 0);

  let html = r#"<!doctype html>
  <html>
    <head>
      <style>
        body { margin: 0; background: white; font-size: 20px; white-space: nowrap; }
        .u {
          text-decoration: underline;
          text-decoration-skip-ink: none;
          text-decoration-inset: 10px;
        }
        .left { background: rgb(255, 0, 0); }
        .right { background: rgb(0, 255, 0); }
      </style>
    </head>
    <body><span class="u left">aaaaaaaaaaaaaaaaaaaa</span><span class="u right">bbbbbbbbbbbbbbbbbbbb</span></body>
  </html>
  "#;

  let list = render_display_list(html, 800, 120);
  let left_rect = find_unique_background_rect(&list, left_bg);
  let right_rect = find_unique_background_rect(&list, right_bg);

  let boundary = (left_rect.max_x() + right_rect.min_x()) * 0.5;
  assert!(
    !any_underline_covers_inline_pos(&list, boundary),
    "expected inset underline to leave a gap at the element boundary (x={boundary})"
  );

  // Ensure underlines are still painted within each element.
  let left_inside = left_rect.max_x() - 20.0;
  let right_inside = right_rect.min_x() + 20.0;
  assert!(
    any_underline_covers_inline_pos(&list, left_inside),
    "expected underline to cover a point inside the left element (x={left_inside})"
  );
  assert!(
    any_underline_covers_inline_pos(&list, right_inside),
    "expected underline to cover a point inside the right element (x={right_inside})"
  );
}

#[test]
fn text_decoration_inset_does_not_create_gaps_inside_one_decorating_box() {
  let left_bg = Rgba::rgb(255, 0, 0);
  let right_bg = Rgba::rgb(0, 255, 0);

  let html = r#"<!doctype html>
  <html>
    <head>
      <style>
        body { margin: 0; background: white; font-size: 20px; white-space: nowrap; }
        u {
          text-decoration: underline;
          text-decoration-skip-ink: none;
          text-decoration-inset: 10px;
        }
        .left { background: rgb(255, 0, 0); }
        .right { background: rgb(0, 255, 0); }
      </style>
    </head>
    <body><u><span class="left">aaaaaaaaaaaaaaaaaaaa</span><span class="right">bbbbbbbbbbbbbbbbbbbb</span></u></body>
  </html>
  "#;

  let list = render_display_list(html, 800, 120);
  let left_rect = find_unique_background_rect(&list, left_bg);
  let right_rect = find_unique_background_rect(&list, right_bg);

  let boundary = (left_rect.max_x() + right_rect.min_x()) * 0.5;
  assert!(
    any_underline_covers_inline_pos(&list, boundary),
    "expected underline to remain continuous across internal style boundaries within one decorating box (x={boundary})"
  );
}

#[test]
fn text_decoration_inset_auto_introduces_gap_between_adjacent_underlines() {
  let left_bg = Rgba::rgb(255, 0, 0);
  let right_bg = Rgba::rgb(0, 255, 0);

  let html = r#"<!doctype html>
  <html>
    <head>
      <style>
        body { margin: 0; background: white; font-size: 20px; white-space: nowrap; }
        .u {
          text-decoration: underline;
          text-decoration-skip-ink: none;
          text-decoration-inset: auto;
        }
        .left { background: rgb(255, 0, 0); }
        .right { background: rgb(0, 255, 0); }
      </style>
    </head>
    <body><span class="u left">aaaaaaaaaaaaaaaaaaaa</span><span class="u right">bbbbbbbbbbbbbbbbbbbb</span></body>
  </html>
  "#;

  let list = render_display_list(html, 800, 120);
  let left_rect = find_unique_background_rect(&list, left_bg);
  let right_rect = find_unique_background_rect(&list, right_bg);

  let boundary = (left_rect.max_x() + right_rect.min_x()) * 0.5;
  assert!(
    !any_underline_covers_inline_pos(&list, boundary),
    "expected text-decoration-inset:auto to introduce a visible gap at the element boundary (x={boundary})"
  );
}

