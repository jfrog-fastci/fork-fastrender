use super::ParseCtx;
use super::Parser;
use crate::ast::expr::pat::ClassOrFuncName;
use crate::ast::expr::pat::Pat;
use crate::ast::expr::Expr;
use crate::ast::node::Node;
use crate::ast::stmt::decl::Accessibility;
use crate::ast::stmt::decl::ParamDecl;
use crate::ast::stmt::Stmt;
use crate::error::SyntaxErrorType;
use crate::error::SyntaxResult;
use crate::loc::Loc;
use crate::token::TT;
use std::collections::HashSet;

impl<'a> Parser<'a> {
  // `scope` should be a newly created closure scope for this function.
  pub fn func_params(&mut self, ctx: ParseCtx) -> SyntaxResult<Vec<Node<ParamDecl>>> {
    self.with_arguments_bound_in_class_init(|p| p.func_params_impl(ctx, true))
  }

  /// Parse arrow function parameter lists.
  ///
  /// Arrow functions do *not* introduce their own `new.target` binding; they can only reference
  /// `new.target` when it is provided by an enclosing non-arrow function or class element.
  pub fn arrow_func_params(&mut self, ctx: ParseCtx) -> SyntaxResult<Vec<Node<ParamDecl>>> {
    self.func_params_impl(ctx, false)
  }

  fn func_params_impl(
    &mut self,
    ctx: ParseCtx,
    introduces_new_target: bool,
  ) -> SyntaxResult<Vec<Node<ParamDecl>>> {
    // `new.target` is syntactically allowed inside parameter initializers when a `new.target`
    // binding exists (functions and class elements).
    let prev_new_target_allowed = self.new_target_allowed;
    let prev_arguments_allowed = self.arguments_allowed;
    if introduces_new_target {
      self.new_target_allowed += 1;
      // Non-arrow functions introduce an `arguments` binding that should be visible to parameter
      // initializers.
      self.arguments_allowed += 1;
    }
    fn parse_params_inner(p: &mut Parser<'_>, ctx: ParseCtx) -> SyntaxResult<Vec<Node<ParamDecl>>> {
      p.require(TT::ParenthesisOpen)?;
      let mut parameters = Vec::new();
      let mut is_first = true;
      if p.consume_if(TT::ParenthesisClose).is_match() {
        return Ok(parameters);
      }

      loop {
        let param = p.with_loc(|p| {
          // TypeScript: check for `this` parameter (can only be first parameter)
          // Syntax: `this: Type`
          if is_first && !p.is_strict_ecmascript() && p.peek().typ == TT::KeywordThis {
            let [_, next] = p.peek_n::<2>();
            if next.typ == TT::Colon {
              // This is a `this` parameter.
              p.consume(); // consume 'this'
              p.require(TT::Colon)?;
              let type_annotation = Some(p.type_expr(ctx)?);
              // Create a pseudo-pattern for the `this` parameter.
              use crate::ast::expr::pat::IdPat;
              use crate::ast::expr::pat::Pat;
              use crate::ast::stmt::decl::PatDecl;
              use crate::loc::Loc;
              let this_pattern = Node::new(
                Loc(0, 0),
                PatDecl {
                  pat: Node::new(
                    Loc(0, 0),
                    Pat::Id(Node::new(
                      Loc(0, 0),
                      IdPat {
                        name: String::from("this"),
                      },
                    )),
                  ),
                },
              );
              return Ok(ParamDecl {
                decorators: Vec::new(),
                rest: false,
                optional: false,
                accessibility: None,
                readonly: false,
                pattern: this_pattern,
                type_annotation,
                default_value: None,
              });
            }
          }

          // TypeScript: parse decorators for parameters (before modifiers).
          let mut decorators = if p.is_typescript() {
            p.decorators(ctx)?
          } else {
            Vec::new()
          };

          // TypeScript: accessibility modifiers and readonly can appear in either order
          // e.g. `readonly public x` or `public readonly x` are both valid
          // Error recovery: allow duplicate modifiers
          let mut accessibility = None;
          let mut readonly = false;

          // Parse modifiers in a loop to allow duplicates.
          loop {
            let mut found_modifier = false;

            // Try readonly.
            if p.consume_if(TT::KeywordReadonly).is_match() {
              readonly = true;
              found_modifier = true;
            }

            // Try accessibility.
            if p.consume_if(TT::KeywordPublic).is_match() {
              accessibility = Some(Accessibility::Public);
              found_modifier = true;
            } else if p.consume_if(TT::KeywordPrivate).is_match() {
              accessibility = Some(Accessibility::Private);
              found_modifier = true;
            } else if p.consume_if(TT::KeywordProtected).is_match() {
              accessibility = Some(Accessibility::Protected);
              found_modifier = true;
            }

            if !found_modifier {
              break;
            }
          }

          // TypeScript: Also allow decorators after modifiers for error recovery
          // e.g. `public @dec p` in addition to `@dec public p`
          if p.is_typescript() && p.peek().typ == TT::At {
            let post_modifiers_decorators = p.decorators(ctx)?;
            decorators.extend(post_modifiers_decorators);
          }

          let rest = p.consume_if(TT::DotDotDot).is_match();
          let pattern = p.pat_decl(ctx)?;

          // TypeScript: optional parameter.
          let optional = !p.is_strict_ecmascript() && p.consume_if(TT::Question).is_match();
          if rest && optional {
            return Err(p.peek().error(SyntaxErrorType::ExpectedSyntax(
              "rest parameters cannot be optional",
            )));
          }

          // TypeScript: type annotation.
          let type_annotation = if !p.is_strict_ecmascript() && p.consume_if(TT::Colon).is_match() {
            Some(p.type_expr(ctx)?)
          } else {
            None
          };

          let default_value = if rest {
            if p.peek().typ == TT::Equals {
              return Err(p.peek().error(SyntaxErrorType::ExpectedSyntax(
                "rest parameters cannot have initializers",
              )));
            }
            None
          } else {
            p.consume_if(TT::Equals)
              .and_then(|| p.expr(ctx, [TT::Comma, TT::ParenthesisClose]))?
          };

          Ok(ParamDecl {
            decorators,
            rest,
            optional,
            accessibility,
            readonly,
            pattern,
            type_annotation,
            default_value,
          })
        })?;

        let is_rest = param.stx.rest;
        parameters.push(param);

        if is_rest {
          // Rest parameters must be last and cannot have a trailing comma.
          if p.peek().typ != TT::ParenthesisClose {
            return Err(p.peek().error(SyntaxErrorType::ExpectedSyntax("`)`")));
          }
          p.require(TT::ParenthesisClose)?;
          break;
        }

        if p.consume_if(TT::Comma).is_match() {
          if p.peek().typ == TT::ParenthesisClose {
            p.consume();
            break;
          }
        } else {
          p.require(TT::ParenthesisClose)?;
          break;
        }

        is_first = false;
      }

        Ok(parameters)
    }

    let res = if introduces_new_target {
      self.with_arguments_bound_in_class_init(|p| parse_params_inner(p, ctx))
    } else {
      parse_params_inner(self, ctx)
    };
    self.new_target_allowed = prev_new_target_allowed;
    self.arguments_allowed = prev_arguments_allowed;
    res
  }

