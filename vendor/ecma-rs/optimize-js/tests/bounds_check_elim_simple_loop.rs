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
fn bounds_check_elim_removes_checks_in_simple_loop() {
  let mut cfg_options = CompileCfgOptions::default();
  cfg_options.enable_bce = true;

  let program = compile_source_typed_cfg_options(
    r#"
      function sum(a: number[]): number {
        let s = 0;
        for (let i = 0; i < a.length; i++) {
          s += a[i];
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
  let cfg = sum_fn.analyzed_cfg();
  let insts = collect_insts(cfg);

  let loads: Vec<_> = insts
    .iter()
    .filter(|inst| inst.t == InstTyp::ArrayLoad)
    .collect();
  assert!(!loads.is_empty(), "expected at least one ArrayLoad in sum()");

  let load_debug = loads
    .iter()
    .map(|inst| format!("{inst:?}"))
    .collect::<Vec<_>>()
    .join("\n");
  assert!(
    loads.iter().any(|inst| !inst.checked),
    "expected at least one ArrayLoad to have checked=false after BCE. loads:\n{load_debug}\n\ncfg:\n{:#?}",
    cfg
  );
}
