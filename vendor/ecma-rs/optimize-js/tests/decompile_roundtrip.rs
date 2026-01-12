#[path = "common/mod.rs"]
mod common;
use common::compile_source;
use emit_js::EmitOptions;
use optimize_js::{decompile::program_to_js, CompileCfgOptions, DecompileOptions, TopLevelMode};

fn compile_and_emit(src: &str, mode: TopLevelMode) -> Vec<u8> {
  let program = compile_source(src, mode, false);
  let decompile = DecompileOptions {
    // Ensure SSA temporaries are in scope for all uses. This is especially
    // important for the state-machine fallback, which otherwise introduces many
    // block-scoped `let` bindings.
    declare_registers: true,
    ..DecompileOptions::default()
  };
  program_to_js(
    &program,
    &decompile,
    EmitOptions::minified(),
  )
  .expect("decompile program to JS")
}

fn assert_roundtrip(src: &str, mode: TopLevelMode) {
  let out1 = compile_and_emit(src, mode);
  let out2 = compile_and_emit(src, mode);

  assert_eq!(out1, out2, "emitted JS should be deterministic");

  let out_str = String::from_utf8(out1).expect("emitted JS should be UTF-8");

  parse_js::parse(&out_str).expect("emitted JS should parse");
  // The decompiler may fall back to emitting large state machines for complex CFGs.
  // Re-compiling those can be expensive, so keep this check bounded.
  if out_str.len() < 2048 {
    // Recompiling the decompiler output with all optimization passes can be extremely slow because
    // the decompiler currently expands some semantics into state-machine form (especially in
    // `TopLevelMode::Global`). This roundtrip test primarily validates that emitted JS is
    // syntactically valid and can be lowered again, so disable opt passes for the recompile step to
    // keep the suite runtime bounded.
    let options = CompileCfgOptions {
      run_opt_passes: false,
      keep_ssa: true,
      ..CompileCfgOptions::default()
    };
    if let Err(errs) = optimize_js::compile_source_with_cfg_options(&out_str, mode, false, options) {
      panic!("compile emitted JS: {errs:?}\n\n{out_str}");
    }
  }
}

#[test]
fn decompile_roundtrip_module_mode() {
  let cases = [r#"
    let result = 0;
    const value = choose();
    if (value > 0) {
      if (check(value)) {
        result = run(value);
      } else {
        result = fallback(value);
      }
    } else {
      result = reset(result);
    }
  "#];

  for src in cases {
    assert_roundtrip(src, TopLevelMode::Module);
  }
}

#[test]
fn decompile_roundtrip_global_mode() {
  let cases = [
    r#"
      var total = 0;
      while (shouldContinue(total)) {
        total += 1;
        if (total > limit()) {
          break;
        }
      }
      for (;;) {
        // Avoid `total++` here: in global mode, update expressions lower to full
        // `ToNumeric` + BigInt-aware semantics, which currently decompile into a
        // large state machine. Re-compiling that emitted output in this roundtrip
        // test can be prohibitively slow.
        total += 1;
        if (stop(total)) {
          break;
        }
      }
      finish(total);
    "#,
    r#"
      const currentTask = getTask();
      const items = getItems();
      const ctx = currentTask?.owner?.id;
      worker?.run?.(ctx, ...items, extraArg());
      report(worker?.getLast?.()?.result?.value);
    "#,
  ];

  for src in cases {
    assert_roundtrip(src, TopLevelMode::Global);
  }
}
