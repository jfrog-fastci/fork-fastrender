//! Benchmarks for "renderer chrome" (browser UI rendered by FastRender).
//!
//! The renderer-chrome workstream (see `instructions/renderer_chrome.md`) aims for a browser UI that
//! *feels* 60fps. These benchmarks provide an objective timing signal for:
//! - first render of the chrome frame HTML (parse + style + layout + paint), and
//! - hover-driven repaints while the user moves the pointer across the chrome.
//!
//! # Running
//!
//! ```bash
//! cargo bench -p fastrender --bench chrome_frame_render
//! ```
//!
//! # Interpreting results
//!
//! * `chrome_frame/initial_render` is the cost to produce the first chrome frame pixmap for a
//!   typical desktop viewport (1200×80 @ dpr=1). This includes HTML parse.
//!
//! * `chrome_frame/hover_repaint_loop` simulates pointer movement across N tab positions that toggle
//!   `:hover`. Criterion reports the total time for the *loop*; divide by `HOVER_POSITIONS` to get an
//!   average "cost per hover frame". For a 60fps feel, aim for <16ms per hover frame.
//!   Regressions of ~20%+ usually correlate with noticeable UI lag.
//!
//! Notes:
//! - The benchmarks are fully offline/deterministic: a custom `ResourceFetcher` serves the chrome CSS
//!   and icon SVGs from `assets/` (no network).
//! - Fonts are bundled-only to avoid host-dependent system font discovery.

use std::sync::Arc;

use criterion::{black_box, criterion_group, criterion_main, Criterion};

use fastrender::geometry::Point;
use fastrender::interaction::{fragment_tree_with_scroll, InteractionEngine};
use fastrender::resource::{FetchRequest, FetchedResource, ResourceFetcher};
use fastrender::text::font_db::FontConfig;
use fastrender::{BrowserDocument, FastRender, RenderOptions};

mod common;

// -----------------------------------------------------------------------------
// Benchmark constants / fixtures
// -----------------------------------------------------------------------------

const VIEWPORT: (u32, u32) = (1200, 80);
const DPR: f32 = 1.0;

// Tab geometry is kept fixed so hover positions are stable and deterministic.
const TAB_STRIP_HEIGHT: f32 = 32.0;
const TAB_STRIP_PADDING_X: f32 = 8.0;
const TAB_GAP_X: f32 = 4.0;
const TAB_WIDTH: f32 = 108.0;
const TAB_COUNT: usize = 10;

// Number of hover transitions we benchmark in a single loop.
const HOVER_CYCLES: usize = 4;
const HOVER_POSITIONS: usize = TAB_COUNT * HOVER_CYCLES;

const URL_CHROME_CSS: &str = "https://fastrender.local/chrome/chrome.css";
const URL_ICON_BACK: &str = "https://fastrender.local/chrome/icons/back.svg";
const URL_ICON_FORWARD: &str = "https://fastrender.local/chrome/icons/forward.svg";
const URL_ICON_RELOAD: &str = "https://fastrender.local/chrome/icons/reload.svg";
const URL_ICON_MENU: &str = "https://fastrender.local/chrome/icons/menu.svg";
const URL_ICON_PLUS: &str = "https://fastrender.local/chrome/icons/plus.svg";

const ICON_BACK: &[u8] = include_bytes!("../assets/browser_icons/back.svg");
const ICON_FORWARD: &[u8] = include_bytes!("../assets/browser_icons/forward.svg");
const ICON_RELOAD: &[u8] = include_bytes!("../assets/browser_icons/reload.svg");
const ICON_MENU: &[u8] = include_bytes!("../assets/browser_icons/menu.svg");
const ICON_PLUS: &[u8] = include_bytes!("../assets/browser_icons/plus.svg");

