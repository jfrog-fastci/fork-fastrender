use criterion::{black_box, criterion_group, criterion_main, Criterion};

use fastrender::ui::{
  BookmarkStore, BookmarksProvider, OmniboxContext, OmniboxProvider, VisitedUrlStore,
};

mod common;

fn build_store(num_bookmarks: usize) -> BookmarkStore {
  let mut store = BookmarkStore::default();

  for i in 0..num_bookmarks {
    // Keep the URL/title casing mixed to exercise ASCII-only case folding in both haystack and
    // needle.
    let url = if i % 250 == 0 {
      format!("https://example.com/RuSt/{i}/")
    } else {
      format!("https://example.com/page{i}/")
    };
    let title = if i % 250 == 0 {
      Some(format!("Systems PROGRAMMING guide {i}"))
    } else {
      Some(format!("Page {i}"))
    };

    store.add(url, title, None).expect("add bookmark");
  }

  store
}

fn bench_omnibox_bookmarks(c: &mut Criterion) {
  common::bench_print_config_once("omnibox_bookmarks", &[]);

  // Large store so we always have plenty of candidates to scan.
  let bookmarks = build_store(20_000);
  let visited = VisitedUrlStore::new();
  let open_tabs = Vec::new();
  let closed_tabs = Vec::new();
  let ctx = OmniboxContext {
    open_tabs: &open_tabs,
    closed_tabs: &closed_tabs,
    visited: &visited,
    active_tab_id: None,
    bookmarks: Some(&bookmarks),
    remote_search_suggest: None,
  };
  let provider = BookmarksProvider;

  // Worst-case for `BookmarkStore::search`: scan up to the omnibox's scan limit but return no
  // matches, exercising substring checks on both URL and title for multiple tokens.
  let query_no_match = "EXAMPLE definitely-missing-token";

  // Best-case for matches: everything matches, exercising provider-level de-dupe and suggestion
  // allocation for up to the scan limit.
  let query_many_matches = "EXAMPLE";

  const BOOKMARK_SCAN_LIMIT: usize = 500;

  let mut group = c.benchmark_group("omnibox_bookmarks");

  group.bench_function("search_no_match", |b| {
    b.iter(|| {
      let matches = bookmarks.search(black_box(query_no_match), black_box(BOOKMARK_SCAN_LIMIT));
      black_box(matches.len());
    })
  });

  group.bench_function("search_many_matches", |b| {
    b.iter(|| {
      let matches = bookmarks.search(
        black_box(query_many_matches),
        black_box(BOOKMARK_SCAN_LIMIT),
      );
      black_box(matches.len());
    })
  });

  group.bench_function("provider_no_match", |b| {
    b.iter(|| {
      let suggestions = provider.suggestions(black_box(&ctx), black_box(query_no_match));
      black_box(suggestions.len());
    })
  });

  group.bench_function("provider_many_matches", |b| {
    b.iter(|| {
      let suggestions = provider.suggestions(black_box(&ctx), black_box(query_many_matches));
      black_box(suggestions.len());
    })
  });

  group.finish();
}

criterion_group!(
  name = benches;
  config = common::perf_criterion();
  targets = bench_omnibox_bookmarks
);
criterion_main!(benches);
