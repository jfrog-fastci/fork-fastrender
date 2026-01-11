#[path = "common/mod.rs"]
mod common;
use common::compile_source;
use emit_js::EmitOptions;
use optimize_js::decompile::program_to_js;
use optimize_js::{DecompileOptions, TopLevelMode};

fn compile_and_emit(src: &str, mode: TopLevelMode) -> String {
  let program = compile_source(src, mode, false);
  let out = program_to_js(
    &program,
    &DecompileOptions::default(),
    EmitOptions::minified(),
  )
  .expect("decompile program to JS");
  String::from_utf8(out).expect("emitted JS should be UTF-8")
}

fn assert_emitted_compiles(emitted: &str, mode: TopLevelMode) {
  parse_js::parse(emitted).expect("emitted JS should parse");
  if let Err(errs) = optimize_js::compile_source(emitted, mode, false) {
    panic!("compile emitted JS: {errs:?}\n\n{emitted}");
  }
}

#[test]
fn decompile_return_in_function_roundtrip() {
  let src = "function f(){ return 1; } f();";

  let out = compile_and_emit(src, TopLevelMode::Module);
  assert!(
    out.contains("return"),
    "expected emitted JS to contain `return`, got:\n{out}"
  );

  assert_emitted_compiles(&out, TopLevelMode::Module);
}

#[test]
fn decompile_throw_top_level_roundtrip() {
  let src = "throw 1;";

  let out = compile_and_emit(src, TopLevelMode::Global);
  assert_emitted_compiles(&out, TopLevelMode::Global);
}