// Minimal-but-representative chrome frame HTML:
// - tab strip (hover targets)
// - toolbar with a few icon buttons + address field stub
const CHROME_FRAME_HTML: &str = r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <link rel="stylesheet" href="https://fastrender.local/chrome/chrome.css">
  </head>
  <body>
    <div class="chrome-frame">
      <div class="tab-strip" role="tablist" aria-label="Tabs">
        <div class="tab active" role="tab" aria-selected="true"><span class="title">FastRender</span><button class="tab-close" aria-label="Close tab">×</button></div>
        <div class="tab" role="tab"><span class="title">Example</span><button class="tab-close" aria-label="Close tab">×</button></div>
        <div class="tab" role="tab"><span class="title">Docs</span><button class="tab-close" aria-label="Close tab">×</button></div>
        <div class="tab" role="tab"><span class="title">Issues</span><button class="tab-close" aria-label="Close tab">×</button></div>
        <div class="tab" role="tab"><span class="title">PRs</span><button class="tab-close" aria-label="Close tab">×</button></div>
        <div class="tab" role="tab"><span class="title">Bench</span><button class="tab-close" aria-label="Close tab">×</button></div>
        <div class="tab" role="tab"><span class="title">Settings</span><button class="tab-close" aria-label="Close tab">×</button></div>
        <div class="tab" role="tab"><span class="title">History</span><button class="tab-close" aria-label="Close tab">×</button></div>
        <div class="tab" role="tab"><span class="title">About</span><button class="tab-close" aria-label="Close tab">×</button></div>
        <div class="tab" role="tab"><span class="title">More</span><button class="tab-close" aria-label="Close tab">×</button></div>
        <button class="tool-btn new-tab" aria-label="New tab"><img src="https://fastrender.local/chrome/icons/plus.svg" alt=""></button>
      </div>
      <div class="toolbar" role="toolbar" aria-label="Navigation">
        <button class="tool-btn" aria-label="Back"><img src="https://fastrender.local/chrome/icons/back.svg" alt=""></button>
        <button class="tool-btn" aria-label="Forward"><img src="https://fastrender.local/chrome/icons/forward.svg" alt=""></button>
        <button class="tool-btn" aria-label="Reload"><img src="https://fastrender.local/chrome/icons/reload.svg" alt=""></button>
        <div class="address">https://example.com</div>
        <button class="tool-btn" aria-label="Menu"><img src="https://fastrender.local/chrome/icons/menu.svg" alt=""></button>
      </div>
    </div>
  </body>
</html>
"#;

const CHROME_CSS: &str = r#"
/* Chrome frame benchmark CSS (offline fixture).
 *
 * Keep this reasonably "realistic": flex layout, rounded corners, borders, and hover fills.
 * Avoid animations/transitions here so the benchmark stays deterministic.
 */

* { box-sizing: border-box; }

html, body {
  width: 100%;
  height: 100%;
  margin: 0;
  padding: 0;
  font: 13px/1.2 system-ui, -apple-system, Segoe UI, sans-serif;
  color: #1f2328;
  background: #f6f7fb;
}

.chrome-frame {
  width: 100%;
  height: 100%;
  display: flex;
  flex-direction: column;
}

.tab-strip {
  height: 32px;
  display: flex;
  align-items: center;
  gap: 4px;
  padding: 0 8px;
  background: #e9e9ef;
  border-bottom: 1px solid #cfd0d4;
}

.tab {
  flex: none;
  width: 108px;
  height: 24px;
  padding: 0 10px;
  display: flex;
  align-items: center;
  gap: 6px;
  border-radius: 10px;
  background: #d8d8e2;
  border: 1px solid rgba(0,0,0,0.08);
}

.tab:hover {
  background: #cfcfdb;
}

.tab.active {
  background: #ffffff;
}

.tab .title {
  flex: 1;
  overflow: hidden;
  white-space: nowrap;
  text-overflow: ellipsis;
}

.tab-close {
  flex: none;
  width: 16px;
  height: 16px;
  padding: 0;
  border: 0;
  border-radius: 8px;
  background: transparent;
  color: #5c5f66;
}

