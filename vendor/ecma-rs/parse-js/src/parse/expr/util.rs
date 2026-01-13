use crate::ast::class_or_object::ClassOrObjKey;
use crate::ast::class_or_object::ClassOrObjMemberDirectKey;
use crate::ast::class_or_object::ClassOrObjVal;
use crate::ast::class_or_object::ObjMember;
use crate::ast::class_or_object::ObjMemberType;
use crate::ast::expr::lit::LitArrElem;
use crate::ast::expr::lit::LitArrExpr;
use crate::ast::expr::lit::LitObjExpr;
use crate::ast::expr::pat::ArrPat;
use crate::ast::expr::pat::ArrPatElem;
use crate::ast::expr::pat::IdPat;
use crate::ast::expr::pat::ObjPat;
use crate::ast::expr::pat::ObjPatProp;
use crate::ast::expr::pat::Pat;
use crate::ast::expr::BinaryExpr;
use crate::ast::expr::Expr;
use crate::ast::node::Node;
use crate::ast::node::CoverInitializedName;
use crate::ast::node::ParenthesizedExpr;
use crate::ast::node::TrailingCommaAfterRestElement;
use crate::error::SyntaxErrorType;
use crate::error::SyntaxResult;
use crate::operator::OperatorName;
use crate::token::TT;

/// Converts a literal expression subtree into a pattern (assignment target).
/// `{ a: [b] }` could be an object literal or object pattern. This function is useful for when a pattern was misinterpreted as a literal expression, without needing to rewind and reparse.
pub fn lit_to_pat(node: Node<Expr>) -> SyntaxResult<Node<Pat>> {
  lit_to_pat_with_recover(node, true)
}

