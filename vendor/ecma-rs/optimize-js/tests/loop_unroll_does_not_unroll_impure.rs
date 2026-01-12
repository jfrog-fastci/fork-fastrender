use optimize_js::analysis::loop_info::LoopInfo;
use optimize_js::dom::Dom;
use optimize_js::{compile_source_with_cfg_options, CompileCfgOptions, TopLevelMode};

#[test]
fn loop_unroll_does_not_unroll_when_loop_body_contains_calls() {
  let source = r#"
    function f(x) { return x; }
    for (let i = 0; i < 3; i = i + 1) {
      f(i);
    }
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
    !loops.loops.is_empty(),
    "expected loop to remain (calls are treated as impure), but it was unrolled"
  );
}
