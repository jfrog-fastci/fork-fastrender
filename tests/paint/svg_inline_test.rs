use fastrender::css::parser::extract_css;
use fastrender::debug::runtime::RuntimeToggles;
use fastrender::dom;
use fastrender::style::cascade::apply_styles_with_media;
use fastrender::style::media::MediaContext;
use fastrender::tree::box_generation::generate_box_tree;
use fastrender::tree::box_tree::{BoxNode, BoxType, ReplacedType, SvgContent};
use fastrender::{DiagnosticsLevel, FastRender, FastRenderConfig, RenderOptions};
use resvg::tiny_skia::Pixmap;
use std::collections::HashMap;

pub(super) fn pixel(pixmap: &Pixmap, x: u32, y: u32) -> [u8; 4] {
  let idx = (y as usize * pixmap.width() as usize + x as usize) * 4;
  let data = pixmap.data();
  [data[idx], data[idx + 1], data[idx + 2], data[idx + 3]]
}

pub(super) fn serialized_inline_svg(html: &str, width: f32, height: f32) -> Option<SvgContent> {
  let dom = dom::parse_html(html).ok()?;
  let stylesheet = extract_css(&dom).ok()?;
  let media = MediaContext::screen(width, height);
  let styled = apply_styles_with_media(&dom, &stylesheet, &media);
  let box_tree = generate_box_tree(&styled).ok()?;

  fn find_svg(node: &BoxNode) -> Option<SvgContent> {
    if let BoxType::Replaced(repl) = &node.box_type {
      if let ReplacedType::Svg { content } = &repl.replaced_type {
        return Some(content.clone());
      }
    }
    for child in node.children.iter() {
      if let Some(content) = find_svg(child) {
        return Some(content);
      }
    }
    None
  }

  find_svg(&box_tree.root)
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
fn inline_svg_applies_document_css_and_current_color() {
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = FastRender::new().expect("renderer");
      let html = r#"
      <style>
        body { margin: 0; background: white; }
        svg { color: rgb(0, 128, 0); display: block; }
        svg .shape { fill: currentColor; }
      </style>
      <svg width="20" height="20" viewBox="0 0 20 20">
        <rect class="shape" width="20" height="20"></rect>
        <text class="shape" x="2" y="14">Hi</text>
      </svg>
      "#;

      let pixmap = renderer.render_html(html, 30, 30).expect("render svg");
      assert_eq!(pixel(&pixmap, 10, 10), [0, 128, 0, 255]);
    })
    .unwrap()
    .join()
    .unwrap();
}

#[test]
fn inline_svg_respects_display_none_when_document_css_injection_disabled_display_list_backend() {
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = FastRender::new().expect("renderer");
      let html = r#"
      <style>
        body { margin: 0; background: white; }
        svg { display: block; }
        rect { display: none; }
      </style>
      <svg width="20" height="20" viewBox="0 0 20 20">
        <rect width="20" height="20" fill="rgb(255, 0, 0)" />
      </svg>
      "#;

      let pixmap = render_html_with_svg_document_css_injection_disabled(&mut renderer, html, 30, 30);
      assert_eq!(pixel(&pixmap, 10, 10), [255, 255, 255, 255]);
    })
    .unwrap()
    .join()
    .unwrap();
}

#[test]
fn inline_svg_respects_visibility_hidden_when_document_css_injection_disabled_display_list_backend() {
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = FastRender::new().expect("renderer");
      let html = r#"
      <style>
        body { margin: 0; background: white; }
        svg { display: block; }
        rect { visibility: hidden; }
      </style>
      <svg width="20" height="20" viewBox="0 0 20 20">
        <rect width="20" height="20" fill="rgb(255, 0, 0)" />
      </svg>
      "#;

      let pixmap = render_html_with_svg_document_css_injection_disabled(&mut renderer, html, 30, 30);
      assert_eq!(pixel(&pixmap, 10, 10), [255, 255, 255, 255]);
    })
    .unwrap()
    .join()
    .unwrap();
}

#[test]
fn inline_svg_respects_opacity_when_document_css_injection_disabled_display_list_backend() {
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = FastRender::new().expect("renderer");
      let html = r#"
      <style>
        body { margin: 0; background: white; }
        svg { display: block; }
        rect { opacity: 0; }
      </style>
      <svg width="20" height="20" viewBox="0 0 20 20">
        <rect width="20" height="20" fill="rgb(255, 0, 0)" />
      </svg>
      "#;

      let pixmap = render_html_with_svg_document_css_injection_disabled(&mut renderer, html, 30, 30);
      assert_eq!(pixel(&pixmap, 10, 10), [255, 255, 255, 255]);
    })
    .unwrap()
    .join()
    .unwrap();
}

#[test]
fn inline_svg_serializes_css_transform_for_child_elements_when_document_css_injection_disabled() {
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = FastRender::new().expect("renderer");
      let html = r#"
      <style>
        body { margin: 0; background: white; }
        svg { display: block; }
        rect { transform: translate(10px, 0px); }
      </style>
      <svg width="20" height="20" viewBox="0 0 20 20">
        <rect width="10" height="10" fill="rgb(255, 0, 0)" />
      </svg>
      "#;

      let pixmap = render_html_with_svg_document_css_injection_disabled(&mut renderer, html, 30, 30);
      assert_eq!(
        pixel(&pixmap, 5, 5),
        [255, 255, 255, 255],
        "rect should be translated via serialized transform attribute"
      );
      assert_eq!(
        pixel(&pixmap, 15, 5),
        [255, 0, 0, 255],
        "translated rect should render at the expected offset"
      );
    })
    .unwrap()
    .join()
    .unwrap();
}

#[test]
fn inline_svg_root_opacity_is_not_double_applied() {
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = FastRender::new().expect("renderer");
      let html = r#"
      <style>body{margin:0;background:white} svg{display:block;opacity:0.5}</style>
      <svg width="10" height="10" viewBox="0 0 10 10">
        <rect width="10" height="10" fill="rgb(255,0,0)"/>
      </svg>
      "#;

      let pixmap = renderer.render_html(html, 20, 20).expect("render svg");
      let sample = pixel(&pixmap, 5, 5);
      assert_eq!(sample[0], 255, "expected red channel to stay max, got {sample:?}");
      assert_eq!(sample[3], 255, "expected opaque output pixel, got {sample:?}");
      assert!(
        (120..=140).contains(&sample[1]) && (120..=140).contains(&sample[2]),
        "expected opacity 0.5 to blend once (g/b ~128), got {sample:?}"
      );
    })
    .unwrap()
    .join()
    .unwrap();
}

