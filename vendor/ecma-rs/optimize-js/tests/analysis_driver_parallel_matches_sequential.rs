#[path = "common/mod.rs"]
mod common;

use common::compile_source;
use optimize_js::analysis::driver::{
  analyze_program_parallel, analyze_program_with_parallelism, annotate_program_parallel,
  annotate_program_with_parallelism, AnalysisParallelism,
};
use optimize_js::TopLevelMode;

fn assert_key_results_equal(a: &optimize_js::analysis::driver::ProgramAnalyses, b: &optimize_js::analysis::driver::ProgramAnalyses) {
  assert_eq!(a.effects_summary, b.effects_summary, "effects summary mismatch");
  assert_eq!(a.purity, b.purity, "purity mismatch");
  assert_eq!(a.escape, b.escape, "escape mismatch");
  assert_eq!(a.ownership, b.ownership, "ownership mismatch");
  assert_eq!(a.range, b.range, "range mismatch");
  assert_eq!(a.nullability, b.nullability, "nullability mismatch");
  assert_eq!(a.encoding, b.encoding, "encoding mismatch");
}

#[test]
fn analysis_driver_parallel_matches_sequential() {
  let source = r#"
    function outer(o) {
      const x = o?.x;
      function add1(z) { return z + 1; }

      const mk = (y) => {
        const obj = { y };
        const inner = () => {
          if (obj.y == null) {
            return "fallback";
          }
          return typeof obj.y + "π";
        };
        return { v: add1(y), s: inner() };
      };

      return mk(x);
    }

    const out = outer({ x: 1 });
    sink(out.s);
  "#;

  let program = compile_source(source, TopLevelMode::Module, false);
  let seq = analyze_program_with_parallelism(&program, AnalysisParallelism::Sequential);
  let par = analyze_program_parallel(&program);
  assert_key_results_equal(&seq, &par);

  let mut program_seq = compile_source(source, TopLevelMode::Module, false);
  let seq = annotate_program_with_parallelism(&mut program_seq, AnalysisParallelism::Sequential);
  let mut program_par = compile_source(source, TopLevelMode::Module, false);
  let par = annotate_program_parallel(&mut program_par);
  assert_key_results_equal(&seq, &par);
}
