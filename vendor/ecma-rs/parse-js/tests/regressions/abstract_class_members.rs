use parse_js::ast::stmt::Stmt;
use parse_js::{parse_with_options, Dialect, ParseOptions, SourceType};

#[test]
fn abstract_class_does_not_mark_all_members_as_abstract() {
  let src = r#"
    abstract class C {
      bar() {}
      abstract foo(): void;
      abstract x: number;
      y: number;
    }
  "#;

  let ast = parse_with_options(
    src,
    ParseOptions {
      dialect: Dialect::Ts,
      source_type: SourceType::Module,
    },
  )
  .expect("input should parse");

  assert_eq!(ast.stx.body.len(), 1);
  let Stmt::ClassDecl(class_decl) = ast.stx.body[0].stx.as_ref() else {
    panic!("expected class decl");
  };
  assert!(class_decl.stx.abstract_);

  // `abstract class` does not imply `abstract` on every member.
  let members = &class_decl.stx.members;
  assert_eq!(members.len(), 4);

  let bar = &members[0];
  assert!(!bar.stx.abstract_, "bar() should not be marked abstract");

  let foo = &members[1];
  assert!(foo.stx.abstract_, "abstract foo() should be marked abstract");

  let x = &members[2];
  assert!(x.stx.abstract_, "abstract x should be marked abstract");

  let y = &members[3];
  assert!(!y.stx.abstract_, "y should not be marked abstract");
}

