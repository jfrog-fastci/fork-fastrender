use fastrender::api::FastRender;

fn find_colored_bbox(
  data: &[u8],
  width: u32,
  height: u32,
  predicate: impl Fn(u8, u8, u8, u8) -> bool,
) -> Option<(u32, u32, u32, u32)> {
  let mut min_x = width;
  let mut min_y = height;
  let mut max_x = 0u32;
  let mut max_y = 0u32;
  let mut any = false;

  for y in 0..height {
    for x in 0..width {
      let i = ((y * width + x) * 4) as usize;
      let (r, g, b, a) = (data[i], data[i + 1], data[i + 2], data[i + 3]);
      if predicate(r, g, b, a) {
        any = true;
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
      }
    }
  }

  any.then_some((min_x, min_y, max_x, max_y))
}

#[test]
fn grid_column_shorthand_then_end_longhand_preserves_start_component() {
  // Regression for `grid-column` shorthand + `grid-column-end` longhand interaction.
  //
  // A common pattern (used by MDN's homepage) is:
  //   .grid > * { grid-column: content; }   /* sets the start component */
  //   .hero { grid-column-end: full-end; }  /* overrides only the end component */
  //
  // When the stored raw shorthand value omits a `/` separator, the end-longhand must treat the
  // entire raw string as the start component (rather than defaulting back to `auto`).
  let html = r#"
    <style>
      body { margin: 0; background: white; }
      .container {
        display: grid;
        width: 120px;
        grid-template-columns: 10px [content-start] 100px [content-end] 10px [extended-full-end];
      }
      .container > * { grid-column: content-start; }
      .item {
        grid-column-end: extended-full-end;
        height: 20px;
        background: rgb(255 0 0);
      }
    </style>
    <div class="container">
      <div class="item"></div>
    </div>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let pixmap = renderer.render_html(html, 200, 60).expect("rendered");
  let bbox = find_colored_bbox(pixmap.data(), pixmap.width(), pixmap.height(), |r, g, b, a| {
    a > 0 && r > 200 && g < 50 && b < 50
  })
  .expect("found red pixels");

  // The rectangle should start after the first 10px track and extend to the end of the 10px track
  // before `extended-full-end` (10 + 100 + 10 = 120px total).
  assert!(
    bbox.0.abs_diff(10) <= 1,
    "expected red rect min_x ~= 10, got {} (bbox={bbox:?})",
    bbox.0
  );
  assert!(
    bbox.2.abs_diff(119) <= 1,
    "expected red rect max_x ~= 119, got {} (bbox={bbox:?})",
    bbox.2
  );
}