pub(crate) fn lit_to_pat_with_recover(node: Node<Expr>, recover: bool) -> SyntaxResult<Node<Pat>> {
  let loc = node.loc;

  // In strict ECMAScript mode, parenthesized expressions are never valid assignment targets.
  //
  // Examples that must be SyntaxErrors:
  // - `(a) = 1`
  // - `for ((a) of b) {}`
  // - `(obj.prop) = 1`
  //
  // We accept these in recovery mode so TypeScript-style parse errors can be surfaced by later
  // semantic validation.
  if !recover && node.assoc.get::<ParenthesizedExpr>().is_some() {
    return Err(loc.error(SyntaxErrorType::InvalidAssigmentTarget, None));
  }

  // TypeScript: Accept member expressions for error recovery, even with optional chaining.
  // Check for member expressions first (without moving the value).
  let is_member = match node.stx.as_ref() {
    Expr::Member(member) => {
      // Parenthesized expressions are never valid assignment targets in strict ECMAScript mode
      // (e.g. `(a) = 1`, `(obj.prop) = 1`).
      if !recover && node.assoc.get::<ParenthesizedExpr>().is_some() {
        return Err(loc.error(SyntaxErrorType::InvalidAssigmentTarget, None));
      }
      if !recover && member.stx.optional_chaining {
        return Err(loc.error(SyntaxErrorType::InvalidAssigmentTarget, None));
      }
      true
    }
    _ => false,
  };
  if is_member {
    return Ok(Node::new(loc, Pat::AssignTarget(node)));
  }
  let is_computed_member = match node.stx.as_ref() {
    Expr::ComputedMember(member) => {
      if !recover && node.assoc.get::<ParenthesizedExpr>().is_some() {
        return Err(loc.error(SyntaxErrorType::InvalidAssigmentTarget, None));
      }
      if !recover && member.stx.optional_chaining {
        return Err(loc.error(SyntaxErrorType::InvalidAssigmentTarget, None));
      }
      true
    }
    _ => false,
  };
  if is_computed_member {
    return Ok(Node::new(loc, Pat::AssignTarget(node)));
  }

  match *node.stx {
    Expr::LitArr(n) => {
      // Parenthesized array literals cannot be assignment targets.
      if !recover && node.assoc.get::<ParenthesizedExpr>().is_some() {
        return Err(loc.error(SyntaxErrorType::InvalidAssigmentTarget, None));
      }
      // `[...x,]` is valid as an array literal but invalid as an assignment/binding
      // pattern (rest elements must not have a trailing comma). The literal parser
      // records this so we can reject it during cover grammar conversion.
      if !recover && n.assoc.get::<TrailingCommaAfterRestElement>().is_some() {
        return Err(loc.error(SyntaxErrorType::InvalidAssigmentTarget, None));
      }
      let LitArrExpr { elements } = *n.stx;
      let mut pat_elements = Vec::<Option<ArrPatElem>>::new();
      let mut rest = None;
      for element in elements {
        if rest.is_some() {
          return Err(loc.error(SyntaxErrorType::InvalidAssigmentTarget, None));
        };
        match element {
          LitArrElem::Single(elem) => {
            match *elem.stx {
              Expr::Binary(n) => {
                // Destructuring assignment/binding patterns do not permit parentheses around
                // an assignment pattern element (e.g. `[(a = 0)] = 1`).
                if !recover && elem.assoc.get::<ParenthesizedExpr>().is_some() {
                  return Err(elem.loc.error(SyntaxErrorType::InvalidAssigmentTarget, None));
                }
                let BinaryExpr {
                  operator,
                  left,
                  right,
                } = *n.stx;
                if operator != OperatorName::Assignment {
                  return Err(loc.error(SyntaxErrorType::InvalidAssigmentTarget, None));
                };
                pat_elements.push(Some(ArrPatElem {
                  target: lit_to_pat_with_recover(left, recover)?,
                  default_value: Some(right),
                }));
              }
              _ => pat_elements.push(Some(ArrPatElem {
                target: lit_to_pat_with_recover(elem, recover)?,
                default_value: None,
              })),
            };
          }
          LitArrElem::Rest(expr) => {
            rest = Some(lit_to_pat_with_recover(expr, recover)?);
          }
          LitArrElem::Empty => pat_elements.push(None),
        };
      }
      Ok(
        Node::new(
          loc,
          ArrPat {
            elements: pat_elements,
            rest,
          },
        )
        .into_wrapped(),
      )
    }
    Expr::LitObj(n) => {
      // Parenthesized object literals cannot be assignment targets.
      if !recover && node.assoc.get::<ParenthesizedExpr>().is_some() {
        return Err(loc.error(SyntaxErrorType::InvalidAssigmentTarget, None));
      }
      let LitObjExpr { members } = *n.stx;
      let mut properties = Vec::new();
      let mut rest: Option<Node<Pat>> = None;
      for member in members {
        let loc = member.loc;
        if rest.is_some() {
          return Err(loc.error(SyntaxErrorType::InvalidAssigmentTarget, None));
        };
        let ObjMember { typ } = *member.stx;
        match typ {
          ObjMemberType::Valued { key, val: value } => {
            // Preserve syntactic shorthand (`{ a }` / `{ a = 1 }`) vs. `key: value` object pattern
            // properties when converting from the object-literal cover grammar.
            //
            // `ObjMemberType::Valued` is used for both:
            // - `key: value` members, and
            // - shorthand members with default initializers (`{ a = 1 }`), which are parsed as a
            //   synthetic assignment expression tagged with `CoverInitializedName`.
            //
            // Downstream emit/minify passes rely on `ObjPatProp.shorthand` to decide whether to emit
            // the `: <target>` portion, so it must be accurate.
            let shorthand = matches!(
              &value,
              ClassOrObjVal::Prop(Some(initializer))
                if initializer.assoc.get::<CoverInitializedName>().is_some()
            );
            let (target, default_value) = match value {
              ClassOrObjVal::Prop(Some(initializer)) => match *initializer.stx {
                Expr::Binary(n) => {
                  if !recover && initializer.assoc.get::<ParenthesizedExpr>().is_some() {
                    return Err(
                      initializer
                        .loc
                        .error(SyntaxErrorType::InvalidAssigmentTarget, None),
                    );
                  }
                  let BinaryExpr {
                    operator,
                    left,
                    right,
                  } = *n.stx;
                  if operator != OperatorName::Assignment {
                    return Err(loc.error(SyntaxErrorType::InvalidAssigmentTarget, None));
                  };
                  (lit_to_pat_with_recover(left, recover)?, Some(right))
                }
                _ => (lit_to_pat_with_recover(initializer, recover)?, None),
              },
              _ => return Err(loc.error(SyntaxErrorType::InvalidAssigmentTarget, None)),
            };
            properties.push(Node::new(
              loc,
              ObjPatProp {
                key,
                target,
                default_value,
                shorthand,
              },
            ));
          }
          ObjMemberType::Shorthand { id } => {
            properties.push(Node::new(
              loc,
              ObjPatProp {
                key: ClassOrObjKey::Direct(id.derive_stx(|id| ClassOrObjMemberDirectKey {
                  key: id.name.clone(),
                  tt: TT::Identifier,
                })),
                target: id
                  .derive_stx(|id| IdPat {
                    name: id.name.clone(),
                  })
                  .into_wrapped(),
                default_value: None,
                shorthand: true,
              },
            ));
          }
          ObjMemberType::Rest { val: value } => {
            if recover {
              // TypeScript: For error recovery, allow any pattern in rest position
              // e.g., `{...{}}` or `{...[]}`.
              rest = Some(lit_to_pat_with_recover(value, recover)?);
            } else {
              // Rest properties must be assignment targets (not patterns) in
              // strict ECMAScript mode.
              let rest_target = match value.stx.as_ref() {
                Expr::Id(_) => lit_to_pat_with_recover(value, recover)?,
                Expr::Member(member) if !member.stx.optional_chaining => {
                  Node::new(loc, Pat::AssignTarget(value))
                }
                Expr::ComputedMember(member) if !member.stx.optional_chaining => {
                  Node::new(loc, Pat::AssignTarget(value))
                }
                _ => return Err(loc.error(SyntaxErrorType::InvalidAssigmentTarget, None)),
              };
              rest = Some(rest_target);
            }
          }
        };
      }
      Ok(Node::new(loc, ObjPat { properties, rest }).into_wrapped())
    }
    Expr::Id(n) => {
      // Parenthesized identifiers are not valid assignment targets in strict ECMAScript mode
      // (e.g. `(a) = 1`, `for ((a) of b) {}`).
      if !recover && node.assoc.get::<ParenthesizedExpr>().is_some() {
        return Err(loc.error(SyntaxErrorType::InvalidAssigmentTarget, None));
      }
      Ok(
        Node::new(
          loc,
          IdPat {
            name: n.stx.name.clone(),
          },
        )
        .into_wrapped(),
      )
    }
    // It's possible to encounter patterns already parsed e.g. `{a: [b] = 1}`, where `[b]` was already converted to a pattern.
    Expr::IdPat(n) => Ok(n.into_wrapped()),
    Expr::ArrPat(n) => Ok(n.into_wrapped()),
    Expr::ObjPat(n) => Ok(n.into_wrapped()),
    // TypeScript: For any other expression type, wrap it as an assignment target for error recovery
    // This allows destructuring with call expressions, unary operators, etc.
    // The type checker will validate these patterns semantically.
    _ => {
      if recover {
        Ok(Node::new(loc, Pat::AssignTarget(node)))
      } else {
        Err(loc.error(SyntaxErrorType::InvalidAssigmentTarget, None))
      }
    }
  }
}

