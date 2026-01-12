use optimize_js::cfg::cfg::Cfg;
use optimize_js::il::inst::InstTyp;
use optimize_js::{compile_source_with_cfg_options, CompileCfgOptions, InlineOptions, TopLevelMode};

fn collect_stats(cfg: &Cfg) -> (usize /*calls*/, bool /*has_cond_goto*/) {
  let mut calls = 0usize;
  let mut has_cond = false;
  for label in cfg.reverse_postorder() {
    for inst in cfg.bblocks.get(label) {
      if inst.t == InstTyp::Call {
        calls += 1;
      }
      if inst.t == InstTyp::CondGoto {
        has_cond = true;
      }
    }
  }
  (calls, has_cond)
}

#[test]
fn recursive_calls_are_not_endlessly_inlined() {
  let src = r#"
    function f(n) {
      if (n) {
        return f(n - 1);
      }
      return 0;
    }
    f(3);
  "#;

  let options = CompileCfgOptions {
    keep_ssa: true,
    inline: InlineOptions {
      enabled: true,
      threshold: 32,
      max_depth: 8,
    },
    ..CompileCfgOptions::default()
  };

  let program = compile_source_with_cfg_options(src, TopLevelMode::Module, false, options)
    .expect("compile");
  let cfg = program.top_level.ssa_body.as_ref().expect("ssa cfg");
  let (calls, has_cond) = collect_stats(cfg);

  assert!(
    has_cond,
    "expected recursive function body to be inlined into top-level (missing CondGoto)"
  );
  assert_eq!(
    calls, 1,
    "expected recursion to remain as a call (no unbounded inlining), got {calls} calls"
  );
}

