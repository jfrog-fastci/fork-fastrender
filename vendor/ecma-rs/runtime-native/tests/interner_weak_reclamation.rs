use runtime_native::gc::weak::debug_global_weak_handles_table_sizes;
use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::{rt_gc_collect, rt_string_intern, rt_thread_deinit, rt_thread_init};

#[test]
fn weak_interned_strings_are_reclaimed_by_global_gc() {
  let _rt = TestRuntimeGuard::new();
  rt_thread_init(0);

  const N: usize = 2000;

  let (slots0, _free0) = debug_global_weak_handles_table_sizes();

  // Intern a batch of unique strings. We intentionally keep only the IDs: the interner stores only
  // weak references to the bytes, so they should become unreachable immediately.
  let mut ids = Vec::with_capacity(N);
  for i in 0..N {
    let s = format!("weak-intern-{i:05}");
    let id = rt_string_intern(s.as_ptr(), s.len());
    ids.push(id);
  }

  let (slots1, free1) = debug_global_weak_handles_table_sizes();
  assert!(
    slots1 >= slots0 + N,
    "expected weak-handle table to grow after interning (slots0={slots0}, slots1={slots1}, N={N})"
  );
  assert!(
    free1 <= slots1,
    "free-list length must not exceed slot length (free1={free1}, slots1={slots1})"
  );

  // Trigger a stop-the-world collection. The global GC should clear weak handles to dead interned
  // string objects, and the interner's weak cleanup should prune those entries.
  rt_gc_collect();

  for &id in ids.iter().take(128) {
    assert!(
      !runtime_native::test_util::interner_lookup_exists(id),
      "expected interned id {id:?} to be reclaimed after GC"
    );
  }

  let (slots2, _free2) = debug_global_weak_handles_table_sizes();
  assert_eq!(
    slots2, slots1,
    "weak-handle slots vector should not shrink during reclamation"
  );

  // Intern another batch of unique strings and ensure weak-handle slots are reused rather than
  // growing without bound.
  for i in 0..N {
    let s = format!("weak-intern-2-{i:05}");
    let _ = rt_string_intern(s.as_ptr(), s.len());
  }

  let (slots3, _free3) = debug_global_weak_handles_table_sizes();
  assert!(
    slots3 <= slots1 + 64,
    "expected weak-handle slots to be reused after GC (slots1={slots1}, slots3={slots3})"
  );

  rt_thread_deinit();
}

