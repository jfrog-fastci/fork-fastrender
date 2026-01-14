use crate::lex::Lexer;
use crate::parse::Parser;
use crate::operator::OperatorName;
use crate::{Dialect, ParseOptions, SourceType};

#[test]
fn await_binding_identifier_is_syntax_error_in_static_block() {
  let src = r#"
    class C {
      static {
        class await {}
      }
    }
  "#;
  let opts = ParseOptions {
    dialect: Dialect::Ecma,
    source_type: SourceType::Script,
  };
  let mut parser = Parser::new(Lexer::new(src), opts);
  let res = parser.parse_top_level();
  assert!(res.is_err(), "parse unexpectedly succeeded: {res:?}");
}

#[test]
fn await_binding_identifier_is_allowed_in_arrow_func_body_in_static_block() {
  let src = r#"
    class C {
      static {
        (() => { class await {} });
      }
    }
  "#;
  let opts = ParseOptions {
    dialect: Dialect::Ecma,
    source_type: SourceType::Script,
  };
  let mut parser = Parser::new(Lexer::new(src), opts);
  let res = parser.parse_top_level();
  assert!(res.is_ok(), "parse failed: {res:?}");
}

#[test]
fn await_binding_identifier_is_syntax_error_in_static_block_lexical_decl() {
  let src = r#"
    class C {
      static {
        let await = 0;
      }
    }
  "#;
  let opts = ParseOptions {
    dialect: Dialect::Ecma,
    source_type: SourceType::Script,
  };
  let mut parser = Parser::new(Lexer::new(src), opts);
  let res = parser.parse_top_level();
  assert!(res.is_err(), "parse unexpectedly succeeded: {res:?}");
}

#[test]
fn await_identifier_reference_is_syntax_error_in_static_block() {
  let src = r#"
    class C {
      static {
        await;
      }
    }
  "#;
  let opts = ParseOptions {
    dialect: Dialect::Ecma,
    source_type: SourceType::Script,
  };
  let mut parser = Parser::new(Lexer::new(src), opts);
  let res = parser.parse_top_level();
  assert!(res.is_err(), "parse unexpectedly succeeded: {res:?}");
}

#[test]
fn await_is_allowed_as_identifier_in_nested_function_in_static_block() {
  let src = r#"
    class C {
      static {
        function f(await) {
          return await;
        }
      }
    }
  "#;
  let opts = ParseOptions {
    dialect: Dialect::Ecma,
    source_type: SourceType::Script,
  };
  let mut parser = Parser::new(Lexer::new(src), opts);
  let res = parser.parse_top_level();
  assert!(res.is_ok(), "parse failed: {res:?}");
}

#[test]
fn await_is_syntax_error_as_arrow_param_in_static_block() {
  let src = r#"
    class C {
      static {
        (await => 1);
      }
    }
  "#;
  let opts = ParseOptions {
    dialect: Dialect::Ecma,
    source_type: SourceType::Script,
  };
  let mut parser = Parser::new(Lexer::new(src), opts);
  let res = parser.parse_top_level();
  assert!(res.is_err(), "parse unexpectedly succeeded: {res:?}");
}

#[test]
fn return_in_static_block_is_syntax_error_even_inside_function() {
  let src = r#"
    function f() {
      class C {
        static {
          return;
        }
      }
    }
  "#;
  let opts = ParseOptions {
    dialect: Dialect::Ecma,
    source_type: SourceType::Script,
  };
  let mut parser = Parser::new(Lexer::new(src), opts);
  let res = parser.parse_top_level();
  assert!(res.is_err(), "parse unexpectedly succeeded: {res:?}");
}

#[test]
fn yield_in_static_block_is_syntax_error_even_inside_generator() {
  let src = r#"
     function *g() {
       class C {
        static {
          yield;
        }
      }
    }
  "#;
  let opts = ParseOptions {
    dialect: Dialect::Ecma,
    source_type: SourceType::Script,
  };
  let mut parser = Parser::new(Lexer::new(src), opts);
  let res = parser.parse_top_level();
  assert!(res.is_err(), "parse unexpectedly succeeded: {res:?}");
}

