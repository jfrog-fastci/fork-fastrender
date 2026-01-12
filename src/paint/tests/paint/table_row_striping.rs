use crate::debug::runtime::RuntimeToggles;
use crate::{FastRender, FastRenderConfig};
use std::collections::HashMap;

#[test]
fn table_row_striping_applies_via_tbody_and_nth_of_type() {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "display_list".to_string(),
  )]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);

  let html = "<!doctype html>\
    <style>\
      html,body{margin:0;}\
      table{border-collapse:collapse;}\
      td{width:60px;height:20px;padding:0;}\
      .table-striped>tbody>tr:nth-of-type(odd){background:rgb(10,20,30);}\
      .table-striped>tbody>tr:nth-of-type(even){background:rgb(200,210,220);}\
    </style>\
    <table class=table-striped>\
      <tr><td></td></tr>\
      <tr><td></td></tr>\
    </table>";

  let mut renderer = FastRender::with_config(config).expect("create renderer");
  let pixmap = renderer.render_html(html, 64, 64).expect("render html");

  let row1 = pixmap.pixel(5, 10).expect("row 1 pixel");
  assert_eq!(
    (row1.red(), row1.green(), row1.blue(), row1.alpha()),
    (10, 20, 30, 255),
    "expected first (odd) row background color"
  );

  let row2 = pixmap.pixel(5, 30).expect("row 2 pixel");
  assert_eq!(
    (row2.red(), row2.green(), row2.blue(), row2.alpha()),
    (200, 210, 220, 255),
    "expected second (even) row background color"
  );
}
