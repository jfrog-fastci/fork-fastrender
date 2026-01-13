use crate::BrowserDocument;

/// Returns `true` when the document contains time-based primitives (CSS animations or transitions)
/// and should be periodically repainted with an advancing animation timestamp.
///
/// This is a shared helper used by both:
/// - the main browser render worker (to decide whether to schedule periodic ticks), and
/// - chrome/runtime integrations that render trusted HTML/CSS (to drive `animation_time_ms`).
pub(crate) fn browser_document_wants_ticks(doc: &BrowserDocument) -> bool {
  doc.prepared().is_some_and(|prepared| {
    let tree = prepared.fragment_tree();
    !tree.keyframes.is_empty() || tree.transition_state.is_some()
  })
}

