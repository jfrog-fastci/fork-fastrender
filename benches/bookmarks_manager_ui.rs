//! Benchmarks for the bookmarks manager egui UI.
//!
//! Run with:
//!   cargo bench --features browser_ui --bench bookmarks_manager_ui

mod common;

#[cfg(feature = "browser_ui")]
use criterion::{criterion_group, criterion_main, Criterion};

#[cfg(feature = "browser_ui")]
use egui::{Context, Pos2, RawInput, Rect, Vec2};

#[cfg(feature = "browser_ui")]
use fastrender::ui::{bookmarks_manager, BookmarkStore};

#[cfg(feature = "browser_ui")]
fn build_large_store() -> BookmarkStore {
  // A large flat list is the worst-case for immediate-mode per-row rendering.
  const BOOKMARK_COUNT: usize = 5_000;
  let mut store = BookmarkStore::default();
  for i in 0..BOOKMARK_COUNT {
    let url = format!("https://example.com/{i}");
    let title = Some(format!("Example {i}"));
    let _ = store.add(url, title, None).expect("add bookmark");
  }
  store
}

#[cfg(feature = "browser_ui")]
fn run_frame(
  ctx: &Context,
  state: &mut bookmarks_manager::BookmarksManagerState,
  store: &mut BookmarkStore,
  t: &mut f64,
) {
  *t += 1.0 / 60.0;
  let input = RawInput {
    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1200.0, 800.0))),
    pixels_per_point: Some(1.0),
    time: Some(*t),
    ..Default::default()
  };
  let _ = ctx.run(input, |ctx| {
    let _out = bookmarks_manager::bookmarks_manager_side_panel(ctx, state, store);
  });
}

#[cfg(feature = "browser_ui")]
fn bench_bookmarks_manager_frame(c: &mut Criterion) {
  common::bench_print_config_once("bookmarks_manager_ui", &[]);

  let mut store = build_large_store();
  let mut state = bookmarks_manager::BookmarksManagerState::default();
  let ctx = Context::default();
  let mut t = 0.0;

  // Warm a single frame to populate egui caches and the bookmarks list flatten cache.
  run_frame(&ctx, &mut state, &mut store, &mut t);

  c.bench_function("bookmarks_manager_frame_5k", |b| {
    b.iter(|| run_frame(&ctx, &mut state, &mut store, &mut t))
  });
}

#[cfg(feature = "browser_ui")]
criterion_group!(benches, bench_bookmarks_manager_frame);
#[cfg(feature = "browser_ui")]
criterion_main!(benches);

#[cfg(not(feature = "browser_ui"))]
fn main() {
  eprintln!("bookmarks_manager_ui bench requires `--features browser_ui`");
}
