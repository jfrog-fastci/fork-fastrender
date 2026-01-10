use std::collections::BTreeMap;
use std::path::Path;

use xtask::webidl::resolve::ExposureTarget;
use xtask::webidl_bindings_codegen::{
  generate_bindings_module_from_idl_with_config, WebIdlBindingsBackend, WebIdlBindingsCodegenConfig,
  WebIdlBindingsGenerationMode,
};

const EXPECTED: &str = include_str!("goldens/webidl_bindings_codegen_attributes_and_constants_expected.rs");

#[test]
fn generated_webidl_bindings_include_attributes_and_constants() {
  let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .expect("xtask has a parent dir");
  let rustfmt_config = repo_root.join(".rustfmt.toml");

  let idl = r#"
    [Exposed=Window]
    interface Foo {
      readonly attribute unsigned long size;
      attribute DOMString href;
      static readonly attribute boolean ok;
      const unsigned short ANSWER = 42;
      undefined doIt();
    };
  "#;

  let config = WebIdlBindingsCodegenConfig {
    mode: WebIdlBindingsGenerationMode::AllMembers,
    allow_interfaces: ["Foo".to_string()].into_iter().collect(),
    interface_allowlist: BTreeMap::new(),
    prototype_chains: true,
  };

  let out = generate_bindings_module_from_idl_with_config(
    idl,
    &rustfmt_config,
    ExposureTarget::Window,
    config,
    WebIdlBindingsBackend::Legacy,
  )
  .unwrap();

  assert_eq!(out, EXPECTED, "expected generated output to match golden snapshot");

  assert!(
    out.contains("define_attribute_accessor(proto_foo, \"size\""),
    "expected instance attribute to define accessor property on prototype"
  );
  assert!(
    out.contains("define_constant(ctor_foo, \"ANSWER\""),
    "expected constant to define a non-enumerable data property on the constructor object"
  );
}
