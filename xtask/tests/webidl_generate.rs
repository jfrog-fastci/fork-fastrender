use std::path::Path;

use xtask::webidl::generate::{generate_rust_module_from_idl, rustfmt, FORBIDDEN_TOKENS};

#[test]
fn generated_bindings_are_panic_free_and_deterministic() {
  let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .expect("xtask has a parent dir");
  let rustfmt_config = repo_root.join(".rustfmt.toml");

  let idl = r#"
    [Exposed=Window]
    interface Foo {
      attribute long a;
    };

    partial interface Foo {
      attribute long b;
    };

    interface mixin FooMixin {
      attribute long c;
    };

    Foo includes FooMixin;

    dictionary FooDict {
      long x;
    };

    enum FooEnum { "a", "b" };

    typedef unsigned long FooULong;

    callback FooCallback = undefined();
  "#;

  let out1 = generate_rust_module_from_idl(idl, &rustfmt_config).expect("generate bindings");
  let out2 = generate_rust_module_from_idl(idl, &rustfmt_config).expect("generate bindings again");
  assert_eq!(out1, out2, "expected deterministic output across runs");

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
