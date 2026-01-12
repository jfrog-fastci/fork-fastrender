use crate::debug::runtime::RuntimeToggles;
use crate::{FastRender, RenderOptions};
use resvg::tiny_skia::Pixmap;
use std::collections::HashMap;

fn pixel(pixmap: &Pixmap, x: u32, y: u32) -> [u8; 4] {
  let idx = (y as usize * pixmap.width() as usize + x as usize) * 4;
  let data = pixmap.data();
  [data[idx], data[idx + 1], data[idx + 2], data[idx + 3]]
}

fn render_html_with_svg_document_css_injection_disabled(
  renderer: &mut FastRender,
  html: &str,
  width: u32,
  height: u32,
) -> Pixmap {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_SVG_EMBED_DOCUMENT_CSS".to_string(),
    "0".to_string(),
  )]));
  let options = RenderOptions::new()
    .with_viewport(width, height)
    .with_runtime_toggles(toggles);
  renderer
    .render_html_with_options(html, options)
    .expect("render svg")
}

#[test]
fn inline_svg_use_hrefs_resolve_across_sibling_svg_roots() {
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = FastRender::new().expect("renderer");
      let html = r##"
      <style>body{margin:0;background:white} svg{display:block}</style>
      <svg style="display:none" width="0" height="0">
        <symbol id="icon" viewBox="0 0 20 20">
          <rect width="20" height="20" fill="rgb(255,0,0)"/>
        </symbol>
      </svg>
      <svg width="40" height="20" viewBox="0 0 40 20" xmlns:xlink="http://www.w3.org/1999/xlink">
        <use href="#icon" width="20" height="20"/>
        <use xlink:href="#icon" x="20" width="20" height="20"/>
      </svg>
      "##;

      let pixmap = renderer.render_html(html, 40, 20).expect("render svg");
      assert_eq!(pixel(&pixmap, 10, 10), [255, 0, 0, 255]);
      assert_eq!(
        pixel(&pixmap, 30, 10),
        [255, 0, 0, 255],
        "expected xlink:href variant to resolve across <svg> roots"
      );
    })
    .unwrap()
    .join()
    .unwrap();
}

#[test]
fn inline_svg_url_fragment_paint_server_resolves_across_sibling_svg_roots() {
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = FastRender::new().expect("renderer");
      let html = r##"
      <style>body{margin:0;background:white} svg{display:block}</style>
      <svg style="display:none" width="0" height="0">
        <defs>
          <linearGradient id="grad" x1="0" y1="0" x2="1" y2="0">
            <stop offset="0" stop-color="rgb(255,0,0)"/>
            <stop offset="0.5" stop-color="rgb(255,0,0)"/>
            <stop offset="0.5" stop-color="rgb(0,0,255)"/>
            <stop offset="1" stop-color="rgb(0,0,255)"/>
          </linearGradient>
        </defs>
      </svg>
      <svg width="20" height="10" viewBox="0 0 20 10">
        <style>.shape{fill:url(#grad)}</style>
        <rect class="shape" x="0" y="0" width="20" height="10"/>
      </svg>
      "##;

      let pixmap =
        render_html_with_svg_document_css_injection_disabled(&mut renderer, html, 30, 20);
      assert_eq!(pixel(&pixmap, 5, 5), [255, 0, 0, 255]);
      assert_eq!(pixel(&pixmap, 15, 5), [0, 0, 255, 255]);
    })
    .unwrap()
    .join()
    .unwrap();
}

#[test]
fn inline_svg_use_currentcolor_inherits_from_referencing_svg_across_roots() {
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = FastRender::new().expect("renderer");
      let html = r##"
      <style>body{margin:0;background:white} svg{display:block}</style>
      <svg style="display:none" width="0" height="0">
        <symbol id="icon" viewBox="0 0 20 20">
          <rect width="20" height="20" fill="currentColor"/>
        </symbol>
      </svg>
      <svg width="20" height="20" viewBox="0 0 20 20" style="color: rgb(0,128,0)">
        <use href="#icon" width="20" height="20"/>
      </svg>
      "##;

      let pixmap = renderer.render_html(html, 20, 20).expect("render svg");
      assert_eq!(pixel(&pixmap, 10, 10), [0, 128, 0, 255]);
    })
    .unwrap()
    .join()
    .unwrap();
}

#[test]
fn inline_svg_paint_server_from_document_css_resolves_across_roots() {
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = FastRender::new().expect("renderer");
      let html = r##"
      <style>
        body{margin:0;background:white}
        svg{display:block}
        .shape { fill: url(#grad); }
      </style>
      <svg style="display:none" width="0" height="0">
        <defs>
          <linearGradient id="grad" x1="0" y1="0" x2="1" y2="0">
            <stop offset="0" stop-color="rgb(255,0,0)"/>
            <stop offset="0.5" stop-color="rgb(255,0,0)"/>
            <stop offset="0.5" stop-color="rgb(0,0,255)"/>
            <stop offset="1" stop-color="rgb(0,0,255)"/>
          </linearGradient>
        </defs>
      </svg>
      <svg width="20" height="10" viewBox="0 0 20 10">
        <rect class="shape" x="0" y="0" width="20" height="10"/>
      </svg>
      "##;

      let pixmap =
        render_html_with_svg_document_css_injection_disabled(&mut renderer, html, 30, 20);
      assert_eq!(pixel(&pixmap, 5, 5), [255, 0, 0, 255]);
      assert_eq!(pixel(&pixmap, 15, 5), [0, 0, 255, 255]);
    })
    .unwrap()
    .join()
    .unwrap();
}

#[test]
fn inline_svg_filter_url_from_document_css_resolves_across_roots() {
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = FastRender::new().expect("renderer");
      let html = r##"
      <style>
        body{margin:0;background:white}
        svg{display:block}
        .shape { filter: url(#recolor); }
      </style>
      <svg style="display:none" width="0" height="0">
        <defs>
          <filter id="recolor">
            <feFlood flood-color="rgb(0,255,0)" result="flood"/>
            <feComposite in="flood" in2="SourceAlpha" operator="in"/>
          </filter>
        </defs>
      </svg>
      <svg width="20" height="20" viewBox="0 0 20 20">
        <rect class="shape" x="0" y="0" width="20" height="20" fill="rgb(255,0,0)"/>
      </svg>
      "##;

      let pixmap =
        render_html_with_svg_document_css_injection_disabled(&mut renderer, html, 20, 20);
      assert_eq!(pixel(&pixmap, 10, 10), [0, 255, 0, 255]);
    })
    .unwrap()
    .join()
    .unwrap();
}
