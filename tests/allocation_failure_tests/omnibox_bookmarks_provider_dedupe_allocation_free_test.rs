use super::{lock_allocator, start_counting, stop_counting};
use fastrender::ui::{BookmarkStore, BookmarksProvider, OmniboxContext, VisitedUrlStore};

#[test]
fn bookmarks_provider_url_dedupe_does_not_allocate_per_candidate_lowercase() {
  let _guard = lock_allocator();

  let open_tabs = Vec::new();
  let closed_tabs = Vec::new();
  let visited = VisitedUrlStore::new();
  let mut bookmarks = BookmarkStore::default();

  // Pick an odd size so counting doesn't accidentally match unrelated allocations.
  //
  // The provider must allocate an owned URL string for the output suggestion once. Previously,
  // provider-level dedupe also allocated a lowercased `String` *per candidate bookmark*, which
  // would show up as additional allocations of this exact size.
  let url_len = 251usize;
  let prefix = "http://example.com/";
  let pad_len = url_len - prefix.len();
  let url_lower = format!("{prefix}{}", "a".repeat(pad_len));
  let url_upper = url_lower.to_ascii_uppercase();

  bookmarks
    .add(url_upper, Some("Upper".to_string()), None)
    .unwrap();
  bookmarks
    .add(url_lower.clone(), Some("Lower".to_string()), None)
    .unwrap();

  let ctx = OmniboxContext {
    open_tabs: &open_tabs,
    closed_tabs: &closed_tabs,
    visited: &visited,
    active_tab_id: None,
    bookmarks: Some(&bookmarks),
    remote_search_suggest: None,
  };

  let provider = BookmarksProvider;

  start_counting(url_len, 1);
  let suggestions = provider.suggestions(&ctx, "example");
  let matches = stop_counting();

  assert_eq!(
    suggestions.len(),
    1,
    "expected provider-level URL dedupe to suppress duplicates, got {suggestions:?}"
  );
  assert_eq!(
    matches, 1,
    "expected exactly one allocation of {url_len} bytes (owned URL) in provider output; extra matches likely indicate per-candidate lowercasing allocations"
  );
}

