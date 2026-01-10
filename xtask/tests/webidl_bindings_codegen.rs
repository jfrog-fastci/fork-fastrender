use std::collections::BTreeMap;
use std::path::Path;

use xtask::webidl::generate::{rustfmt, FORBIDDEN_TOKENS};
use xtask::webidl::resolve::ExposureTarget;
use xtask::webidl_bindings_codegen::{
  generate_bindings_module_from_idl_with_config, WebIdlBindingsCodegenConfig,
  WebIdlBindingsGenerationMode,
};

const EXPECTED: &str = include_str!("goldens/webidl_bindings_codegen_expected.rs");

#[test]
fn generated_webidl_bindings_are_deterministic_and_match_golden() {
  let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .expect("xtask has a parent dir");
  let rustfmt_config = repo_root.join(".rustfmt.toml");

  let idl = r#"
    [Exposed=Window]
    interface Foo {
      undefined baz(DOMString s);
      undefined baz(long x);
      undefined doThing(DOMString name, optional (FooOptions or boolean) options = {});
      undefined doThing(DOMString name, sequence<DOMString> items);
      undefined doThing(DOMString name, DOMString item);
      undefined qux(optional FooOptions options);
      undefined takesSequence([Clamp] sequence<long> values);
      undefined takesFrozenArray([EnforceRange] FrozenArray<long> values);
    };

    dictionary FooOptions {
      boolean capture;
    };
  "#;

  let config = WebIdlBindingsCodegenConfig {
    mode: WebIdlBindingsGenerationMode::AllMembers,
    allow_interfaces: ["Foo".to_string()].into_iter().collect(),
    interface_allowlist: BTreeMap::new(),
    prototype_chains: true,
  };

  let out1 =
    generate_bindings_module_from_idl_with_config(idl, &rustfmt_config, ExposureTarget::Window, config.clone()).unwrap();
  let out2 =
    generate_bindings_module_from_idl_with_config(idl, &rustfmt_config, ExposureTarget::Window, config).unwrap();
  assert_eq!(out1, out2, "expected deterministic output across runs");

  // Spot-check that overload resolution is driven by an argument-count dispatch plan (not the old
  // `args.len() >= ... && predicate(...)` heuristic).
  assert!(
    out1.contains("match argcount"),
    "expected overload dispatch to group by argument count"
  );
  assert!(
    out1.contains("rt.is_number("),
    "expected overload dispatch to use a runtime type predicate"
  );
  assert!(
    !out1.contains("args.len() >= 1 && args.len() <= 1"),
    "expected old overload-dispatch heuristic to be absent"
  );

  assert_eq!(
    out1, EXPECTED,
    "expected generated output to match the committed golden snapshot"
  );

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
  )
  .unwrap();

  // Default-handling for EventListenerOptions.capture.
  assert!(
    out.contains("out_dict.insert(\"capture\".to_string(), BindingValue::Bool(false))"),
    "expected EventListenerOptions.capture default to be materialized"
  );

  // Inheritance flattening and deterministic member order:
  // capture (base) then once/passive/signal (derived, lexicographical within dictionary).
  let capture_pos = out
    .find("rt.property_key(\"capture\")")
    .expect("capture property access");
  let once_pos = out.find("rt.property_key(\"once\")").expect("once access");
  let passive_pos = out
    .find("rt.property_key(\"passive\")")
    .expect("passive property access");
  let signal_pos = out
    .find("rt.property_key(\"signal\")")
    .expect("signal access");
  assert!(
    capture_pos < once_pos && once_pos < passive_pos && passive_pos < signal_pos,
    "expected deterministic member read order capture -> once -> passive -> signal"
  );

  // Required-member errors should include the dictionary and member name.
  assert!(
    out.contains("Missing required dictionary member RequiredDict.x"),
    "expected required member error message to include dictionary + member name"
  );
}
