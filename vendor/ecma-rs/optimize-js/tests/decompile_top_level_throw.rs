use ahash::HashMap;
use emit_js::EmitOptions;
use optimize_js::cfg::cfg::Cfg;
use optimize_js::il::inst::{Arg, Const, Inst};
use optimize_js::{program_to_js, DecompileOptions, Program, ProgramFunction, TopLevelMode};
use parse_js::num::JsNumber;

fn program_throw_one() -> Program {
  let mut bblocks: HashMap<u32, Vec<Inst>> = HashMap::default();
  bblocks.insert(
    0,
    vec![Inst::throw(Arg::Const(Const::Num(JsNumber(1.0))))],
  );
  let cfg = Cfg::from_bblocks(bblocks, vec![0]);
  Program {
    functions: Vec::new(),
    top_level: ProgramFunction {
      debug: None,
      body: cfg,
      params: Vec::new(),
      stats: Default::default(),
    },
    top_level_mode: TopLevelMode::Module,
    symbols: None,
  }
}

fn assert_throw_output(bytes: Vec<u8>) {
  let out = String::from_utf8(bytes).expect("utf-8 output");
  assert!(
    out.contains("throw 1"),
    "expected emitted JS to contain `throw 1`, got {out:?}"
  );
  assert!(
    !out.contains("__optimize_js"),
    "expected throw to decompile without internal helpers, got {out:?}"
  );
}

#[test]
fn top_level_throw_is_decompiled() {
  let opts = DecompileOptions::default();
  let emit = EmitOptions::minified();

  let program = program_throw_one();
  let bytes = program_to_js(&program, &opts, emit).expect("decompile manual program");
  assert_throw_output(bytes);

  // Once Task 222 lands (top-level throw supported by the compiler), also ensure
  // `compile_source` round-trips through the decompiler.
  if let Ok(program) = optimize_js::compile_source("throw 1;", TopLevelMode::Module, false) {
    let bytes = program_to_js(&program, &opts, emit).expect("decompile compiled throw");
    assert_throw_output(bytes);
  }
}
