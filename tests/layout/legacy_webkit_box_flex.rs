use fastrender::api::FastRender;

#[test]
fn legacy_webkit_box_centers_child_with_box_pack_and_align() {
  // Regression test for the 2009 WebKit flexbox syntax:
  // `display: -webkit-box` + `-webkit-box-pack` + `-webkit-box-align`.
  //
  // Without legacy property support, `display: -webkit-box` was treated as flow layout and the
  // child would stay in the top-left corner. With the legacy mapping implemented, the child should
  // be centered in both axes.
  let html = r#"
    <style>
      body { margin: 0; background: rgb(255, 255, 255); }
      #container {
        display: -webkit-box;
        -webkit-box-pack: center;
        -webkit-box-align: center;
        width: 100px;
        height: 100px;
        background: rgb(0, 0, 255);
      }
      #child {
        width: 20px;
        height: 20px;
        background: rgb(255, 0, 0);
      }
    </style>
    <div id="container">
      <div id="child"></div>
    </div>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let pixmap = renderer.render_html(html, 100, 100).expect("render");

  let center = pixmap.pixel(50, 50).expect("center pixel");
  assert_eq!(
    (center.red(), center.green(), center.blue(), center.alpha()),
    (255, 0, 0, 255),
    "center pixel should be within the child"
  );

  let corner = pixmap.pixel(5, 5).expect("corner pixel");
  assert_eq!(
    (corner.red(), corner.green(), corner.blue(), corner.alpha()),
    (0, 0, 255, 255),
    "corner pixel should remain background"
  );
}
