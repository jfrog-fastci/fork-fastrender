#![cfg(feature = "typed")]

use optimize_js::cfg::cfg::Cfg;
use optimize_js::il::inst::{Inst, InstTyp};
use optimize_js::{compile_source_typed_cfg_options, CompileCfgOptions, TopLevelMode};

fn collect_insts(cfg: &Cfg) -> Vec<Inst> {
  cfg
    .graph
    .labels_sorted()
    .into_iter()
    .flat_map(|label| cfg.bblocks.get(label).iter().cloned())
    .collect()
}

#[test]
fn bounds_check_elim_is_conservative_when_oob_not_proven() {
  let mut cfg_options = CompileCfgOptions::default();
  cfg_options.enable_bce = true;

  let program = compile_source_typed_cfg_options(
    r#"
      function sum(a: number[]): number {
        let s = 0;
        for (let i = 0; i < a.length; i++) {
          s += a[i + 1];
        }
        return s;
      }
      const out = sum([1, 2, 3, 4]);
      console.log(out);
    "#,
    TopLevelMode::Module,
    false,
    cfg_options,
  )
  .expect("compile typed source");

  let sum_fn = program.functions.first().expect("expected one nested function");
  let insts = collect_insts(sum_fn.analyzed_cfg());

  let loads: Vec<_> = insts
    .iter()
    .filter(|inst| inst.t == InstTyp::ArrayLoad)
    .collect();
  assert!(!loads.is_empty(), "expected at least one ArrayLoad in sum()");

  assert!(
    loads.iter().all(|inst| inst.checked),
    "expected all ArrayLoad instructions to remain checked when OOB is not proven"
  );
}

