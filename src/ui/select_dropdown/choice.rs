#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectDropdownChoice {
  pub select_node_id: usize,
  pub option_node_id: usize,
}

impl SelectDropdownChoice {
  pub fn new(select_node_id: usize, option_node_id: usize) -> Self {
    Self {
      select_node_id,
      option_node_id,
    }
  }
}

