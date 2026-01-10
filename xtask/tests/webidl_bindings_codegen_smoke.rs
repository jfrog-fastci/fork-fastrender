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

  let window_bindings_path = repo_root.join("src/js/bindings/generated/mod.rs");
  let window_bindings = fs::read_to_string(&window_bindings_path)
    .unwrap_or_else(|_| panic!("read {}", window_bindings_path.display()));

  assert!(
    window_bindings.contains("rt.define_data_property_str(global, \"URL\","),
    "expected window bindings to install URL constructor"
  );
  assert!(
    window_bindings.contains("rt.define_data_property_str(global, \"URLSearchParams\","),
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
}
