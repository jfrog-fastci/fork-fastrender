use std::collections::BTreeMap;
use std::path::Path;

use xtask::webidl::generate::{rustfmt, FORBIDDEN_TOKENS};
use xtask::webidl::resolve::ExposureTarget;
use xtask::webidl_bindings_codegen::{
  generate_bindings_module_from_idl_with_config, WebIdlBindingsBackend,
  WebIdlBindingsCodegenConfig, WebIdlBindingsGenerationMode,
};

const EXPECTED_LEGACY: &str = include_str!("goldens/webidl_bindings_codegen_expected.rs");
const EXPECTED_VMJS: &str = include_str!("goldens/webidl_bindings_codegen_expected_vmjs.rs");

fn assert_backend_matches_golden(backend: WebIdlBindingsBackend, expected: &str) {
  let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .expect("xtask has a parent dir");
  let rustfmt_config = repo_root.join(".rustfmt.toml");

  let idl = r#"
    [Exposed=Window]
    interface Foo {
      constructor();
      undefined baz(DOMString s);
      undefined baz(long x);
      undefined doThing(DOMString name, optional (FooOptions or boolean) options = {});
      undefined doThing(DOMString name, sequence<DOMString> items);
      undefined doThing(DOMString name, DOMString item);
      undefined qux(optional FooOptions options);
      undefined takesSequence([Clamp] sequence<long> values);
      undefined takesFrozenArray([EnforceRange] FrozenArray<long> values);
      iterable<DOMString, DOMString>;
    };

    [Exposed=Window]
    interface Bar {
      constructor();
      iterable<DOMString>;
    };

    dictionary FooOptions {
      boolean capture;
    };
  "#;

  let config = WebIdlBindingsCodegenConfig {
    mode: WebIdlBindingsGenerationMode::AllMembers,
    allow_interfaces: ["Foo".to_string(), "Bar".to_string()].into_iter().collect(),
    interface_allowlist: BTreeMap::new(),
    prototype_chains: true,
  };

  let out1 = generate_bindings_module_from_idl_with_config(
    idl,
    &rustfmt_config,
    ExposureTarget::Window,
    config.clone(),
    backend,
  )
  .unwrap();
  let out2 = generate_bindings_module_from_idl_with_config(
    idl,
    &rustfmt_config,
    ExposureTarget::Window,
    config,
    backend,
  )
  .unwrap();
  assert_eq!(out1, out2, "expected deterministic output across runs");

  // Spot-check that the legacy backend keeps using the shared WebIDL overload resolution +
  // conversion algorithms.
  if backend == WebIdlBindingsBackend::Legacy {
    assert!(
      out1.contains("resolve_overload"),
      "expected legacy bindings to call shared overload resolution"
    );
    assert!(
      out1.contains("convert_arguments"),
      "expected legacy bindings to call shared WebIDL conversions"
    );
  }

  if backend == WebIdlBindingsBackend::Vmjs {
    assert!(
      out1.contains("install_foo_bindings_vm_js"),
      "expected vm-js backend to emit per-interface installer functions"
    );
    // vm-js bindings intentionally avoid the legacy runtime's conversion/overload machinery; they
    // call vm-js specific helpers instead.
    assert!(
      !out1.contains("resolve_overload"),
      "expected vm-js bindings to not depend on legacy overload resolution helpers"
    );
    assert!(
      !out1.contains("convert_arguments"),
      "expected vm-js bindings to not depend on legacy conversion helpers"
    );
  }

  assert_eq!(out1, expected, "expected generated output to match golden");

  // Ensure rustfmt is stable (what CI's `cargo fmt -- --check` effectively enforces).
  let formatted_again = rustfmt(&out1, &rustfmt_config).expect("rustfmt generated output");
  assert_eq!(
    out1, formatted_again,
    "expected generated output to be rustfmt-idempotent"
  );

  for token in FORBIDDEN_TOKENS {
    assert!(
      !out1.contains(token),
      "generated output unexpectedly contains forbidden token: {token}"
    );
  }
}

