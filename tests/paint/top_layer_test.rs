use fastrender::FastRender;

fn pixel_rgba(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let p = pixmap.pixel(x, y).unwrap();
  (p.red(), p.green(), p.blue(), p.alpha())
}

#[test]
fn modal_dialog_adds_backdrop_and_inert() {
  let mut renderer = FastRender::new().expect("renderer");
  let baseline = r#"
    <style>
      body { margin: 0; }
      button { width: 40px; height: 40px; background: rgb(0, 255, 0); border: none; }
      button:focus { background: rgb(255, 0, 0); }
      dialog { width: 60px; height: 60px; padding: 0; }
    </style>
    <button></button>
  "#;

  let html = r#"
    <style>
      body { margin: 0; }
      button { width: 40px; height: 40px; background: rgb(0, 255, 0); border: none; }
      button:focus { background: rgb(255, 0, 0); }
      dialog { width: 60px; height: 60px; padding: 0; }
    </style>
    <button data-fastr-focus="true"></button>
    <dialog open data-fastr-modal="true"></dialog>
  "#;

  let baseline_pixmap = renderer
    .render_html(baseline, 120, 120)
    .expect("paint baseline");
  let (base_r, base_g, base_b, _) = pixel_rgba(&baseline_pixmap, 5, 5);
  assert!(
    base_g > base_r + 80 && base_g > base_b + 80 && base_g > 80,
    "baseline should be green (r={base_r}, g={base_g}, b={base_b})"
  );

  let pixmap = renderer.render_html(html, 120, 120).expect("paint dialog");
  let (r, g, b, _) = pixel_rgba(&pixmap, 5, 5);

  assert!(
    r < 80 && b < 80,
    "inert background should keep focus state off (r={r}, g={g}, b={b})"
  );
  assert!(
    g + 20 < base_g,
    "UA ::backdrop should dim underlying content (baseline_g={base_g}, r={r}, g={g}, b={b})"
  );
}

#[test]
fn non_modal_dialog_allows_focus() {
  let mut renderer = FastRender::new().expect("renderer");
  let html = r#"
    <style>
      body { margin: 0; }
      button { width: 40px; height: 40px; background: rgb(0, 255, 0); border: none; }
      button:focus { background: rgb(255, 0, 0); }
      dialog { width: 60px; height: 60px; padding: 0; }
    </style>
    <button data-fastr-focus="true"></button>
    <dialog open></dialog>
  "#;

  let pixmap = renderer.render_html(html, 120, 120).expect("paint dialog");
  let (r, g, b, _) = pixel_rgba(&pixmap, 5, 5);

  assert!(
    r > g + 80 && r > b + 80 && r > 80,
    "focus should remain active without modal inertness (r={r}, g={g}, b={b})"
  );
}

#[test]
fn popovers_stack_in_dom_order() {
  let mut renderer = FastRender::new().expect("renderer");
  let html = r#"
    <style>
      body { margin: 0; }
      .base { position: fixed; inset: 0; background: rgb(0, 128, 0); }
      [popover] { width: 60px; height: 60px; top: 10px; left: 10px; }
      #first { background: rgb(0, 0, 255); }
      #second { background: rgb(255, 255, 0); top: 20px; left: 20px; }
    </style>
    <div class="base"></div>
    <div id="first" popover open></div>
    <div id="second" popover open></div>
  "#;

  let pixmap = renderer
    .render_html(html, 120, 120)
    .expect("paint popovers");
  let (r, g, b, _) = pixel_rgba(&pixmap, 30, 30);
  let (sr, sg, sb, _) = pixel_rgba(&pixmap, 75, 75);

  assert!(
    sr > 200 && sg > 200,
    "second popover should paint its own area, got ({sr},{sg},{sb})"
  );

  assert!(
    r > 200 && g > 200 && b < 80,
    "DOM order should stack popovers (later on top), got ({r},{g},{b})"
  );
}