#[test]
fn inline_svg_root_fill_defaults_unstyled_shapes_to_current_color() {
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = FastRender::new().expect("renderer");
      let html = r#"
      <style>body{margin:0;background:white} svg{display:block;color:rgb(0,128,0)}</style>
      <svg width="20" height="20" viewBox="0 0 20 20">
        <rect width="20" height="20"></rect>
      </svg>
      "#;

      let pixmap = renderer.render_html(html, 30, 30).expect("render svg");
      assert_eq!(pixel(&pixmap, 10, 10), [0, 128, 0, 255]);
    })
    .unwrap()
    .join()
    .unwrap();
}

#[test]
fn inline_svg_root_fill_presentation_attribute_is_not_overridden() {
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = FastRender::new().expect("renderer");
      let html = r#"
      <style>body{margin:0;background:white} svg{display:block}</style>
      <svg width="20" height="20" fill="rgb(255,0,0)" viewBox="0 0 20 20">
        <rect width="20" height="20"></rect>
      </svg>
      "#;

      let pixmap = renderer.render_html(html, 30, 30).expect("render svg");
      assert_eq!(pixel(&pixmap, 10, 10), [255, 0, 0, 255]);
    })
    .unwrap()
    .join()
    .unwrap();
}

#[test]
fn inline_svg_root_fill_style_attribute_is_not_overridden() {
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = FastRender::new().expect("renderer");
      let html = r#"
      <style>body{margin:0;background:white} svg{display:block}</style>
      <svg width="20" height="20" viewBox="0 0 20 20" style="fill: url(#grad)">
        <defs>
          <linearGradient id="grad" x1="0" x2="1" y1="0" y2="0">
            <stop offset="0%" stop-color="red" />
            <stop offset="100%" stop-color="blue" />
          </linearGradient>
        </defs>
        <rect width="100%" height="100%"></rect>
      </svg>
      "#;

      let pixmap = renderer.render_html(html, 30, 30).expect("render svg");
      let left = pixel(&pixmap, 5, 10);
      let right = pixel(&pixmap, 15, 10);
      assert_ne!(
        left,
        [0, 0, 0, 255],
        "left pixel should render gradient fill, got {left:?}"
      );
      assert_ne!(
        right,
        [0, 0, 0, 255],
        "right pixel should render gradient fill, got {right:?}"
      );
      assert_eq!(left[3], 255, "left pixel should be opaque, got {left:?}");
      assert_eq!(right[3], 255, "right pixel should be opaque, got {right:?}");
      assert!(
        left[0] > right[0],
        "expected more red on the left side of the gradient, got left={left:?} right={right:?}"
      );
      assert!(
        right[2] > left[2],
        "expected more blue on the right side of the gradient, got left={left:?} right={right:?}"
      );
    })
    .unwrap()
    .join()
    .unwrap();
}

#[test]
fn inline_svg_does_not_record_spurious_fetch_errors() {
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = FastRender::new().expect("renderer");
      let html = r#"
      <style>
        body { margin: 0; background: white; }
        svg { display: block; }
      </style>
      <svg width="10" height="10" viewBox="0 0 10 10">
        <rect width="10" height="10" fill="red"></rect>
      </svg>
      "#;

      let options = RenderOptions::default()
        .with_viewport(20, 20)
        .with_diagnostics_level(DiagnosticsLevel::Basic);
      let result = renderer
        .render_html_with_diagnostics(html, options)
        .expect("render svg");

      assert!(
        result.diagnostics.fetch_errors.is_empty(),
        "inline SVG should not be fetched as a URL: {:?}",
        result.diagnostics.fetch_errors
      );
    })
    .unwrap()
    .join()
    .unwrap();
}

#[test]
fn inline_svg_renders_gradients_with_clip_and_mask() {
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = FastRender::new().expect("renderer");
      let html = r#"
      <style>
        body { margin: 0; background: white; }
        svg { display: block; }
      </style>
      <svg width="20" height="20" viewBox="0 0 20 20" style="display: block">
        <defs>
          <linearGradient id="grad" x1="0" x2="1" y1="0" y2="0">
            <stop offset="0%" stop-color="red" />
            <stop offset="100%" stop-color="blue" />
          </linearGradient>
          <mask id="fade">
            <rect width="20" height="20" fill="white" />
            <rect width="10" height="20" fill="black" />
          </mask>
        </defs>
        <rect width="20" height="20" fill="url(#grad)" mask="url(#fade)" />
        <rect width="4" height="4" fill="black" transform="translate(12 2)" />
      </svg>
      "#;

      let serialized = serialized_inline_svg(html, 30.0, 30.0).expect("serialize svg");
      assert!(serialized.svg.contains("mask=\"url(#fade)\""));

      let cache = fastrender::image_loader::ImageCache::new();
      let svg_image = cache
        .render_svg(&serialized.svg)
        .expect("render serialized svg");
      let svg_rgba = svg_image.image.to_rgba8();
      let left_alpha = svg_rgba.get_pixel(8, 10)[3];
      let right_alpha = svg_rgba.get_pixel(14, 10)[3];
      assert!(
        left_alpha < right_alpha,
        "mask should reduce alpha in standalone rendering"
      );

      let pixmap = renderer.render_html(html, 30, 30).expect("render svg");
      assert_eq!(
        pixel(&pixmap, 13, 3),
        [0, 0, 0, 255],
        "transforms should offset shapes"
      );
      let masked = pixel(&pixmap, 8, 10);
      let visible = pixel(&pixmap, 14, 10);
      assert_eq!(
        masked,
        [255, 255, 255, 255],
        "masked region should show the page background"
      );
      assert_ne!(
        visible,
        [255, 255, 255, 255],
        "unmasked region should render content"
      );
      assert!(
        visible[2] > visible[0],
        "gradient should shift toward blue on the right"
      );
    })
    .unwrap()
    .join()
    .unwrap();
}

#[test]
fn inline_svg_renders_foreign_object_html() {
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = FastRender::new().expect("renderer");
      let html = r#"
      <style>
        body { margin: 0; background: white; }
        svg { display: block; }
      </style>
      <svg width="16" height="12" viewBox="0 0 16 12">
        <foreignObject x="0" y="0" width="10" height="12">
          <div xmlns="http://www.w3.org/1999/xhtml" style="width:10px;height:12px;background: rgb(0, 0, 255);"></div>
        </foreignObject>
      </svg>
      "#;

      let pixmap = renderer.render_html(html, 20, 20).expect("render svg");
      assert_eq!(pixel(&pixmap, 5, 6), [0, 0, 255, 255], "foreignObject content should paint");
      // Area outside the foreignObject should stay white.
      assert_eq!(pixel(&pixmap, 14, 6), [255, 255, 255, 255]);
    })
    .unwrap()
    .join()
    .unwrap();
}

