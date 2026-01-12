#![cfg(test)]

use std::sync::Once;

/// Initialize the Rayon global thread pool for tests.
///
/// This is intentionally:
/// - **Idempotent** (safe to call from many tests/modules).
/// - **Non-panicking** if another part of the process already initialized Rayon.
/// - **Pure** with respect to environment variables (does not set/modify them).
pub(crate) fn init_rayon_for_tests(num_threads: usize) {
  static INIT: Once = Once::new();
  let num_threads = num_threads.max(1);

  INIT.call_once(|| {
    // If the global pool was already initialized (e.g. by another test that raced, or by
    // production code invoked by a test), Rayon returns `GlobalPoolAlreadyInitialized`. Tests
    // should remain deterministic and not panic in that scenario.
    let _ = rayon::ThreadPoolBuilder::new()
      .num_threads(num_threads)
      .build_global();
  });
}

/// Run `f` on a freshly spawned thread with a larger-than-default stack size.
///
/// This is useful for layout/paint tests which can traverse deeply nested trees and overflow the
/// default stack in debug builds.
///
/// Panics inside `f` are propagated to the caller.
pub(crate) fn with_large_stack<R: Send + 'static>(
  stack_size: usize,
  f: impl FnOnce() -> R + Send + 'static,
) -> R {
  let handle = std::thread::Builder::new()
    .stack_size(stack_size)
    .spawn(f)
    .expect("failed to spawn test thread");

  match handle.join() {
    Ok(result) => result,
    Err(panic) => std::panic::resume_unwind(panic),
  }
}

/// Assert that two floats are approximately equal within `eps`.
pub(crate) fn assert_approx_eq(actual: f32, expected: f32, eps: f32, context: &str) {
  let diff = (actual - expected).abs();
  if !(diff <= eps) {
    panic!(
      "{context}: expected {expected} ± {eps}, got {actual} (diff: {diff})"
    );
  }
}

pub(crate) fn parse_dom(html: &str) -> crate::dom::DomNode {
  crate::dom::parse_html(html).unwrap_or_else(|err| {
    panic!("parse_dom failed: {err:?}\n\nHTML:\n{html}");
  })
}

pub(crate) fn parse_css(css: &str) -> crate::css::types::StyleSheet {
  crate::css::parser::parse_stylesheet(css).unwrap_or_else(|err| {
    panic!("parse_css failed: {err:?}\n\nCSS:\n{css}");
  })
}

pub(crate) struct StyledTreeFixture {
  pub(crate) dom: crate::dom::DomNode,
  pub(crate) stylesheet: crate::css::types::StyleSheet,
  pub(crate) styled: crate::style::cascade::StyledNode,
}

pub(crate) fn styled_tree(html: &str, css: &str, viewport: (f32, f32)) -> StyledTreeFixture {
  let dom = parse_dom(html);
  let stylesheet = parse_css(css);
  let media_ctx = crate::style::media::MediaContext::screen(viewport.0, viewport.1);
  let styled = crate::style::cascade::apply_styles_with_media(&dom, &stylesheet, &media_ctx);
  StyledTreeFixture {
    dom,
    stylesheet,
    styled,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn smoke_testing_utils_compile_and_work() {
    init_rayon_for_tests(1);

    let result = with_large_stack(2 * 1024 * 1024, || 123_u32);
    assert_eq!(result, 123);

    assert_approx_eq(1.0, 1.0 + 1e-4, 1e-3, "approx smoke");

    let tree = styled_tree(
      "<div id=a>Hello</div>",
      "#a { width: 10px; }",
      (800.0, 600.0),
    );
    // A minimal sanity check that we got a styled tree back.
    assert!(!tree.styled.children.is_empty());
    assert!(!tree.dom.children.is_empty());
    assert!(!tree.stylesheet.rules.is_empty());
  }

  #[test]
  fn with_large_stack_propagates_panics() {
    let result = std::panic::catch_unwind(|| {
      with_large_stack(2 * 1024 * 1024, || panic!("boom"));
    });
    assert!(result.is_err());
  }

  #[test]
  fn assert_approx_eq_panics_on_large_diff() {
    let result = std::panic::catch_unwind(|| {
      assert_approx_eq(1.0, 2.0, 1e-3, "diff smoke");
    });
    assert!(result.is_err());
  }
}
