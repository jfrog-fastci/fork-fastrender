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

