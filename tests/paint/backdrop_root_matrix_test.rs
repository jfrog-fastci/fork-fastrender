use fastrender::image_loader::ImageCache;
use fastrender::paint::display_list_renderer::PaintParallelism;
use fastrender::paint::painter::paint_tree_with_resources_scaled_offset;
use fastrender::scroll::ScrollState;
use fastrender::{FastRender, Point, Rgba};

fn ensure_small_rayon_thread_pool() {
  let threads = std::env::var("RAYON_NUM_THREADS")
    .ok()
    .and_then(|v| v.parse::<usize>().ok())
    .unwrap_or(4)
    .max(1);
  crate::rayon_test_util::init_rayon_for_tests(threads);
}

fn render(html: &str, width: u32, height: u32) -> tiny_skia::Pixmap {
  ensure_small_rayon_thread_pool();
  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("parsed");
  let fragment_tree = renderer
    .layout_document(&dom, width, height)
    .expect("laid out");

  let font_ctx = renderer.font_context().clone();
  let image_cache = ImageCache::new();

  paint_tree_with_resources_scaled_offset(
    &fragment_tree,
    width,
    height,
    Rgba::WHITE,
    font_ctx,
    image_cache,
    1.0,
    Point::ZERO,
    // Keep painting deterministic; these cases focus on Backdrop Root and blending boundaries.
    PaintParallelism::disabled(),
    &ScrollState::default(),
  )
  .expect("painted")
}

fn pixel(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let p = pixmap.pixel(x, y).unwrap();
  (p.red(), p.green(), p.blue(), p.alpha())
}

fn assert_redish(rgba: (u8, u8, u8, u8), label: &str) {
  let (r, g, b, a) = rgba;
  assert!(
    r >= 230 && g <= 30 && b <= 30 && a >= 250,
    "{label}: expected red-ish, got rgba=({r},{g},{b},{a})"
  );
}

fn assert_cyanish(rgba: (u8, u8, u8, u8), label: &str) {
  let (r, g, b, a) = rgba;
  assert!(
    r <= 30 && g >= 220 && b >= 220 && a >= 250,
    "{label}: expected cyan-ish, got rgba=({r},{g},{b},{a})"
  );
}

fn assert_blackish(rgba: (u8, u8, u8, u8), label: &str) {
  let (r, g, b, a) = rgba;
  assert!(
    r <= 30 && g <= 30 && b <= 30 && a >= 250,
    "{label}: expected black-ish, got rgba=({r},{g},{b},{a})"
  );
}

fn html_with_styles(root_style: &str, target_style: &str) -> String {
  // Keep this tiny and deterministic (WPT-style):
  // - No gradients except the fully-opaque mask used to trigger `mask-image`.
  // - Small rects with sampling points away from edges to avoid AA differences.
  format!(
    r#"<!doctype html>
      <style>
        html, body {{
          margin: 0;
          padding: 0;
          background: rgb(255 0 0);
        }}
        #root {{
          position: absolute;
          inset: 0;
          {root_style}
        }}
        #target {{
          position: absolute;
          left: 0;
          top: 0;
          width: 40px;
          height: 40px;
          {target_style}
        }}
      </style>
      <div id="root"><div id="target"></div></div>
    "#,
  )
}

#[test]
fn backdrop_root_matrix_backdrop_filter_sampling() {
  struct Case {
    name: &'static str,
    root_style: &'static str,
    expect_cyan: bool,
  }

  // Backdrop Root triggers per Filter Effects Level 2.
  let stop_sampling = [
    Case {
      name: "filter",
      root_style: "filter: blur(0px);",
      expect_cyan: false,
    },
    Case {
      name: "opacity",
      root_style: "opacity: 0.5;",
      expect_cyan: false,
    },
    Case {
      name: "mask-image",
      root_style: r#"
        mask-image: linear-gradient(to bottom, black 0% 100%);
        mask-mode: alpha;
        mask-repeat: no-repeat;
        mask-size: 100% 100%;
      "#,
      expect_cyan: false,
    },
    Case {
      name: "clip-path",
      root_style: "clip-path: inset(0px);",
      expect_cyan: false,
    },
    Case {
      name: "will-change: filter",
      root_style: "will-change: filter;",
      expect_cyan: false,
    },
    Case {
      name: "mix-blend-mode",
      root_style: "mix-blend-mode: difference;",
      expect_cyan: false,
    },
  ];

  // Properties that create stacking contexts but are *not* Backdrop Root triggers must not stop
  // backdrop-filter sampling.
  let sample_through = [
    Case {
      name: "no-trigger",
      root_style: "",
      expect_cyan: true,
    },
    Case {
      name: "transform (non-trigger)",
      root_style: "transform: translateX(0px);",
      expect_cyan: true,
    },
    Case {
      name: "will-change: transform (non-trigger)",
      root_style: "will-change: transform;",
      expect_cyan: true,
    },
  ];

  for case in stop_sampling.into_iter().chain(sample_through) {
    let html = html_with_styles(
      case.root_style,
      "backdrop-filter: invert(1); background: transparent;",
    );
    let pixmap = render(&html, 64, 64);

    let inside = pixel(&pixmap, 20, 20);
    let outside = pixel(&pixmap, 50, 50);

    if case.expect_cyan {
      assert_cyanish(inside, &format!("{} (inside)", case.name));
    } else {
      assert_redish(inside, &format!("{} (inside)", case.name));
    }
    assert_redish(outside, &format!("{} (outside)", case.name));
  }
}

#[test]
fn backdrop_root_matrix_mix_blend_mode_isolation() {
  struct Case {
    name: &'static str,
    root_style: &'static str,
    expect_black: bool,
  }

  // The mix-blend-mode box paints red (same as the page background). When it can blend with the
  // page backdrop, `difference(red, red)` becomes black. If an ancestor isolates blending, the box
  // sees a transparent backdrop and stays red (unchanged vs the page background).
  let cases = [
    Case {
      name: "no-trigger (blends with page)",
      root_style: "",
      expect_black: true,
    },
    Case {
      name: "filter",
      root_style: "filter: blur(0px);",
      expect_black: false,
    },
    Case {
      name: "opacity",
      root_style: "opacity: 0.5;",
      expect_black: false,
    },
    Case {
      name: "mask-image",
      root_style: r#"
        mask-image: linear-gradient(to bottom, black 0% 100%);
        mask-mode: alpha;
        mask-repeat: no-repeat;
        mask-size: 100% 100%;
      "#,
      expect_black: false,
    },
    Case {
      name: "clip-path",
      root_style: "clip-path: inset(0px);",
      expect_black: false,
    },
    Case {
      name: "will-change: filter",
      root_style: "will-change: filter;",
      expect_black: false,
    },
    Case {
      name: "mix-blend-mode (ancestor) (non-isolated)",
      root_style: "mix-blend-mode: multiply;",
      expect_black: true,
    },
  ];

  for case in cases {
    let html = html_with_styles(
      case.root_style,
      "background: rgb(255 0 0); mix-blend-mode: difference;",
    );
    let pixmap = render(&html, 64, 64);

    let inside = pixel(&pixmap, 20, 20);
    let outside = pixel(&pixmap, 50, 50);

    if case.expect_black {
      assert_blackish(inside, &format!("{} (inside)", case.name));
    } else {
      assert_redish(inside, &format!("{} (inside)", case.name));
    }
    assert_redish(outside, &format!("{} (outside)", case.name));
  }
}
