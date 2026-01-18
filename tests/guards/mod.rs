//! Guard tests that enforce repository-wide invariants.
//!
//! These are typically "policy" or "post-migration" checks (workspace layout, dependency choices,
//! etc.) rather than behavioural renderer tests.

use std::ffi::OsStr;
use std::path::Path;

use walkdir::DirEntry;

mod browser_stub_feature_gate;
mod crates_directory_guard;
mod debug_info_semantics_guard;
mod docs_conformance_presence;
mod docs_presence;
mod no_quickjs_docs;
mod ecma_rs_workspace_exclude_guard;
mod fetch_and_render_exit_presence;
mod js_runtime_consolidation_guard;
mod pageset_js_failures_report_guard;
mod no_deprecated_test_harness_names;
mod no_extra_test_binaries;
mod no_path_env_mutation;
mod no_fastr_use_bundled_fonts_env_mutation;
mod no_rayon_num_threads_env_mutation;
mod no_process_global_env_mutation;
mod no_process_global_stage_listener_mutation;
mod no_process_global_runtime_toggles_override;
mod no_merge_markers;
mod no_merge_markers_script;
mod no_orphan_test_modules;
mod no_path_shims_in_tests;
mod no_production_panics;
#[cfg(target_os = "macos")]
mod macos_relaxed_sandbox_home_guard;
mod resource_net_helpers_guard;
mod scroll_unit_tests_live_in_src;
mod stage_listener_guard_tests;
mod style_regressions_presence;
mod test_cleanup_inventory_guard;
mod webidl_consolidation_guard;
mod webidl_stale_crates_paths_guard;
mod webidl_vm_js_workspace_guard;

/// Many guard tests scan `tests/**/*.rs` for forbidden patterns. The `tests/` tree also contains
/// large fixture directories (HTML/images/fonts) that can dominate runtime if we traverse them.
///
/// This helper centralizes the list of fixture subtrees that are known to contain no Rust source,
/// so `WalkDir`-based scans can skip descending into them.
fn should_skip_tests_entry(entry: &DirEntry, tests_root: &Path) -> bool {
  let path = entry.path();
  let rel = match path.strip_prefix(tests_root) {
    Ok(rel) => rel,
    Err(_) => return false,
  };
  let mut components = rel.components();
  let Some(std::path::Component::Normal(first)) = components.next() else {
    return false;
  };

  // Large non-Rust fixture trees.
  if first == OsStr::new("pages")
    || first == OsStr::new("fonts")
    || first == OsStr::new("fuzz_corpus")
    || first == OsStr::new("wpt_dom")
  {
    return true;
  }

  // Web Platform Tests: skip the massive HTML/image fixture subtrees but keep `tests/wpt/*.rs`.
  if first == OsStr::new("wpt") {
    if matches!(
      components.next(),
      Some(std::path::Component::Normal(seg))
        if seg == OsStr::new("tests") || seg == OsStr::new("expected")
    ) {
      return true;
    }
  }

  false
}
