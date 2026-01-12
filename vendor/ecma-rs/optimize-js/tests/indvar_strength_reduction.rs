use optimize_js::analysis::loop_canon::find_counted_loops;
use optimize_js::dom::Dom;
use optimize_js::il::inst::{BinOp, Const, InstTyp};
use optimize_js::{compile_source_with_cfg_options, CompileCfgOptions, TopLevelMode};
use parse_js::num::JsNumber;

#[test]
fn indvar_strength_reduction_eliminates_mul_in_loop() {
  let source = r#"
    let sum = 0;
    let a = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
    // Use a trip count larger than the full-unroll threshold so we can observe strength reduction
    // without the loop being eliminated entirely.
    for (let i = 0; i < 16; i = i + 1) {
      sum += a[i * 4];
    }
    void sum;
  "#;

  let options = CompileCfgOptions {
    enable_loop_opts: true,
    ..CompileCfgOptions::default()
  };
  let program =
    compile_source_with_cfg_options(source, TopLevelMode::Module, false, options).expect("compile");
  let cfg = program.top_level.analyzed_cfg();

  let dom = Dom::calculate(cfg);
  let loops = find_counted_loops(cfg, &dom);
  assert_eq!(loops.len(), 1, "expected exactly one counted loop");

  let l = &loops[0];
  let mut saw_stride_add = false;
  for &label in &l.nodes {
    for inst in cfg.bblocks.get(label) {
      if inst.t == InstTyp::Bin && inst.bin_op == BinOp::Mul {
        panic!("unexpected Mul remaining inside loop: label={label}, inst={inst:?}");
      }
      if inst.t == InstTyp::Bin && inst.bin_op == BinOp::Add {
        if inst.args.iter().any(|arg| {
          matches!(
            arg,
            optimize_js::il::inst::Arg::Const(Const::Num(JsNumber(4.0)))
          )
        }) {
          saw_stride_add = true;
        }
      }
    }
  }
  assert!(
    saw_stride_add,
    "expected strength reduction to introduce an `+ 4` accumulator update inside the loop"
  );
}
