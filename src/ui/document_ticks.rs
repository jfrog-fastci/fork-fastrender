use crate::BrowserDocument;

/// Returns `true` when the document contains time-based primitives (CSS animations or transitions)
/// and should be periodically repainted with an advancing animation timestamp.
///
/// This helper is shared by both the main browser render worker and the in-process chrome runtime so
/// they agree on when to drive `UiToWorker::Tick`-style updates.
pub fn browser_document_wants_ticks(doc: &BrowserDocument) -> bool {
  crate::document_ticks::browser_document_wants_ticks(doc)
}
