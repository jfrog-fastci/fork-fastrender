use fastrender::debug::runtime::{
  set_thread_runtime_toggles, with_thread_runtime_toggles, RuntimeToggles,
};
use fastrender::image_loader::ImageCache;
use fastrender::paint::display_list::DisplayList;
use fastrender::paint::display_list_builder::DisplayListBuilder;
use fastrender::paint::display_list_renderer::{DisplayListRenderer, PaintParallelism};
use fastrender::resource::{CachingFetcher, HttpFetcher};
use fastrender::scroll::ScrollState;
use fastrender::text::font_loader::FontContext;
use fastrender::{FastRender, FontConfig, ResourcePolicy, Rgba};
use rayon::ThreadPoolBuilder;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use url::Url;

fn fnv1a64(bytes: &[u8]) -> u64 {
  // Deterministic, cheap hash used only for test diagnostics.
  let mut hash = 0xcbf2_9ce4_8422_2325u64;
  for &b in bytes {
    hash ^= u64::from(b);
    hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
  }
  hash
}

fn first_byte_diff(expected: &[u8], actual: &[u8]) -> Option<usize> {
  expected
    .iter()
    .zip(actual.iter())
    .position(|(a, b)| a != b)
}

fn format_diff(expected: &[u8], actual: &[u8], width: u32) -> String {
  if expected.len() != actual.len() {
    return format!("len mismatch: expected {} bytes, got {}", expected.len(), actual.len());
  }
  let Some(idx) = first_byte_diff(expected, actual) else {
    return "no diff".to_string();
  };

  let mut diff_pixels = 0usize;
  let mut min_x = u32::MAX;
  let mut min_y = u32::MAX;
  let mut max_x = 0u32;
  let mut max_y = 0u32;
  for (px_idx, (exp_px, act_px)) in expected
    .chunks_exact(4)
    .zip(actual.chunks_exact(4))
    .enumerate()
  {
    if exp_px != act_px {
      diff_pixels += 1;
      let x = (px_idx as u32) % width;
      let y = (px_idx as u32) / width;
      min_x = min_x.min(x);
      min_y = min_y.min(y);
      max_x = max_x.max(x);
      max_y = max_y.max(y);
    }
  }

  let px = idx / 4;
  let chan = idx % 4;
  let x = (px as u32) % width;
  let y = (px as u32) / width;
  let exp_px = &expected[px * 4..px * 4 + 4];
  let act_px = &actual[px * 4..px * 4 + 4];
  format!(
    "first diff at byte {idx} (pixel x={x}, y={y}, channel={chan}) expected={exp_px:?} actual={act_px:?}\n  diff_pixels={diff_pixels} bbox=({min_x},{min_y})-({max_x},{max_y})"
  )
}

fn fixture_path() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("tests/pages/fixtures/filter_backdrop_scene/index.html")
}

fn base_url_for(html_path: &Path) -> String {
  let dir = html_path
    .parent()
    .unwrap_or_else(|| panic!("fixture HTML has no parent directory: {}", html_path.display()));
  Url::from_directory_path(dir)
    .unwrap_or_else(|_| panic!("Failed to build file:// base URL for {}", dir.display()))
    .to_string()
}

fn build_display_list(width: u32, height: u32) -> (DisplayList, FontContext) {
  let html_path = fixture_path();
  let html = fs::read_to_string(&html_path)
    .unwrap_or_else(|e| panic!("Failed to read {}: {e}", html_path.display()));
  let base_url = base_url_for(&html_path);

  let policy = ResourcePolicy::default()
    .allow_http(false)
    .allow_https(false)
    .allow_file(true)
    .allow_data(true);

  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .base_url(base_url.clone())
    .resource_policy(policy.clone())
    .build()
    .expect("renderer");

  let dom = renderer.parse_html(&html).expect("parsed");
  let tree = renderer
    .layout_document(&dom, width, height)
    .expect("laid out");
  let font_ctx = renderer.font_context().clone();

  // Recreate the image cache with an explicit file://-only policy so the test exercises the same
  // offline fixture pipeline as pageset fixtures.
  let fetcher = Arc::new(
    CachingFetcher::new(HttpFetcher::new().with_policy(policy.clone())).with_policy(policy),
  );
  let image_cache = ImageCache::with_base_url_and_fetcher(base_url, fetcher);

  let viewport = tree.viewport_size();
  let list = DisplayListBuilder::with_image_cache(image_cache)
    .with_font_context(font_ctx.clone())
    .with_svg_filter_defs(tree.svg_filter_defs.clone())
    .with_scroll_state(ScrollState::default())
    .with_device_pixel_ratio(1.0)
    // Keep display-list building deterministic; this test focuses on paint-time determinism.
    .with_parallelism(&PaintParallelism::disabled())
    .with_viewport_size(viewport.width, viewport.height)
    .build_tree_with_stacking_checked(&tree)
    .expect("display list");

  (list, font_ctx)
}

