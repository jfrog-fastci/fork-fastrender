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
    "expected constant to be defined on the constructor object"
  );
  assert!(
    out.contains("define_constant(proto_foo, \"ANSWER\""),
    "expected constant to also be defined on the interface prototype object"
  );
}

#[test]
fn generated_vmjs_webidl_bindings_include_attributes_and_constants() {
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
    WebIdlBindingsBackend::Vmjs,
  )
  .unwrap();

  // `rustfmt` may choose to wrap long argument lists, so normalize whitespace before checking for
  // key codegen constructs.
  let compact: String = out.chars().filter(|c| !c.is_whitespace()).collect();

  assert!(
    compact.contains("define_accessor_property_str(proto_foo,\"size\""),
    "expected instance attribute to define accessor property on prototype"
  );
  assert!(
    compact.contains("define_data_property_str(ctor_foo,\"ANSWER\""),
    "expected constant to be defined on the constructor object"
  );
  assert!(
    compact.contains("define_data_property_str(proto_foo,\"ANSWER\""),
    "expected constant to be defined on the interface prototype object"
  );
}

#[test]
fn generated_vmjs_webidl_bindings_emits_symbol_iterator_for_iterables() {
  let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .expect("xtask has a parent dir");
  let rustfmt_config = repo_root.join(".rustfmt.toml");

  let idl = r#"
    [Exposed=Window]
    interface Foo {
      iterable<DOMString, DOMString>;
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
    WebIdlBindingsBackend::Vmjs,
  )
  .unwrap();

  let compact: String = out.chars().filter(|c| !c.is_whitespace()).collect();
  assert!(
    compact.contains("PropertyKey::from_symbol(realm.well_known_symbols().iterator)"),
    "expected vm-js bindings to define @@iterator for iterable interfaces"
  );
  assert!(
    compact.contains(
      "define_data_property(proto_foo,iterator_key,Value::Object(func),DataPropertyAttributes::METHOD"
    ),
    "expected @@iterator to alias the iterable's default iterator method"
  );
}

#[test]
fn generated_vmjs_webidl_bindings_emits_symbol_async_iterator_for_async_iterables() {
  let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .expect("xtask has a parent dir");
  let rustfmt_config = repo_root.join(".rustfmt.toml");

  let idl = r#"
    [Exposed=Window]
    interface Foo {
      async iterable<DOMString, DOMString>;
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
    WebIdlBindingsBackend::Vmjs,
  )
  .unwrap();

  let compact: String = out.chars().filter(|c| !c.is_whitespace()).collect();
  assert!(
    compact.contains("PropertyKey::from_symbol(realm.well_known_symbols().async_iterator)"),
    "expected vm-js bindings to define @@asyncIterator for async iterable interfaces"
  );
  assert!(
    compact.contains(
      "define_data_property(proto_foo,iterator_key,Value::Object(func),DataPropertyAttributes::METHOD"
    ),
    "expected @@asyncIterator to alias the iterable's default iterator method"
  );
}

#[test]
fn generated_vmjs_webidl_bindings_uses_min_required_arg_count_for_overload_length() {
  let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .expect("xtask has a parent dir");
  let rustfmt_config = repo_root.join(".rustfmt.toml");

  // Put the 1-arg overload first so codegen cannot rely on IDL ordering to compute `.length`.
  let idl = r#"
    [Exposed=Window]
    interface Foo {
      undefined bar(DOMString message);
      undefined bar();
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
    WebIdlBindingsBackend::Vmjs,
  )
  .unwrap();

  let compact: String = out.chars().filter(|c| !c.is_whitespace()).collect();
  assert!(
    compact.contains("alloc_native_function(foo_bar,None,\"bar\",0)?;"),
    "expected overload set to use min required argument count (0) for function length"
  );
}