#[test]
fn foreign_object_rasterization_accounts_for_svg_view_box_scale_display_list_backend() {
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = FastRender::new().expect("renderer");
      let html = r#"
      <style>body{margin:0;background:white} svg{display:block}</style>
      <svg width="160" height="160" viewBox="0 0 16 16">
        <foreignObject x="0" y="0" width="16" height="16">
          <div xmlns="http://www.w3.org/1999/xhtml" style="width:16px;height:16px;background:white;position:relative;">
            <div style="position:absolute;left:8px;top:0;width:1px;height:16px;background:black;"></div>
          </div>
        </foreignObject>
      </svg>
      "#;

      let toggles = RuntimeToggles::from_map(HashMap::from([(
        "FASTR_PAINT_BACKEND".to_string(),
        "display_list".to_string(),
      )]));
      let options = RenderOptions::new()
        .with_viewport(200, 200)
        .with_runtime_toggles(toggles);
      let pixmap = renderer
        .render_html_with_options(html, options)
        .expect("render svg");

      let left = pixel(&pixmap, 79, 80);
      let start = pixel(&pixmap, 80, 80);
      let end = pixel(&pixmap, 89, 80);
      let right = pixel(&pixmap, 90, 80);
      assert!(
        left[0] > 240 && left[1] > 240 && left[2] > 240,
        "expected crisp white pixel just outside the line, got {left:?}"
      );
      assert!(
        start[0] < 20 && start[1] < 20 && start[2] < 20,
        "expected dark pixel inside the line, got {start:?}"
      );
      assert!(
        end[0] < 20 && end[1] < 20 && end[2] < 20,
        "expected dark pixel inside the line, got {end:?}"
      );
      assert!(
        right[0] > 240 && right[1] > 240 && right[2] > 240,
        "expected crisp white pixel just outside the line, got {right:?}"
      );
    })
    .unwrap()
    .join()
    .unwrap();
}

#[test]
fn foreign_object_rasterization_accounts_for_svg_view_box_scale_legacy_backend() {
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = FastRender::new().expect("renderer");
      let html = r#"
      <style>body{margin:0;background:white} svg{display:block}</style>
      <svg width="160" height="160" viewBox="0 0 16 16">
        <foreignObject x="0" y="0" width="16" height="16">
          <div xmlns="http://www.w3.org/1999/xhtml" style="width:16px;height:16px;background:white;position:relative;">
            <div style="position:absolute;left:8px;top:0;width:1px;height:16px;background:black;"></div>
          </div>
        </foreignObject>
      </svg>
      "#;

      let toggles = RuntimeToggles::from_map(HashMap::from([(
        "FASTR_PAINT_BACKEND".to_string(),
        "legacy".to_string(),
      )]));
      let options = RenderOptions::new()
        .with_viewport(200, 200)
        .with_runtime_toggles(toggles);
      let pixmap = renderer
        .render_html_with_options(html, options)
        .expect("render svg");

      let left = pixel(&pixmap, 79, 80);
      let start = pixel(&pixmap, 80, 80);
      let end = pixel(&pixmap, 89, 80);
      let right = pixel(&pixmap, 90, 80);
      assert!(
        left[0] > 240 && left[1] > 240 && left[2] > 240,
        "expected crisp white pixel just outside the line, got {left:?}"
      );
      assert!(
        start[0] < 20 && start[1] < 20 && start[2] < 20,
        "expected dark pixel inside the line, got {start:?}"
      );
      assert!(
        end[0] < 20 && end[1] < 20 && end[2] < 20,
        "expected dark pixel inside the line, got {end:?}"
      );
      assert!(
        right[0] > 240 && right[1] > 240 && right[2] > 240,
        "expected crisp white pixel just outside the line, got {right:?}"
      );
    })
    .unwrap()
    .join()
    .unwrap();
}

#[test]
fn foreign_object_rasterization_accounts_for_nested_svg_view_box_scale() {
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = FastRender::new().expect("renderer");
      let html = r#"
      <style>body{margin:0;background:white} svg{display:block}</style>
      <svg width="160" height="160" viewBox="0 0 160 160">
        <svg x="0" y="0" width="160" height="160" viewBox="0 0 16 16">
          <foreignObject x="0" y="0" width="16" height="16">
            <div xmlns="http://www.w3.org/1999/xhtml" style="width:16px;height:16px;background:white;position:relative;">
              <div style="position:absolute;left:8px;top:0;width:1px;height:16px;background:black;"></div>
            </div>
          </foreignObject>
        </svg>
      </svg>
      "#;

      for backend in ["display_list", "legacy"] {
        let toggles = RuntimeToggles::from_map(HashMap::from([(
          "FASTR_PAINT_BACKEND".to_string(),
          backend.to_string(),
        )]));
        let options = RenderOptions::new()
          .with_viewport(400, 200)
          .with_runtime_toggles(toggles);
        let pixmap = renderer
          .render_html_with_options(html, options)
          .expect("render svg");

        let left = pixel(&pixmap, 79, 80);
        let start = pixel(&pixmap, 80, 80);
        let end = pixel(&pixmap, 89, 80);
        let right = pixel(&pixmap, 90, 80);
        assert!(
          left[0] > 240 && left[1] > 240 && left[2] > 240,
          "({backend}) expected crisp white pixel just outside the line, got {left:?}"
        );
        assert!(
          start[0] < 20 && start[1] < 20 && start[2] < 20,
          "({backend}) expected dark pixel inside the line, got {start:?}"
        );
        assert!(
          end[0] < 20 && end[1] < 20 && end[2] < 20,
          "({backend}) expected dark pixel inside the line, got {end:?}"
        );
        assert!(
          right[0] > 240 && right[1] > 240 && right[2] > 240,
          "({backend}) expected crisp white pixel just outside the line, got {right:?}"
        );
      }
    })
    .unwrap()
    .join()
    .unwrap();
}

