use fastrender::style::grid::parse_grid_line;
use std::collections::HashMap;

#[test]
fn grid_line_auto_keyword_is_case_insensitive() {
  let named_lines: HashMap<String, Vec<usize>> = HashMap::new();
  assert_eq!(parse_grid_line("auto", &named_lines), 0);
  assert_eq!(parse_grid_line("AUTO", &named_lines), 0);
  assert_eq!(parse_grid_line("AuTo", &named_lines), 0);
}

