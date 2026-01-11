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
  let src = "enum E{A}";

  let file = FileId(0);
  let mut ast = parse_with_options(
    src,
    ParseOptions {
      dialect: Dialect::Ts,
      source_type: SourceType::Module,
    },
  )
  .expect("input should parse");

  let diagnostics = erase_types_strict_native(file, SourceType::Module, &mut ast)
    .expect_err("enum should be rejected in strict native mode");

  assert!(
    diagnostics.iter().any(|diag| diag.code.as_str() == "MINIFYTS0001"),
    "expected MINIFYTS0001 diagnostic, got: {diagnostics:?}"
  );
}
