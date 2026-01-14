use super::ParseCtx;
use super::Parser;
use crate::ast::expr::pat::Pat;
use crate::ast::expr::Expr;
use crate::ast::func::Func;
use crate::ast::node::ParenthesizedExpr;
use crate::ast::node::Node;
use crate::ast::stmt::decl::ClassDecl;
use crate::ast::stmt::decl::FuncDecl;
use crate::ast::stmt::decl::PatDecl;
use crate::ast::stmt::decl::VarDecl;
use crate::ast::stmt::decl::VarDeclMode;
use crate::ast::stmt::decl::VarDeclarator;
use crate::error::SyntaxErrorType;
use crate::error::SyntaxResult;
use crate::operator::OperatorName;
use crate::parse::expr::pat::is_valid_class_or_func_name;
use crate::parse::expr::pat::ParsePatternRules;
use crate::parse::expr::Asi;
use crate::parse::AsiContext;
use crate::token::TT;
use crate::token::keyword_from_str;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum VarDeclParseMode {
  // Standard parsing mode for var/let/const statement.
  Asi,
  // Parse as many valid declarators as possible, then break before the first invalid token (i.e. not a comma). Used by for-loop parser.
  Leftmost,
}

impl<'a> Parser<'a> {
  pub fn pat_decl(&mut self, ctx: ParseCtx) -> SyntaxResult<Node<PatDecl>> {
    self.with_loc(|p| {
      let pat = p.pat(ctx)?;
      Ok(PatDecl { pat })
    })
  }

  pub fn id_pat_decl(&mut self, ctx: ParseCtx) -> SyntaxResult<Node<PatDecl>> {
    self.with_loc(|p| {
      let pat = p.id_pat(ctx)?.into_wrapped();
      Ok(PatDecl { pat })
    })
  }

  pub fn var_decl_mode(&mut self) -> SyntaxResult<VarDeclMode> {
    let t = self.consume();
    Ok(match t.typ {
      TT::KeywordLet => VarDeclMode::Let,
      TT::KeywordConst => VarDeclMode::Const,
      TT::KeywordVar => VarDeclMode::Var,
      TT::KeywordUsing => VarDeclMode::Using,
      TT::KeywordAwait => {
        // Check if followed by 'using'
        if self.peek().typ == TT::KeywordUsing {
          self.consume(); // consume 'using'
          VarDeclMode::AwaitUsing
        } else {
          return Err(t.error(SyntaxErrorType::ExpectedSyntax("variable declaration")));
        }
      }
      _ => return Err(t.error(SyntaxErrorType::ExpectedSyntax("variable declaration"))),
    })
  }

