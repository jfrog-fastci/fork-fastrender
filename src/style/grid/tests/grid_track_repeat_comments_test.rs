use super::*;

#[test]
fn parses_repeat_with_comments_in_count() {
  let (tracks, names, _) = parse_grid_tracks_with_names("repeat(2/*comment*/, 10px)");
  assert_eq!(tracks.len(), 2);
  assert!(names.is_empty());
}