#[test]
fn foreign_object_rasterization_accounts_for_svg_transform_scale_display_list_backend() {
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = FastRender::new().expect("renderer");
      let html = r#"
      <style>body{margin:0;background:white} svg{display:block}</style>
      <svg width="320" height="160" viewBox="0 0 320 160">
        <g transform="scale(10)">
          <foreignObject x="0" y="0" width="16" height="16">
            <div xmlns="http://www.w3.org/1999/xhtml" style="width:16px;height:16px;background:white;position:relative;">
              <div style="position:absolute;left:8px;top:0;width:1px;height:16px;background:black;"></div>
            </div>
          </foreignObject>
        </g>
        <foreignObject x="0" y="0" width="16" height="16" transform="translate(160 0) scale(10)">
          <div xmlns="http://www.w3.org/1999/xhtml" style="width:16px;height:16px;background:white;position:relative;">
            <div style="position:absolute;left:8px;top:0;width:1px;height:16px;background:black;"></div>
          </div>
        </foreignObject>
      </svg>
      "#;

      let toggles = RuntimeToggles::from_map(HashMap::from([(
        "FASTR_PAINT_BACKEND".to_string(),
        "display_list".to_string(),
      )]));
      let options = RenderOptions::new()
        .with_viewport(400, 200)
        .with_runtime_toggles(toggles);
      let pixmap = renderer
        .render_html_with_options(html, options)
        .expect("render svg");

      for (name, x0) in [("ancestor", 80u32), ("self", 240u32)] {
        let left = pixel(&pixmap, x0 - 1, 80);
        let start = pixel(&pixmap, x0, 80);
        let end = pixel(&pixmap, x0 + 9, 80);
        let right = pixel(&pixmap, x0 + 10, 80);
        assert!(
          left[0] > 240 && left[1] > 240 && left[2] > 240,
          "({name}) expected crisp white pixel just outside the line, got {left:?}"
        );
        assert!(
          start[0] < 20 && start[1] < 20 && start[2] < 20,
          "({name}) expected dark pixel inside the line, got {start:?}"
        );
        assert!(
          end[0] < 20 && end[1] < 20 && end[2] < 20,
          "({name}) expected dark pixel inside the line, got {end:?}"
        );
        assert!(
          right[0] > 240 && right[1] > 240 && right[2] > 240,
          "({name}) expected crisp white pixel just outside the line, got {right:?}"
        );
      }
    })
    .unwrap()
    .join()
    .unwrap();
}

#[test]
fn foreign_object_rasterization_accounts_for_svg_transform_scale_legacy_backend() {
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = FastRender::new().expect("renderer");
      let html = r#"
      <style>body{margin:0;background:white} svg{display:block}</style>
      <svg width="320" height="160" viewBox="0 0 320 160">
        <g transform="scale(10)">
          <foreignObject x="0" y="0" width="16" height="16">
            <div xmlns="http://www.w3.org/1999/xhtml" style="width:16px;height:16px;background:white;position:relative;">
              <div style="position:absolute;left:8px;top:0;width:1px;height:16px;background:black;"></div>
            </div>
          </foreignObject>
        </g>
        <foreignObject x="0" y="0" width="16" height="16" transform="translate(160 0) scale(10)">
          <div xmlns="http://www.w3.org/1999/xhtml" style="width:16px;height:16px;background:white;position:relative;">
            <div style="position:absolute;left:8px;top:0;width:1px;height:16px;background:black;"></div>
          </div>
        </foreignObject>
      </svg>
      "#;

      let toggles = RuntimeToggles::from_map(HashMap::from([(
        "FASTR_PAINT_BACKEND".to_string(),
        "legacy".to_string(),
      )]));
      let options = RenderOptions::new()
        .with_viewport(400, 200)
        .with_runtime_toggles(toggles);
      let pixmap = renderer
        .render_html_with_options(html, options)
        .expect("render svg");

      for (name, x0) in [("ancestor", 80u32), ("self", 240u32)] {
        let left = pixel(&pixmap, x0 - 1, 80);
        let start = pixel(&pixmap, x0, 80);
        let end = pixel(&pixmap, x0 + 9, 80);
        let right = pixel(&pixmap, x0 + 10, 80);
        assert!(
          left[0] > 240 && left[1] > 240 && left[2] > 240,
          "({name}) expected crisp white pixel just outside the line, got {left:?}"
        );
        assert!(
          start[0] < 20 && start[1] < 20 && start[2] < 20,
          "({name}) expected dark pixel inside the line, got {start:?}"
        );
        assert!(
          end[0] < 20 && end[1] < 20 && end[2] < 20,
          "({name}) expected dark pixel inside the line, got {end:?}"
        );
        assert!(
          right[0] > 240 && right[1] > 240 && right[2] > 240,
          "({name}) expected crisp white pixel just outside the line, got {right:?}"
        );
      }
    })
    .unwrap()
    .join()
    .unwrap();
}

#[test]
fn inline_svg_applies_foreign_object_opacity_presentation_attribute() {
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = FastRender::new().expect("renderer");
      let html = r#"
      <style>
        body { margin: 0; background: white; }
        svg { display: block; }
      </style>
      <svg width="16" height="12" viewBox="0 0 16 12">
        <foreignObject x="0" y="0" width="10" height="12" opacity="0.5">
          <div xmlns="http://www.w3.org/1999/xhtml" style="width:10px;height:12px;background: rgb(255, 0, 0);"></div>
        </foreignObject>
      </svg>
      "#;

      let pixmap = renderer.render_html(html, 20, 20).expect("render svg");
      let sample = pixel(&pixmap, 5, 6);
      assert_ne!(
        sample,
        [255, 0, 0, 255],
        "opacity should blend foreignObject content with the page background"
      );
      assert_ne!(sample, [255, 255, 255, 255], "foreignObject content should remain visible");
      assert!(
        sample[0] > 200 && sample[1] > 80 && sample[2] > 80,
        "expected blended pink-ish pixel, got {sample:?}"
      );
    })
    .unwrap()
    .join()
    .unwrap();
}

#[test]
fn foreign_object_unpremultiplies_alpha_when_encoded_into_svg_image() {
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = FastRender::new().expect("renderer");
      let html = r#"
      <style>body{margin:0;background:white} svg{display:block}</style>
      <svg width="16" height="16" viewBox="0 0 16 16">
        <foreignObject x="0" y="0" width="16" height="16">
          <div xmlns="http://www.w3.org/1999/xhtml" style="width:16px;height:16px;background: rgba(255, 0, 0, 0.5);"></div>
        </foreignObject>
      </svg>
      "#;

      let pixmap = renderer.render_html(html, 20, 20).expect("render svg");
      let sample = pixel(&pixmap, 8, 8);
      assert_eq!(sample[3], 255, "page background is opaque, got {sample:?}");
      assert!(
        sample[0] >= 240,
        "semi-transparent red should preserve full red channel after compositing, got {sample:?}"
      );
      assert!(
        sample[1] >= 110 && sample[1] <= 150,
        "expected blended green channel around 50% over white, got {sample:?}"
      );
      assert!(
        (sample[1] as i16 - sample[2] as i16).abs() <= 3,
        "expected green/blue channels to match, got {sample:?}"
      );
    })
    .unwrap()
    .join()
    .unwrap();
}