  /// Parses a variable declaration, which contains one or more declarators, each with an optional initializer. Examples of variable declarations:
  /// - `const a = 1`
  /// - `let a, b = 2, c`
  /// - `let a = 1, b = 2`
  /// - `var a`
  /// - `var a, b`
  pub fn var_decl(
    &mut self,
    ctx: ParseCtx,
    parse_mode: VarDeclParseMode,
  ) -> SyntaxResult<Node<VarDecl>> {
    self.with_loc(|p| {
      let export = p.consume_if(TT::KeywordExport).is_match();
      let mode = p.var_decl_mode()?;
      let mut declarators = Vec::new();
      loop {
        // Explicit Resource Management declarations (`using` / `await using`) only allow a
        // BindingIdentifier (not a BindingPattern).
        let pattern = match mode {
          VarDeclMode::Using | VarDeclMode::AwaitUsing => p.id_pat_decl(ctx)?,
          _ => p.pat_decl(ctx)?,
        };

        // TypeScript: definite assignment assertion
        let definite_assignment =
          !p.is_strict_ecmascript() && p.consume_if(TT::Exclamation).is_match();

        // TypeScript: type annotation
        // Note: We use type_expr_or_predicate for error recovery - type predicates
        // are semantically invalid in variable declarations but should parse
        let type_annotation = if !p.is_strict_ecmascript() && p.consume_if(TT::Colon).is_match() {
          Some(p.type_expr_or_predicate(ctx)?)
        } else {
          None
        };

        let mut asi = match parse_mode {
          VarDeclParseMode::Asi => Asi::can(),
          VarDeclParseMode::Leftmost => Asi::no(),
        };
        let initializer = if parse_mode == VarDeclParseMode::Leftmost
          && matches!(ctx.asi, AsiContext::StatementHeader)
        {
          p.consume_if(TT::Equals).and_then(|| {
            p.expr_with_asi(
              ctx,
              [TT::Semicolon, TT::Comma, TT::KeywordIn, TT::KeywordOf],
              &mut asi,
            )
          })?
        } else {
          p.consume_if(TT::Equals)
            .and_then(|| p.expr_with_asi(ctx, [TT::Semicolon, TT::Comma], &mut asi))?
        };

        if p.is_strict_ecmascript() {
          // Destructuring declarations require an initializer, except in `for (... in/of ...)`
          // headers where the binding is initialised by the loop.
          let in_for_in_of_header = parse_mode == VarDeclParseMode::Leftmost
            && matches!(ctx.asi, AsiContext::StatementHeader)
            && matches!(p.peek().typ, TT::KeywordIn | TT::KeywordOf);
          if initializer.is_none() && !in_for_in_of_header {
            match mode {
              VarDeclMode::Const | VarDeclMode::Using | VarDeclMode::AwaitUsing => {
                return Err(pattern.loc.error(SyntaxErrorType::ExpectedSyntax("initializer"), None));
              }
              VarDeclMode::Let | VarDeclMode::Var => {
                if !matches!(pattern.stx.pat.stx.as_ref(), Pat::Id(_)) {
                  return Err(
                    pattern
                      .loc
                      .error(SyntaxErrorType::ExpectedSyntax("initializer"), None),
                  );
                }
              }
            }
          }
        }
        declarators.push(VarDeclarator {
          pattern,
          definite_assignment,
          type_annotation,
          initializer,
        });
        match parse_mode {
          VarDeclParseMode::Asi => {
            if p.consume_if(TT::Semicolon).is_match() || asi.did_end_with_asi {
              break;
            }
            let t = p.peek();
            if t.typ == TT::EOF
              || t.typ == TT::BraceClose
              || (t.preceded_by_line_terminator && t.typ != TT::Comma)
            {
              break;
            };
            p.require(TT::Comma)?;
          }
          VarDeclParseMode::Leftmost => {
            if !p.consume_if(TT::Comma).is_match() {
              break;
            }
          }
        }
      }
      Ok(VarDecl {
        export,
        mode,
        declarators,
      })
    })
  }

