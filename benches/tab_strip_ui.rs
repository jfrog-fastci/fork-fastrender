// Criterion benchmark for the browser chrome tab strip.
//
// This bench is feature-gated because the egui-based chrome UI lives behind the `browser_ui`
// feature to keep the core renderer dependency surface small.

#[cfg(feature = "browser_ui")]
use criterion::{black_box, criterion_group, criterion_main, Criterion};
#[cfg(feature = "browser_ui")]
use egui::{vec2, Pos2, RawInput, Rect};
#[cfg(feature = "browser_ui")]
use fastrender::ui::{chrome_ui, BrowserAppState, BrowserTabState, TabId};

#[cfg(feature = "browser_ui")]
fn begin_frame(ctx: &egui::Context, screen_size: egui::Vec2, time: f64) {
  let mut raw = RawInput::default();
  raw.screen_rect = Some(Rect::from_min_size(Pos2::new(0.0, 0.0), screen_size));
  raw.time = Some(time);
  raw.focused = true;
  ctx.begin_frame(raw);
}

#[cfg(feature = "browser_ui")]
fn build_app_with_tabs(tab_count: usize, pinned_count: usize) -> BrowserAppState {
  let mut app = BrowserAppState::new();

  for i in 0..tab_count {
    let tab_id = TabId((i + 1) as u64);
    let mut tab = BrowserTabState::new(tab_id, format!("https://example.com/{}/index.html", i + 1));
    if i < pinned_count {
      tab.pinned = true;
    }
    app.push_tab(tab, i == 0);
  }

  // Create some tab groups among the unpinned tabs to exercise chip + group layout paths.
  let unpinned_ids: Vec<TabId> = app.tabs.iter().skip(pinned_count).map(|t| t.id).collect();
  let group_size = 12usize;
  let group_count = (unpinned_ids.len() / group_size).min(8);
  for g in 0..group_count {
    let start = g * group_size;
    let end = start + group_size;
    let _ = app.create_group_with_tabs(&unpinned_ids[start..end]);
  }

  // Collapse a couple of groups to ensure collapse/expand animation state is exercised (even if
  // reduced-motion is enabled in some environments).
  for (idx, group) in app.tab_groups.values_mut().enumerate() {
    if idx % 3 == 0 {
      group.collapsed = true;
    }
  }

  app
}

#[cfg(feature = "browser_ui")]
fn bench_tab_strip_chrome_frame(c: &mut Criterion) {
  // 200+ tabs with a mix of pinned/unpinned + groups.
  let mut app = build_app_with_tabs(260, 24);
  let ctx = egui::Context::default();

  // Warm-up frame: populate egui caches (fonts, icons) and the tab strip layout snapshot.
  begin_frame(&ctx, vec2(1200.0, 800.0), 0.0);
  let _ = chrome_ui(&ctx, &mut app, false, |_| None);
  let _ = ctx.end_frame();

  let mut t = 1.0 / 60.0;
  c.bench_function("chrome_frame/tab_strip_260_tabs", |b| {
    b.iter(|| {
      begin_frame(&ctx, vec2(1200.0, 800.0), t);
      t += 1.0 / 60.0;
      let actions = chrome_ui(&ctx, &mut app, false, |_| None);
      black_box(actions);
      let output = ctx.end_frame();
      black_box(output);
    })
  });
}

#[cfg(feature = "browser_ui")]
criterion_group!(benches, bench_tab_strip_chrome_frame);
#[cfg(feature = "browser_ui")]
criterion_main!(benches);

#[cfg(not(feature = "browser_ui"))]
fn main() {}
