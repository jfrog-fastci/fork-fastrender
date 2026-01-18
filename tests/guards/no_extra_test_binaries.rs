//! Guard that enforces the top-level integration-test crate allowlist.
//!
//! Cargo treats each top-level `tests/*.rs` file as its own integration-test binary. While most test
//! coverage should live under the unified `tests/integration.rs` harness, we keep a small number of
//! focused crate roots for workflows that benefit from compiling/linking a narrower slice of the
//! suite.
//!
//! If you add or remove a `tests/*.rs` crate root, update this allowlist and
//! `progress/test_cleanup_inventory.md` so CI and agent workflows stay deterministic.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

fn repo_root() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn list_top_level_test_crates(root: &PathBuf) -> BTreeSet<String> {
  let tests_dir = root.join("tests");
  let mut crates = BTreeSet::new();

  let entries = fs::read_dir(&tests_dir)
    .unwrap_or_else(|err| panic!("failed to read tests dir {}: {err}", tests_dir.display()));
  for entry in entries {
    let entry = entry.expect("read tests dir entry");
    let path = entry.path();
    if !path.is_file() {
      continue;
    }
    if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
      continue;
    }
    let rel = path
      .strip_prefix(root)
      .unwrap_or(&path)
      .display()
      .to_string();
    crates.insert(rel);
  }

  crates
}

#[test]
fn no_extra_integration_test_binaries_exist() {
  let root = repo_root();
  let actual = list_top_level_test_crates(&root);
  let expected = BTreeSet::from([
    "tests/accesskit_dom2_node_ids.rs".to_string(),
    "tests/accesskit_scroll.rs".to_string(),
    "tests/accesskit_show_context_menu.rs".to_string(),
    "tests/allocation_failure.rs".to_string(),
    "tests/appearance_none_form_control_ua_stripping.rs".to_string(),
    "tests/audio_groups.rs".to_string(),
    "tests/audio_wav_backend.rs".to_string(),
    "tests/chrome_action_url.rs".to_string(),
    "tests/chrome_command_queue.rs".to_string(),
    "tests/chrome_frame_document_ime.rs".to_string(),
    "tests/chrome_frame_dom_mutation.rs".to_string(),
    "tests/chrome_frame_geometry.rs".to_string(),
    "tests/dump_a11y_include_bounds.rs".to_string(),
    "tests/global_downloads_merge.rs".to_string(),
    "tests/integration.rs".to_string(),
    "tests/ipc_framed_codec.rs".to_string(),
    "tests/media_aac_duration.rs".to_string(),
    "tests/media_opus.rs".to_string(),
    "tests/media_player.rs".to_string(),
    "tests/media_wake_scheduler.rs".to_string(),
    "tests/media_yuv.rs".to_string(),
    "tests/multiprocess_registry.rs".to_string(),
    "tests/network_process_smoke.rs".to_string(),
    "tests/networkless_fetcher.rs".to_string(),
    "tests/profile_downloads_persistence.rs".to_string(),
    "tests/public_media_module.rs".to_string(),
    "tests/range_detach_noop.rs".to_string(),
    "tests/renderer_sandbox_render_smoke.rs".to_string(),
    "tests/resource_fetch_destination.rs".to_string(),
    "tests/sandbox_diagnostics_smoke.rs".to_string(),
    "tests/sandbox_linux_prctl_dumpable.rs".to_string(),
    "tests/sandbox_linux_seccomp_fs_mutation.rs".to_string(),
    "tests/sandbox_smoke_render.rs".to_string(),
    "tests/site_isolation_sandbox_iframe.rs".to_string(),
    "tests/text_decoration_solid_snapping.rs".to_string(),
    "tests/webm_duration.rs".to_string(),
    "tests/websocket_ipc_framing.rs".to_string(),
    "tests/wpt_smoke.rs".to_string(),
  ]);

  assert!(
    actual == expected,
    "unexpected set of top-level integration test crates (tests/*.rs).\n\
     Expected: {expected:?}\n\
     Actual:   {actual:?}\n\
     \n\
     Keep the set of top-level `tests/*.rs` crates stable.\n\
     If you truly need to add/remove a crate root, update this guard and document it in \
     progress/test_cleanup_inventory.md."
   );
}
