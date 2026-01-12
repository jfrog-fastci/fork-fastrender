use optimize_js::cfg::cfg::Cfg;
use optimize_js::il::inst::InstTyp;
use optimize_js::{compile_source_with_cfg_options, CompileCfgOptions, InlineOptions, TopLevelMode};

fn count_calls(cfg: &Cfg) -> usize {
  let mut count = 0;
  let mut labels: Vec<u32> = cfg.bblocks.all().map(|(label, _)| label).collect();
  labels.sort_unstable();
  for label in labels {
    for inst in cfg.bblocks.get(label) {
      if inst.t == InstTyp::Call {
        count += 1;
      }
    }
  }
  count
}

#[test]
fn small_function_is_inlined_even_when_called_multiple_times() {
  let src = r#"
    function inc(x) {
      return x + 1;
    }
    const a = inc(1);
    const b = inc(2);
    void a;
    void b;
  "#;

  let options = CompileCfgOptions {
    keep_ssa: true,
    inline: InlineOptions {
      enabled: true,
      threshold: 16,
      max_depth: 8,
    },
    ..CompileCfgOptions::default()
  };
  let program = compile_source_with_cfg_options(src, TopLevelMode::Module, false, options)
    .expect("compile");
  let cfg = program.top_level.ssa_body.as_ref().expect("ssa cfg");

  assert_eq!(
    count_calls(cfg),
    0,
    "expected all calls to `inc` to be inlined"
  );
}