// Trying to check if every object, array, or identifier expression operand is actually an assignment target first is too expensive and wasteful, so simply retroactively transform the LHS of a BinaryExpr with Assignment* operator into a target, raising an error if it can't (and is an invalid assignment target). A valid target is:
// - A chain of non-optional-chaining member, computed member, and call operators, not ending in a call.
// - A pattern.
// TypeScript: Be maximally permissive and accept most expression types for error recovery.
pub fn lhs_expr_to_assign_target(
  lhs: Node<Expr>,
  operator_name: OperatorName,
) -> SyntaxResult<Node<Expr>> {
  lhs_expr_to_assign_target_with_recover(lhs, operator_name, true)
}

pub(crate) fn lhs_expr_to_assign_target_with_recover(
  lhs: Node<Expr>,
  operator_name: OperatorName,
  recover: bool,
) -> SyntaxResult<Node<Expr>> {
  if !recover {
    return match lhs.stx.as_ref() {
      e @ (Expr::LitArr(_) | Expr::LitObj(_) | Expr::Id(_)) => {
        if operator_name != OperatorName::Assignment && !matches!(e, Expr::Id(_)) {
          return Err(lhs.error(SyntaxErrorType::InvalidAssigmentTarget));
        }
        // We must transform into a pattern.
        let root = lit_to_pat_with_recover(lhs, false)?;
        Ok(root.into_stx())
      }
      Expr::ComputedMember(member) if !member.stx.optional_chaining => Ok(lhs),
      Expr::Member(member) if !member.stx.optional_chaining => Ok(lhs),
      _ => Err(lhs.error(SyntaxErrorType::InvalidAssigmentTarget)),
    };
  }

  match lhs.stx.as_ref() {
    e @ (Expr::LitArr(_) | Expr::LitObj(_) | Expr::Id(_)) => {
      if operator_name != OperatorName::Assignment && !matches!(e, Expr::Id(_)) {
        return Err(lhs.error(SyntaxErrorType::InvalidAssigmentTarget));
      }
      // We must transform into a pattern.
      let root = lit_to_pat_with_recover(lhs, true)?;
      Ok(root.into_stx())
    }
    // TypeScript: Accept member/computed member expressions for error recovery, even with optional chaining
    // Patterns like `obj?.a = 1` are syntactically parseable but semantically invalid
    // The type checker will validate these
    Expr::ComputedMember(_) => Ok(lhs),
    Expr::Member(_) => Ok(lhs),
    // TypeScript: Accept call expressions, unary expressions, and postfix expressions for error recovery
    // This allows patterns like `foo() = bar`, `++x = 5`, `x++ = 5`, `++x++`, etc.
    // The type checker will reject these, but the parser accepts them.
    Expr::Call(_) | Expr::Unary(_) | Expr::UnaryPostfix(_) => Ok(lhs),
    // TypeScript: Accept literal expressions for error recovery
    // This allows patterns like `1 >>= 2`, `"str" = value`, etc.
    // The type checker will reject these, but the parser accepts them.
    Expr::LitNum(_)
    | Expr::LitStr(_)
    | Expr::LitBool(_)
    | Expr::LitNull(_)
    | Expr::LitBigInt(_)
    | Expr::LitRegex(_) => Ok(lhs),
    // TypeScript: Accept this, super, type assertions, and other TypeScript expressions for error recovery
    // This allows patterns like `this *= value`, `super = value`, `(expr as T) = value`, `expr! = value`
    Expr::This(_)
    | Expr::Super(_)
    | Expr::TypeAssertion(_)
    | Expr::NonNullAssertion(_)
    | Expr::SatisfiesExpr(_)
    // Allow import.meta as an assignment target for error recovery.
    // While it's not a valid target, TypeScript still parses it to produce semantic errors.
    | Expr::ImportMeta(_) => Ok(lhs),
    Expr::Binary(binary) if binary.stx.operator.is_assignment() => Ok(lhs),
    _ => Err(lhs.error(SyntaxErrorType::InvalidAssigmentTarget)),
  }
}
