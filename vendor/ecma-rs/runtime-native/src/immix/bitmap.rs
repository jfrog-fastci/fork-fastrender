use super::LINES_PER_BLOCK;
use super::LINE_MAP_WORDS;

use core::sync::atomic::{AtomicU64, Ordering};

pub type LineMap = [AtomicU64; LINE_MAP_WORDS];

#[inline]
pub fn clear(map: &LineMap) {
  for w in map {
    w.store(0, Ordering::Relaxed);
  }
}

#[inline]
pub fn is_empty(map: &LineMap) -> bool {
  map.iter().all(|w| w.load(Ordering::Relaxed) == 0)
}

#[inline]
pub fn is_line_marked(map: &LineMap, line: usize) -> bool {
  debug_assert!(line < LINES_PER_BLOCK);
  let word = line / 64;
  let bit = line % 64;
  let mask = 1u64 << bit;
  (map[word].load(Ordering::Relaxed) & mask) != 0
}

#[inline]
pub fn set_line(map: &LineMap, line: usize) {
  debug_assert!(line < LINES_PER_BLOCK);
  let word = line / 64;
  let bit = line % 64;
  let mask = 1u64 << bit;
  map[word].fetch_or(mask, Ordering::Relaxed);
}

pub fn set_range(map: &LineMap, start_line: usize, end_line: usize) {
  debug_assert!(start_line <= end_line);
  debug_assert!(end_line <= LINES_PER_BLOCK);

  if start_line == end_line {
    return;
  }

  let start_word = start_line / 64;
  let end_word = (end_line - 1) / 64;

  for word in start_word..=end_word {
    let word_start = word * 64;
    let word_end = word_start + 64;
    let s = start_line.max(word_start) - word_start;
    let e = end_line.min(word_end) - word_start;
    debug_assert!(s < 64);
    debug_assert!(e <= 64);
    debug_assert!(s < e);

    let lower = if s == 0 { 0 } else { (1u64 << s) - 1 };
    let upper = if e == 64 { !0u64 } else { (1u64 << e) - 1 };
    let mask = upper & !lower;

    map[word].fetch_or(mask, Ordering::Relaxed);
  }
}

pub fn used_lines(map: &LineMap) -> usize {
  map
    .iter()
    .map(|w| w.load(Ordering::Relaxed).count_ones() as usize)
    .sum()
}

pub fn free_lines(map: &LineMap) -> usize {
  LINES_PER_BLOCK - used_lines(map)
}

pub fn largest_hole_lines(map: &LineMap) -> usize {
  let mut largest = 0usize;
  let mut current = 0usize;
  for line in 0..LINES_PER_BLOCK {
    if is_line_marked(map, line) {
      current = 0;
    } else {
      current += 1;
      largest = largest.max(current);
    }
  }
  largest
}

pub fn find_hole(map: &LineMap, start_line: usize, min_lines: usize) -> Option<(usize, usize)> {
  debug_assert!(start_line <= LINES_PER_BLOCK);
  debug_assert!(min_lines > 0);

  let mut i = start_line;
  while i < LINES_PER_BLOCK {
    while i < LINES_PER_BLOCK && is_line_marked(map, i) {
      i += 1;
    }
    if i == LINES_PER_BLOCK {
      return None;
    }

    let hole_start = i;
    while i < LINES_PER_BLOCK && !is_line_marked(map, i) {
      i += 1;
    }
    let hole_end = i;

    if hole_end - hole_start >= min_lines {
      return Some((hole_start, hole_end));
    }
  }

  None
}
