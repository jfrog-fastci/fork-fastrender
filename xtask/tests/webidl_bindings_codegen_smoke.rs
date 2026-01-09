use std::fs;
use std::path::{Path, PathBuf};

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

  let dom_bindings_path = repo_root.join("src/js/bindings/dom_generated.rs");
  let dom_bindings = fs::read_to_string(&dom_bindings_path)
    .unwrap_or_else(|_| panic!("read {}", dom_bindings_path.display()));

  assert!(
    dom_bindings.contains("Document.createElement: expected at least 1 arguments"),
    "expected DOM scaffold to include Document.createElement argument checks"
  );
  assert!(
    dom_bindings.contains("Document.querySelector: expected at least 1 arguments"),
    "expected DOM scaffold to include Document.querySelector argument checks"
  );
}
