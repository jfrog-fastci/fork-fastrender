use optimize_js::cfg::cfg::Cfg;
use optimize_js::il::inst::{BinOp, Inst, InstTyp};
use optimize_js::{compile_source_with_cfg_options, CompileCfgOptions, InlineOptions, TopLevelMode};

fn collect_insts(cfg: &Cfg) -> Vec<Inst> {
  let mut labels: Vec<u32> = cfg.bblocks.all().map(|(label, _)| label).collect();
  labels.sort_unstable();
  let mut insts = Vec::new();
  for label in labels {
    insts.extend(cfg.bblocks.get(label).iter().cloned());
  }
  insts
}

#[test]
fn called_once_function_is_inlined() {
  let src = r#"
    function add1(x) {
      return x + 1;
    }
    const y = add1(41);
    void y;
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
  let insts = collect_insts(cfg);

  assert!(
    insts.iter().all(|inst| inst.t != InstTyp::Call),
    "expected call to be removed after inlining, found calls: {insts:?}"
  );
  assert!(
    insts
      .iter()
      .any(|inst| inst.t == InstTyp::Bin && inst.bin_op == BinOp::Add),
    "expected callee body to be spliced into caller (missing `+` binop): {insts:?}"
  );
}

