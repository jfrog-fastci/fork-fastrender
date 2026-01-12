use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .expect("xtask has a parent dir")
    .to_path_buf()
}

#[test]
fn legacy_bindings_contain_url_origin_getters() {
  let repo_root = repo_root();
  let legacy_bindings_path = repo_root.join("src/js/webidl/bindings/generated_legacy.rs");
  let legacy_bindings = fs::read_to_string(&legacy_bindings_path)
    .unwrap_or_else(|_| panic!("read {}", legacy_bindings_path.display()));

  let worker_start = legacy_bindings
    .find("pub mod worker")
    .expect("expected legacy bindings to include a worker module");

  let window_bindings = &legacy_bindings[..worker_start];
  let worker_bindings = &legacy_bindings[worker_start..];

  assert_eq!(
    window_bindings
      .matches("fn u_r_l_get_attribute_origin")
      .count(),
    1,
    "expected legacy window bindings to define exactly one u_r_l_get_attribute_origin wrapper"
  );
  assert_eq!(
    worker_bindings
      .matches("fn u_r_l_get_attribute_origin")
      .count(),
    1,
    "expected legacy worker bindings to define exactly one u_r_l_get_attribute_origin wrapper"
  );

  // Be resilient to rustfmt / codegen line-wrapping changes by ignoring whitespace.
  let window_no_whitespace: String = window_bindings
    .chars()
    .filter(|c| !c.is_whitespace())
    .collect();
  let worker_no_whitespace: String = worker_bindings
    .chars()
    .filter(|c| !c.is_whitespace())
    .collect();

  assert_eq!(
    window_no_whitespace
      .matches("u_r_l_get_attribute_origin::<Host,R>")
      .count(),
    1,
    "expected legacy window bindings to install URL.origin getter exactly once"
  );
  assert_eq!(
    worker_no_whitespace
      .matches("u_r_l_get_attribute_origin::<Host,R>")
      .count(),
    1,
    "expected legacy worker bindings to install URL.origin getter exactly once"
  );
}
