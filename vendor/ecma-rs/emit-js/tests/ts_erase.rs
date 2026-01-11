#![cfg(feature = "ts_erase")]

use diagnostics::FileId;
use emit_js::{emit_ecma_from_ts_top_level, EmitOptions};
use parse_js::{parse_with_options, Dialect, ParseOptions, SourceType};

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
 import { type Foo as Foo3, baz } from "foo";
 import { type Foo } from "type-only-import";
 export type { Bar };
 export { type Foo3, baz };
 export { type Foo } from "type-only-export";
 export { type Foo };
 import {} from "side-effect-import";
 export {} from "side-effect-export";

// Expression wrappers.
(x as any).y;
x!.y;
f<T>(x);
(x satisfies any).y;
((a + b) as any).c;

// TS `this` parameters are type-only and must be erased.
function f(this: any, x: number) { return this; }
class C { m(this: any, x: number) { return this; } }

// TS parameter properties are runtime semantics that must be lowered.
class ParamProps {
  constructor(public x: number, readonly y: number) {}
}
class Base {}
class Derived extends Base {
  constructor(public x: number) {
    super();
  }
}

// TS class field modifiers/annotations must be erased into valid JS class fields.
class Fields {
  public a: number;
  readonly b!: string;
  c?: boolean;
  declare d: number;
}
"#;

  let mut parsed = parse_ts(source);
  let out = emit_ecma_from_ts_top_level(FileId(0), SourceType::Module, &mut parsed, EmitOptions::minified())
    .expect("emit erased JS");

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
  assert!(
    out.contains("import{baz}from") || out.contains("import {baz} from"),
    "output should keep value imports when erasing type-only specifiers: {out}"
  );
  assert!(
    out.contains("export{baz}") || out.contains("export {baz}"),
    "output should keep value exports when erasing type-only specifiers: {out}"
  );
  assert!(
    !out.contains("type-only-import"),
    "output should drop imports that become empty after stripping type-only specifiers: {out}"
  );
  assert!(
    !out.contains("type-only-export"),
    "output should drop exports that become empty after stripping type-only specifiers: {out}"
  );
  assert!(
    out.contains("side-effect-import"),
    "output should preserve side-effect imports (`import {{}} from ...`): {out}"
  );
  assert!(
    out.contains("side-effect-export"),
    "output should preserve side-effect exports (`export {{}} from ...`): {out}"
  );
  assert!(
    out.contains("this[\"x\"]=x") || out.contains("this.x=x"),
    "output should lower parameter properties into assignments: {out}"
  );
  assert!(
    out.contains("super();this[\"x\"]=x") || out.contains("super();this.x=x"),
    "output should insert derived ctor parameter property assignments after super(): {out}"
  );
  assert!(
    !out.contains("readonly") && !out.contains("declare") && !out.contains("?:") && !out.contains("!:"),
    "output should erase TS-only class field modifiers/annotations: {out}"
  );
  assert!(
    !out.contains("Fields{a;b;c;d"),
    "output should drop `declare` class fields entirely (no runtime field): {out}"
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
  for (label, dialect, source) in [
    ("enum", Dialect::Ts, "enum E { A }"),
    ("decorators", Dialect::Ts, "@dec class C {}"),
    ("jsx", Dialect::Tsx, "const el = <div>{x}</div>;"),
  ] {
    let mut parsed = parse_with_options(
      source,
      ParseOptions {
        dialect,
        source_type: SourceType::Module,
      },
    )
    .unwrap_or_else(|err| panic!("parse {label}: {err:?}\nsource:\n{source}"));

    let diagnostics = emit_ecma_from_ts_top_level(
      FileId(0),
      SourceType::Module,
      &mut parsed,
      EmitOptions::minified(),
    )
    .expect_err(&format!("expected {label} to be rejected"));

    assert!(
      diagnostics.iter().any(|diag| diag.code.as_str() == "MINIFYTS0001"),
      "expected MINIFYTS0001 diagnostic for {label}, got: {diagnostics:?}"
    );
  }
}
