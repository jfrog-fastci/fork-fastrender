#![cfg(all(feature = "serde", feature = "typed"))]

use optimize_js::analysis::annotate_program;
use optimize_js::dump::{dump_program, DumpOptions};
use optimize_js::{compile_source_typed, TopLevelMode};

fn dump_json(source: &str) -> String {
  let mut program = compile_source_typed(source, TopLevelMode::Module, false).expect("compile");
  annotate_program(&mut program);
  dump_program(
    &program,
    DumpOptions {
      include_symbols: true,
      include_analyses: true,
    },
  )
  .to_json_string()
}

#[test]
fn program_dump_is_deterministic() {
  let source = r#"
    const f = (x: number) => x + 1;
    let y = f(1);
    if (y > 0) {
      y = y + 1;
    } else {
      y = y - 1;
    }
    console.log(y);
  "#;

  let first = dump_json(source);
  let second = dump_json(source);

  assert_eq!(first, second, "program dumps should be deterministic");
}
