use crate::style::grid::parse_subgrid_line_names;

#[test]
fn parse_subgrid_line_names_distinguishes_omitted_vs_explicit_empty() {
  assert_eq!(
    parse_subgrid_line_names("subgrid"),
    Some(Vec::<Vec<String>>::new())
  );
  assert_eq!(
    parse_subgrid_line_names("subgrid []"),
    Some(vec![Vec::<String>::new()])
  );
  assert_eq!(
    parse_subgrid_line_names("subgrid [a] [b]"),
    Some(vec![vec!["a".to_string()], vec!["b".to_string()]])
  );
}

#[test]
fn parse_subgrid_line_names_repeat_expands_integer() {
  assert_eq!(
    parse_subgrid_line_names("subgrid repeat(2, [a])"),
    Some(vec![vec!["a".to_string()], vec!["a".to_string()]])
  );
  assert_eq!(
    parse_subgrid_line_names("subgrid repeat(2, [a]) [b]"),
    Some(vec![
      vec!["a".to_string()],
      vec!["a".to_string()],
      vec!["b".to_string()],
    ])
  );
}

#[test]
fn parse_subgrid_line_names_repeat_mixed_with_plain_brackets() {
  assert_eq!(
    parse_subgrid_line_names("subgrid [start] repeat(2, [mid]) [end]"),
    Some(vec![
      vec!["start".to_string()],
      vec!["mid".to_string()],
      vec!["mid".to_string()],
      vec!["end".to_string()],
    ])
  );
}

#[test]
fn parse_subgrid_line_names_repeat_auto_fill_is_unsupported() {
  assert_eq!(parse_subgrid_line_names("subgrid repeat(auto-fill, [a])"), None);
}
