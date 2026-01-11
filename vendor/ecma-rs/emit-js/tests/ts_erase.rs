use diagnostics::FileId;
use emit_js::{emit_top_level_diagnostic, EmitOptions};
use parse_js::{parse_with_options, Dialect, ParseOptions, SourceType};
use ts_erase::erase_types_strict_native;

fn parse_ts(src: &str) -> parse_js::ast::node::Node<parse_js::ast::stx::TopLevel> {
  parse_with_options(
    src,
    ParseOptions {
      dialect: Dialect::Ts,
      source_type: SourceType::Module,
    },
  )
  .expect("parse TS")
}

fn assert_parses_as_ecma(src: &str) {
  parse_with_options(
    src,
    ParseOptions {
      dialect: Dialect::Ecma,
      source_type: SourceType::Module,
    },
  )
  .expect("parse Ecma");
}

#[test]
fn erases_ts_wrappers_and_drops_type_only_stmts() {
  let source = r#"
interface Foo { x: string }
type Bar = number;
import type { Foo as Foo2 } from "foo";
export type { Bar };

// Expression wrappers.
(x as any).y;
x!.y;
f<T>(x);
(x satisfies any).y;
((a + b) as any).c;

// TS `this` parameters are type-only and must be erased.
function f(this: any, x: number) { return this; }
"#;

  let mut parsed = parse_ts(source);
  let file = FileId(0);
  erase_types_strict_native(file, SourceType::Module, &mut parsed).expect("erase TS types");
  let out =
    emit_top_level_diagnostic(file, &parsed, EmitOptions::minified()).expect("emit erased JS");

  // Must not contain TS-only statement keywords.
  assert!(!out.contains("interface"), "output should erase interfaces: {out}");
  // `type` can appear as part of other tokens; check the original alias name.
  assert!(!out.contains("Bar"), "output should erase type aliases: {out}");
  assert!(
    !out.contains("import type"),
    "output should erase `import type` statements: {out}"
  );
  assert!(
    !out.contains("export type"),
    "output should erase `export type` statements: {out}"
  );

  // Must erase TS-only expression wrappers.
  assert!(!out.contains(" as "), "`as` assertions must be erased: {out}");
  assert!(
    !out.contains("satisfies"),
    "`satisfies` assertions must be erased: {out}"
  );
  assert!(!out.contains("!."), "non-null assertions must be erased: {out}");
  assert!(
    !out.contains('<') && !out.contains('>'),
    "type arguments must be erased: {out}"
  );

  // Precedence: after erasing `as`, `(a + b)` must remain parenthesized as a member receiver.
  assert!(
    out.contains("(a+b).c") || out.contains("(a + b).c"),
    "expected erased output to keep parentheses for member receiver: {out}"
  );

  assert_parses_as_ecma(&out);
}

#[test]
fn reports_unsupported_ts_runtime_constructs() {
  let source = "enum E { A }";
  let mut parsed = parse_ts(source);
  let diagnostics = erase_types_strict_native(FileId(0), SourceType::Module, &mut parsed)
    .expect_err("expected enum to be rejected");
  assert!(!diagnostics.is_empty(), "expected at least one diagnostic");
}
