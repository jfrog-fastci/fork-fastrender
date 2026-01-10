use super::util::create_stacking_context_bounds_renderer;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;

#[test]
fn vertical_align_middle_inline_images_cover_the_top_device_row() {
  // Regression for cdc.gov: a middle-aligned inline image ended up with its top edge just past the
  // device pixel center (y=13.5), causing the first painted row of the downscaled image to be
  // skipped and yielding a visible Chrome diff.
  //
  // The key interaction: `vertical-align: middle` uses half of the parent's x-height. Some fonts
  // (including the Nunito webfont used by the CDC fixture) have an OS/2 sxHeight smaller than the
  // actual 'x' glyph height, so relying solely on sxHeight can place middle-aligned replaced
  // elements slightly too low.
  //
  // The padding-top value is chosen so that when using sxHeight the image begins just below the
  // 13.5 boundary; using the glyph-derived x-height shifts it upward so the top row is painted.

  // Nunito Regular webfont from the CDC offline fixture.
  let nunito = std::fs::read(
    "tests/pages/fixtures/cdc.gov/assets/eecd9875ba1c2555504cf3404f1fd8f1.woff2",
  )
  .expect("read nunito woff2");
  let nunito_b64 = BASE64.encode(&nunito);

  // 64x44 solid blue PNG (matches the CDC header flag image intrinsic size).
  // Downscaling to 16x11 should take the pre-scaled pixmap cache path.
  let data_png = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAEAAAAAsCAYAAADVX77/AAAAcElEQVR4nO3QQREAIAzAsIF/z0NGHjQKej0zu/OxqwO0BugArQE6QGuADtAaoAO0BugArQE6QGuADtAaoAO0BugArQE6QGuADtAaoAO0BugArQE6wHVqAAJthua9AAAAAElFTkSuQmCC";

  let html = format!(
    r#"<!doctype html>
      <style>
        @font-face {{
          font-family: NunitoTest;
          src: url("data:font/woff2;base64,{nunito_b64}") format('woff2');
          font-weight: 400;
          font-style: normal;
        }}
        body {{ margin: 0; background: white; }}
        #line {{
          padding-top: 3.977px;
          font-family: NunitoTest, sans-serif;
          font-size: 18px;
          line-height: 27px;
        }}
        img {{
          width: 16px;
          height: 11px;
          vertical-align: middle;
        }}
      </style>
      <div id="line"><img src="{data_png}"><span>x</span></div>
    "#
  );

  let mut renderer = create_stacking_context_bounds_renderer();
  let pixmap = renderer.render_html(&html, 100, 40).expect("render");

  let pixel = pixmap.pixel(0, 13).expect("pixel");
  assert_ne!(
    (pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()),
    (255, 255, 255, 255),
    "expected the middle-aligned image to paint the top row of device pixels"
  );
}
