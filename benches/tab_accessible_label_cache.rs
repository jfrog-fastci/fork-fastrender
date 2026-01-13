use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use criterion::{black_box, criterion_group, criterion_main, Criterion};

// Pull in the implementation directly so the bench stays lightweight (does not require enabling
// `browser_ui`) while still tracking real code changes.
#[path = "../src/ui/tab_accessible_label.rs"]
mod tab_accessible_label;

struct CountingAllocator;

static ALLOC_CALLS: AtomicUsize = AtomicUsize::new(0);
static ALLOC_BYTES: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
    ALLOC_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
    System.alloc(layout)
  }

  unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
    ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
    ALLOC_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
    System.alloc_zeroed(layout)
  }

  unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
    ALLOC_BYTES.fetch_add(new_size, Ordering::Relaxed);
    System.realloc(ptr, layout, new_size)
  }

  unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
    System.dealloc(ptr, layout)
  }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

fn allocation_counts() -> (usize, usize) {
  (
    ALLOC_CALLS.load(Ordering::Relaxed),
    ALLOC_BYTES.load(Ordering::Relaxed),
  )
}

fn allocation_delta<R, F: FnOnce() -> R>(f: F) -> (usize, usize, R) {
  let (start_calls, start_bytes) = allocation_counts();
  let value = f();
  let (end_calls, end_bytes) = allocation_counts();
  (
    end_calls.saturating_sub(start_calls),
    end_bytes.saturating_sub(start_bytes),
    value,
  )
}

fn build_titles(count: usize) -> Vec<String> {
  (0..count).map(|i| format!("Tab {i}")).collect()
}

fn tab_accessible_label_cache_benchmarks(c: &mut Criterion) {
  let tab_count = 256usize;
  let titles = build_titles(tab_count);
  let pinned_count = 12usize;

  // Baseline: rebuild all labels every frame (matches the pre-cache tab strip behaviour).
  c.bench_function("tab_a11y_label/rebuild_256_tabs", |b| {
    b.iter(|| {
      let (calls, bytes, sum) = allocation_delta(|| {
        let mut sum = 0usize;
        for (i, title) in titles.iter().enumerate() {
          let is_active = i == 0;
          let is_pinned = i < pinned_count;
          let loading = i % 50 == 0;
          let has_error = i % 97 == 0;
          let has_warning = i % 97 == 1;
          let label = tab_accessible_label::format_tab_accessible_label(
            title,
            is_active,
            is_pinned,
            loading,
            has_error,
            has_warning,
          );
          sum ^= label.len();
          black_box(label);
        }
        sum
      });
      black_box((calls, bytes, sum));
    });
  });

  // Cached: reuse label allocations when inputs are stable.
  c.bench_function("tab_a11y_label/cached_256_tabs_stable", |b| {
    let mut caches = vec![tab_accessible_label::TabAccessibleLabelCache::default(); tab_count];
    for (i, title) in titles.iter().enumerate() {
      let is_active = i == 0;
      let is_pinned = i < pinned_count;
      let loading = i % 50 == 0;
      let has_error = i % 97 == 0;
      let has_warning = i % 97 == 1;
      let _ = caches[i].get_or_update(title, is_active, is_pinned, loading, has_error, has_warning);
    }
    b.iter(|| {
      let (calls, bytes, sum) = allocation_delta(|| {
        let mut sum = 0usize;
        for (i, title) in titles.iter().enumerate() {
          let is_active = i == 0;
          let is_pinned = i < pinned_count;
          let loading = i % 50 == 0;
          let has_error = i % 97 == 0;
          let has_warning = i % 97 == 1;
          let label =
            caches[i].get_or_update(title, is_active, is_pinned, loading, has_error, has_warning);
          sum ^= label.len();
          black_box(label);
        }
        sum
      });
      black_box((calls, bytes, sum));
    });
  });

  // Cached: active tab changes each frame, forcing two cache updates per iteration.
  c.bench_function("tab_a11y_label/cached_256_tabs_active_switch", |b| {
    let mut caches = vec![tab_accessible_label::TabAccessibleLabelCache::default(); tab_count];
    let mut active = 0usize;
    for (i, title) in titles.iter().enumerate() {
      let is_active = i == active;
      let is_pinned = i < pinned_count;
      let loading = i % 50 == 0;
      let has_error = i % 97 == 0;
      let has_warning = i % 97 == 1;
      let _ = caches[i].get_or_update(title, is_active, is_pinned, loading, has_error, has_warning);
    }
    b.iter(|| {
      active = (active + 1) % tab_count;
      let (calls, bytes, sum) = allocation_delta(|| {
        let mut sum = 0usize;
        for (i, title) in titles.iter().enumerate() {
          let is_active = i == active;
          let is_pinned = i < pinned_count;
          let loading = i % 50 == 0;
          let has_error = i % 97 == 0;
          let has_warning = i % 97 == 1;
          let label =
            caches[i].get_or_update(title, is_active, is_pinned, loading, has_error, has_warning);
          sum ^= label.len();
          black_box(label);
        }
        sum
      });
      black_box((calls, bytes, sum));
    });
  });
}

criterion_group!(benches, tab_accessible_label_cache_benchmarks);
criterion_main!(benches);
