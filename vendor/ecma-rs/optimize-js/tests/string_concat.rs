use optimize_js::analysis::encoding::analyze_cfg_encoding;
use optimize_js::cfg::cfg::{Cfg, CfgBBlocks, CfgGraph};
use optimize_js::il::inst::{Arg, Const, Inst, InstTyp, StringEncoding};
use optimize_js::{CompileCfgOptions, TopLevelMode};

#[cfg(feature = "typed")]
use optimize_js::il::inst::BinOp;

fn cfg_labels_sorted(cfg: &Cfg) -> Vec<u32> {
  let mut labels: Vec<u32> = cfg.bblocks.all().map(|(label, _)| label).collect();
  labels.sort_unstable();
  labels
}

#[test]
fn template_literals_lower_to_string_concat_when_enabled() {
  let options = CompileCfgOptions {
    run_opt_passes: false,
    ..CompileCfgOptions::default()
  };
  let program = optimize_js::compile_source_with_cfg_options(
    "let t = `a${1}b${2}c`;",
    TopLevelMode::Module,
    false,
    options,
  )
  .expect("compile source");

  let cfg = &program.top_level.body;
  let mut has_string_concat = false;
  let mut has_template_call = false;
  for label in cfg_labels_sorted(cfg) {
    for inst in cfg.bblocks.get(label).iter() {
      if inst.t == InstTyp::StringConcat && inst.meta.string_concat_is_template {
        has_string_concat = true;
      }
      if inst.t == InstTyp::Call {
        let (_tgt, callee, _this, _args, _spreads) = inst.as_call();
        if matches!(callee, Arg::Builtin(path) if path == "__optimize_js_template") {
          has_template_call = true;
        }
      }
    }
  }

  #[cfg(feature = "typed")]
  {
    assert!(has_string_concat, "expected template literal to lower to StringConcat");
    assert!(
      !has_template_call,
      "legacy __optimize_js_template call should not be emitted when enabled"
    );
  }
  #[cfg(not(feature = "typed"))]
  {
    assert!(
      !has_string_concat,
      "expected template literal to lower via __optimize_js_template marker call"
    );
    assert!(
      has_template_call,
      "expected template literal to lower via __optimize_js_template marker call"
    );
  }
}

#[cfg(feature = "typed")]
#[test]
fn typed_string_add_chains_lower_to_string_concat() {
  let options = CompileCfgOptions {
    run_opt_passes: false,
    ..CompileCfgOptions::default()
  };

  let program = optimize_js::compile_source_typed_cfg_options(
    r#"
      const a: string = "a";
      const b: string = "b";
      const out: string = a + b + "c";
      console.log(out);
    "#,
    TopLevelMode::Module,
    false,
    options,
  )
  .expect("compile typed source");

  let cfg = &program.top_level.body;
  let mut has_string_concat = false;
  let mut has_bin_add = false;
  for label in cfg_labels_sorted(cfg) {
    for inst in cfg.bblocks.get(label).iter() {
      if inst.t == InstTyp::StringConcat && !inst.meta.string_concat_is_template {
        has_string_concat = true;
      }
      if inst.t == InstTyp::Bin && inst.bin_op == BinOp::Add {
        has_bin_add = true;
      }
    }
  }

  assert!(
    has_string_concat,
    "expected typed string `+` chain to lower to StringConcat"
  );
  assert!(
    !has_bin_add,
    "expected no Bin(Add) when typed string `+` chain lowering is enabled"
  );
}

#[test]
fn encoding_propagates_through_string_concat() {
  let mut graph = CfgGraph::default();
  graph.connect(0, 1);
  let mut bblocks = CfgBBlocks::default();
  bblocks.add(
    0,
    vec![
      Inst::var_assign(0, Arg::Const(Const::Str("a".to_string()))),
      Inst::var_assign(1, Arg::Const(Const::Str("b".to_string()))),
      Inst::string_concat(2, vec![Arg::Var(0), Arg::Var(1)]),
    ],
  );
  bblocks.add(1, Vec::new());
  let cfg = Cfg {
    graph,
    bblocks,
    entry: 0,
  };

  let result = analyze_cfg_encoding(&cfg);
  assert_eq!(result.encoding_at_entry(1, 2), StringEncoding::Ascii);
}
