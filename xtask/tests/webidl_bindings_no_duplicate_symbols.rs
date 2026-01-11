use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use tempfile::TempDir;
use xtask::webidl::resolve::ExposureTarget;
use xtask::webidl_bindings_codegen::{
  run_webidl_bindings_codegen, WebIdlBindingsBackend, WebIdlBindingsCodegenArgs,
};

fn module_slice<'a>(src: &'a str, start_pat: &str, end_pat: Option<&str>) -> &'a str {
  let start = src
    .find(start_pat)
    .unwrap_or_else(|| panic!("missing module start marker `{start_pat}`"));
  let end = end_pat
    .and_then(|pat| src.find(pat))
    .unwrap_or_else(|| src.len());
  &src[start..end]
}

fn assert_no_duplicate_fn_defs(module_name: &str, src: &str) {
  let mut counts: BTreeMap<String, usize> = BTreeMap::new();

  for line in src.lines() {
    let trimmed = line.trim_start();
    let rest = trimmed
      .strip_prefix("pub fn ")
      .or_else(|| trimmed.strip_prefix("fn "));
    let Some(rest) = rest else {
      continue;
    };
    let name: String = rest
      .chars()
      .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
      .collect();
    if name.is_empty() {
      continue;
    }
    *counts.entry(name).or_insert(0) += 1;
  }

  let duplicates: BTreeSet<String> = counts
    .into_iter()
    .filter_map(|(name, count)| (count > 1).then(|| format!("{name} ({count}x)")))
    .collect();

  assert!(
    duplicates.is_empty(),
    "found duplicate `fn` definitions in `{module_name}` module: {duplicates:?}"
  );
}

#[test]
fn legacy_bindings_do_not_duplicate_url_origin_getter_wrapper() {
  let tmp = TempDir::new().expect("create temp dir");
  let out = tmp.path().join("generated_legacy.rs");
  let dom_out = tmp.path().join("dom_generated.rs");

  run_webidl_bindings_codegen(WebIdlBindingsCodegenArgs {
    backend: WebIdlBindingsBackend::Legacy,
    out: Some(out.clone()),
    window_allowlist: PathBuf::from("tools/webidl/window_bindings_allowlist.toml"),
    dom_allowlist: PathBuf::from("tools/webidl/bindings_allowlist.toml"),
    dom_out: dom_out.clone(),
    check: false,
    exposure_target: ExposureTarget::All,
    allow_interfaces: Vec::new(),
  })
  .expect("legacy codegen should succeed");

  let src = std::fs::read_to_string(&out).expect("read generated legacy bindings");

  let window_src = module_slice(&src, "pub mod window {", Some("pub mod worker {"));
  let worker_src = module_slice(&src, "pub mod worker {", None);

  assert_eq!(
    window_src.matches("fn u_r_l_get_attribute_origin<").count(),
    1,
    "expected exactly one URL.origin getter wrapper in window module"
  );
  assert_eq!(
    worker_src.matches("fn u_r_l_get_attribute_origin<").count(),
    1,
    "expected exactly one URL.origin getter wrapper in worker module"
  );

  assert_no_duplicate_fn_defs("window", window_src);
  assert_no_duplicate_fn_defs("worker", worker_src);
}