#[test]
fn foreign_object_background_alpha_is_applied_once() {
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = FastRender::new().expect("renderer");
      let html = r#"
      <style>body{margin:0;background:white} svg{display:block}</style>
      <svg width="16" height="16" viewBox="0 0 16 16">
        <foreignObject x="0" y="0" width="16" height="16" style="background: rgba(255, 0, 0, 0.5);">
          <div xmlns="http://www.w3.org/1999/xhtml" style="width:16px;height:16px;"></div>
        </foreignObject>
      </svg>
      "#;

      let pixmap = renderer.render_html(html, 20, 20).expect("render svg");
      let sample = pixel(&pixmap, 8, 8);
      assert_eq!(sample[3], 255, "page background is opaque, got {sample:?}");
      assert!(
        sample[0] >= 240,
        "semi-transparent red background should preserve full red channel after compositing, got {sample:?}"
      );
      assert!(
        sample[1] >= 110 && sample[1] <= 150,
        "expected blended green channel around 50% over white, got {sample:?}"
      );
      assert!(
        (sample[1] as i16 - sample[2] as i16).abs() <= 3,
        "expected green/blue channels to match, got {sample:?}"
      );
    })
    .unwrap()
    .join()
    .unwrap();
}

#[test]
fn foreign_object_background_from_svg_style_attribute_is_captured() {
  let html = r#"
  <svg width="16" height="16" viewBox="0 0 16 16">
    <foreignObject x="0" y="0" width="16" height="16" style="background: rgba(255, 0, 0, 0.5);">
      <div xmlns="http://www.w3.org/1999/xhtml" style="width:16px;height:16px;"></div>
    </foreignObject>
  </svg>
  "#;

  let serialized = serialized_inline_svg(html, 20.0, 20.0).expect("serialize svg");
  assert_eq!(
    serialized.foreign_objects.len(),
    1,
    "expected one foreignObject to be captured"
  );
  let bg = serialized.foreign_objects[0]
    .background
    .expect("foreignObject background should be captured");
  assert_eq!(bg.r, 255);
  assert_eq!(bg.g, 0);
  assert_eq!(bg.b, 0);
  assert!(
    (bg.a - 0.5).abs() < 0.01,
    "expected alpha ~0.5, got {:?}",
    bg
  );
}

#[test]
fn foreign_object_renders_nested_html_children() {
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = FastRender::new().expect("renderer");
      let html = r#"
      <style>body{margin:0;background:white} svg{display:block}</style>
      <svg width="16" height="12" viewBox="0 0 16 12">
        <foreignObject x="0" y="0" width="10" height="12">
          <div xmlns="http://www.w3.org/1999/xhtml" style="width:10px;height:12px;background: rgb(0, 0, 255);">
            <div style="width:10px;height:12px;background: rgb(255, 0, 0);"></div>
          </div>
        </foreignObject>
      </svg>
      "#;

      // Explicitly exercise the display-list paint backend (the default) to ensure nested
      // foreignObject HTML is rendered rather than falling back to placeholder SVG output.
      let toggles = fastrender::debug::runtime::RuntimeToggles::from_map(
        std::collections::HashMap::from([(
          "FASTR_PAINT_BACKEND".to_string(),
          "display_list".to_string(),
        )]),
      );
      let options = RenderOptions::new()
        .with_viewport(20, 20)
        .with_runtime_toggles(toggles);

      let pixmap = renderer
        .render_html_with_options(html, options)
        .expect("render svg");
      assert_eq!(
        pixel(&pixmap, 5, 6),
        [255, 0, 0, 255],
        "nested foreignObject content should paint"
      );
    })
    .unwrap()
    .join()
    .unwrap();
}

#[test]
fn foreign_object_html_percent_sizing_fills_viewport() {
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = FastRender::new().expect("renderer");
      let html = r#"
      <style>body{margin:0;background:white} svg{display:block}</style>
      <svg width="16" height="12" viewBox="0 0 16 12">
        <foreignObject x="0" y="0" width="10" height="12">
          <div xmlns="http://www.w3.org/1999/xhtml" style="width:100%;height:100%;background: rgb(0, 0, 255);"></div>
        </foreignObject>
      </svg>
      "#;

      let pixmap = renderer.render_html(html, 20, 20).expect("render svg");
      assert_eq!(pixel(&pixmap, 5, 6), [0, 0, 255, 255]);
    })
    .unwrap()
    .join()
    .unwrap();
}

#[test]
fn inline_svg_renders_use_xlink_href_without_explicit_xmlns_xlink() {
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = FastRender::new().expect("renderer");
      let html = r##"
      <style>body{margin:0;background:white} svg{display:block}</style>
      <svg width="20" height="20" viewBox="0 0 20 20">
        <defs>
          <rect id="shape" width="20" height="20" fill="rgb(0, 128, 0)" />
        </defs>
        <use xlink:href="#shape"></use>
      </svg>
      "##;

      let pixmap = renderer.render_html(html, 30, 30).expect("render svg");
      assert_eq!(pixel(&pixmap, 10, 10), [0, 128, 0, 255]);
    })
    .unwrap()
    .join()
    .unwrap();
}

#[test]
fn display_list_backend_renders_foreign_object_html_not_placeholder() {
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let toggles = RuntimeToggles::from_map(HashMap::from([(
        "FASTR_PAINT_BACKEND".to_string(),
        "display_list".to_string(),
      )]));
      let config = FastRenderConfig::new().with_runtime_toggles(toggles);

      let mut renderer = FastRender::with_config(config).expect("renderer");
      let html = r#"
      <style>body{margin:0;background:white} svg{display:block}</style>
      <svg width="16" height="16" viewBox="0 0 16 16">
        <foreignObject x="0" y="0" width="16" height="16">
          <div xmlns="http://www.w3.org/1999/xhtml" style="width:16px;height:16px;border:4px solid rgb(255,0,0);"></div>
        </foreignObject>
      </svg>
      "#;

      let pixmap = renderer.render_html(html, 20, 20).expect("render svg");
      assert_eq!(
        pixel(&pixmap, 1, 1),
        [255, 0, 0, 255],
        "foreignObject border should paint red (sample={:?})",
        pixel(&pixmap, 0, 0)
      );
    })
    .unwrap()
    .join()
    .unwrap();
}

