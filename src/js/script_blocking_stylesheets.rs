use rustc_hash::FxHashSet;

#[derive(Debug, Default, Clone)]
pub struct ScriptBlockingStyleSheetSet {
  keys: FxHashSet<usize>,
}

impl ScriptBlockingStyleSheetSet {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn register_blocking_stylesheet(&mut self, key: usize) {
    self.keys.insert(key);
  }

  pub fn unregister_blocking_stylesheet(&mut self, key: usize) {
    self.keys.remove(&key);
  }

  pub fn has_blocking_stylesheet(&self) -> bool {
    !self.keys.is_empty()
  }
}

