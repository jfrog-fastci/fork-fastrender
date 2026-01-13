use criterion::{black_box, criterion_group, criterion_main, Criterion};

use fastrender::ui::tab_search;
use fastrender::ui::{BrowserTabState, TabId};

fn build_tabs(count: usize) -> Vec<BrowserTabState> {
  (0..count)
    .map(|i| {
      let id = TabId((i + 1) as u64);
      let mut tab = BrowserTabState::new(id, format!("https://example{i}.com/path/{i}?q=rust"));
      tab.title = Some(format!("Example {i} - Git repo {i}"));
      tab
    })
    .collect()
}

// Baseline matcher (pre-optimization) retained in the benchmark to quantify improvements.
fn ranked_matches_baseline(query: &str, tabs: &[BrowserTabState]) -> Vec<(TabId, usize, u8)> {
  let query = query.trim();
  if query.is_empty() {
    return tabs
      .iter()
      .enumerate()
      .map(|(idx, tab)| (tab.id, idx, 0))
      .collect();
  }

  let q = query.to_lowercase();
  let mut out = Vec::new();

  for (idx, tab) in tabs.iter().enumerate() {
    let title = tab
      .title
      .as_deref()
      .filter(|s| !s.trim().is_empty())
      .or_else(|| {
        tab
          .committed_title
          .as_deref()
          .filter(|s| !s.trim().is_empty())
      })
      .unwrap_or("");
    let url = tab
      .committed_url
      .as_deref()
      .or_else(|| tab.current_url.as_deref())
      .unwrap_or("");

    let mut best: Option<u8> = None;
    let title_lc = title.to_lowercase();
    let url_lc = url.to_lowercase();

    if let Some(pos) = title_lc.find(&q) {
      best = Some(if pos == 0 { 0 } else { 2 });
    }
    if let Some(pos) = url_lc.find(&q) {
      let score = if pos == 0 { 1 } else { 3 };
      best = Some(best.map_or(score, |existing| existing.min(score)));
    }

    if let Some(score) = best {
      out.push((tab.id, idx, score));
    }
  }

  out.sort_by_key(|m| m.2);
  out
}

fn bench_ranked_matches(c: &mut Criterion) {
  let tabs = build_tabs(500);
  let query = "git";

  c.bench_function("tab_search_ranked_matches/baseline", |b| {
    b.iter(|| {
      let matches = ranked_matches_baseline(black_box(query), black_box(&tabs));
      black_box(matches.len());
    })
  });

  c.bench_function("tab_search_ranked_matches/optimized", |b| {
    let mut out = Vec::new();
    b.iter(|| {
      tab_search::ranked_matches_into(black_box(query), black_box(&tabs), &mut out);
      black_box(out.len());
    })
  });
}

#[cfg(feature = "browser_ui")]
fn bench_render_visible_rows(c: &mut Criterion) {
  use egui::{vec2, Context, RawInput, Rect};

  let tabs = build_tabs(500);
  let matches = tab_search::ranked_matches("git", &tabs);

  let ctx = Context::default();
  let mut raw = RawInput::default();
  raw.screen_rect = Some(Rect::from_min_size(egui::Pos2::ZERO, vec2(800.0, 600.0)));
  // Keep deterministic (avoid OS time).
  raw.time = Some(0.0);
  raw.focused = true;

  c.bench_function("tab_search_render_rows/virtualized_visible", |b| {
    b.iter(|| {
      ctx.begin_frame(raw.clone());
      egui::CentralPanel::default().show(&ctx, |ui| {
        let row_height = ui.spacing().interact_size.y.max(28.0);
        egui::ScrollArea::vertical().max_height(360.0).show_rows(
          ui,
          row_height,
          matches.len(),
          |ui, row_range| {
            for idx in row_range {
              let m = matches[idx];
              let tab = &tabs[m.tab_index];
              let title = tab.display_title();
              let url = tab
                .committed_url
                .as_deref()
                .or_else(|| tab.current_url.as_deref())
                .unwrap_or_default();
              let secondary = tab_search::http_host(url).unwrap_or(url);

              ui.horizontal(|ui| {
                ui.label(black_box(title));
                ui.label(black_box(secondary));
              });
            }
          },
        );
      });
      let _ = ctx.end_frame();
    })
  });
}

#[cfg(not(feature = "browser_ui"))]
fn bench_render_visible_rows(_c: &mut Criterion) {}

criterion_group!(benches, bench_ranked_matches, bench_render_visible_rows);
criterion_main!(benches);
