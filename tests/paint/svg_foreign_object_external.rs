use base64::prelude::BASE64_STANDARD;
use base64::Engine as _;
use fastrender::api::FastRender;

#[test]
fn external_svg_foreign_object_renders_html() {
  let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="20">
    <foreignObject x="0" y="0" width="20" height="20">
      <div xmlns="http://www.w3.org/1999/xhtml" style="width:20px;height:20px;background:red"></div>
    </foreignObject>
  </svg>"#;
  let data_url = format!("data:image/svg+xml;base64,{}", BASE64_STANDARD.encode(svg));
  let html = format!(
    r#"
      <style>
        body {{ margin: 0; background: rgb(255 255 255); }}
        img {{ display: block; width: 20px; height: 20px; }}
      </style>
      <img src="{data_url}" alt="">
    "#
  );

  let mut renderer = FastRender::new().expect("renderer");
  let pixmap = renderer.render_html(&html, 20, 20).expect("render html");
  let px = pixmap.pixel(10, 10).expect("center pixel");
  assert_eq!((px.red(), px.green(), px.blue(), px.alpha()), (255, 0, 0, 255));
}

