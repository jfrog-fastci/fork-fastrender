use diagnostics::FileId;
use emit_js::{emit_top_level_diagnostic, EmitOptions};
use parse_js::{parse_with_options, Dialect, ParseOptions, SourceType};
use ts_erase::{erase_types_strict_native, erase_types_with_options, TsEraseOptions};

fn erase_to_minified_js(src: &str, dialect: Dialect, source_type: SourceType) -> String {
  let file = FileId(0);
  let mut ast = parse_with_options(
    src,
    ParseOptions {
      dialect,
      source_type,
    },
  )
  .expect("input should parse");

  erase_types_with_options(file, source_type, &mut ast, TsEraseOptions::default())
    .expect("TypeScript erasure should succeed");

  let output =
    emit_top_level_diagnostic(file, &ast, EmitOptions::minified()).expect("emission should succeed");

  parse_with_options(
    &output,
    ParseOptions {
      dialect: Dialect::Ecma,
      source_type,
    },
  )
  .expect("erased output should parse as strict ECMAScript");

  output
}

fn strict_erase_to_minified_js(src: &str, dialect: Dialect, source_type: SourceType) -> String {
  let file = FileId(0);
  let mut ast = parse_with_options(
    src,
    ParseOptions {
      dialect,
      source_type,
    },
  )
  .expect("input should parse");

  erase_types_strict_native(file, source_type, &mut ast).expect("strict-native erasure should succeed");

  let output =
    emit_top_level_diagnostic(file, &ast, EmitOptions::minified()).expect("emission should succeed");

  parse_with_options(
    &output,
    ParseOptions {
      dialect: Dialect::Ecma,
      source_type,
    },
  )
  .expect("erased output should parse as strict ECMAScript");

  output
}

#[test]
fn erases_ts_expression_wrappers() {
  let src = r#"
    const asserted = foo as number;
    const angle = <number>foo;
    const nonNull = foo!;
    const instantiated = wrap<string>(asserted);
    const satisfied = ({ a: 1 } satisfies { a: number }).a;
    function wrap<T>(v: T): T { return v; }
  "#;

  let output = erase_to_minified_js(src, Dialect::Ts, SourceType::Module);

  assert_eq!(
    output,
    "const asserted=foo;const angle=foo;const nonNull=foo;const instantiated=wrap(asserted);const satisfied={a:1}.a;function wrap(v){return v;}"
  );
}

#[test]
fn erases_type_only_imports_and_exports() {
  let src = r#"
    import type { Foo } from "mod";
    export type { Foo } from "mod";
    export type { Foo };
    export const x = 1;
  "#;

  let output = erase_to_minified_js(src, Dialect::Ts, SourceType::Module);
  assert_eq!(output, "export const x=1;");
}

#[test]
fn strict_native_mode_rejects_runtime_ts_constructs() {
  let cases = [
    ("enum E{A}", "enum"),
    ("namespace N{export const x=1}", "namespace"),
    ("module M{export const x=1}", "module"),
    ("import x=require('y');", "import="),
    ("export=1;", "export="),
  ];

  for (src, label) in cases {
    let file = FileId(0);
    let mut ast = parse_with_options(
      src,
      ParseOptions {
        dialect: Dialect::Ts,
        source_type: SourceType::Module,
      },
    )
    .unwrap_or_else(|err| panic!("input should parse for {label}: {err}"));

    let diagnostics = match erase_types_strict_native(file, SourceType::Module, &mut ast) {
      Ok(()) => panic!("expected {label} to be rejected in strict-native mode"),
      Err(diags) => diags,
    };

    assert!(
      diagnostics.iter().any(|diag| diag.code.as_str() == "MINIFYTS0001"),
      "expected MINIFYTS0001 diagnostic for {label}, got: {diagnostics:?}"
    );
  }
}

#[test]
fn strict_native_mode_rejects_decorators() {
  let cases = [
    ("@dec class C {}", "class decorator"),
    ("class C { @dec m() {} }", "member decorator"),
    ("class C { m(@dec x: any) {} }", "parameter decorator"),
    ("const C = (@dec class C {});", "class expression decorator"),
  ];

  for (src, label) in cases {
    let file = FileId(0);
    let mut ast = parse_with_options(
      src,
      ParseOptions {
        dialect: Dialect::Ts,
        source_type: SourceType::Module,
      },
    )
    .unwrap_or_else(|err| panic!("input should parse for {label}: {err}"));

    let diagnostics = match erase_types_strict_native(file, SourceType::Module, &mut ast) {
      Ok(()) => panic!("expected decorators to be rejected in strict-native mode for {label}"),
      Err(diags) => diags,
    };

    assert!(
      diagnostics.iter().any(|diag| diag.code.as_str() == "MINIFYTS0001"),
      "expected MINIFYTS0001 diagnostic for {label}, got: {diagnostics:?}"
    );

    // Even though strict-native erasure returns an error, it should still clear decorator syntax so
    // the partially-erased AST remains valid strict ECMAScript.
    let output =
      emit_top_level_diagnostic(file, &ast, EmitOptions::minified()).expect("emission should succeed");
    parse_with_options(
      &output,
      ParseOptions {
        dialect: Dialect::Ecma,
        source_type: SourceType::Module,
      },
    )
    .expect("decorator syntax should be erased from strict-native output");
  }
}

