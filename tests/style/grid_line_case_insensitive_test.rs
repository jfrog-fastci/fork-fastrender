use fastrender::style::grid::parse_grid_line;
use std::collections::HashMap;

#[test]
fn grid_line_auto_keyword_is_ascii_case_insensitive() {
  // Even if a named line exists called `AUTO`, the `auto` keyword must win because CSS keywords are
  // ASCII case-insensitive.
  let mut named_lines: HashMap<String, Vec<usize>> = HashMap::new();
  named_lines.insert("AUTO".to_string(), vec![2]);

  assert_eq!(parse_grid_line("AUTO", &named_lines), 0);
  assert_eq!(parse_grid_line("auto", &named_lines), 0);
  assert_eq!(parse_grid_line("AuTo", &named_lines), 0);
}

