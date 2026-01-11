use std::path::PathBuf;

use xtask::webidl::resolve::ExposureTarget;
use xtask::webidl_bindings_codegen::{
  run_webidl_bindings_codegen, WebIdlBindingsBackend, WebIdlBindingsCodegenArgs,
};

#[test]
fn webidl_bindings_snapshots_are_up_to_date() {
  let mk_args = |backend: WebIdlBindingsBackend, out: &str| WebIdlBindingsCodegenArgs {
    backend,
    out: Some(PathBuf::from(out)),
    window_allowlist: PathBuf::from("tools/webidl/window_bindings_allowlist.toml"),
    dom_allowlist: PathBuf::from("tools/webidl/bindings_allowlist.toml"),
    dom_out: PathBuf::from("src/js/legacy/dom_generated.rs"),
    check: true,
    exposure_target: ExposureTarget::All,
    allow_interfaces: Vec::new(),
  };

  run_webidl_bindings_codegen(mk_args(
    WebIdlBindingsBackend::Vmjs,
    "src/js/webidl/bindings/generated/mod.rs",
  ))
  .expect("vmjs bindings snapshot should match codegen output");

  run_webidl_bindings_codegen(mk_args(
    WebIdlBindingsBackend::Legacy,
    "src/js/webidl/bindings/generated_legacy.rs",
  ))
  .expect("legacy bindings snapshot should match codegen output");
}
