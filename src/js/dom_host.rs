use crate::dom2::Document;

/// Abstraction over a live `dom2::Document` that allows DOM mutation while keeping renderer cache
/// invalidation coalesced in the host.
///
/// JS bindings should **not** own the document directly (e.g. `Rc<RefCell<Document>>`), because
/// that would bypass host invalidation hooks and lead to stale renders.
///
/// Instead, DOM bindings should route all mutations through [`DomHost::mutate_dom`] and report
/// whether the DOM actually changed. The host can then invalidate style/layout/paint caches only
/// when needed.
pub trait DomHost {
  /// Borrow the live DOM immutably.
  fn with_dom<R, F>(&self, f: F) -> R
  where
    F: FnOnce(&Document) -> R;

  /// Mutate the live DOM and report whether anything changed.
  ///
  /// The closure returns `(result, changed)`.
  ///
  /// Hosts should only invalidate renderer caches when `changed == true`.
  fn mutate_dom<R, F>(&mut self, f: F) -> R
  where
    F: FnOnce(&mut Document) -> (R, bool);
}