.tab-close:hover {
  background: rgba(0,0,0,0.10);
}

.toolbar {
  height: 48px;
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 0 8px;
  background: #f7f7f9;
}

.tool-btn {
  flex: none;
  width: 32px;
  height: 32px;
  padding: 0;
  border: 0;
  border-radius: 8px;
  background: transparent;
}

.tool-btn:hover {
  background: rgba(0,0,0,0.07);
}

.tool-btn img {
  width: 18px;
  height: 18px;
  display: block;
  margin: 7px;
}

.address {
  flex: 1;
  height: 32px;
  display: flex;
  align-items: center;
  padding: 0 12px;
  border-radius: 16px;
  border: 1px solid #d0d1d6;
  background: #ffffff;
  overflow: hidden;
  white-space: nowrap;
  text-overflow: ellipsis;
}

.address:hover {
  border-color: #b2b3bb;
}
"#;

// -----------------------------------------------------------------------------
// Offline chrome asset fetcher
// -----------------------------------------------------------------------------

/// Deterministic, offline fetcher that serves chrome UI assets from the repo.
///
/// This intentionally rejects any unexpected URL so the benchmark never hits the network.
#[derive(Debug, Default)]
struct ChromeAssetFetcher;

impl ChromeAssetFetcher {
  fn fetch_inner(&self, url: &str) -> fastrender::Result<FetchedResource> {
    match url {
      URL_CHROME_CSS => Ok(FetchedResource::with_final_url(
        CHROME_CSS.as_bytes().to_vec(),
        Some("text/css".to_string()),
        Some(url.to_string()),
      )),
      URL_ICON_BACK => Ok(FetchedResource::with_final_url(
        ICON_BACK.to_vec(),
        Some("image/svg+xml".to_string()),
        Some(url.to_string()),
      )),
      URL_ICON_FORWARD => Ok(FetchedResource::with_final_url(
        ICON_FORWARD.to_vec(),
        Some("image/svg+xml".to_string()),
        Some(url.to_string()),
      )),
      URL_ICON_RELOAD => Ok(FetchedResource::with_final_url(
        ICON_RELOAD.to_vec(),
        Some("image/svg+xml".to_string()),
        Some(url.to_string()),
      )),
      URL_ICON_MENU => Ok(FetchedResource::with_final_url(
        ICON_MENU.to_vec(),
        Some("image/svg+xml".to_string()),
        Some(url.to_string()),
      )),
      URL_ICON_PLUS => Ok(FetchedResource::with_final_url(
        ICON_PLUS.to_vec(),
        Some("image/svg+xml".to_string()),
        Some(url.to_string()),
      )),
      other => Err(fastrender::Error::Other(format!(
        "chrome_frame_render bench attempted unexpected fetch: {other}"
      ))),
    }
  }
}

impl ResourceFetcher for ChromeAssetFetcher {
  fn fetch(&self, url: &str) -> fastrender::Result<FetchedResource> {
    self.fetch_inner(url)
  }

  fn fetch_with_request(&self, req: FetchRequest<'_>) -> fastrender::Result<FetchedResource> {
    // Preserve a strict offline invariant even when the renderer starts passing richer metadata.
    let url = req.url;
    self.fetch_inner(url)
  }
}

fn chrome_renderer() -> FastRender {
  let fetcher: Arc<dyn ResourceFetcher> = Arc::new(ChromeAssetFetcher::default());
  FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .fetcher(fetcher)
    .build()
    .expect("build renderer")
}

fn chrome_render_options() -> RenderOptions {
  RenderOptions::new()
    .with_viewport(VIEWPORT.0, VIEWPORT.1)
    .with_device_pixel_ratio(DPR)
}

// -----------------------------------------------------------------------------
// Benchmarks
// -----------------------------------------------------------------------------

