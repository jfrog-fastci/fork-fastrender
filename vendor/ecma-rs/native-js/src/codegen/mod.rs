//! Code generation from internal IR/MIR to LLVM IR.

mod builtins;
mod llvm;

use crate::CompileOptions;
use parse_js::ast::node::Node;
use parse_js::ast::stx::TopLevel;

#[derive(thiserror::Error, Debug)]
pub enum CodegenError {
  #[error("unsupported statement")]
  UnsupportedStmt,

  #[error("unsupported expression")]
  UnsupportedExpr,

  #[error("unsupported operator: {0:?}")]
  UnsupportedOperator(parse_js::operator::OperatorName),

  #[error("builtins disabled")]
  BuiltinsDisabled,

  #[error("type error: {0}")]
  TypeError(String),
}

pub fn emit_llvm_module(ast: &Node<TopLevel>, opts: CompileOptions) -> Result<String, CodegenError> {
  llvm::emit_llvm_module(ast, opts)
}

