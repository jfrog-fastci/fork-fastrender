use crate::css::types::Declaration;
use std::collections::HashMap;
use std::sync::Arc;

/// Registry of `@position-try` rules available within a given tree scope.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PositionTryRegistry {
  rules: HashMap<String, Arc<[Declaration]>>,
}

impl PositionTryRegistry {
  pub fn get(&self, name: &str) -> Option<&[Declaration]> {
    self.rules.get(name).map(|decls| decls.as_ref())
  }

  pub fn register(&mut self, name: String, declarations: Vec<Declaration>) {
    self.rules.insert(name, Arc::from(declarations));
  }
}