  pub fn func_decl(&mut self, ctx: ParseCtx) -> SyntaxResult<Node<FuncDecl>> {
    let prev_disallow_arguments_in_class_init = self.disallow_arguments_in_class_init;
    // Class field initializers and static initialization blocks disallow `arguments` identifier
    // references, but regular functions introduce their own `arguments` binding. Disable the
    // check while parsing the function declaration's parameters and body.
    self.disallow_arguments_in_class_init = 0;
    let res = self.with_loc(|p| {
      let export = p.consume_if(TT::KeywordExport).is_match();
      let export_default = export && p.consume_if(TT::KeywordDefault).is_match();
      let is_async = p.consume_if(TT::KeywordAsync).is_match();
      let start = p.require(TT::KeywordFunction)?.loc;
      let generator = p.consume_if(TT::Asterisk).is_match();
      let is_module = p.is_module();
      let name = p.maybe_class_or_func_name(ctx);
      // The name can only be omitted in default exports.
      if name.is_none() && !export_default {
        return Err(start.error(SyntaxErrorType::ExpectedSyntax("function name"), None));
      };
      if let Some(name) = name.as_ref() {
        if let Some(keyword_tt) = keyword_from_str(&name.stx.name) {
          if !is_valid_class_or_func_name(keyword_tt, ctx.rules) {
            return Err(name.error(SyntaxErrorType::ExpectedSyntax("identifier")));
          }
        }
      }
      let function = p.with_loc(|p| {
        // TypeScript: generic type parameters
        let type_parameters = if !p.is_strict_ecmascript()
          && p.peek().typ == TT::ChevronLeft
          && p.is_start_of_type_arguments()
        {
          Some(p.type_parameters(ctx)?)
        } else {
          None
        };
        // Parameters and body use the function's own context, not the parent's
        let fn_ctx = ctx.with_rules(ParsePatternRules {
          await_allowed: if is_module { false } else { !is_async },
          yield_allowed: if is_module { false } else { !generator },
          await_expr_allowed: is_async,
          yield_expr_allowed: generator,
        });
        // Regular functions do not have a `super` binding. Ensure we don't inherit
        // `super` allowances from an enclosing method/constructor when parsing
        // parameter initializers.
        let prev_super_prop_allowed = p.super_prop_allowed;
        let prev_super_call_allowed = p.super_call_allowed;
        p.super_prop_allowed = 0;
        p.super_call_allowed = 0;
        let parameters = p.func_params(fn_ctx);
        p.super_prop_allowed = prev_super_prop_allowed;
        p.super_call_allowed = prev_super_call_allowed;
        let parameters = parameters?;
        // TypeScript: return type annotation (may be type predicate)
        let return_type = if !p.is_strict_ecmascript() && p.consume_if(TT::Colon).is_match() {
          Some(p.type_expr_or_predicate(ctx)?)
        } else {
          None
        };
        // TypeScript: function overload signatures have no body
        let body = if p.peek().typ == TT::BraceOpen {
          let contains_use_strict =
            p.is_strict_ecmascript() && p.has_use_strict_directive_in_block_body()?;
          let simple_params = Parser::is_simple_parameter_list(&parameters);
          if p.is_strict_ecmascript() && contains_use_strict && !simple_params {
            return Err(p.peek().error(SyntaxErrorType::ExpectedSyntax(
              "`use strict` directive not allowed with a non-simple parameter list",
            )));
          }

          let prev_strict_mode = p.strict_mode;
          if p.is_strict_ecmascript() && contains_use_strict && !p.is_strict_mode() {
            p.strict_mode += 1;
          }
          let res = (|| {
            p.validate_formal_parameters(name.as_ref(), &parameters, simple_params, false)?;
            p.parse_non_arrow_func_block_body(fn_ctx)
          })();
          p.strict_mode = prev_strict_mode;
          Some(res?.into())
        } else {
          if p.is_strict_ecmascript() {
            return Err(
              p.peek()
                .error(SyntaxErrorType::RequiredTokenNotFound(TT::BraceOpen)),
            );
          }
          // Overload signature - consume semicolon or allow ASI
          let _ = p.consume_if(TT::Semicolon);
          None
        };
        Ok(Func {
          arrow: false,
          async_: is_async,
          generator,
          type_parameters,
          parameters,
          return_type,
          body,
        })
      })?;
      Ok(FuncDecl {
        export,
        export_default,
        name,
        function,
      })
    });
    self.disallow_arguments_in_class_init = prev_disallow_arguments_in_class_init;
    res
  }

  pub fn class_decl(&mut self, ctx: ParseCtx) -> SyntaxResult<Node<ClassDecl>> {
    self.class_decl_impl(ctx, false)
  }

