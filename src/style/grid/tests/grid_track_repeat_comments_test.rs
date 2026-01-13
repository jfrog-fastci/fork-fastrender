use super::*;

#[test]
fn parses_repeat_with_comments_in_count() {
  let (tracks, names, _) = parse_grid_tracks_with_names("repeat(2/*comment*/, 10px)");
  assert_eq!(tracks.len(), 2);
  assert!(names.is_empty());
}

#[test]
fn parses_minmax_with_comments_in_arguments() {
  let (tracks, _, _) = parse_grid_tracks_with_names("minmax(10px/*comment*/, 1fr)");
  assert_eq!(tracks.len(), 1);
  assert!(matches!(tracks[0], GridTrack::MinMax(_, _)));
}

#[test]
fn parses_fit_content_with_comments_in_arguments() {
  let (tracks, _, _) = parse_grid_tracks_with_names("fit-content(50%/*comment*/)");
  assert_eq!(tracks.len(), 1);
  assert!(matches!(tracks[0], GridTrack::FitContent(_)));
}
