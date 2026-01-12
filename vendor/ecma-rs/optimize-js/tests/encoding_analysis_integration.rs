use optimize_js::analysis::encoding::analyze_cfg_encoding;
use optimize_js::cfg::cfg::Cfg;
use optimize_js::il::inst::{Arg, Const, InstTyp, StringEncoding};
use optimize_js::{CompileCfgOptions, TopLevelMode};

fn cfg_labels_sorted(cfg: &Cfg) -> Vec<u32> {
  let mut labels: Vec<u32> = cfg.bblocks.all().map(|(label, _)| label).collect();
  labels.sort_unstable();
  labels
}

fn find_def_label(cfg: &Cfg, var: u32) -> Option<u32> {
  for label in cfg_labels_sorted(cfg) {
    for inst in cfg.bblocks.get(label).iter() {
      if inst.tgts.first() == Some(&var) {
        return Some(label);
      }
    }
  }
  None
}

fn resolve_one_hop_copy(cfg: &Cfg, label: u32, var: u32) -> (u32, u32) {
  // Some optimisation passes rewrite constant defs into `t = u` copies. When we
  // locate a candidate temp, follow at most one `VarAssign(t, Var(u))` hop to
  // reach the original defining temp.
  for inst in cfg.bblocks.get(label).iter() {
    if inst.t != InstTyp::VarAssign {
      continue;
    }
    let (tgt, arg) = inst.as_var_assign();
    if tgt == var && matches!(arg, Arg::Var(_)) {
      let other = arg.to_var();
      return (
        find_def_label(cfg, other).unwrap_or(label),
        other,
      );
    }
  }
  (label, var)
}

fn find_literal_def(cfg: &Cfg, value: &str) -> (u32, u32) {
  let mut matches = Vec::new();
  for label in cfg_labels_sorted(cfg) {
    for inst in cfg.bblocks.get(label).iter() {
      if inst.t != InstTyp::VarAssign {
        continue;
      }
      let (tgt, arg) = inst.as_var_assign();
      if matches!(arg, Arg::Const(Const::Str(s)) if s == value) {
        matches.push((label, tgt));
      }
    }
  }
  matches.sort_unstable();
  match matches.as_slice() {
    [single] => *single,
    [] => panic!("missing `VarAssign` with Const::Str({value:?}) in CFG"),
    _ => panic!(
      "expected exactly one `VarAssign` defining {value:?}, found: {matches:?}"
    ),
  }
}

fn find_template_def(cfg: &Cfg, value: &str) -> (u32, u32) {
  let mut matches = Vec::new();
  for label in cfg_labels_sorted(cfg) {
    for inst in cfg.bblocks.get(label).iter() {
      #[cfg(feature = "typed")]
      {
        if inst.t != InstTyp::StringConcat || !inst.meta.string_concat_is_template {
          continue;
        }
        let (tgt, parts) = inst.as_string_concat();
        if parts.len() != 1 {
          continue;
        }
        if matches!(&parts[0], Arg::Const(Const::Str(s)) if s == value) {
          matches.push((label, tgt));
        }
      }
      #[cfg(not(feature = "typed"))]
      {
        if inst.t != InstTyp::TemplateLit {
          continue;
        }
        let (tgt, parts) = inst.as_template_lit();
        let Some(tgt) = tgt else {
          continue;
        };
        if parts.len() != 1 {
          continue;
        }
        if matches!(&parts[0], Arg::Const(Const::Str(s)) if s == value) {
          matches.push((label, tgt));
        }
      }
    }
  }
  matches.sort_unstable();
  match matches.as_slice() {
    [single] => *single,
    [] => {
      #[cfg(feature = "typed")]
      panic!("missing StringConcat template literal for {value:?} in CFG");
      #[cfg(not(feature = "typed"))]
      panic!("missing TemplateLit({value:?}) instruction in CFG");
    }
    _ => panic!(
      "expected exactly one template literal definition for {value:?}, found: {matches:?}"
    ),
  }
}

fn assert_var_encoding(
  result: &optimize_js::analysis::encoding::EncodingResult,
  label: u32,
  var: u32,
  expected: StringEncoding,
) {
  let actual = result.encoding_at_exit(label, var);
  assert_eq!(
    actual, expected,
    "unexpected encoding for var {var} at exit of block {label}"
  );
}

#[test]
fn encoding_analysis_distinguishes_ascii_and_utf8() {
  // Compile without optimisation passes so the lowering patterns we want to
  // exercise remain visible (DCE would otherwise delete unused `let` bindings).
  let options = CompileCfgOptions {
    run_opt_passes: false,
    ..CompileCfgOptions::default()
  };

  let program = optimize_js::compile_source_with_cfg_options(
    r#"
       let a = "hello";
       let b = "ÿ";      // non-ASCII (U+00FF)
       let c = "π";      // non-ASCII (U+03C0)
       let t0 = `hello`;  // lowered as TemplateLit (untyped) or StringConcat (typed) inst
       let t1 = `ÿ`;
       let t2 = `π`;
      "#,
    TopLevelMode::Module,
    false,
    options,
  )
  .expect("compile source");

  let cfg = &program.top_level.body;
  let result = analyze_cfg_encoding(cfg);

  // Direct string literals.
  let (hello_label, hello_var) = find_literal_def(cfg, "hello");
  let (y_label, y_var) = find_literal_def(cfg, "ÿ");
  let (pi_label, pi_var) = find_literal_def(cfg, "π");

  let (hello_label, hello_var) = resolve_one_hop_copy(cfg, hello_label, hello_var);
  let (y_label, y_var) = resolve_one_hop_copy(cfg, y_label, y_var);
  let (pi_label, pi_var) = resolve_one_hop_copy(cfg, pi_label, pi_var);

  assert_var_encoding(&result, hello_label, hello_var, StringEncoding::Ascii);
  assert_var_encoding(&result, y_label, y_var, StringEncoding::Utf8);
  assert_var_encoding(&result, pi_label, pi_var, StringEncoding::Utf8);

  // Template literals lowered as either `InstTyp::TemplateLit` (untyped) or
  // `StringConcat` (typed builds).
  let (t0_label, t0_var) = find_template_def(cfg, "hello");
  let (t1_label, t1_var) = find_template_def(cfg, "ÿ");
  let (t2_label, t2_var) = find_template_def(cfg, "π");

  let (t0_label, t0_var) = resolve_one_hop_copy(cfg, t0_label, t0_var);
  let (t1_label, t1_var) = resolve_one_hop_copy(cfg, t1_label, t1_var);
  let (t2_label, t2_var) = resolve_one_hop_copy(cfg, t2_label, t2_var);

  assert_var_encoding(&result, t0_label, t0_var, StringEncoding::Ascii);
  assert_var_encoding(&result, t1_label, t1_var, StringEncoding::Utf8);
  assert_var_encoding(&result, t2_label, t2_var, StringEncoding::Utf8);
}