#[test]
fn strict_native_mode_rejects_jsx() {
  let file = FileId(0);
  let src = "const el = <div>{x}</div>;";
  let mut ast = parse_with_options(
    src,
    ParseOptions {
      dialect: Dialect::Tsx,
      source_type: SourceType::Module,
    },
  )
  .expect("input should parse as TSX");

  let diagnostics = match erase_types_strict_native(file, SourceType::Module, &mut ast) {
    Ok(()) => panic!("expected JSX to be rejected in strict-native mode"),
    Err(diags) => diags,
  };

  assert!(
    diagnostics.iter().any(|diag| diag.code.as_str() == "MINIFYTS0001"),
    "expected MINIFYTS0001 diagnostic for JSX, got: {diagnostics:?}"
  );

  // Even though strict-native erasure returns an error, it should still clear JSX syntax so the
  // partially-erased AST remains valid strict ECMAScript.
  let output =
    emit_top_level_diagnostic(file, &ast, EmitOptions::minified()).expect("emission should succeed");
  parse_with_options(
    &output,
    ParseOptions {
      dialect: Dialect::Ecma,
      source_type: SourceType::Module,
    },
  )
  .expect("JSX syntax should be erased from strict-native output");
}

#[test]
fn strict_native_mode_rejects_using_decls() {
  let cases = [
    ("using x = null;", "using declaration"),
    ("await using x = null;", "await using declaration"),
    ("for (using x of y) {}", "using for-of"),
    ("for (await using x of y) {}", "await using for-of"),
  ];

  for (src, label) in cases {
    let file = FileId(0);
    let mut ast = parse_with_options(
      src,
      ParseOptions {
        dialect: Dialect::Ts,
        source_type: SourceType::Module,
      },
    )
    .unwrap_or_else(|err| panic!("input should parse for {label}: {err}"));

    let diagnostics = match erase_types_strict_native(file, SourceType::Module, &mut ast) {
      Ok(()) => panic!("expected `using` to be rejected in strict-native mode for {label}"),
      Err(diags) => diags,
    };

    assert!(
      diagnostics.iter().any(|diag| diag.code.as_str() == "MINIFYTS0001"),
      "expected MINIFYTS0001 diagnostic for {label}, got: {diagnostics:?}"
    );

    // Even though strict-native erasure returns an error, it should still rewrite the unsupported
    // declaration into parseable strict ECMAScript so tooling can continue operating on the AST.
    let output =
      emit_top_level_diagnostic(file, &ast, EmitOptions::minified()).expect("emission should succeed");
    assert!(
      !output.contains("using"),
      "expected strict-native output to erase `using` keyword: {output}"
    );
    parse_with_options(
      &output,
      ParseOptions {
        dialect: Dialect::Ecma,
        source_type: SourceType::Module,
      },
    )
    .expect("`using` syntax should be erased from strict-native output");
  }
}

#[test]
fn strict_native_mode_erases_ambient_decls_and_this_params() {
  let src = r#"
    declare enum E { A }
    declare namespace N { export const x: number }
    export as namespace UMD;
    declare const z: number;
    declare let w: number;
    declare var v: number;
    export declare const exported: number;
    function f(this: any, x: number) { return x; }
    export const y = 1;
  "#;

  let output = strict_erase_to_minified_js(src, Dialect::Ts, SourceType::Module);
  assert_eq!(output, "function f(x){return x;}export const y=1;");
}

#[test]
fn full_mode_lowers_runtime_ts_constructs() {
  let src = r#"
    export enum E { A, B = 5 }
    export namespace N { export const x = E.A; }
    export const y = N.x;
  "#;

  let output = erase_to_minified_js(src, Dialect::Ts, SourceType::Module);

  // Lowered runtime enums include a reverse mapping, which contains member names
  // as string literals. (The source only contains identifier names `A`/`B`.)
  assert!(
    output.contains("\"A\"") || output.contains("'A'"),
    "expected lowered enum to include string literal \"A\", got: {output}"
  );
  assert!(
    output.contains("\"B\"") || output.contains("'B'"),
    "expected lowered enum to include string literal \"B\", got: {output}"
  );
}

#[test]
fn full_mode_lowers_import_equals_and_export_assignment() {
  let src = r#"
    import x = require("y");
    export = x;
  "#;

  let output = erase_to_minified_js(src, Dialect::Ts, SourceType::Module);
  assert!(
    output.contains("require(\"y\")") || output.contains("require('y')"),
    "expected lowered import= to include require call, got: {output}"
  );
  assert!(
    output.contains("module.exports"),
    "expected lowered export= to include module.exports assignment, got: {output}"
  );
}
