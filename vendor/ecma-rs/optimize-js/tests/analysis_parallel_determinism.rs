#![cfg(all(feature = "parallel-analyses", feature = "serde"))]

use optimize_js::analysis::{annotate_program_with_parallelism, AnalysisParallelism};
use optimize_js::{compile_source, TopLevelMode};

fn test_source() -> &'static str {
  r#"
    function makeObj(x) {
      return { x, y: "π" };
    }
    function add1(n) {
      return n + 1;
    }
    function sum(n) {
      let s = 0;
      for (let i = 0; i < n; i++) {
        s = s + i;
      }
      return s;
    }
    const o = makeObj(1);
    const v = add1(o?.x ?? 0);
    const a = sum(5);
    sink(typeof v, o.y, a);
    void v;
  "#
}

#[test]
fn program_analyses_deterministic_parallel_twice() {
  let mut program1 = compile_source(test_source(), TopLevelMode::Module, false).expect("compile");
  let mut program2 = compile_source(test_source(), TopLevelMode::Module, false).expect("compile");

  let analyses1 = annotate_program_with_parallelism(&mut program1, AnalysisParallelism::Parallel);
  let analyses2 = annotate_program_with_parallelism(&mut program2, AnalysisParallelism::Parallel);

  let json1 = serde_json::to_string(&analyses1).expect("serialize analyses");
  let json2 = serde_json::to_string(&analyses2).expect("serialize analyses");
  assert_eq!(json1, json2);
}

#[test]
fn program_analyses_parallel_matches_sequential() {
  let mut program_seq = compile_source(test_source(), TopLevelMode::Module, false).expect("compile");
  let mut program_par = compile_source(test_source(), TopLevelMode::Module, false).expect("compile");

  let analyses_seq = annotate_program_with_parallelism(&mut program_seq, AnalysisParallelism::Sequential);
  let analyses_par = annotate_program_with_parallelism(&mut program_par, AnalysisParallelism::Parallel);

  let json_seq = serde_json::to_string(&analyses_seq).expect("serialize analyses");
  let json_par = serde_json::to_string(&analyses_par).expect("serialize analyses");
  assert_eq!(json_seq, json_par);
}

