use super::expr::pat::ParsePatternRules;
use super::ParseCtx;
use super::Parser;
use crate::ast::expr::Expr;
use crate::ast::node::CoverInitializedName;
use crate::ast::node::Node;
use crate::ast::stx::TopLevel;
use crate::error::SyntaxErrorType;
use crate::error::SyntaxResult;
use crate::loc::Loc;
use crate::token::TT;
use derive_visitor::Drive;
use derive_visitor::Visitor;

type ExprNode = Node<Expr>;

#[derive(Default, Visitor)]
#[visitor(ExprNode(enter))]
struct CoverInitializedNameFinder {
  first: Option<Loc>,
}

impl CoverInitializedNameFinder {
  fn enter_expr_node(&mut self, expr: &ExprNode) {
    if self.first.is_some() {
      return;
    }
    if expr.assoc.get::<CoverInitializedName>().is_some() {
      self.first = Some(expr.loc);
    }
  }
}

impl<'a> Parser<'a> {
  pub fn parse_top_level(&mut self) -> SyntaxResult<Node<TopLevel>> {
    let is_module = self.is_module();
    if self.is_strict_ecmascript()
      && !is_module
      && self.has_use_strict_directive_in_prologue(TT::EOF)?
    {
      self.strict_mode += 1;
    }
    let ctx = ParseCtx {
      rules: ParsePatternRules {
        await_allowed: !is_module,
        yield_allowed: !is_module,
        await_expr_allowed: is_module || self.allow_top_level_await_in_script,
        yield_expr_allowed: false,
      },
      top_level: true,
      in_namespace: false,
      asi: super::AsiContext::Statements,
    };
    let body = self.stmts(ctx, TT::EOF)?;
    self.require(TT::EOF)?;
    let top_level_node = Node::new(self.source_range(), TopLevel { body });
    if self.is_strict_ecmascript() {
      let mut finder = CoverInitializedNameFinder::default();
      top_level_node.drive(&mut finder);
      if let Some(loc) = finder.first {
        return Err(loc.error(
          SyntaxErrorType::ExpectedSyntax("invalid shorthand property initializer"),
          None,
        ));
      }
    }
    Ok(top_level_node)
  }
}
