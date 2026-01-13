use criterion::{black_box, criterion_group, criterion_main, Criterion};
use fastrender::ui::bookmarks::{BookmarkNode, BookmarkStore};

mod common;

fn contains_url_linear(store: &BookmarkStore, url: &str) -> bool {
  store
    .nodes
    .values()
    .any(|node| matches!(node, BookmarkNode::Bookmark(b) if b.url == url))
}

fn build_store(num_bookmarks: usize) -> (BookmarkStore, String, String) {
  let mut store = BookmarkStore::default();
  let mut last_url = String::new();

  for i in 0..num_bookmarks {
    let url = format!("https://example.com/page{i}/");
    if i + 1 == num_bookmarks {
      last_url = url.clone();
    }
    store
      .add(url, Some(format!("Page {i}")), None)
      .expect("add bookmark");
  }

  let miss_url = "https://example.com/definitely-missing/".to_string();
  (store, last_url, miss_url)
}

fn bench_bookmark_contains_url(c: &mut Criterion) {
  common::bench_print_config_once("bookmark_contains_url", &[]);

  // A few thousand bookmarks is enough to make linear scans expensive while keeping bench setup
  // time reasonable.
  let (store, hit_url, miss_url) = build_store(20_000);

  let mut group = c.benchmark_group("bookmark_contains_url");

  group.bench_function("indexed_hit", |b| {
    b.iter(|| black_box(store.contains_url(black_box(hit_url.as_str()))));
  });
  group.bench_function("indexed_miss", |b| {
    b.iter(|| black_box(store.contains_url(black_box(miss_url.as_str()))));
  });

  group.bench_function("linear_hit", |b| {
    b.iter(|| black_box(contains_url_linear(black_box(&store), black_box(hit_url.as_str()))));
  });
  group.bench_function("linear_miss", |b| {
    b.iter(|| black_box(contains_url_linear(black_box(&store), black_box(miss_url.as_str()))));
  });

  group.finish();
}

criterion_group!(
  name = benches;
  config = common::perf_criterion();
  targets = bench_bookmark_contains_url
);
criterion_main!(benches);

