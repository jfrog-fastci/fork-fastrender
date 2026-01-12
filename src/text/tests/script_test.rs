//! Script itemization and emoji helpers from the shaping pipeline.

use crate::style::ComputedStyle;
use crate::text::emoji;
use crate::text::pipeline::itemize_text;
use crate::text::pipeline::BidiAnalysis;
use crate::text::pipeline::Script;

fn itemize(text: &str) -> Vec<crate::text::pipeline::ItemizedRun> {
  let style = ComputedStyle::default();
  let bidi = BidiAnalysis::analyze(text, &style);
  itemize_text(text, &bidi)
}

#[test]
fn single_script_latin() {
  let text = "The quick brown fox jumps over the lazy dog";
  let runs = itemize(text);

  assert_eq!(runs.len(), 1);
  assert_eq!(runs[0].script, Script::Latin);
  assert_eq!(runs[0].text, text);
}

#[test]
fn mixed_latin_and_hebrew_split_runs() {
  let text = "Hello שלום World";
  let runs = itemize(text);

  assert!(runs.len() >= 3);
  assert!(runs.iter().any(|r| r.script == Script::Latin));
  assert!(runs.iter().any(|r| r.script == Script::Hebrew));
}

#[test]
fn common_characters_merge_with_neighbors() {
  let text = "Hello, world!";
  let runs = itemize(text);

  assert_eq!(runs.len(), 1);
  assert_eq!(runs[0].script, Script::Latin);
}

#[test]
fn emoji_helpers_match_pipeline_data() {
  assert!(emoji::is_emoji('😀'));
  assert!(emoji::is_emoji_modifier('\u{1F3FB}'));
  assert!(emoji::is_variation_selector('\u{FE0F}'));
  assert!(emoji::is_zwj('\u{200D}'));
}

#[test]
fn itemize_empty_text_returns_no_runs() {
  let runs = itemize("");
  assert!(runs.is_empty());
}
