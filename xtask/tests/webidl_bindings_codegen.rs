use std::path::Path;

use xtask::webidl::generate::{rustfmt, FORBIDDEN_TOKENS};
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
      undefined qux(optional FooOptions options);
    };

    dictionary FooOptions {
      boolean capture;
    };
  "#;

  let config = WebIdlBindingsCodegenConfig {
    mode: WebIdlBindingsGenerationMode::AllMembers,
    allow_interfaces: ["Foo".to_string()].into_iter().collect(),
  };

  let out1 =
    generate_bindings_module_from_idl_with_config(idl, &rustfmt_config, config.clone()).unwrap();
  let out2 = generate_bindings_module_from_idl_with_config(idl, &rustfmt_config, config).unwrap();
  assert_eq!(out1, out2, "expected deterministic output across runs");

  assert_eq!(
    out1,
    EXPECTED,
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
