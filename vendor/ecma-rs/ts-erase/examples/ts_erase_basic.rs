use diagnostics::FileId;
use emit_js::{emit_top_level_diagnostic, EmitOptions};
use parse_js::{parse_with_options, Dialect, ParseOptions, SourceType};
use ts_erase::erase_types;

fn main() {
  // Minimal TS → JS erasure example.
  //
  // This performs "full" TS erasure/lowering (removes TS-only syntax and lowers
  // runtime TS constructs where supported) and prints minified JavaScript.
  let src = r#"
    const x: number = 1 as number;
    function wrap<T>(v: T): T { return v; }
    export const y = wrap<string>(x!);
  "#;

  let file = FileId(0);
  let mut ast = parse_with_options(
    src,
    ParseOptions {
      dialect: Dialect::Ts,
      source_type: SourceType::Module,
    },
  )
  .expect("input should parse");

  erase_types(file, SourceType::Module, &mut ast).expect("erasure should succeed");

  let out = emit_top_level_diagnostic(file, &ast, EmitOptions::minified()).expect("emission should succeed");
  println!("{out}");
}

