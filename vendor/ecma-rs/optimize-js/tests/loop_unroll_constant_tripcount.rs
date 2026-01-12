use optimize_js::analysis::loop_info::LoopInfo;
use optimize_js::dom::Dom;
use optimize_js::{compile_source_with_cfg_options, CompileCfgOptions, TopLevelMode};

#[test]
fn loop_unroll_fully_unrolls_small_constant_tripcount() {
  let source = r#"
    let a = [0, 0, 0, 0];
    for (let i = 0; i < 3; i = i + 1) {
      a[i] = i;
    }
    void a;
  "#;

  let options = CompileCfgOptions {
    enable_loop_opts: true,
    ..CompileCfgOptions::default()
  };
  let program =
    compile_source_with_cfg_options(source, TopLevelMode::Module, false, options).expect("compile");
  let cfg = program.top_level.analyzed_cfg();

  let dom = Dom::calculate(cfg);
  let loops = LoopInfo::compute(cfg, &dom);
  assert!(
    loops.loops.is_empty(),
    "expected loop to be fully unrolled, but LoopInfo still reports loops: {loops:?}"
  );
}
