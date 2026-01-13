use super::*;

#[test]
fn subgrid_line_names_accepts_specified_grammar() {
  assert_eq!(
    parse_subgrid_line_names("subgrid"),
    Some(Vec::<Vec<String>>::new())
  );
  assert_eq!(
    parse_subgrid_line_names("subgrid [a] [b]"),
    Some(vec![vec!["a".to_string()], vec!["b".to_string()]])
  );
  assert_eq!(
    parse_subgrid_line_names("subgrid []"),
    Some(vec![Vec::<String>::new()])
  );
}

#[test]
fn subgrid_line_names_accepts_repeat_forms() {
  assert_eq!(
    parse_subgrid_line_names("subgrid repeat(2, [a])"),
    Some(vec![vec!["a".to_string()], vec!["a".to_string()]])
  );
  assert_eq!(
    parse_subgrid_line_names("subgrid repeat(2, [a] [b])"),
    Some(vec![
      vec!["a".to_string()],
      vec!["b".to_string()],
      vec!["a".to_string()],
      vec!["b".to_string()]
    ])
  );
}

#[test]
fn subgrid_line_names_rejects_invalid_token_order_or_duplicates() {
  assert!(parse_subgrid_line_names("[a] subgrid").is_none());
  assert!(parse_subgrid_line_names("subgrid 10px").is_none());
  assert!(parse_subgrid_line_names("subgrid subgrid").is_none());
  assert!(parse_subgrid_line_names("subgrid [a] junk").is_none());

  // `subgridsubgrid` is a single identifier token and must not be treated as the `subgrid` keyword.
  assert!(parse_subgrid_line_names("subgridsubgrid").is_none());
}
