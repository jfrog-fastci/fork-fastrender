use crate::debug::runtime::RuntimeToggles;
use crate::{FastRender, FastRenderConfig};
use std::collections::HashMap;

#[test]
fn display_list_form_control_background_color_initial_is_transparent() {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "display_list".to_string(),
  )]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);

  // UA styles give text inputs a white background. Authors often use `background-color: initial`
  // (e.g. on MDN sidebar filter inputs) to reset it back to the property's initial value:
  // transparent.
  let html = "<!doctype html>
    <style>
      html, body { margin: 0; background: rgb(0, 128, 0); }
      mdn-sidebar-filter { display: block; }
    </style>
    <mdn-sidebar-filter>
      <template shadowrootmode=\"open\">
        <style>
          .input {
            display: block;
            width: 40px;
            height: 40px;
            border: none;
            padding: 0;
            background-color: initial;
          }
        </style>
        <input class=\"input\">
      </template>
    </mdn-sidebar-filter>";

  let mut renderer = FastRender::with_config(config).expect("create renderer");
  let pixmap = renderer
    .render_html(html, 60, 60)
    .expect("render form control");

  // Sample a pixel well inside the input box. If `background-color: initial` is respected, the
  // pixel should match the parent's green background (transparent input). If it is ignored, the
  // UA white background will be visible.
  let px = pixmap.pixel(20, 20).expect("pixel inside input");
  assert_eq!(px.red(), 0);
  assert_eq!(px.green(), 128);
  assert_eq!(px.blue(), 0);
  assert_eq!(px.alpha(), 255);
}
