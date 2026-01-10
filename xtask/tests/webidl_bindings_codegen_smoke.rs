use std::fs;
use std::path::{Path, PathBuf};

use fastrender::webidl::generated::WORLD;

fn repo_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .expect("xtask has a parent dir")
    .to_path_buf()
}

#[test]
fn generated_bindings_snapshots_contain_known_members() {
  let repo_root = repo_root();

  let window_bindings_path = repo_root.join("src/js/webidl/bindings/generated/mod.rs");
  let window_bindings = fs::read_to_string(&window_bindings_path)
    .unwrap_or_else(|_| panic!("read {}", window_bindings_path.display()));

  // rustfmt (or codegen) may reflow argument lists across multiple lines, so make
  // these substring checks insensitive to whitespace.
  let window_bindings_no_whitespace: String = window_bindings
    .chars()
    .filter(|c| !c.is_whitespace())
    .collect();

  assert!(
    window_bindings_no_whitespace.contains("pubfninstall_window_bindings_vm_js"),
    "expected window bindings to export the vm-js install entrypoint"
  );
  assert!(
    window_bindings_no_whitespace.contains("rt.define_data_property_str(global,\"URL\","),
    "expected window bindings to install URL constructor"
  );
  assert!(
    window_bindings_no_whitespace.contains("rt.define_data_property_str(global,\"URLSearchParams\","),
    "expected window bindings to install URLSearchParams constructor"
  );
  assert!(
    window_bindings.contains("fn u_r_l_search_params_append"),
    "expected URLSearchParams.append wrapper to be present in generated window bindings"
  );
  assert!(
    WORLD
      .interface("URLSearchParams")
      .expect("generated WORLD should include URLSearchParams")
      .members
      .iter()
      .any(|m| m.name == Some("sort")),
    "expected committed WORLD to contain URLSearchParams.sort (guard for allowlist test)"
  );
  assert!(
    !window_bindings.contains("u_r_l_search_params_sort"),
    "expected URLSearchParams.sort to be excluded by tools/webidl/window_bindings_allowlist.toml"
  );

  let worker_start = window_bindings
    .find("pub mod worker")
    .expect("expected generated bindings to include a worker module");
  let worker_bindings = &window_bindings[worker_start..];
  let worker_bindings_no_whitespace: String = worker_bindings
    .chars()
    .filter(|c| !c.is_whitespace())
    .collect();
  assert!(
    worker_bindings_no_whitespace.contains("pubfninstall_worker_bindings_vm_js"),
    "expected worker bindings to export the vm-js install entrypoint"
  );
}