#[test]
fn foreign_object_display_none_prevents_replacement_rendering() {
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = FastRender::new().expect("renderer");
      let html = r#"
      <style>
        body{margin:0;background:white}
        svg{display:block}
        foreignObject{display:none}
      </style>
      <svg width="16" height="12" viewBox="0 0 16 12">
        <foreignObject x="0" y="0" width="10" height="12">
          <div xmlns="http://www.w3.org/1999/xhtml" style="width:10px;height:12px;background: rgb(0,0,255);"></div>
        </foreignObject>
      </svg>
      "#;

      let pixmap = renderer.render_html(html, 20, 20).expect("render svg");
      assert_eq!(
        pixel(&pixmap, 5, 6),
        [255, 255, 255, 255],
        "display:none foreignObject should not paint"
      );
    })
    .unwrap()
    .join()
    .unwrap();
}

#[test]
fn foreign_object_without_dimensions_uses_placeholder_comment() {
  let html = r#"
  <svg width="16" height="12" viewBox="0 0 16 12">
    <foreignObject>
      <div xmlns="http://www.w3.org/1999/xhtml" style="width:10px;height:12px;background: rgb(255, 0, 0);"></div>
    </foreignObject>
  </svg>
  "#;

  let serialized = serialized_inline_svg(html, 20.0, 20.0).expect("serialize svg");
  assert!(
    serialized
      .svg
      .contains("FASTRENDER_FOREIGN_OBJECT_UNRESOLVED"),
    "missing dimensions should keep placeholder path"
  );
}

#[test]
fn foreign_object_with_dimensions_emits_marker() {
  let html = r#"
  <svg width="16" height="12" viewBox="0 0 16 12">
    <foreignObject x="0" y="0" width="10" height="12">
      <div xmlns="http://www.w3.org/1999/xhtml" style="width:10px;height:12px;background: rgb(0, 255, 0);"></div>
    </foreignObject>
  </svg>
  "#;

  let serialized = serialized_inline_svg(html, 20.0, 20.0).expect("serialize svg");
  assert!(
    serialized.svg.contains("FASTRENDER_FOREIGN_OBJECT_0"),
    "foreignObject should be replaced with marker for nested rendering"
  );
  assert!(
    !serialized
      .svg
      .contains("FASTRENDER_FOREIGN_OBJECT_UNRESOLVED"),
    "valid dimensions should avoid unresolved placeholder comments"
  );
}

#[test]
fn foreign_object_display_none_does_not_emit_foreign_object_info() {
  let html = r#"
  <style>
    foreignObject{display:none}
  </style>
  <svg width="16" height="12" viewBox="0 0 16 12">
    <foreignObject x="0" y="0" width="10" height="12">
      <div xmlns="http://www.w3.org/1999/xhtml" style="width:10px;height:12px;background: rgb(0, 0, 255);"></div>
    </foreignObject>
  </svg>
  "#;

  let serialized = serialized_inline_svg(html, 20.0, 20.0).expect("serialize svg");
  assert!(
    serialized.foreign_objects.is_empty(),
    "display:none foreignObject should not allocate foreign object info"
  );
  assert!(
    !serialized.svg.contains("FASTRENDER_FOREIGN_OBJECT_0"),
    "display:none foreignObject should not emit a placeholder marker"
  );
}

#[test]
fn foreign_object_accepts_absolute_units_for_dimensions() {
  let html = r#"
  <svg width="2in" height="2in" viewBox="0 0 192 192">
    <foreignObject x="1in" y="0" width="1in" height="1in">
      <div xmlns="http://www.w3.org/1999/xhtml" style="width:96px;height:96px;background: rgb(0, 255, 0);"></div>
    </foreignObject>
  </svg>
  "#;

  let serialized = serialized_inline_svg(html, 200.0, 200.0).expect("serialize svg");
  assert!(
    serialized.svg.contains("FASTRENDER_FOREIGN_OBJECT_0"),
    "absolute units should resolve to a valid foreignObject"
  );
  assert!(
    !serialized
      .svg
      .contains("FASTRENDER_FOREIGN_OBJECT_UNRESOLVED"),
    "converted dimensions should avoid unresolved placeholder"
  );
}

#[test]
fn foreign_object_accepts_percentage_units_for_dimensions() {
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = FastRender::new().expect("renderer");
      let html = r#"
      <style>body{margin:0;background:white} svg{display:block}</style>
      <svg width="16" height="12" viewBox="0 0 16 12">
        <foreignObject x="0" y="0" width="100%" height="100%">
          <div xmlns="http://www.w3.org/1999/xhtml" style="width:100%;height:100%;background: rgb(0, 0, 255);"></div>
        </foreignObject>
      </svg>
      "#;

      let pixmap = renderer.render_html(html, 20, 20).expect("render svg");
      assert_eq!(
        pixel(&pixmap, 8, 6),
        [0, 0, 255, 255],
        "foreignObject should resolve percentage sizing"
      );
    })
    .unwrap()
    .join()
    .unwrap();
}

#[test]
fn foreign_object_overflow_hidden_clips_filter_effects() {
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = FastRender::new().expect("renderer");
      let html = r#"
         <style>body{margin:0;background:white} svg{display:block}</style>
         <svg width="40" height="40" viewBox="0 0 40 40">
           <defs>
              <filter id="blur" x="-50%" y="-50%" width="200%" height="200%">
                <feGaussianBlur stdDeviation="4"/>
            </filter>
          </defs>
          <foreignObject x="10" y="10" width="20" height="20" style="overflow:hidden" filter="url(#blur)">
             <div xmlns="http://www.w3.org/1999/xhtml" style="width:20px;height:20px;background: rgb(255,0,0);"></div>
          </foreignObject>
        </svg>
          "#;

      let toggles = fastrender::debug::runtime::RuntimeToggles::from_map(std::collections::HashMap::from([(
        "FASTR_PAINT_BACKEND".to_string(),
        "display_list".to_string(),
      )]));
      let options = RenderOptions::new()
        .with_viewport(40, 40)
        .with_runtime_toggles(toggles);
      let pixmap = renderer
        .render_html_with_options(html, options)
        .expect("render svg");
      // ForeignObject contents should be visible inside the bounds.
      assert_ne!(pixel(&pixmap, 20, 20), [255, 255, 255, 255]);
      // Filter output that spills outside the foreignObject should be clipped when overflow is
      // hidden.
      assert_eq!(pixel(&pixmap, 8, 20), [255, 255, 255, 255]);
    })
    .unwrap()
    .join()
    .unwrap();
}

