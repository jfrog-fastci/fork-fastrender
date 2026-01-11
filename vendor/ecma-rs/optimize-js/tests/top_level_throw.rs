#[path = "common/mod.rs"]
mod common;

use optimize_js::cfg::cfg::Cfg;
use optimize_js::TopLevelMode;

fn serialize_cfg(cfg: &Cfg) -> String {
  let mut labels: Vec<u32> = cfg.bblocks.all().map(|(label, _)| label).collect();
  labels.sort_unstable();

  let mut out = String::new();
  for label in labels {
    out.push_str(&format!("block {label}\n"));
    for inst in cfg.bblocks.get(label).iter() {
      out.push_str(&format!("{:?}\n", inst));
    }
    let children = cfg.graph.children_sorted(label);
    out.push_str(&format!("children {children:?}\n"));
  }
  out
}

fn serialize_program(program: &optimize_js::Program) -> String {
  let mut out = String::new();

  out.push_str("top_level\n");
  out.push_str(&serialize_cfg(&program.top_level.body));

  // These are already stable by index.
  for (idx, func) in program.functions.iter().enumerate() {
    out.push_str(&format!("function {idx}\n"));
    out.push_str(&serialize_cfg(&func.body));
  }

  out
}

#[test]
fn top_level_throw_is_allowed_in_global_mode() {
  common::compile_source("throw new Error('x');", TopLevelMode::Global, false);
}

#[test]
fn top_level_throw_is_allowed_in_module_mode() {
  common::compile_source("throw new Error('x');", TopLevelMode::Module, false);
}

#[test]
fn top_level_throw_compilation_is_deterministic() {
  let src = "throw new Error('x');";
  let first = common::compile_source(src, TopLevelMode::Global, false);
  let second = common::compile_source(src, TopLevelMode::Global, false);

  assert_eq!(
    serialize_program(&first),
    serialize_program(&second),
    "compilation should be deterministic"
  );
}

#[test]
fn top_level_return_is_still_rejected() {
  let err = optimize_js::compile_source("return 1;", TopLevelMode::Global, false)
    .expect_err("top-level return should be rejected");

  assert!(
    err.iter().any(|diag| {
      diag.code == "OPT0002" && diag.message.contains("return statement outside function")
    }),
    "expected OPT0002 diagnostic for top-level return, got {err:?}"
  );
}
