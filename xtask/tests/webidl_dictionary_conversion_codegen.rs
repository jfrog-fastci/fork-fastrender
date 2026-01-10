use std::collections::BTreeMap;
use std::path::Path;

use xtask::webidl::resolve::ExposureTarget;
use xtask::webidl_bindings_codegen::{
  generate_bindings_module_from_idl_with_config, WebIdlBindingsCodegenConfig,
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
  )
  .unwrap();

  // Required member missing error should mention dictionary + member.
  assert!(
    generated.contains("Missing required dictionary member TestDict.requiredBool"),
    "expected required-member TypeError message to mention dict+member"
  );

  // Default value evaluation for booleans/numbers/strings/empty dict/empty sequence.
  assert!(
    generated.contains("out_dict.insert(\"defaultBool\".to_string(), BindingValue::Bool(false));"),
    "expected boolean default to be emitted"
  );
  assert!(
    generated.contains("out_dict.insert(\"defaultLong\".to_string(), BindingValue::Number(42.0));"),
    "expected numeric default to be emitted"
  );
  assert!(
    generated.contains("BindingValue::String(\"hello\".to_string())"),
    "expected string default to be emitted"
  );
  assert!(
    generated.contains("out_dict.insert(\"defaultSeq\".to_string(), BindingValue::Sequence(Vec::new()));"),
    "expected empty-sequence default to be emitted"
  );
  assert!(
    generated.contains("map.insert(\"innerBool\".to_string(), BindingValue::Bool(true));"),
    "expected empty-dictionary default to be evaluated (including inner defaults)"
  );

  // Inherited dictionary member flattening order: base first, then derived (with lexicographic
  // ordering per dictionary).
  let start = generated
    .find("fn js_to_dict_derived_order")
    .expect("DerivedOrder dict converter should be generated");
  let slice = &generated[start..];
  let alpha = slice
    .find("rt.property_key(\"alpha\")")
    .expect("expected alpha member to be read");
  let zeta = slice
    .find("rt.property_key(\"zeta\")")
    .expect("expected zeta member to be read");
  let middle = slice
    .find("rt.property_key(\"middle\")")
    .expect("expected middle member to be read");
  assert!(
    alpha < zeta && zeta < middle,
    "expected flattened member order alpha -> zeta -> middle (base before derived)"
  );
}