#[test]
fn foreign_object_percentage_units_do_not_emit_unresolved_placeholder() {
  let html = r#"
  <svg width="16" height="12" viewBox="0 0 16 12">
    <foreignObject x="0" y="0" width="100%" height="100%">
      <div xmlns="http://www.w3.org/1999/xhtml" style="width:100%;height:100%;background: rgb(0, 0, 255);"></div>
    </foreignObject>
  </svg>
  "#;

  let serialized = serialized_inline_svg(html, 20.0, 20.0).expect("serialize svg");
  assert!(
    serialized.svg.contains("FASTRENDER_FOREIGN_OBJECT_0"),
    "percentage dimensions should resolve to a valid foreignObject"
  );
  assert!(
    !serialized
      .svg
      .contains("FASTRENDER_FOREIGN_OBJECT_UNRESOLVED"),
    "percentage dimensions should avoid unresolved placeholder comments"
  );
}

#[test]
fn foreign_object_overflow_visible_allows_filter_effects_outside_bounds() {
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = FastRender::new().expect("renderer");
      let html = r#"
         <style>body{margin:0;background:white} svg{display:block}</style>
         <svg width="40" height="40" viewBox="0 0 40 40">
           <defs>
             <filter id="blur" x="-50%" y="-50%" width="200%" height="200%">
               <feGaussianBlur stdDeviation="4"/>
           </filter>
         </defs>
         <foreignObject x="10" y="10" width="20" height="20" style="overflow:visible" filter="url(#blur)">
            <div xmlns="http://www.w3.org/1999/xhtml" style="width:20px;height:20px;background: rgb(255,0,0);"></div>
         </foreignObject>
       </svg>
          "#;

      let toggles = fastrender::debug::runtime::RuntimeToggles::from_map(std::collections::HashMap::from([(
        "FASTR_PAINT_BACKEND".to_string(),
        "display_list".to_string(),
      )]));
      let options = RenderOptions::new()
        .with_viewport(40, 40)
        .with_runtime_toggles(toggles);
      let pixmap = renderer
        .render_html_with_options(html, options)
        .expect("render svg");
      // ForeignObject contents should be visible inside the bounds.
      assert_ne!(pixel(&pixmap, 20, 20), [255, 255, 255, 255]);
      // Without overflow clipping, the blur filter should be visible just outside the bounds.
      assert_ne!(pixel(&pixmap, 8, 20), [255, 255, 255, 255]);
    })
    .unwrap()
    .join()
    .unwrap();
}

#[test]
fn foreign_object_overflow_x_visible_y_clip_allows_horizontal_filter_bleed() {
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = FastRender::new().expect("renderer");
      let html = r#"
          <style>body{margin:0;background:white} svg{display:block}</style>
          <svg width="40" height="40" viewBox="0 0 40 40">
            <defs>
              <filter id="blur" x="-50%" y="-50%" width="200%" height="200%">
                <feGaussianBlur stdDeviation="4"/>
            </filter>
          </defs>
          <foreignObject x="10" y="10" width="20" height="20" style="overflow-x:visible; overflow-y:clip" filter="url(#blur)">
             <div xmlns="http://www.w3.org/1999/xhtml" style="width:20px;height:20px;background: rgb(255,0,0);"></div>
          </foreignObject>
        </svg>
           "#;

      let toggles = fastrender::debug::runtime::RuntimeToggles::from_map(std::collections::HashMap::from([(
        "FASTR_PAINT_BACKEND".to_string(),
        "display_list".to_string(),
      )]));
      let options = RenderOptions::new()
        .with_viewport(40, 40)
        .with_runtime_toggles(toggles);
      let pixmap = renderer
        .render_html_with_options(html, options)
        .expect("render svg");

      assert_ne!(pixel(&pixmap, 20, 20), [255, 255, 255, 255]);
      // Horizontal overflow is visible, so the blur should show just outside the x-bounds.
      assert_ne!(pixel(&pixmap, 8, 20), [255, 255, 255, 255]);
      // Vertical overflow is clipped, so the blur should be cut off outside the y-bounds.
      assert_eq!(pixel(&pixmap, 20, 8), [255, 255, 255, 255]);
    })
    .unwrap()
    .join()
    .unwrap();
}

#[test]
fn foreign_object_overflow_x_clip_y_visible_allows_vertical_filter_bleed() {
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = FastRender::new().expect("renderer");
      let html = r#"
          <style>body{margin:0;background:white} svg{display:block}</style>
          <svg width="40" height="40" viewBox="0 0 40 40">
            <defs>
              <filter id="blur" x="-50%" y="-50%" width="200%" height="200%">
                <feGaussianBlur stdDeviation="4"/>
            </filter>
          </defs>
          <foreignObject x="10" y="10" width="20" height="20" style="overflow-x:clip; overflow-y:visible" filter="url(#blur)">
             <div xmlns="http://www.w3.org/1999/xhtml" style="width:20px;height:20px;background: rgb(255,0,0);"></div>
          </foreignObject>
        </svg>
           "#;

      let toggles = fastrender::debug::runtime::RuntimeToggles::from_map(std::collections::HashMap::from([(
        "FASTR_PAINT_BACKEND".to_string(),
        "display_list".to_string(),
      )]));
      let options = RenderOptions::new()
        .with_viewport(40, 40)
        .with_runtime_toggles(toggles);
      let pixmap = renderer
        .render_html_with_options(html, options)
        .expect("render svg");

      assert_ne!(pixel(&pixmap, 20, 20), [255, 255, 255, 255]);
      // Horizontal overflow is clipped, so the blur should be cut off outside the x-bounds.
      assert_eq!(pixel(&pixmap, 8, 20), [255, 255, 255, 255]);
      // Vertical overflow is visible, so the blur should show just outside the y-bounds.
      assert_ne!(pixel(&pixmap, 20, 8), [255, 255, 255, 255]);
    })
    .unwrap()
    .join()
    .unwrap();
}

