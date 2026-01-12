use optimize_js::{program_to_js, CompileCfgOptions, DecompileOptions, TopLevelMode};

#[test]
fn decompile_roundtrip_preserves_core_literals_and_allocs() {
  // Compile without opt passes so the lowering patterns stay visible and we don't
  // rely on decompiler/optimizer interaction.
  let program = optimize_js::compile_source_with_cfg_options(
    r#"
      console.log(
        [1, ...xs],
        {x:1, ...y},
        /a+/.test("aa"),
        `t${1}x`,
        String.raw`a${1}b`,
        new Array(1),
        delete y.prop,
        "prop" in y,
        y instanceof Object,
      );
    "#,
    TopLevelMode::Module,
    false,
    CompileCfgOptions {
      run_opt_passes: false,
      ..CompileCfgOptions::default()
    },
  )
  .expect("compile source");

  let bytes = program_to_js(
    &program,
    &DecompileOptions::default(),
    emit_js::EmitOptions::minified(),
  )
  .expect("emit JS");
  let js = std::str::from_utf8(&bytes).expect("UTF-8 output");

  for helper in [
    "__optimize_js_array",
    "__optimize_js_array_hole",
    "__optimize_js_object",
    "__optimize_js_object_prop",
    "__optimize_js_object_prop_computed",
    "__optimize_js_object_spread",
    "__optimize_js_regex",
    "__optimize_js_template",
    "__optimize_js_tagged_template",
    "__optimize_js_new",
    "__optimize_js_delete",
    "__optimize_js_in",
    "__optimize_js_instanceof",
  ] {
    assert!(
      !js.contains(helper),
      "internal helper {helper} leaked into output: {js}"
    );
  }

  fn contains_tagged_template(js: &str, template: &str) -> bool {
    js.match_indices(template).any(|(idx, _)| {
      idx > 0
        && matches!(
          js.as_bytes()[idx - 1],
          b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'$'
        )
    })
  }

  assert!(
    js.contains("[1,..."),
    "expected array literal with spread in output: {js}"
  );
  assert!(
    js.contains("{x:1,..."),
    "expected object literal with spread in output: {js}"
  );
  assert!(
    js.contains("/a+/"),
    "expected regex literal in output: {js}"
  );
  assert!(
    js.contains("`t${1}x`"),
    "expected template literal in output: {js}"
  );
  assert!(
    contains_tagged_template(js, "`a${1}b`"),
    "expected tagged template literal in output: {js}"
  );
  assert!(
    js.contains("new Array(1)"),
    "expected new expression in output: {js}"
  );
  assert!(
    js.contains("delete"),
    "expected delete operator in output: {js}"
  );
  assert!(
    js.contains("\"prop\"in"),
    "expected `in` operator in output: {js}"
  );
  assert!(
    js.contains("instanceof"),
    "expected `instanceof` operator in output: {js}"
  );
}
