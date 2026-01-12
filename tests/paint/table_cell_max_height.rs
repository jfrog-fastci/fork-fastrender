use fastrender::debug::runtime::RuntimeToggles;
use fastrender::{FastRender, FastRenderConfig};
use std::collections::HashMap;

#[test]
fn table_cell_max_height_does_not_collapse_row_height() {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "display_list".to_string(),
  )]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);

  // Gentoo uses `max-height` on `td` (with `box-sizing: border-box` + padding) as part of a table
  // truncation pattern. Chrome effectively ignores that `max-height` for table row sizing; applying
  // it collapses rows and makes striping/backgrounds wildly divergent.
  let html = "<!doctype html>\
    <style>\
      html,body{margin:0;background:#fff;}\
      table{border-collapse:collapse;width:200px;}\
      td{width:100px;padding:8px;box-sizing:border-box;font:14px/20px sans-serif;}\
      td{max-height:1.2em;overflow:hidden;white-space:nowrap;text-overflow:ellipsis;}\
      .table-striped>tbody>tr:nth-of-type(odd){background:rgb(241,241,241);}\
    </style>\
    <table class=table-striped>\
      <tr><td>A</td><td></td></tr>\
      <tr><td>B</td><td></td></tr>\
    </table>";

  let mut renderer = FastRender::with_config(config).expect("create renderer");
  let pixmap = renderer.render_html(html, 200, 80).expect("render html");

  // If `max-height` is incorrectly applied to the table cell's border box, the first row collapses
  // to ~16.8px tall (8px padding top + ~0.8px content + 8px padding bottom). In that broken case,
  // y=20 is already inside the second (even) row.
  let row1_mid = pixmap.pixel(150, 20).expect("row 1 pixel");
  assert_eq!(
    (
      row1_mid.red(),
      row1_mid.green(),
      row1_mid.blue(),
      row1_mid.alpha()
    ),
    (241, 241, 241, 255),
    "expected pixel to still be inside the first (odd) table row"
  );

  let row2_mid = pixmap.pixel(150, 40).expect("row 2 pixel");
  assert_eq!(
    (
      row2_mid.red(),
      row2_mid.green(),
      row2_mid.blue(),
      row2_mid.alpha()
    ),
    (255, 255, 255, 255),
    "expected pixel to be inside the second (even) table row"
  );
}