#[test]
fn await_expression_parses_in_static_block() {
  use crate::ast::class_or_object::ClassOrObjVal;
  use crate::ast::expr::Expr;
  use crate::ast::stmt::Stmt;
  use crate::num::JsNumber;
  let src = r#"
    class A {
      static {
        await 0;
      }
    }
  "#;
  let opts = ParseOptions {
    dialect: Dialect::Ecma,
    source_type: SourceType::Script,
  };
  let mut parser = Parser::new(Lexer::new(src), opts);
  let res = parser
    .parse_top_level()
    .unwrap_or_else(|err| panic!("parse failed unexpectedly: {err:?}"));

  let [class_stmt] = res.stx.body.as_slice() else {
    panic!("expected exactly one top-level statement");
  };
  let Stmt::ClassDecl(class_decl) = &*class_stmt.stx else {
    panic!("expected a class declaration statement");
  };
  let static_block = class_decl
    .stx
    .members
    .iter()
    .find_map(|member| match &member.stx.val {
      ClassOrObjVal::StaticBlock(block) => Some(block),
      _ => None,
    })
    .expect("expected a static initialization block member");

  let [stmt] = static_block.stx.body.as_slice() else {
    panic!("expected exactly one statement in static block");
  };
  let Stmt::Expr(expr_stmt) = &*stmt.stx else {
    panic!("expected an expression statement in static block");
  };
  let Expr::Unary(unary) = &*expr_stmt.stx.expr.stx else {
    panic!("expected a unary expression statement");
  };
  assert_eq!(unary.stx.operator, OperatorName::Await);

  let Expr::LitNum(num) = &*unary.stx.argument.stx else {
    panic!("expected await operand to be a number literal");
  };
  assert_eq!(num.stx.value, JsNumber(0.0));
}

#[test]
fn await_expression_parses_in_static_block_inside_function() {
  let src = r#"
    function f() {
      class C {
        static {
          await 0;
        }
      }
    }
  "#;
  let opts = ParseOptions {
    dialect: Dialect::Ecma,
    source_type: SourceType::Script,
  };
  let mut parser = Parser::new(Lexer::new(src), opts);
  let res = parser.parse_top_level();
  assert!(res.is_ok(), "parse failed unexpectedly: {res:?}");
}

#[test]
fn arguments_identifier_reference_parses_in_static_block() {
  let src = r#"
    class C {
      static {
        arguments;
      }
    }
  "#;
  let opts = ParseOptions {
    dialect: Dialect::Ecma,
    source_type: SourceType::Script,
  };
  let mut parser = Parser::new(Lexer::new(src), opts);
  let res = parser.parse_top_level();
  assert!(res.is_ok(), "parse failed: {res:?}");
}

#[test]
fn arguments_identifier_reference_parses_in_arrow_in_static_block() {
  let src = r#"
    class C {
      static {
        (() => arguments);
      }
    }
  "#;
  let opts = ParseOptions {
    dialect: Dialect::Ecma,
    source_type: SourceType::Script,
  };
  let mut parser = Parser::new(Lexer::new(src), opts);
  let res = parser.parse_top_level();
  assert!(res.is_ok(), "parse failed: {res:?}");
}

#[test]
fn arguments_identifier_reference_is_allowed_in_function_in_static_block() {
  let src = r#"
    class C {
      static {
        (function({x = arguments}) {
          arguments;
        });
      }
    }
  "#;
  let opts = ParseOptions {
    dialect: Dialect::Ecma,
    source_type: SourceType::Script,
  };
  let mut parser = Parser::new(Lexer::new(src), opts);
  let res = parser.parse_top_level();
  assert!(res.is_ok(), "parse failed: {res:?}");
}

#[test]
fn for_await_of_parses_in_static_block() {
  let src = r#"
    class C {
      static {
        for await (const x of []) {}
      }
    }
  "#;
  let opts = ParseOptions {
    dialect: Dialect::Ecma,
    source_type: SourceType::Script,
  };
  let mut parser = Parser::new(Lexer::new(src), opts);
  let res = parser.parse_top_level();
  assert!(res.is_ok(), "parse failed: {res:?}");
}
