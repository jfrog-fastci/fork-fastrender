use emit_js::{emit_js_top_level, EmitOptions, Emitter};
use parse_js::{Dialect, ParseOptions, SourceType};

#[test]
fn js_emitter_parenthesizes_ts_wrapped_unary_base_of_exponentiation() {
  let source = "(-x as any)**2;";
  let parsed = parse_js::parse(source).expect("parse TS");

  let mut emitter = Emitter::new(EmitOptions::minified());
  emit_js_top_level(&mut emitter, parsed.stx.as_ref()).expect("emit JS");
  let out = String::from_utf8(emitter.into_bytes()).expect("JS output is UTF-8");

  assert!(
    out.contains("(-x)**2"),
    "expected JS emitter to parenthesize exponentiation base, got `{out}`"
  );
  assert!(
    !out.contains("-x**2"),
    "unparenthesized unary base is invalid JS, got `{out}`"
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

