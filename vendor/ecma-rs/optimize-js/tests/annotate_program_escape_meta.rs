use optimize_js::analysis::annotate_program;
use optimize_js::analysis::escape::EscapeState;
use optimize_js::compile_source;
use optimize_js::il::inst::InstTyp;
use optimize_js::TopLevelMode;

fn find_object_alloc<'a>(
  cfg: &'a optimize_js::cfg::cfg::Cfg,
) -> Option<&'a optimize_js::il::inst::Inst> {
  cfg
    .bblocks
    .all()
    .flat_map(|(_, block)| block.iter())
    .find(|inst| {
      inst.t == InstTyp::ObjectLit
    })
}

#[test]
fn annotate_program_populates_escape_metadata() {
  let mut program = compile_source(
    r#"
      let f = () => {
        const a = {};
        return a;
      };
    "#,
    TopLevelMode::Module,
    false,
  )
  .expect("compile");

  let _analyses = annotate_program(&mut program);

  assert_eq!(program.functions.len(), 1, "expected one nested function");
  let func = &program.functions[0];
  let alloc = find_object_alloc(func.analyzed_cfg()).expect("object allocation should exist");
  assert_eq!(
    alloc.meta.result_escape,
    Some(EscapeState::ReturnEscape),
    "expected returned object allocation to be marked as ReturnEscape"
  );
}