fn chrome_frame_initial_render(c: &mut Criterion) {
  common::bench_print_config_once(
    "chrome_frame_render",
    &[
      ("viewport", format!("{}x{}", VIEWPORT.0, VIEWPORT.1)),
      ("dpr", DPR.to_string()),
    ],
  );

  let renderer = chrome_renderer();
  let options = chrome_render_options();

  // Reuse a single renderer + BrowserDocument to avoid measuring fontdb/image cache setup.
  let mut doc = BrowserDocument::new(renderer, "<!doctype html><html></html>", options.clone())
    .expect("create BrowserDocument");
  let interaction = InteractionEngine::new();

  c.bench_function("chrome_frame/initial_render", |b| {
    b.iter(|| {
      doc
        .reset_with_html(CHROME_FRAME_HTML, options.clone())
        .expect("reset html");
      let frame = doc
        .render_frame_with_scroll_state_and_interaction_state(Some(interaction.interaction_state()))
        .expect("render frame");
      black_box(frame.pixmap.data());
    })
  });
}

fn chrome_frame_hover_repaint_loop(c: &mut Criterion) {
  let renderer = chrome_renderer();
  let options = chrome_render_options();

  // Reuse a single renderer + BrowserDocument to keep the signal focused on hover-driven renders.
  let mut doc = BrowserDocument::new(renderer, "<!doctype html><html></html>", options.clone())
    .expect("create BrowserDocument");

  // Generate deterministic hover positions over the tab-strip. This matches the fixed CSS layout
  // constants above.
  let mut hover_positions = Vec::with_capacity(HOVER_POSITIONS);
  for _cycle in 0..HOVER_CYCLES {
    for tab_idx in 0..TAB_COUNT {
      let x = TAB_STRIP_PADDING_X + (TAB_WIDTH / 2.0) + (tab_idx as f32) * (TAB_WIDTH + TAB_GAP_X);
      let y = TAB_STRIP_HEIGHT / 2.0;
      hover_positions.push(Point::new(x, y));
    }
  }
  debug_assert_eq!(
    hover_positions.len(),
    HOVER_POSITIONS,
    "hover positions length mismatch"
  );

  c.bench_function("chrome_frame/hover_repaint_loop", |b| {
    b.iter(|| {
      // Start each sample from a clean, un-hovered document.
      doc
        .reset_with_html(CHROME_FRAME_HTML, options.clone())
        .expect("reset html");
      let mut interaction = InteractionEngine::new();

      // Prime the layout cache. (Using an explicit empty InteractionState keeps the browser-UI call
      // sites consistent: they typically pass `Some(interaction_state)` on every paint.)
      let frame = doc
        .render_frame_with_scroll_state_and_interaction_state(Some(interaction.interaction_state()))
        .expect("initial render");
      black_box(frame.pixmap.data());

      for viewport_point in &hover_positions {
        // Update `InteractionEngine` hover state using the latest layout artifacts.
        let scroll = doc.scroll_state();
        doc
          .mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
            let scrolled =
              (!scroll.elements.is_empty()).then(|| fragment_tree_with_scroll(fragment_tree, &scroll));
            let fragment_tree = scrolled.as_ref().unwrap_or(fragment_tree);
            let _ = interaction.pointer_move(dom, box_tree, fragment_tree, &scroll, *viewport_point);
            // Hover state changes should not invalidate the DOM/layout caches directly; rendering is
            // driven by the interaction state passed to `render_frame_*`.
            (false, ())
          })
          .expect("pointer move");

        let frame = doc
          .render_frame_with_scroll_state_and_interaction_state(Some(interaction.interaction_state()))
          .expect("hover repaint");
        black_box(frame.pixmap.data());
      }
    })
  });
}

criterion_group! {
  name = chrome_frame_group;
  config = common::perf_criterion();
  targets = chrome_frame_initial_render, chrome_frame_hover_repaint_loop
}
criterion_main!(chrome_frame_group);
