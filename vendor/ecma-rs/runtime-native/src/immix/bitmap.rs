use super::LINE_MAP_BYTES;
use super::LINES_PER_BLOCK;

pub type LineMap = [u8; LINE_MAP_BYTES];

#[inline]
pub fn clear(map: &mut LineMap) {
  map.fill(0);
}

#[inline]
pub fn is_empty(map: &LineMap) -> bool {
  map.iter().all(|&b| b == 0)
}

#[inline]
pub fn is_line_marked(map: &LineMap, line: usize) -> bool {
  debug_assert!(line < LINES_PER_BLOCK);
  let byte = line / 8;
  let bit = line % 8;
  (map[byte] & (1 << bit)) != 0
}

#[inline]
pub fn set_line(map: &mut LineMap, line: usize) {
  debug_assert!(line < LINES_PER_BLOCK);
  let byte = line / 8;
  let bit = line % 8;
  map[byte] |= 1 << bit;
}

pub fn set_range(map: &mut LineMap, start_line: usize, end_line: usize) {
  debug_assert!(start_line <= end_line);
  debug_assert!(end_line <= LINES_PER_BLOCK);
  for line in start_line..end_line {
    set_line(map, line);
  }
}

pub fn used_lines(map: &LineMap) -> usize {
  map.iter().map(|b| b.count_ones() as usize).sum()
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