  pub fn class_decl_impl(&mut self, ctx: ParseCtx, declare: bool) -> SyntaxResult<Node<ClassDecl>> {
    self.with_loc(|p| {
      // TypeScript: parse decorators before export/class
      let decorators = p.decorators(ctx)?;

      let export = p.consume_if(TT::KeywordExport).is_match();
      let export_default = export && p.consume_if(TT::KeywordDefault).is_match();
      // TypeScript: abstract keyword
      let abstract_ = p.consume_if(TT::KeywordAbstract).is_match();
      let start = p.require(TT::KeywordClass)?.loc;

      let prev_strict_mode = p.strict_mode;
      if p.is_strict_ecmascript() {
        p.strict_mode += 1;
      }
      let res = (|| {
        // Names can be omitted only in default exports.
        let name = p.maybe_class_or_func_name(ctx);
        if name.is_none() && !export_default {
          return Err(start.error(SyntaxErrorType::ExpectedSyntax("class name"), None));
        };
        if let Some(name) = name.as_ref() {
          if let Some(keyword_tt) = keyword_from_str(&name.stx.name) {
            if !is_valid_class_or_func_name(keyword_tt, ctx.rules) {
              return Err(name.error(SyntaxErrorType::ExpectedSyntax("identifier")));
            }
          }
        }
        if let Some(name) = name.as_ref() {
          p.validate_strict_binding_identifier_name(name.loc, &name.stx.name)?;
        }

        // TypeScript: generic type parameters
        let type_parameters = if !p.is_strict_ecmascript()
          && p.peek().typ == TT::ChevronLeft
          && p.is_start_of_type_arguments()
        {
          Some(p.type_parameters(ctx)?)
        } else {
          None
        };

        // Unlike functions, classes are scoped to their block.
        let extends = if p.consume_if(TT::KeywordExtends).is_match() {
          // TypeScript: extends clause can have type arguments: class C<T> extends Base<T>
          // Parse expression, which will handle type arguments via expr_with_ts_type_args
          let expr = p.expr_with_ts_type_args(ctx, [TT::BraceOpen, TT::KeywordImplements])?;
          if p.is_strict_ecmascript() {
            let is_valid = if expr.assoc.get::<ParenthesizedExpr>().is_some() {
              true
            } else {
              match expr.stx.as_ref() {
                Expr::Id(_)
                | Expr::This(_)
                | Expr::Import(_)
                | Expr::ImportMeta(_)
                | Expr::Func(_)
                | Expr::Class(_)
                | Expr::Member(_)
                | Expr::ComputedMember(_)
                | Expr::Call(_)
                | Expr::TaggedTemplate(_)
                | Expr::LitArr(_)
                | Expr::LitObj(_)
                | Expr::LitNum(_)
                | Expr::LitStr(_)
                | Expr::LitBool(_)
                | Expr::LitNull(_)
                | Expr::LitBigInt(_)
                | Expr::LitRegex(_)
                | Expr::LitTemplate(_)
                | Expr::Instantiation(_) => true,
                Expr::Unary(unary) => unary.stx.operator == OperatorName::New,
                _ => false,
              }
            };
            if !is_valid {
              return Err(expr.loc.error(
                SyntaxErrorType::ExpectedSyntax("class heritage expression"),
                None,
              ));
            }
          }
          Some(expr)
        } else {
          None
        };

        // TypeScript: implements clause
        let mut implements = Vec::new();
        if p.consume_if(TT::KeywordImplements).is_match() {
          loop {
            // Parse as expression to allow optional chaining (A?.B) even though it's semantically invalid
            // TypeScript parser accepts this syntax and lets the type checker reject it
            implements.push(p.expr_with_ts_type_args(ctx, [TT::Comma, TT::BraceOpen])?);
            if !p.consume_if(TT::Comma).is_match() {
              break;
            }
          }
        }

        let is_derived_class = extends.is_some();
        let prev_class_depth = p.class_is_derived.len();
        p.class_is_derived.push(is_derived_class);
        // `abstract class` does not make all members abstract; only members explicitly marked
        // `abstract` should carry `ClassMember.abstract_ = true`.
        //
        // However, in `declare class` declarations, methods often omit bodies (they're ambient), so
        // we keep passing the `declare` flag as the "ambient" context for the class body parser.
        let members = p.class_body_with_context(ctx, declare);
        p.class_is_derived.truncate(prev_class_depth);
        let members = members?;
        Ok(ClassDecl {
          decorators,
          export,
          export_default,
          declare,
          abstract_,
          name,
          type_parameters,
          extends,
          implements,
          members,
        })
      })();
      p.strict_mode = prev_strict_mode;
      res
    })
  }
}
