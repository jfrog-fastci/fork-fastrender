use std::collections::BTreeMap;
use std::path::Path;

use xtask::webidl::resolve::ExposureTarget;
use xtask::webidl_bindings_codegen::{
  generate_bindings_module_from_idl_with_config, WebIdlBindingsBackend, WebIdlBindingsCodegenConfig,
  WebIdlBindingsGenerationMode,
};

#[test]
fn webidl_bindings_codegen_filters_by_exposure_target() {
  let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .expect("xtask has a parent dir");
  let rustfmt_config = repo_root.join(".rustfmt.toml");

  let idl = r#"
    [Exposed=Window]
    interface WindowOnly {
      undefined windowMethod();
    };

    [Exposed=Worker]
    interface WorkerOnly {
      undefined workerMethod();
    };

    interface UnknownExposure {
      undefined unknownMethod();
    };

    [Exposed=(Window,Worker)]
    interface BothExposed {
      undefined bothMethod();
      [Exposed=Window] undefined windowMember();
      [Exposed=Worker] undefined workerMember();
    };
  "#;

  let config = WebIdlBindingsCodegenConfig {
    mode: WebIdlBindingsGenerationMode::AllMembers,
    allow_interfaces: ["WindowOnly", "WorkerOnly", "UnknownExposure", "BothExposed"]
      .into_iter()
      .map(|s| s.to_string())
      .collect(),
    interface_allowlist: BTreeMap::new(),
    prototype_chains: true,
  };

  let window = generate_bindings_module_from_idl_with_config(
    idl,
    &rustfmt_config,
    ExposureTarget::Window,
    config.clone(),
    WebIdlBindingsBackend::Legacy,
  )
  .expect("generate window bindings");
  assert!(
    window.contains("proto_window_only"),
    "Window target should include the WindowOnly interface"
  );
  assert!(
    !window.contains("proto_worker_only"),
    "Window target should not include the WorkerOnly interface"
  );
  assert!(
    window.contains("proto_unknown_exposure"),
    "Window target should retain UnknownExposure definitions"
  );
  assert!(
    window.contains("proto_both_exposed"),
    "Window target should include BothExposed interface"
  );
  assert!(
    window.contains("both_exposed_window_member"),
    "Window target should include window-only members"
  );
  assert!(
    !window.contains("both_exposed_worker_member"),
    "Window target should not include worker-only members"
  );

  let worker = generate_bindings_module_from_idl_with_config(
    idl,
    &rustfmt_config,
    ExposureTarget::Worker,
    config,
    WebIdlBindingsBackend::Legacy,
  )
  .expect("generate worker bindings");
  assert!(
    !worker.contains("proto_window_only"),
    "Worker target should not include the WindowOnly interface"
  );
  assert!(
    worker.contains("proto_worker_only"),
    "Worker target should include the WorkerOnly interface"
  );
  assert!(
    worker.contains("proto_unknown_exposure"),
    "Worker target should retain UnknownExposure definitions"
  );
  assert!(
    worker.contains("proto_both_exposed"),
    "Worker target should include BothExposed interface"
  );
  assert!(
    worker.contains("both_exposed_worker_member"),
    "Worker target should include worker-only members"
  );
  assert!(
    !worker.contains("both_exposed_window_member"),
    "Worker target should not include window-only members"
  );
}
