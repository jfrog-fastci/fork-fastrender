use optimize_js::cfg::cfg::Cfg;
use optimize_js::{compile_source_with_cfg_options, CompileCfgOptions, TopLevelMode};

fn dump_cfg(cfg: &Cfg) -> String {
  let mut labels: Vec<u32> = cfg.bblocks.all().map(|(label, _)| label).collect();
  labels.sort_unstable();
  let mut out = String::new();
  out.push_str(&format!("entry {}\n", cfg.entry));
  for label in labels {
    out.push_str(&format!("block {label}:\n"));
    for inst in cfg.bblocks.get(label) {
      out.push_str(&format!("  {inst:?}\n"));
    }
    let children = cfg.graph.children_sorted(label);
    out.push_str(&format!("  children {children:?}\n"));
  }
  out
}

#[test]
fn loop_unroll_is_deterministic() {
  let source = r#"
    let a = [0, 0, 0, 0];
    for (let i = 0; i < 4; i = i + 1) {
      a[i] = i + 1;
    }
    void a;
  "#;

  let options = CompileCfgOptions {
    enable_loop_opts: true,
    ..CompileCfgOptions::default()
  };

  let first = compile_source_with_cfg_options(source, TopLevelMode::Module, false, options)
    .expect("compile1");
  let second = compile_source_with_cfg_options(source, TopLevelMode::Module, false, options)
    .expect("compile2");

  let dump1 = dump_cfg(first.top_level.analyzed_cfg());
  let dump2 = dump_cfg(second.top_level.analyzed_cfg());
  assert_eq!(dump1, dump2);
}
