use emit_js::{emit_js_top_level, EmitOptions, Emitter};
use parse_js::{Dialect, ParseOptions, SourceType};

#[test]
fn js_emitter_erases_ts_wrappers_before_numeric_member_access() {
  let source = r#"
    (1 as number).toString;
    (1!).toString;
    (1 satisfies number).toString;
  "#;

  let parsed = parse_js::parse(source).expect("parse TS");

  let mut emitter = Emitter::new(EmitOptions::minified());
  emit_js_top_level(&mut emitter, parsed.stx.as_ref()).expect("emit JS");
  let out = String::from_utf8(emitter.into_bytes()).expect("JS output is UTF-8");

  assert!(
    out.contains("1..toString"),
    "expected JS emitter to use `1..toString` for numeric member access, got `{out}`"
  );
  assert!(
    !out.contains("1.toString"),
    "JS output must not contain invalid numeric member access `1.toString`: `{out}`"
  );

  parse_js::parse_with_options(
    &out,
    ParseOptions {
      dialect: Dialect::Ecma,
      source_type: SourceType::Script,
    },
  )
  .expect("emitted JS should parse as strict ECMAScript");
}

