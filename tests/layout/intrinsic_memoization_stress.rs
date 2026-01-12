use fastrender::{DiagnosticsLevel, FastRender, RenderOptions};
use std::fmt::Write;
use std::time::Duration;

#[test]
fn intrinsic_memoization_stress_completes_under_timeout() {
  let mut renderer = FastRender::new().expect("renderer should construct");
  // The first render for a `FastRender` instance can pay one-time costs (font database, shaping
  // caches, etc.). Warm those up so this regression test focuses on layout/intrinsic sizing rather
  // than cold-start initialization.
  renderer
    .render_html_with_diagnostics(
      "<div>Warmup</div>",
      RenderOptions::new()
        .with_viewport(600, 400)
        .with_timeout(None)
        .with_diagnostics_level(DiagnosticsLevel::None),
    )
    .expect("warmup render should succeed");
  let options = RenderOptions::new()
    .with_viewport(600, 400)
    .with_timeout(Some(Duration::from_secs(2)))
    .with_diagnostics_level(DiagnosticsLevel::Basic);

  let mut html = String::new();
  html.push_str(
    r#"
    <style>
      .outer {
        display: grid;
        grid-template-columns: repeat(8, minmax(min-content, max-content));
        width: max-content;
        gap: 0px;
      }
      .item {
        display: block;
        padding: 1px;
        border: 1px solid #000;
      }
      .float {
        float: left;
        width: auto;
        max-width: 100%;
        margin-right: 2px;
      }
      .flex {
        display: flex;
        flex-direction: row;
        flex-wrap: nowrap;
      }
      .cell {
        width: max-content;
        min-width: min-content;
        max-width: max-content;
        padding: 1px;
      }
      .cell span {
        display: inline;
      }
    </style>
    <div class="outer">
  "#,
  );

  for i in 0..60 {
    html.push_str(r#"<div class="item">"#);
    html.push_str(r#"<div class="float"><span>float</span></div>"#);
    html.push_str(r#"<div class="flex">"#);
    html.push_str(r#"<div class="cell"><span>"#);
    html.push_str("Supercalifragilisticexpialidocious ");
    html.push_str("inline ");
    write!(&mut html, "{i}").expect("write to string");
    html.push_str(r#"</span></div>"#);
    html.push_str(r#"<div class="cell"><span>"#);
    html.push_str("pneumonoultramicroscopicsilicovolcanoconiosis ");
    html.push_str("wrap wrap wrap");
    html.push_str(r#"</span></div>"#);
    html.push_str(r#"</div></div>"#);
  }

  html.push_str("</div>");

  let result = renderer
    .render_html_with_diagnostics(&html, options)
    .expect("render should succeed");

  let stats = result
    .diagnostics
    .stats
    .as_ref()
    .expect("expected diagnostics stats");
  let hits = stats.layout.intrinsic_hits.unwrap_or(0);
  assert!(
    hits > 0,
    "expected intrinsic sizing cache hits for stress fixture"
  );
}
