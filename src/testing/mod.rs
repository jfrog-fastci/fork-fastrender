#![cfg(test)]

//! Shared helpers for unit-style tests that live in `src/`.
//!
//! These utilities exist so tests migrated from `tests/**` into `src/**` can keep using stable
//! fixture paths and common image comparison logic without depending on integration-test-only
//! modules.

#![allow(dead_code)]

use std::sync::{Mutex, MutexGuard, OnceLock};

mod golden;
mod net;
mod paths;
mod pixmap;
mod rayon;
mod stack;

pub(crate) use golden::{compare_config_from_env, compare_pngs, ArtifactPaths, CompareEnvVars};
pub(crate) use net::{net_test_lock, try_bind_localhost};
pub(crate) use paths::{
  fixture_path, fixtures_dir, manifest_dir, ref_fixtures_dir, repo_root, tests_dir,
};
pub(crate) use pixmap::{
  assert_pixmap_eq, compare_pixmaps, pixmap_from_rgba_image, pixmap_to_rgba_image,
};
pub(crate) use rayon::init_rayon_for_tests;
pub(crate) use stack::{run_with_large_stack, run_with_stack_size, LARGE_STACK_BYTES};

/// Serialises tests that mutate process-wide state.
///
/// This is the unit-test equivalent of `tests/common::global_test_lock()`. Prefer this over ad-hoc
/// `static Mutex` guards so "reset + assert" tests stay deterministic when the Rust harness runs in
/// parallel.
pub(crate) fn global_test_lock() -> MutexGuard<'static, ()> {
  static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
  LOCK
    .get_or_init(|| Mutex::new(()))
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Run `f` on a freshly spawned thread with a larger-than-default stack size.
///
/// Prefer [`run_with_stack_size`] for new call sites; this wrapper exists for compatibility with
/// older tests.
pub(crate) fn with_large_stack<R: Send + 'static>(
  stack_size: usize,
  f: impl FnOnce() -> R + Send + 'static,
) -> R {
  stack::run_with_stack_size(stack_size, f)
}

/// Assert that two floats are approximately equal within `eps`.
pub(crate) fn assert_approx_eq(actual: f32, expected: f32, eps: f32, context: &str) {
  let diff = (actual - expected).abs();
  if !(diff <= eps) {
    panic!("{context}: expected {expected} ± {eps}, got {actual} (diff: {diff})");
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
    init_rayon_for_tests(2);

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

  #[test]
  fn no_merge_conflict_markers_in_rust_sources() {
    use std::path::{Path, PathBuf};

    fn scan_dir(root: &Path, dir: &Path, hits: &mut Vec<(PathBuf, usize, String)>) {
      let entries = std::fs::read_dir(dir)
        .unwrap_or_else(|err| panic!("failed to read dir {}: {err}", dir.display()));
      for entry in entries {
        let entry = entry.unwrap_or_else(|err| {
          panic!("failed to read dir entry under {}: {err}", dir.display())
        });
        let path = entry.path();
        if path.is_dir() {
          scan_dir(root, &path, hits);
          continue;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("rs") {
          continue;
        }
        let text = std::fs::read_to_string(&path)
          .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
        for (idx, line) in text.lines().enumerate() {
          let line = line.trim_end();
          if line.starts_with("<<<<<<<")
            || line.starts_with(">>>>>>>")
            || line.starts_with("|||||||")
            || line == "======="
          {
            let rel = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
            hits.push((rel, idx + 1, line.to_string()));
            break;
          }
        }
      }
    }

    let root = repo_root();
    let src_dir = root.join("src");
    let mut hits: Vec<(PathBuf, usize, String)> = Vec::new();
    scan_dir(&root, &src_dir, &mut hits);

    if !hits.is_empty() {
      let mut msg = String::from("merge conflict markers found in Rust sources:\n");
      for (path, line_no, line) in hits {
        msg.push_str(&format!("  {}:{}: {}\n", path.display(), line_no, line));
      }
      panic!("{msg}");
    }
  }
}