#[test]
fn dialog_backdrop_paints_behind_dialog_box() {
  let mut renderer = FastRender::new().expect("renderer");
  let baseline = r#"
    <style>
      body { margin: 0; }
      .base { position: fixed; inset: 0; background: rgb(0, 255, 0); }
    </style>
    <div class="base"></div>
  "#;
  let html = r#"
    <style>
      body { margin: 0; }
      .base { position: fixed; inset: 0; background: rgb(0, 255, 0); }
      dialog {
        position: fixed;
        top: 0;
        left: 0;
        width: 60px;
        height: 60px;
        margin: 0;
        padding: 0;
        border: none;
        background: rgb(255, 0, 0);
      }
      dialog::backdrop { background: rgba(0, 0, 0, 0.5); }
    </style>
    <div class="base"></div>
    <dialog open data-fastr-modal="true"></dialog>
  "#;

  let baseline_pixmap = renderer
    .render_html(baseline, 120, 120)
    .expect("paint baseline");
  let (base_r, base_g, base_b, _) = pixel_rgba(&baseline_pixmap, 90, 90);
  assert!(
    base_g > base_r + 80 && base_g > base_b + 80 && base_g > 80,
    "baseline should be green (r={base_r}, g={base_g}, b={base_b})"
  );

  let pixmap = renderer.render_html(html, 120, 120).expect("paint dialog");

  // Outside the dialog box, the backdrop should dim the green base.
  let (r, g, b, _) = pixel_rgba(&pixmap, 90, 90);
  assert!(
    g + 20 < base_g,
    "backdrop should dim underlying content (baseline_g={base_g}, r={r}, g={g}, b={b})"
  );

  // Inside the dialog box, the dialog background should not be dimmed by the backdrop.
  let (dr, dg, db, _) = pixel_rgba(&pixmap, 30, 30);
  assert!(
    dr > 200 && dg < 80 && db < 80,
    "dialog box should paint above ::backdrop (r={dr}, g={dg}, b={db})"
  );
}

#[test]
fn dialog_display_contents_paints_above_backdrop() {
  let mut renderer = FastRender::new().expect("renderer");
  let baseline = r#"
    <style>
      body { margin: 0; }
      .base { position: fixed; inset: 0; background: rgb(0, 255, 0); }
    </style>
    <div class="base"></div>
  "#;
  let html = r#"
    <style>
      body { margin: 0; }
      .base { position: fixed; inset: 0; background: rgb(0, 255, 0); }
      dialog { display: contents; }
      .panel {
        position: fixed;
        top: 0;
        left: 0;
        width: 60px;
        height: 60px;
        background: rgb(255, 0, 0);
      }
      dialog::backdrop { background: rgba(0, 0, 0, 0.5); }
    </style>
    <div class="base"></div>
    <dialog open data-fastr-modal="true">
      <div class="panel"></div>
    </dialog>
  "#;

  let baseline_pixmap = renderer
    .render_html(baseline, 120, 120)
    .expect("paint baseline");
  let (base_r, base_g, base_b, _) = pixel_rgba(&baseline_pixmap, 90, 90);
  assert!(
    base_g > base_r + 80 && base_g > base_b + 80 && base_g > 80,
    "baseline should be green (r={base_r}, g={base_g}, b={base_b})"
  );

  let pixmap = renderer.render_html(html, 120, 120).expect("paint dialog");

  // Outside the panel, the backdrop should still dim the base.
  let (r, g, b, _) = pixel_rgba(&pixmap, 90, 90);
  assert!(
    g + 20 < base_g,
    "backdrop should dim underlying content (baseline_g={base_g}, r={r}, g={g}, b={b})"
  );

  // The panel (a promoted child of the display:contents dialog) must paint above the backdrop.
  let (pr, pg, pb, _) = pixel_rgba(&pixmap, 30, 30);
  assert!(
    pr > 200 && pg < 80 && pb < 80,
    "dialog contents should paint above ::backdrop even with display:contents (r={pr}, g={pg}, b={pb})"
  );
}

#[test]
fn dialog_backdrop_respects_author_styles() {
  let mut renderer = FastRender::new().expect("renderer");
  let baseline = r#"
    <style>
      body { margin: 0; }
      .base { position: fixed; inset: 0; background: rgb(0, 255, 0); }
    </style>
    <div class="base"></div>
  "#;
  let html = r#"
    <style>
      body { margin: 0; }
      .base { position: fixed; inset: 0; background: rgb(0, 255, 0); }
      dialog { width: 80px; height: 80px; padding: 0; }
      dialog::backdrop { background: rgba(255, 0, 0, 0.5); }
    </style>
    <div class="base"></div>
    <dialog open data-fastr-modal="true"></dialog>
  "#;

  let baseline_pixmap = renderer
    .render_html(baseline, 200, 200)
    .expect("paint baseline");
  let (base_r, base_g, base_b, _) = pixel_rgba(&baseline_pixmap, 0, 0);
  assert!(
    base_g > base_r + 80 && base_g > base_b + 80 && base_g > 80,
    "baseline should be green (r={base_r}, g={base_g}, b={base_b})"
  );

  let pixmap = renderer.render_html(html, 200, 200).expect("paint dialog");
  let (r, g, b, _) = pixel_rgba(&pixmap, 0, 0);

  let expected_r = (255 + base_r) / 2;
  let expected_g = base_g / 2;
  let expected_b = base_b / 2;

  assert!(
    r.abs_diff(expected_r) <= 20 && g.abs_diff(expected_g) <= 20 && b.abs_diff(expected_b) <= 20,
    "custom ::backdrop should blend with background (expected~{expected_r},{expected_g},{expected_b}; got {r},{g},{b})"
  );
}
