use super::util::{bounding_box_for_color, create_stacking_context_bounds_renderer_legacy};
use crate::RenderOptions;

fn red_dominant((r, g, b, a): (u8, u8, u8, u8)) -> bool {
  a > 0 && r > g.saturating_add(20) && r > b.saturating_add(20)
}

fn green_dominant((r, g, b, a): (u8, u8, u8, u8)) -> bool {
  a > 0 && g > r.saturating_add(20) && g > b.saturating_add(20)
}

fn blue_dominant((r, g, b, a): (u8, u8, u8, u8)) -> bool {
  a > 0 && b > r.saturating_add(20) && b > g.saturating_add(20)
}

#[test]
fn text_decoration_spelling_grammar_legacy_renders_and_ignores_authored_paint() {
  let html = r#"
    <style>
      body { margin: 0; background: white; }
      #target {
        position: absolute;
        left: 20px;
        top: 40px;
        font-size: 64px;
        font-family: sans-serif;
        color: black;
        -webkit-font-smoothing: antialiased;

        text-decoration-line: spelling-error grammar-error;
        text-decoration-color: blue;
        text-decoration-style: solid;
        text-decoration-thickness: 8px;
        text-decoration-skip-ink: all;
        text-underline-offset: 20px;
      }
    </style>
    <div id="target">Hello</div>
  "#;

  let mut renderer = create_stacking_context_bounds_renderer_legacy();
  let pixmap = renderer
    .render_html_with_options(html, RenderOptions::new().with_viewport(420, 220))
    .expect("render html (legacy)");

  assert!(
    bounding_box_for_color(&pixmap, red_dominant).is_some(),
    "expected spelling-error (red) underline to render in legacy painter"
  );
  assert!(
    bounding_box_for_color(&pixmap, green_dominant).is_some(),
    "expected grammar-error (green) underline to render in legacy painter"
  );
  assert!(
    bounding_box_for_color(&pixmap, blue_dominant).is_none(),
    "expected authored text-decoration-color to be ignored for spelling/grammar decorations"
  );
}

#[test]
fn text_decoration_spelling_grammar_legacy_vertical_writing_mode_renders() {
  let html = r#"
    <style>
      body { margin: 0; background: white; }
      #target {
        position: absolute;
        left: 200px;
        top: 20px;
        font-size: 64px;
        font-family: sans-serif;
        color: black;
        -webkit-font-smoothing: antialiased;
        writing-mode: vertical-rl;

        text-decoration-line: spelling-error grammar-error;
        text-decoration-color: blue;
        text-decoration-style: solid;
        text-decoration-thickness: 8px;
        text-decoration-skip-ink: all;
        text-underline-offset: 20px;
      }
    </style>
    <div id="target">Hello</div>
  "#;

  let mut renderer = create_stacking_context_bounds_renderer_legacy();
  let pixmap = renderer
    .render_html_with_options(html, RenderOptions::new().with_viewport(420, 420))
    .expect("render html (legacy)");

  assert!(
    bounding_box_for_color(&pixmap, red_dominant).is_some(),
    "expected spelling-error (red) underline to render in vertical writing mode (legacy painter)"
  );
  assert!(
    bounding_box_for_color(&pixmap, green_dominant).is_some(),
    "expected grammar-error (green) underline to render in vertical writing mode (legacy painter)"
  );
  assert!(
    bounding_box_for_color(&pixmap, blue_dominant).is_none(),
    "expected authored text-decoration-color to be ignored for spelling/grammar decorations"
  );
}