  pub fn parse_func_block_body(&mut self, ctx: ParseCtx) -> SyntaxResult<Vec<Node<Stmt>>> {
    let prev_in_function = self.in_function;
    let prev_in_iteration = self.in_iteration;
    let prev_in_switch = self.in_switch;
    let prev_labels = std::mem::take(&mut self.labels);
    self.in_function += 1;
    self.in_iteration = 0;
    self.in_switch = 0;
    let res = (|| {
      self.require(TT::BraceOpen)?;
      let body = self.stmts(ctx.non_top_level(), TT::BraceClose)?;
      self.require(TT::BraceClose)?;
      Ok(body)
    })();
    self.in_function = prev_in_function;
    self.in_iteration = prev_in_iteration;
    self.in_switch = prev_in_switch;
    self.labels = prev_labels;
    res
  }

  pub fn parse_non_arrow_func_block_body(
    &mut self,
    ctx: ParseCtx,
  ) -> SyntaxResult<Vec<Node<Stmt>>> {
    let prev_new_target_allowed = self.new_target_allowed;
    let prev_arguments_allowed = self.arguments_allowed;
    let prev_super_prop_allowed = self.super_prop_allowed;
    let prev_super_call_allowed = self.super_call_allowed;
    self.new_target_allowed += 1;
    self.arguments_allowed += 1;
    // Regular functions do not have a `super` binding.
    self.super_prop_allowed = 0;
    self.super_call_allowed = 0;
    let res = self.with_arguments_bound_in_class_init(|p| p.parse_func_block_body(ctx));
    self.new_target_allowed = prev_new_target_allowed;
    self.arguments_allowed = prev_arguments_allowed;
    self.super_prop_allowed = prev_super_prop_allowed;
    self.super_call_allowed = prev_super_call_allowed;
    res
  }

