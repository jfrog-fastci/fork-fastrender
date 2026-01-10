use std::collections::BTreeMap;
use std::path::Path;

use xtask::webidl::resolve::ExposureTarget;
use xtask::webidl_bindings_codegen::{
  generate_bindings_module_from_idl_with_config, WebIdlBindingsBackend, WebIdlBindingsCodegenConfig,
  WebIdlBindingsGenerationMode,
};

#[test]
fn dictionary_conversion_emits_required_defaults_and_inheritance_order() {
  let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .expect("xtask has a parent dir");
  let rustfmt_config = repo_root.join(".rustfmt.toml");

  let idl = r#"
    [Exposed=Window]
    interface Foo {
      undefined doStuff(TestDict dict);
      undefined takeOrder(DerivedOrder order);
    };

    dictionary BaseOrder {
      boolean zeta;
      boolean alpha;
    };

    dictionary DerivedOrder : BaseOrder {
      boolean middle;
    };

    dictionary EmptyInner {
      boolean innerBool = true;
    };

    dictionary TestDict {
      required boolean requiredBool;
      boolean defaultBool = false;
      long defaultLong = 42;
      DOMString defaultString = "hello";
      EmptyInner defaultDict = {};
      sequence<DOMString> defaultSeq = [];
      DerivedOrder order;
    };
  "#;

  let config = WebIdlBindingsCodegenConfig {
    mode: WebIdlBindingsGenerationMode::AllMembers,
    allow_interfaces: ["Foo".to_string()].into_iter().collect(),
    interface_allowlist: BTreeMap::new(),
    prototype_chains: true,
  };

  let generated = generate_bindings_module_from_idl_with_config(
    idl,
    &rustfmt_config,
    ExposureTarget::Window,
    config,
    WebIdlBindingsBackend::Legacy,
  )
  .unwrap();

  // Dictionaries should be emitted into `type_context()` with inheritance and defaults.
  let base_idx = generated
    .find("name: \"BaseOrder\".to_string()")
    .expect("BaseOrder dictionary schema should be emitted");
  let derived_idx = generated
    .find("name: \"DerivedOrder\".to_string()")
    .expect("DerivedOrder dictionary schema should be emitted");
  assert!(
    base_idx < derived_idx,
    "expected BaseOrder dictionary schema to be emitted before DerivedOrder"
  );
  assert!(
    generated.contains("inherits: Some(\"BaseOrder\".to_string())"),
    "expected DerivedOrder to inherit BaseOrder"
  );

  // Required member + default value shapes.
  assert!(
    generated.contains("name: \"requiredBool\".to_string()")
      && generated.contains("required: true")
      && generated.contains("default: None"),
    "expected required dictionary member schema for TestDict.requiredBool"
  );

  assert!(
    generated.contains("name: \"defaultBool\".to_string()")
      && generated.contains("default: Some(DefaultValue::Boolean(false))"),
    "expected boolean default to be emitted in schema"
  );
  assert!(
    generated.contains("name: \"defaultLong\".to_string()")
      && generated.contains("DefaultValue::Number(NumericLiteral::Integer(")
      && generated.contains("\"42\".to_string()"),
    "expected numeric default to be emitted in schema"
  );
  assert!(
    generated.contains("name: \"defaultString\".to_string()")
      && generated.contains("DefaultValue::String(\"hello\".to_string())"),
    "expected string default to be emitted in schema"
  );
  assert!(
    generated.contains("name: \"defaultDict\".to_string()")
      && generated.contains("default: Some(DefaultValue::EmptyDictionary)"),
    "expected empty-dictionary default to be emitted in schema"
  );
  assert!(
    generated.contains("name: \"defaultSeq\".to_string()")
      && generated.contains("default: Some(DefaultValue::EmptySequence)"),
    "expected empty-sequence default to be emitted in schema"
  );
}
