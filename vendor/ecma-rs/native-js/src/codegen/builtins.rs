use parse_js::ast::expr::{CallArg, CallExpr, Expr};
use parse_js::ast::node::Node;

#[derive(Clone, Copy, Debug)]
pub enum BuiltinCall<'a> {
  Print { arg: &'a Node<Expr> },
  Assert {
    cond: &'a Node<Expr>,
    msg: Option<&'a Node<Expr>>,
  },
}

fn arg_value<'a>(arg: &'a Node<CallArg>) -> Option<&'a Node<Expr>> {
  if arg.stx.spread {
    return None;
  }
  Some(&arg.stx.value)
}

fn is_ident(expr: &Node<Expr>, name: &str) -> bool {
  match expr.stx.as_ref() {
    Expr::Id(id) => id.stx.name == name,
    _ => false,
  }
}

pub fn recognize_builtin(call: &Node<CallExpr>) -> Option<BuiltinCall<'_>> {
  // Only support direct calls (no optional chaining).
  if call.stx.optional_chaining {
    return None;
  }

  let callee = &call.stx.callee;

  // `print(x)`
  if is_ident(callee, "print") {
    if call.stx.arguments.len() != 1 {
      return None;
    }
    let arg = arg_value(&call.stx.arguments[0])?;
    return Some(BuiltinCall::Print { arg });
  }

  // `assert(cond, msg?)`
  if is_ident(callee, "assert") {
    if !(call.stx.arguments.len() == 1 || call.stx.arguments.len() == 2) {
      return None;
    }
    let cond = arg_value(&call.stx.arguments[0])?;
    let msg = call.stx.arguments.get(1).and_then(arg_value);
    return Some(BuiltinCall::Assert { cond, msg });
  }

  // `console.log(x)`
  if let Expr::Member(member) = callee.stx.as_ref() {
    if member.stx.optional_chaining {
      return None;
    }

    if member.stx.right != "log" {
      return None;
    }
    if !is_ident(&member.stx.left, "console") {
      return None;
    }

    if call.stx.arguments.len() != 1 {
      return None;
    }
    let arg = arg_value(&call.stx.arguments[0])?;
    return Some(BuiltinCall::Print { arg });
  }

  None
}