  pub fn parse_method_block_body(
    &mut self,
    ctx: ParseCtx,
    allow_super_call: bool,
  ) -> SyntaxResult<Vec<Node<Stmt>>> {
    let prev_new_target_allowed = self.new_target_allowed;
    let prev_arguments_allowed = self.arguments_allowed;
    let prev_super_prop_allowed = self.super_prop_allowed;
    let prev_super_call_allowed = self.super_call_allowed;
    self.new_target_allowed += 1;
    self.arguments_allowed += 1;
    self.super_prop_allowed += 1;
    if allow_super_call {
      self.super_call_allowed += 1;
    } else {
      // `super()` is only valid in derived constructors (and arrow functions nested
       // within them); it is never valid in methods/fields/static blocks.
      self.super_call_allowed = 0;
    }
    // Methods are non-arrow functions and have their own `arguments` binding.
    let res = self.with_arguments_bound_in_class_init(|p| p.parse_func_block_body(ctx));
    self.new_target_allowed = prev_new_target_allowed;
    self.arguments_allowed = prev_arguments_allowed;
    self.super_prop_allowed = prev_super_prop_allowed;
    self.super_call_allowed = prev_super_call_allowed;
    res
  }

  pub(crate) fn is_simple_parameter_list(params: &[Node<ParamDecl>]) -> bool {
    params.iter().all(|param| {
      if param.stx.rest || param.stx.default_value.is_some() {
        return false;
      }
      matches!(param.stx.pattern.stx.pat.stx.as_ref(), Pat::Id(_))
    })
  }

  fn collect_bound_names_from_pat(pat: &Node<Pat>, out: &mut Vec<(String, Loc)>) {
    match pat.stx.as_ref() {
      Pat::Id(id) => out.push((id.stx.name.clone(), id.loc)),
      Pat::Arr(arr) => {
        for elem in arr.stx.elements.iter() {
          if let Some(elem) = elem.as_ref() {
            Self::collect_bound_names_from_pat(&elem.target, out);
          }
        }
        if let Some(rest) = arr.stx.rest.as_ref() {
          Self::collect_bound_names_from_pat(rest, out);
        }
      }
      Pat::Obj(obj) => {
        for prop in obj.stx.properties.iter() {
          Self::collect_bound_names_from_pat(&prop.stx.target, out);
        }
        if let Some(rest) = obj.stx.rest.as_ref() {
          Self::collect_bound_names_from_pat(rest, out);
        }
      }
      Pat::AssignTarget(expr) => {
        // Error recovery: treat identifier assignment targets as bound names.
        match expr.stx.as_ref() {
          Expr::Id(id) => out.push((id.stx.name.clone(), expr.loc)),
          Expr::IdPat(id) => out.push((id.stx.name.clone(), expr.loc)),
          _ => {}
        }
      }
    }
  }

  pub(crate) fn validate_formal_parameters(
    &self,
    name: Option<&Node<ClassOrFuncName>>,
    params: &[Node<ParamDecl>],
    simple: bool,
    always_disallow_duplicates: bool,
  ) -> SyntaxResult<()> {
    if !self.is_strict_ecmascript() {
      return Ok(());
    }

    let mut bound_names = Vec::new();
    for param in params {
      Self::collect_bound_names_from_pat(&param.stx.pattern.stx.pat, &mut bound_names);
    }

    if always_disallow_duplicates || self.is_strict_mode() || !simple {
      let mut seen: HashSet<String> = HashSet::new();
      for (name, loc) in bound_names.iter() {
        if !seen.insert(name.clone()) {
          return Err(loc.error(
            SyntaxErrorType::ExpectedSyntax("unique parameter names"),
            None,
          ));
        }
      }
    }

    if self.is_strict_mode() {
      if let Some(name) = name {
        self.validate_strict_binding_identifier_name(name.loc, &name.stx.name)?;
      }
      for (name, loc) in bound_names.iter() {
        self.validate_strict_binding_identifier_name(*loc, name)?;
      }
    }

    Ok(())
  }
}
