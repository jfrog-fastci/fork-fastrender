use super::*;

#[test]
fn subgrid_line_names_accepts_specified_grammar() {
  let nbsp = "\u{00A0}";
  assert_eq!(
    parse_subgrid_line_names("subgrid"),
    Some(Vec::<Vec<String>>::new())
  );
  assert_eq!(
    parse_subgrid_line_names("/*comment*/subgrid"),
    Some(Vec::<Vec<String>>::new())
  );
  assert_eq!(
    parse_subgrid_line_names("subgrid/*comment*/"),
    Some(Vec::<Vec<String>>::new())
  );
  assert!(
    parse_subgrid_line_names(&format!("{nbsp}subgrid")).is_none(),
    "NBSP must not be treated as CSS whitespace"
  );
  assert_eq!(
    parse_subgrid_line_names("subgrid [a] [b]"),
    Some(vec![vec!["a".to_string()], vec!["b".to_string()]])
  );
  assert_eq!(
    parse_subgrid_line_names("subgrid [a]/*comment*/[b]"),
    Some(vec![vec!["a".to_string()], vec!["b".to_string()]])
  );
  assert_eq!(
    parse_subgrid_line_names("subgrid/*comment*/[a]"),
    Some(vec![vec!["a".to_string()]])
  );
  assert!(
    parse_subgrid_line_names(&format!("subgrid{nbsp}[a]")).is_none(),
    "NBSP must not be treated as CSS whitespace between tokens"
  );
  assert_eq!(
    parse_subgrid_line_names("subgrid [a/*comment*/b]"),
    Some(vec![vec!["a".to_string(), "b".to_string()]])
  );
  assert_eq!(
    parse_subgrid_line_names("subgrid [a/*]*/b]"),
    Some(vec![vec!["a".to_string(), "b".to_string()]])
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
    parse_subgrid_line_names("subgrid repeat(+2, [a])"),
    Some(vec![vec!["a".to_string()], vec!["a".to_string()]])
  );
  assert!(parse_subgrid_line_names("subgrid repeat(-2, [a])").is_none());
  assert_eq!(
    parse_subgrid_line_names("subgrid repeat(/*comment*/2, [a])"),
    Some(vec![vec!["a".to_string()], vec!["a".to_string()]])
  );
  assert_eq!(
    parse_subgrid_line_names("subgrid repeat(2/*comment*/, [a])"),
    Some(vec![vec!["a".to_string()], vec!["a".to_string()]])
  );
  assert_eq!(
    parse_subgrid_line_names("subgrid repeat(2/*,*/, [a])"),
    Some(vec![vec!["a".to_string()], vec!["a".to_string()]])
  );
  assert_eq!(
    parse_subgrid_line_names("subgrid repeat(2, /*)*/[a])"),
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
  assert!(
    parse_subgrid_line_names("subgrid repeat(2, repeat(2, [a]))").is_none(),
    "nested repeat() is not allowed in a subgrid line-name-list"
  );
  assert!(
    parse_subgrid_line_names("subgrid repeat(2, [a] repeat(2, [b]))").is_none(),
    "repeat() patterns must contain only bracketed line-name lists"
  );

  // `subgridsubgrid` is a single identifier token and must not be treated as the `subgrid` keyword.
  assert!(parse_subgrid_line_names("subgridsubgrid").is_none());
}