fn blur_cache_toggles() -> Arc<RuntimeToggles> {
  // Force blur paths that use rayon even on smaller surfaces by reducing the filter cache max-bytes
  // threshold. This triggers `tile_blur` inside `apply_gaussian_blur_cached`, which processes blur
  // tiles in parallel when the rayon pool has multiple threads.
  //
  // We install this as a per-thread override for the scoped rayon pools used by this test so we
  // don't perturb other tests running in the same process.
  // The determinism regression renders at a relatively small viewport, so the default blur paths
  // would run serially. Drop the filter cache max-bytes threshold below the typical blur surface
  // size so `apply_gaussian_blur_cached` routes through `tile_blur`, which uses rayon to fan out
  // over blur tiles when the pool has multiple threads.
  const MAX_BYTES: usize = 120_000;

  let mut raw = std::env::vars()
    .filter(|(k, _)| k.starts_with("FASTR_"))
    .collect::<HashMap<String, String>>();
  raw.insert("FASTR_SVG_FILTER_CACHE_BYTES".to_string(), MAX_BYTES.to_string());
  Arc::new(RuntimeToggles::from_map(raw))
}

fn thread_pool_with_toggles(threads: usize, toggles: Arc<RuntimeToggles>) -> rayon::ThreadPool {
  ThreadPoolBuilder::new()
    .num_threads(threads)
    .start_handler(move |_| {
      // Keep the thread-local override installed for the lifetime of the pool thread. We intentionally
      // leak the guard to avoid relying on TLS drop ordering when the worker thread exits.
      let guard = set_thread_runtime_toggles(toggles.clone());
      std::mem::forget(guard);
    })
    .build()
    .expect("rayon pool")
}

#[test]
fn backdrop_filter_pipeline_is_deterministic_across_rayon_thread_pools() {
  // Mirror the fixture harnesses (e.g. `render_fixtures`) by running on a larger stack. Some of
  // the backdrop-filter fixtures need deep recursion during layout / display-list building.
  const STACK_SIZE: usize = 128 * 1024 * 1024; // 128MB

  std::thread::Builder::new()
    .name("backdrop-filter-determinism".to_string())
    .stack_size(STACK_SIZE)
    .spawn(|| {
      const WIDTH: u32 = 192;
      const HEIGHT: u32 = 192;
      const RUNS_PER_POOL: usize = 3;

      // Build the display list once so layout/paint ordering stays stable. Individual renders below
      // still exercise blur/backdrop-filter internals that use Rayon for fan-out.
      let (list, font_ctx) = build_display_list(WIDTH, HEIGHT);
      let toggles = blur_cache_toggles();

      let mut reference: Option<Vec<u8>> = None;
      for threads in [1usize, 2, 4] {
        let pool = thread_pool_with_toggles(threads, toggles.clone());

        for run in 0..RUNS_PER_POOL {
          let pixmap = pool.install(|| {
            with_thread_runtime_toggles(toggles.clone(), || {
              DisplayListRenderer::new(WIDTH, HEIGHT, Rgba::WHITE, font_ctx.clone())
                .expect("renderer")
                .with_parallelism(PaintParallelism::disabled())
                .render(&list)
                .expect("render")
            })
          });
          let bytes = pixmap.data().to_vec();
          match reference.as_ref() {
            None => reference = Some(bytes),
            Some(expected) => {
              if expected != &bytes {
                panic!(
                  "backdrop-filter output changed (threads={threads}, run={run})\n  expected_hash={:016x}\n  actual_hash={:016x}\n  {}",
                  fnv1a64(expected),
                  fnv1a64(&bytes),
                  format_diff(expected, &bytes, WIDTH)
                );
              }
            }
          }
        }
      }

      let reference = reference.expect("at least one render produced output");

      // Also validate that parallel tiling (when available) produces the exact same output.
      let parallelism = PaintParallelism {
        tile_size: 128,
        ..PaintParallelism::enabled()
      };
      let pool = thread_pool_with_toggles(4, toggles.clone());
      let report = pool.install(|| {
        with_thread_runtime_toggles(toggles.clone(), || {
          DisplayListRenderer::new(WIDTH, HEIGHT, Rgba::WHITE, font_ctx)
            .expect("renderer")
            .with_parallelism(parallelism)
            .render_with_report(&list)
            .expect("parallel render")
        })
      });
      if report.parallel_used {
        assert!(report.tiles > 1, "expected multiple tiles to be rendered");
      }
      if reference.as_slice() != report.pixmap.data() {
        panic!(
          "parallel tiling output diverged from serial (parallel_used={}, tiles={}, fallback={:?})\n  serial_hash={:016x}\n  parallel_hash={:016x}\n  {}",
          report.parallel_used,
          report.tiles,
          report.fallback_reason,
          fnv1a64(&reference),
          fnv1a64(report.pixmap.data()),
          format_diff(&reference, report.pixmap.data(), WIDTH)
        );
      }
    })
    .expect("spawn test thread")
    .join()
    .expect("test thread panicked");
}