#[test]
fn generated_webidl_bindings_vmjs_are_deterministic_and_match_golden() {
  assert_backend_matches_golden(WebIdlBindingsBackend::Vmjs, EXPECTED_VMJS);
}

#[test]
fn generated_webidl_bindings_legacy_still_match_golden() {
  assert_backend_matches_golden(WebIdlBindingsBackend::Legacy, EXPECTED_LEGACY);
}

#[test]
fn vmjs_overload_dispatch_does_not_emit_usize_len_ge_zero_checks() {
  let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .expect("xtask has a parent dir");
  let rustfmt_config = repo_root.join(".rustfmt.toml");

  // Exercise a 0-arg overload alongside a 1-arg overload (similar to `Window.alert`).
  let idl = r#"
    [Exposed=Window]
    interface Foo {
      undefined bar();
      undefined bar(DOMString s);
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

  assert!(
    !out.contains("len() >= 0"),
    "vm-js backend should not emit useless `args.len() >= 0` checks (usize is always >= 0)"
  );
  assert!(
    out.contains("args.is_empty()") || out.contains("args.len() == 0"),
    "expected 0-arg overload dispatch to use an `args is empty` check"
  );
}

#[test]
fn legacy_overload_dispatch_generates_single_wrapper_for_optional_args() {
  let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .expect("xtask has a parent dir");
  let rustfmt_config = repo_root.join(".rustfmt.toml");

  // Model `Window.alert(optional DOMString message = "")` which previously regressed in the legacy
  // bindings (duplicate wrapper functions with the same Rust symbol name).
  let idl = r#"
    [Exposed=Window]
    interface Window {
      undefined alert(optional DOMString message = "");
    };
  "#;

  let config = WebIdlBindingsCodegenConfig {
    mode: WebIdlBindingsGenerationMode::AllMembers,
    allow_interfaces: ["Window".to_string()].into_iter().collect(),
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

  assert_eq!(
    out.matches("fn window_alert").count(),
    1,
    "expected exactly one legacy wrapper fn for Window.alert"
  );
}

#[test]
fn legacy_attribute_wrappers_do_not_duplicate_rust_symbols() {
  let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .expect("xtask has a parent dir");
  let rustfmt_config = repo_root.join(".rustfmt.toml");

  // Regression test: the legacy bindings snapshot briefly emitted duplicate attribute wrappers for
  // `URL.origin` (same Rust fn name emitted twice), which broke downstream compilation.
  let idl = r#"
    [Exposed=Window]
    interface URL {
      stringifier attribute USVString href;
      readonly attribute USVString origin;
    };
  "#;

  let config = WebIdlBindingsCodegenConfig {
    mode: WebIdlBindingsGenerationMode::AllMembers,
    allow_interfaces: ["URL".to_string()].into_iter().collect(),
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

  assert_eq!(
    out.matches("fn u_r_l_get_attribute_origin").count(),
    1,
    "expected exactly one legacy wrapper fn for URL.origin"
  );
}

#[test]
fn legacy_converted_value_to_binding_value_preserves_union_and_record_wrappers() {
  let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .expect("xtask has a parent dir");
  let rustfmt_config = repo_root.join(".rustfmt.toml");

  let idl = r#"
    [Exposed=Window]
    interface Foo {
      undefined bar(optional (DOMString or long) value);
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

  let record_start = out
    .find("ConvertedValue::Record")
    .expect("expected legacy bindings to match ConvertedValue::Record");
  let dictionary_start = out[record_start..]
    .find("ConvertedValue::Dictionary")
    .map(|idx| record_start + idx)
    .expect("expected ConvertedValue::Dictionary to appear after ConvertedValue::Record");
  let record_arm = &out[record_start..dictionary_start];

  assert!(
    record_arm.contains("BindingValue::Record("),
    "expected ConvertedValue::Record arm to construct BindingValue::Record"
  );
  assert!(
    !record_arm.contains("BindingValue::Dictionary("),
    "ConvertedValue::Record must not be coerced into BindingValue::Dictionary"
  );
  assert!(
    !record_arm.contains("BTreeMap"),
    "ConvertedValue::Record arm must not allocate a BTreeMap (records preserve ordering)"
  );

  let union_start = out
    .find("ConvertedValue::Union")
    .expect("expected legacy bindings to match ConvertedValue::Union");
  let union_arm = &out[union_start..];

  assert!(
    union_arm.contains("let member_type = member_ty.to_string();"),
    "expected union conversion to preserve the selected member type"
  );
  assert!(
    union_arm.contains("BindingValue::Union"),
    "expected union conversion to wrap values in BindingValue::Union"
  );
  assert!(
    union_arm.contains("value: Box::new(value)"),
    "expected union conversion to box the inner value"
  );
  assert!(
    !union_arm.contains("return converted_value_to_binding_value"),
    "union conversion must not discard the union wrapper by returning the inner value"
  );
}

#[test]
fn generated_dictionary_converters_handle_required_defaults_and_inheritance() {
  let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .expect("xtask has a parent dir");
  let rustfmt_config = repo_root.join(".rustfmt.toml");

  let idl = r#"
    [Exposed=Window]
    interface EventTarget {
      undefined addEventListener(
        DOMString type,
        object listener,
        optional (AddEventListenerOptions or boolean) options = {}
      );
    };

    dictionary EventListenerOptions {
      boolean capture = false;
    };

    dictionary AddEventListenerOptions : EventListenerOptions {
      boolean passive;
      boolean once = false;
      object signal;
    };

    dictionary RequiredDict {
      required DOMString x;
    };

    [Exposed=Window]
    interface Foo {
      undefined takesRequired(RequiredDict dict);
    };
  "#;

  let config = WebIdlBindingsCodegenConfig {
    mode: WebIdlBindingsGenerationMode::AllMembers,
    allow_interfaces: ["EventTarget".to_string(), "Foo".to_string()]
      .into_iter()
      .collect(),
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

  // WebIDL dictionaries treat `undefined`/`null` as "missing dictionary object" and apply defaults /
  // required-member checks, rather than throwing solely due to the dictionary value being missing.
  assert!(
    !out.contains("allow_missing: bool"),
    "dictionary converters should not have an allow_missing flag; missing dictionaries are handled per WebIDL"
  );

  // Schema should include dictionaries + defaults (conversion happens via shared WebIDL algorithms).
  assert!(
    out.contains("name: \"EventListenerOptions\".to_string()"),
    "expected EventListenerOptions dictionary schema to be emitted into type_context()"
  );
  assert!(
    out.contains("name: \"capture\".to_string()")
      && out.contains("default: Some(DefaultValue::Boolean(false))"),
    "expected EventListenerOptions.capture boolean default to be emitted in schema"
  );

  assert!(
    out.contains("name: \"AddEventListenerOptions\".to_string()"),
    "expected AddEventListenerOptions dictionary schema to be emitted into type_context()"
  );
  assert!(
    out.contains("inherits: Some(\"EventListenerOptions\".to_string())"),
    "expected AddEventListenerOptions to inherit EventListenerOptions"
  );
  assert!(
    out.contains("name: \"once\".to_string()")
      && out.contains("default: Some(DefaultValue::Boolean(false))"),
    "expected AddEventListenerOptions.once boolean default to be emitted in schema"
  );

  // Optional argument default `{}` should be represented as an EmptyDictionary default value.
  assert!(
    out.contains("default: Some(DefaultValue::EmptyDictionary)"),
    "expected optional parameter default `{{}}` to be emitted as DefaultValue::EmptyDictionary"
  );

  // Required member should be encoded into the schema.
  assert!(
    out.contains("name: \"RequiredDict\".to_string()"),
    "expected RequiredDict dictionary schema to be emitted"
  );
  assert!(
    out.contains("name: \"x\".to_string()")
      && out.contains("required: true")
      && out.contains("default: None"),
    "expected RequiredDict.x to be emitted as a required dictionary member schema"
  );
}