#[test]
fn foreign_object_overflow_x_visible_y_clip_allows_horizontal_content_overflow() {
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = FastRender::new().expect("renderer");
      let html = r#"
        <style>body{margin:0;background:white} svg{display:block}</style>
        <svg width="60" height="60" viewBox="0 0 60 60">
          <foreignObject x="10" y="10" width="20" height="20" style="overflow-x:visible; overflow-y:clip">
            <div xmlns="http://www.w3.org/1999/xhtml" style="width:40px;height:40px;background: rgb(255,0,0);"></div>
          </foreignObject>
        </svg>
      "#;

      let toggles = RuntimeToggles::from_map(HashMap::from([(
        "FASTR_PAINT_BACKEND".to_string(),
        "display_list".to_string(),
      )]));
      let options = RenderOptions::new()
        .with_viewport(60, 60)
        .with_runtime_toggles(toggles);
      let pixmap = renderer
        .render_html_with_options(html, options)
        .expect("render svg");

      // Content should paint inside the foreignObject.
      assert_eq!(pixel(&pixmap, 20, 20), [255, 0, 0, 255]);
      // Horizontal overflow is visible, so the extra width should paint outside the x-bounds.
      assert_eq!(pixel(&pixmap, 35, 20), [255, 0, 0, 255]);
      // Vertical overflow is clipped, so content should not appear outside the y-bounds.
      assert_eq!(pixel(&pixmap, 20, 35), [255, 255, 255, 255]);
      assert_eq!(pixel(&pixmap, 35, 35), [255, 255, 255, 255]);
    })
    .unwrap()
    .join()
    .unwrap();
}

#[test]
fn foreign_object_overflow_x_clip_y_visible_allows_vertical_content_overflow() {
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = FastRender::new().expect("renderer");
      let html = r#"
        <style>body{margin:0;background:white} svg{display:block}</style>
        <svg width="60" height="60" viewBox="0 0 60 60">
          <foreignObject x="10" y="10" width="20" height="20" style="overflow-x:clip; overflow-y:visible">
            <div xmlns="http://www.w3.org/1999/xhtml" style="width:40px;height:40px;background: rgb(255,0,0);"></div>
          </foreignObject>
        </svg>
      "#;

      let toggles = RuntimeToggles::from_map(HashMap::from([(
        "FASTR_PAINT_BACKEND".to_string(),
        "display_list".to_string(),
      )]));
      let options = RenderOptions::new()
        .with_viewport(60, 60)
        .with_runtime_toggles(toggles);
      let pixmap = renderer
        .render_html_with_options(html, options)
        .expect("render svg");

      // Content should paint inside the foreignObject.
      assert_eq!(pixel(&pixmap, 20, 20), [255, 0, 0, 255]);
      // Vertical overflow is visible, so the extra height should paint outside the y-bounds.
      assert_eq!(pixel(&pixmap, 20, 35), [255, 0, 0, 255]);
      // Horizontal overflow is clipped, so content should not appear outside the x-bounds.
      assert_eq!(pixel(&pixmap, 35, 20), [255, 255, 255, 255]);
      assert_eq!(pixel(&pixmap, 35, 35), [255, 255, 255, 255]);
    })
    .unwrap()
    .join()
    .unwrap();
}

#[test]
fn inline_svg_respects_display_none_when_document_css_injection_disabled_with_legacy_paint_backend() {
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = FastRender::new().expect("renderer");
      let html = r#"
      <style>
        body { margin: 0; background: white; }
        svg { display: block; }
        rect { display: none; }
      </style>
      <svg width="10" height="10" viewBox="0 0 10 10">
        <rect width="10" height="10" fill="rgb(255, 0, 0)" />
      </svg>
      "#;

      let toggles = RuntimeToggles::from_map(HashMap::from([
        (
          "FASTR_SVG_EMBED_DOCUMENT_CSS".to_string(),
          "0".to_string(),
        ),
        ("FASTR_PAINT_BACKEND".to_string(), "legacy".to_string()),
      ]));
      let options = RenderOptions::new()
        .with_viewport(20, 20)
        .with_runtime_toggles(toggles);

      let pixmap = renderer
        .render_html_with_options(html, options)
        .expect("render svg");
      assert_eq!(
        pixel(&pixmap, 5, 5),
        [255, 255, 255, 255],
        "rect should be hidden via serialized display:none"
      );
    })
    .unwrap()
    .join()
    .unwrap();
}

#[test]
fn inline_svg_respects_visibility_hidden_when_document_css_injection_disabled_with_legacy_paint_backend() {
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = FastRender::new().expect("renderer");
      let html = r#"
      <style>
        body { margin: 0; background: white; }
        svg { display: block; }
        rect { visibility: hidden; }
      </style>
      <svg width="10" height="10" viewBox="0 0 10 10">
        <rect width="10" height="10" fill="rgb(255, 0, 0)" />
      </svg>
      "#;

      let toggles = RuntimeToggles::from_map(HashMap::from([
        (
          "FASTR_SVG_EMBED_DOCUMENT_CSS".to_string(),
          "0".to_string(),
        ),
        ("FASTR_PAINT_BACKEND".to_string(), "legacy".to_string()),
      ]));
      let options = RenderOptions::new()
        .with_viewport(20, 20)
        .with_runtime_toggles(toggles);

      let pixmap = renderer
        .render_html_with_options(html, options)
        .expect("render svg");
      assert_eq!(
        pixel(&pixmap, 5, 5),
        [255, 255, 255, 255],
        "rect should be hidden via serialized visibility:hidden"
      );
    })
    .unwrap()
    .join()
    .unwrap();
}

#[test]
fn inline_svg_respects_opacity_when_document_css_injection_disabled_with_legacy_paint_backend() {
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = FastRender::new().expect("renderer");
      let html = r#"
      <style>
        body { margin: 0; background: white; }
        svg { display: block; }
        rect { opacity: 0; }
      </style>
      <svg width="10" height="10" viewBox="0 0 10 10">
        <rect width="10" height="10" fill="rgb(255, 0, 0)" />
      </svg>
      "#;

      let toggles = RuntimeToggles::from_map(HashMap::from([
        (
          "FASTR_SVG_EMBED_DOCUMENT_CSS".to_string(),
          "0".to_string(),
        ),
        ("FASTR_PAINT_BACKEND".to_string(), "legacy".to_string()),
      ]));
      let options = RenderOptions::new()
        .with_viewport(20, 20)
        .with_runtime_toggles(toggles);

      let pixmap = renderer
        .render_html_with_options(html, options)
        .expect("render svg");
      assert_eq!(
        pixel(&pixmap, 5, 5),
        [255, 255, 255, 255],
        "rect should be transparent via serialized opacity"
      );
    })
    .unwrap()
    .join()
    .unwrap();
}
