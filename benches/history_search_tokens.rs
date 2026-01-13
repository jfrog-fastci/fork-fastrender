use criterion::{black_box, criterion_group, criterion_main, Criterion};

use fastrender::ui::global_history::{GlobalHistoryEntry, GlobalHistoryStore};

mod common;

fn build_store(num_entries: usize) -> GlobalHistoryStore {
  let mut store = GlobalHistoryStore::with_capacity(num_entries);

  for i in 0..num_entries {
    // Keep the URL/title casing mixed to exercise ASCII-only case folding.
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
    store.record(url, title);
  }

  store
}

#[inline]
fn contains_ascii_case_insensitive_baseline(haystack: &str, needle_lower_ascii: &str) -> bool {
  if needle_lower_ascii.is_empty() {
    return true;
  }
  let hay = haystack.as_bytes();
  let needle = needle_lower_ascii.as_bytes();
  if needle.len() > hay.len() {
    return false;
  }

  // Naive O(n*m) scan without `memchr` acceleration; retained in the benchmark to quantify
  // improvements from `ui::string_match`.
  for start in 0..=hay.len() - needle.len() {
    let mut matched = true;
    for (idx, &n) in needle.iter().enumerate() {
      let h = hay[start + idx];
      if h == n {
        continue;
      }
      // `n` is guaranteed to already be ASCII-lowercased; allow uppercase matches in `haystack`.
      if n.is_ascii_lowercase() && h == n - 32 {
        continue;
      }
      matched = false;
      break;
    }
    if matched {
      return true;
    }
  }

  false
}

fn search_baseline<'a>(
  store: &'a GlobalHistoryStore,
  query: &str,
  limit: usize,
) -> Vec<(usize, &'a GlobalHistoryEntry)> {
  if limit == 0 {
    return Vec::new();
  }

  let query_lower = query.to_ascii_lowercase();
  let tokens: Vec<&str> = query_lower
    .split_whitespace()
    .filter(|t| !t.is_empty())
    .collect();
  if tokens.is_empty() {
    return store.iter_recent().take(limit).collect();
  }

  let mut out = Vec::with_capacity(limit.min(store.entries.len()));
  'entries: for (idx, entry) in store.iter_recent() {
    for token in &tokens {
      let in_url = contains_ascii_case_insensitive_baseline(&entry.url, token);
      let in_title = entry
        .title
        .as_deref()
        .is_some_and(|t| contains_ascii_case_insensitive_baseline(t, token));
      if !in_url && !in_title {
        continue 'entries;
      }
    }

    out.push((idx, entry));
    if out.len() >= limit {
      break;
    }
  }

  out
}

fn bench_history_search_tokens(c: &mut Criterion) {
  common::bench_print_config_once("history_search_tokens", &[]);

  // Mirrors a "realistic enough" history size for the UI history panel.
  let store = build_store(10_000);

  // Ensure we scan the entire store (no matches) while still exercising multiple tokens and both
  // URL+title fields per token.
  let query = "EXAMPLE definitely-missing-token";
  let limit = 200;

  let mut group = c.benchmark_group("history_search_tokens");

  group.bench_function("baseline", |b| {
    b.iter(|| {
      let matches = search_baseline(black_box(&store), black_box(query), black_box(limit));
      black_box(matches.len());
    })
  });

  group.bench_function("optimized", |b| {
    b.iter(|| {
      let matches = store.search(black_box(query), black_box(limit));
      black_box(matches.len());
    })
  });

  group.finish();
}

criterion_group!(
  name = benches;
  config = common::perf_criterion();
  targets = bench_history_search_tokens
);
criterion_main!(benches);
