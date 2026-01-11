#[path = "common/mod.rs"]
mod common;
use common::compile_source;
use optimize_js::il::inst::InstTyp;
use optimize_js::TopLevelMode;

fn cfg_contains_phi(cfg: &optimize_js::cfg::cfg::Cfg) -> bool {
  cfg
    .bblocks
    .all()
    .any(|(_, insts)| insts.iter().any(|inst| inst.t == InstTyp::Phi))
}

#[test]
fn preserves_ssa_cfg_with_phis_alongside_deconstructed_cfg() {
  let source = r#"
    (() => {
      let x = 0;
      if (cond) {
        side_effect();
        x = 1;
      } else {
        side_effect();
        x = 2;
      }
      sink(x);
    })();
  "#;

  let program = compile_source(source, TopLevelMode::Module, false);

  let mut found_phi = false;
  for func in std::iter::once(&program.top_level).chain(program.functions.iter()) {
    let ssa_cfg = func.ssa_body.as_ref().expect("ssa_body should be populated");
    found_phi |= cfg_contains_phi(ssa_cfg);

    assert!(
      !cfg_contains_phi(&func.body),
      "deconstructed CFG should not contain Phi nodes"
    );
  }

  assert!(
    found_phi,
    "expected at least one Phi node in the preserved SSA CFG"
  );
}
