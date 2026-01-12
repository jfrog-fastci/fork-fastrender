use std::collections::HashMap;

use super::bitmap;
use super::Block;
use super::BLOCK_SIZE;
use super::LINE_SIZE;

#[derive(Clone, Copy, Debug)]
pub struct BumpCursor {
  pub cursor: *mut u8,
  pub limit: *mut u8,
  pub block_id: Option<usize>,
  claimed_line_limit: usize,
}

impl BumpCursor {
  pub fn new() -> Self {
    Self {
      cursor: std::ptr::null_mut(),
      limit: std::ptr::null_mut(),
      block_id: None,
      claimed_line_limit: 0,
    }
  }

  pub fn reset(&mut self) {
    *self = Self::new();
  }
}

pub struct ImmixSpace {
  blocks: Vec<Block>,
  block_by_start: HashMap<usize, usize>,
  /// Blocks indexed by the size (in lines) of their largest free hole.
  ///
  /// This acts as an availability structure so allocation can find space without
  /// linearly scanning all blocks.
  ///
  /// Index `i` contains blocks whose current `largest_hole_lines == i`.
  available_by_hole: Vec<Vec<usize>>,
  bump: BumpCursor,
}

impl ImmixSpace {
  pub fn new() -> Self {
    Self {
      blocks: Vec::new(),
      block_by_start: HashMap::new(),
      available_by_hole: vec![Vec::new(); super::LINES_PER_BLOCK + 1],
      bump: BumpCursor::new(),
    }
  }

  #[inline]
  pub fn block_count(&self) -> usize {
    self.blocks.len()
  }

  #[inline]
  pub fn free_block_count(&self) -> usize {
    self.available_by_hole[super::LINES_PER_BLOCK].len()
  }

  pub fn block_id_for_ptr(&self, ptr: *mut u8) -> Option<usize> {
    if ptr.is_null() {
      return None;
    }

    let addr = ptr as usize;
    let block_base = addr & !(BLOCK_SIZE - 1);
    self.block_by_start.get(&block_base).copied()
  }

  pub fn clear_block_line_map(&mut self, block_id: usize) {
    if let Some(block) = self.blocks.get_mut(block_id) {
      block.clear_line_map();
    }
  }

  pub fn contains(&self, ptr: *mut u8) -> bool {
    if ptr.is_null() {
      return false;
    }

    let addr = ptr as usize;
    let block_base = addr & !(BLOCK_SIZE - 1);
    self
      .block_by_start
      .get(&block_base)
      .is_some_and(|&block_id| self.blocks[block_id].contains_addr(addr))
  }

  pub fn free_bytes(&self) -> usize {
    self
      .blocks
      .iter()
      .map(|block| bitmap::free_lines(&block.line_map) * LINE_SIZE)
      .sum()
  }

  /// Returns the number of bytes whose lines are currently marked in the line map.
  ///
  /// During major GC this represents a line-granularity approximation of live bytes.
  /// During normal allocation it represents the amount of space consumed by
  /// allocations (including internal fragmentation to line boundaries).
  pub fn line_map_used_bytes(&self) -> usize {
    self
      .blocks
      .iter()
      .map(|block| bitmap::used_lines(&block.line_map) * LINE_SIZE)
      .sum()
  }

  pub fn block_metrics(&self, block_id: usize) -> Option<crate::immix::BlockMetrics> {
    self.blocks.get(block_id).map(|block| block.metrics())
  }

  pub(crate) fn clear_line_marks(&mut self) {
    self.clear_all_line_maps();
  }

  pub fn alloc_old(&mut self, size: usize, align: usize) -> Option<*mut u8> {
    let Self {
      blocks,
      block_by_start,
      available_by_hole,
      bump,
    } = self;
    alloc_old_with_cursor(blocks, block_by_start, available_by_hole, bump, size, align)
  }

  pub fn alloc_old_with_cursor(&mut self, bump: &mut BumpCursor, size: usize, align: usize) -> Option<*mut u8> {
    let Self {
      blocks,
      block_by_start,
      available_by_hole,
      bump: _,
    } = self;
    alloc_old_with_cursor(blocks, block_by_start, available_by_hole, bump, size, align)
  }

  pub fn alloc_old_with_cursor_excluding(
    &mut self,
    bump: &mut BumpCursor,
    size: usize,
    align: usize,
    excluded_blocks: &[bool],
  ) -> Option<*mut u8> {
    let Self {
      blocks,
      block_by_start,
      available_by_hole,
      bump: _,
    } = self;
    alloc_old_with_cursor_excluding(
      blocks,
      block_by_start,
      available_by_hole,
      bump,
      size,
      align,
      excluded_blocks,
    )
  }

  /// Reserve an Immix hole for thread-local bump allocation.
  ///
  /// This is the multithreaded "refill" path used by the runtime allocator:
  /// - The selected hole (or entire block) is **claimed** by marking its lines in the block's line
  ///   map, so other threads won't allocate from it.
  /// - The caller is expected to perform bump allocation within the returned `start..limit`
  ///   range **without** mutating Immix metadata on the hot path.
  ///
  /// The reserved range is line-aligned and may be larger than `min_lines`.
  pub fn reserve_hole(&mut self, min_lines: usize, grow: bool) -> Option<(*mut u8, *mut u8)> {
    if min_lines == 0 || min_lines > super::LINES_PER_BLOCK {
      return None;
    }

    // Prefer fully free blocks (largest_hole_lines == LINES_PER_BLOCK), then fall back to any block
    // with a large-enough hole. We keep `available_by_hole` up-to-date by removing the block from
    // its old bucket and reinserting it under its new `largest_hole_lines` after claiming.
    for hole_lines in (min_lines..=super::LINES_PER_BLOCK).rev() {
      while let Some(block_id) = self.available_by_hole[hole_lines].pop() {
        let block = &self.blocks[block_id];
        if let Some((hole_start, hole_end)) = bitmap::find_hole(&block.line_map, 0, min_lines) {
          let start = block.start_addr() + (hole_start * LINE_SIZE);
          let limit = block.start_addr() + (hole_end * LINE_SIZE);

          let block = &mut self.blocks[block_id];
          block.mark_addr_range(start, limit);

          let largest = bitmap::largest_hole_lines(&block.line_map);
          if largest > 0 {
            self.available_by_hole[largest].push(block_id);
          }

          return Some((start as *mut u8, limit as *mut u8));
        }

        // Stale bucket entry (should be rare): recompute and reinsert if the block still has space.
        let largest = bitmap::largest_hole_lines(&block.line_map);
        if largest > 0 {
          self.available_by_hole[largest].push(block_id);
        }
      }
    }

    if !grow {
      return None;
    }

    // No holes: allocate a new block.
    let block_id = self.blocks.len();
    let block = Block::new(block_id)?;
    let start = block.start_addr();
    debug_assert_eq!(start % BLOCK_SIZE, 0);
    let limit = start + BLOCK_SIZE;
    block.mark_addr_range(start, limit);
    self.block_by_start.insert(start, block_id);
    self.blocks.push(block);
    Some((start as *mut u8, limit as *mut u8))
  }

  /// Clear all line maps in preparation for a full-heap marking pass.
  pub fn clear_all_line_maps(&mut self) {
    for block in &mut self.blocks {
      block.clear_line_map();
    }
    for bucket in &mut self.available_by_hole {
      bucket.clear();
    }
    self.bump.reset();
  }

  /// Mark the lines spanned by a live object.
  pub fn set_lines_for_live_object(&self, obj_start: *mut u8, obj_size: usize) {
    if obj_size == 0 {
      return;
    }

    let obj_addr = obj_start as usize;
    let block_base = obj_addr & !(BLOCK_SIZE - 1);
    let block_id = *self
      .block_by_start
      .get(&block_base)
      .expect("object pointer does not belong to this ImmixSpace");

    let obj_end = obj_addr
      .checked_add(obj_size)
      .expect("object size overflow while marking");

    let block = &self.blocks[block_id];
    debug_assert!(block.contains_addr(obj_addr));
    debug_assert!(obj_end <= block.end_addr(), "object crosses block boundary");
    block.mark_addr_range(obj_addr, obj_end);
  }

  /// Finalize marking: identify fully free blocks and rebuild the free list.
  pub fn finalize_after_marking(&mut self) {
    for bucket in &mut self.available_by_hole {
      bucket.clear();
    }
    for (i, block) in self.blocks.iter().enumerate() {
      let largest = bitmap::largest_hole_lines(&block.line_map);
      if largest > 0 {
        self.available_by_hole[largest].push(i);
      }
    }
    self.bump.reset();
  }
}

#[inline]
fn align_up(addr: usize, align: usize) -> usize {
  debug_assert!(align.is_power_of_two());
  (addr + (align - 1)) & !(align - 1)
}

fn alloc_old_with_cursor(
  blocks: &mut Vec<Block>,
  block_by_start: &mut HashMap<usize, usize>,
  available_by_hole: &mut Vec<Vec<usize>>,
  bump: &mut BumpCursor,
  size: usize,
  align: usize,
) -> Option<*mut u8> {
  if size == 0 {
    return None;
  }
  if align == 0 || !align.is_power_of_two() {
    return None;
  }

  // Large objects should live in a separate large object space.
  if size > BLOCK_SIZE {
    return None;
  }

  let min_lines = size.div_ceil(LINE_SIZE);

  loop {
    if bump.block_id.is_none() {
      acquire_hole(blocks, block_by_start, available_by_hole, bump, min_lines)?;
    }

    let cursor_addr = bump.cursor as usize;
    let aligned_addr = align_up(cursor_addr, align);
    let end_addr = aligned_addr.checked_add(size)?;

    if end_addr <= bump.limit as usize {
      let block_id = bump.block_id.expect("cursor has a block id");
      let block = &blocks[block_id];
      let block_start = block.start_addr();
      let start_off = cursor_addr - block_start;
      let end_off = end_addr - block_start;
      let start_line = start_off / LINE_SIZE;
      let end_line = end_off.div_ceil(LINE_SIZE);

      // If this allocation would extend beyond the lines we've already
      // allocated into, ensure the additional lines are still free. This keeps
      // `alloc_old_with_cursor` safe to use with multiple interleaved cursors
      // in a single `ImmixSpace`.
      if end_line > bump.claimed_line_limit {
        let check_start = bump.claimed_line_limit.max(start_line);
        if let Some(conflict_line) = (check_start..end_line)
          .find(|&line| bitmap::is_line_marked(&block.line_map, line))
        {
          if let Some((hole_start, hole_end)) =
            bitmap::find_hole(&block.line_map, conflict_line + 1, min_lines)
          {
            let start = block_start + (hole_start * LINE_SIZE);
            let limit = block_start + (hole_end * LINE_SIZE);
            bump.cursor = start as *mut u8;
            bump.limit = limit as *mut u8;
            bump.claimed_line_limit = hole_start;
            continue;
          }
          release_block(blocks, available_by_hole, bump);
          bump.reset();
          continue;
        }
      }

      // Mark all lines consumed by the allocation, including alignment padding.
      let block = &mut blocks[block_id];
      block.mark_addr_range(cursor_addr, end_addr);
      bump.claimed_line_limit = end_line;

      bump.cursor = end_addr as *mut u8;
      return Some(aligned_addr as *mut u8);
    }

    let block_id = bump.block_id.expect("cursor has a block id");
    let block = &blocks[block_id];
    let after_line = ((bump.limit as usize) - block.start_addr()) / LINE_SIZE;

    if let Some((hole_start, hole_end)) = bitmap::find_hole(&block.line_map, after_line, min_lines) {
      let start = block.start_addr() + (hole_start * LINE_SIZE);
      let limit = block.start_addr() + (hole_end * LINE_SIZE);
      bump.cursor = start as *mut u8;
      bump.limit = limit as *mut u8;
      bump.claimed_line_limit = hole_start;
    } else {
      release_block(blocks, available_by_hole, bump);
      bump.reset();
    }
  }
}

fn alloc_old_with_cursor_excluding(
  blocks: &mut Vec<Block>,
  block_by_start: &mut HashMap<usize, usize>,
  available_by_hole: &mut Vec<Vec<usize>>,
  bump: &mut BumpCursor,
  size: usize,
  align: usize,
  excluded_blocks: &[bool],
) -> Option<*mut u8> {
  if size == 0 {
    return None;
  }
  if align == 0 || !align.is_power_of_two() {
    return None;
  }

  // Large objects should live in a separate large object space.
  if size > BLOCK_SIZE {
    return None;
  }

  let min_lines = size.div_ceil(LINE_SIZE);

  loop {
    if bump.block_id.is_none() {
      acquire_hole_excluding(
        blocks,
        block_by_start,
        available_by_hole,
        bump,
        min_lines,
        excluded_blocks,
      )?;
    }

    let cursor_addr = bump.cursor as usize;
    let aligned_addr = align_up(cursor_addr, align);
    let end_addr = aligned_addr.checked_add(size)?;

    if end_addr <= bump.limit as usize {
      let block_id = bump.block_id.expect("cursor has a block id");
      let block = &blocks[block_id];
      let block_start = block.start_addr();
      let start_off = cursor_addr - block_start;
      let end_off = end_addr - block_start;
      let start_line = start_off / LINE_SIZE;
      let end_line = end_off.div_ceil(LINE_SIZE);

      // If this allocation would extend beyond the lines we've already
      // allocated into, ensure the additional lines are still free. This keeps
      // `alloc_old_with_cursor_excluding` safe to use with multiple interleaved
      // cursors in a single `ImmixSpace`.
      if end_line > bump.claimed_line_limit {
        let check_start = bump.claimed_line_limit.max(start_line);
        if let Some(conflict_line) = (check_start..end_line).find(|&line| bitmap::is_line_marked(&block.line_map, line))
        {
          if let Some((hole_start, hole_end)) = bitmap::find_hole(&block.line_map, conflict_line + 1, min_lines) {
            let start = block_start + (hole_start * LINE_SIZE);
            let limit = block_start + (hole_end * LINE_SIZE);
            bump.cursor = start as *mut u8;
            bump.limit = limit as *mut u8;
            bump.claimed_line_limit = hole_start;
            continue;
          }
          release_block(blocks, available_by_hole, bump);
          bump.reset();
          continue;
        }
      }

      // Mark all lines consumed by the allocation, including alignment padding.
      let block = &mut blocks[block_id];
      block.mark_addr_range(cursor_addr, end_addr);
      bump.claimed_line_limit = end_line;

      bump.cursor = end_addr as *mut u8;
      return Some(aligned_addr as *mut u8);
    }

    let block_id = bump.block_id.expect("cursor has a block id");
    let block = &blocks[block_id];
    let after_line = ((bump.limit as usize) - block.start_addr()) / LINE_SIZE;

    if let Some((hole_start, hole_end)) = bitmap::find_hole(&block.line_map, after_line, min_lines) {
      let start = block.start_addr() + (hole_start * LINE_SIZE);
      let limit = block.start_addr() + (hole_end * LINE_SIZE);
      bump.cursor = start as *mut u8;
      bump.limit = limit as *mut u8;
      bump.claimed_line_limit = hole_start;
    } else {
      release_block(blocks, available_by_hole, bump);
      bump.reset();
    }
  }
}

fn acquire_hole(
  blocks: &mut Vec<Block>,
  block_by_start: &mut HashMap<usize, usize>,
  available_by_hole: &mut Vec<Vec<usize>>,
  bump: &mut BumpCursor,
  min_lines: usize,
) -> Option<()> {
  // Preserve the original allocator's behavior of preferring fully-free blocks
  // first (this keeps allocation bump-fast in the common case and avoids
  // needlessly fragmenting partially-live blocks).
  while let Some(block_id) = available_by_hole[super::LINES_PER_BLOCK].pop() {
    let block = &blocks[block_id];
    if let Some((hole_start, hole_end)) = bitmap::find_hole(&block.line_map, 0, min_lines) {
      let start = block.start_addr() + (hole_start * LINE_SIZE);
      let limit = block.start_addr() + (hole_end * LINE_SIZE);
      bump.block_id = Some(block_id);
      bump.cursor = start as *mut u8;
      bump.limit = limit as *mut u8;
      bump.claimed_line_limit = hole_start;
      return Some(());
    }
    // Stale entry (should be rare): reinsert according to current metrics.
    let largest = bitmap::largest_hole_lines(&block.line_map);
    if largest > 0 {
      available_by_hole[largest].push(block_id);
    }
  }

  // Fall back to partially-free blocks.
  for hole_lines in min_lines..super::LINES_PER_BLOCK {
    while let Some(block_id) = available_by_hole[hole_lines].pop() {
      let block = &blocks[block_id];
      if let Some((hole_start, hole_end)) = bitmap::find_hole(&block.line_map, 0, min_lines) {
        let start = block.start_addr() + (hole_start * LINE_SIZE);
        let limit = block.start_addr() + (hole_end * LINE_SIZE);
        bump.block_id = Some(block_id);
        bump.cursor = start as *mut u8;
        bump.limit = limit as *mut u8;
        bump.claimed_line_limit = hole_start;
        return Some(());
      }

      // Stale bucket entry: recompute and reinsert if the block still has space.
      let largest = bitmap::largest_hole_lines(&block.line_map);
      if largest > 0 {
        available_by_hole[largest].push(block_id);
      }
    }
  }

  let block_id = blocks.len();
  let block = Block::new(block_id)?;
  let start = block.start_addr();
  debug_assert_eq!(start % BLOCK_SIZE, 0);
  block_by_start.insert(start, block_id);
  blocks.push(block);

  bump.block_id = Some(block_id);
  bump.cursor = start as *mut u8;
  bump.limit = (start + BLOCK_SIZE) as *mut u8;
  bump.claimed_line_limit = 0;
  Some(())
}

fn acquire_hole_excluding(
  blocks: &mut Vec<Block>,
  block_by_start: &mut HashMap<usize, usize>,
  available_by_hole: &mut Vec<Vec<usize>>,
  bump: &mut BumpCursor,
  min_lines: usize,
  excluded_blocks: &[bool],
) -> Option<()> {
  let mut skipped: Vec<(usize, usize)> = Vec::new();

  // Prefer fully-free blocks first, matching the main allocator.
  while let Some(block_id) = available_by_hole[super::LINES_PER_BLOCK].pop() {
    let block = &blocks[block_id];

    if excluded_blocks.get(block_id).copied().unwrap_or(false) {
      let largest = bitmap::largest_hole_lines(&block.line_map);
      if largest > 0 {
        skipped.push((largest, block_id));
      }
      continue;
    }

    if let Some((hole_start, hole_end)) = bitmap::find_hole(&block.line_map, 0, min_lines) {
      let start = block.start_addr() + (hole_start * LINE_SIZE);
      let limit = block.start_addr() + (hole_end * LINE_SIZE);
      bump.block_id = Some(block_id);
      bump.cursor = start as *mut u8;
      bump.limit = limit as *mut u8;
      bump.claimed_line_limit = hole_start;
      for (bucket, id) in skipped.drain(..) {
        available_by_hole[bucket].push(id);
      }
      return Some(());
    }

    // Stale entry: reinsert according to current metrics.
    let largest = bitmap::largest_hole_lines(&block.line_map);
    if largest > 0 {
      available_by_hole[largest].push(block_id);
    }
  }

  // Fall back to partially-free blocks.
  for hole_lines in min_lines..super::LINES_PER_BLOCK {
    while let Some(block_id) = available_by_hole[hole_lines].pop() {
      let block = &blocks[block_id];

      if excluded_blocks.get(block_id).copied().unwrap_or(false) {
        let largest = bitmap::largest_hole_lines(&block.line_map);
        if largest > 0 {
          skipped.push((largest, block_id));
        }
        continue;
      }

      if let Some((hole_start, hole_end)) = bitmap::find_hole(&block.line_map, 0, min_lines) {
        let start = block.start_addr() + (hole_start * LINE_SIZE);
        let limit = block.start_addr() + (hole_end * LINE_SIZE);
        bump.block_id = Some(block_id);
        bump.cursor = start as *mut u8;
        bump.limit = limit as *mut u8;
        bump.claimed_line_limit = hole_start;
        for (bucket, id) in skipped.drain(..) {
          available_by_hole[bucket].push(id);
        }
        return Some(());
      }

      // Stale bucket entry: recompute and reinsert if the block still has space.
      let largest = bitmap::largest_hole_lines(&block.line_map);
      if largest > 0 {
        available_by_hole[largest].push(block_id);
      }
    }
  }

  let block_id = blocks.len();
  let Some(block) = Block::new(block_id) else {
    for (bucket, id) in skipped.drain(..) {
      available_by_hole[bucket].push(id);
    }
    return None;
  };
  let start = block.start_addr();
  debug_assert_eq!(start % BLOCK_SIZE, 0);
  block_by_start.insert(start, block_id);
  blocks.push(block);

  bump.block_id = Some(block_id);
  bump.cursor = start as *mut u8;
  bump.limit = (start + BLOCK_SIZE) as *mut u8;
  bump.claimed_line_limit = 0;
  for (bucket, id) in skipped.drain(..) {
    available_by_hole[bucket].push(id);
  }
  Some(())
}

fn release_block(blocks: &mut Vec<Block>, available_by_hole: &mut Vec<Vec<usize>>, bump: &mut BumpCursor) {
  let Some(block_id) = bump.block_id else {
    return;
  };
  let block = &blocks[block_id];
  let largest = bitmap::largest_hole_lines(&block.line_map);
  if largest > 0 {
    available_by_hole[largest].push(block_id);
  }
}

#[cfg(test)]
mod tests {
  use std::collections::HashSet;

  use super::BumpCursor;
  use super::ImmixSpace;
  use crate::immix::BLOCK_SIZE;
  use crate::immix::LINE_SIZE;
  use crate::immix::LINES_PER_BLOCK;

  #[test]
  fn alloc_small_objects_stay_within_block() {
    let mut space = ImmixSpace::new();
    for _ in 0..10_000 {
      let size = 24;
      let ptr = space.alloc_old(size, 8).expect("alloc");
      let addr = ptr as usize;
      let block_base = addr & !(BLOCK_SIZE - 1);
      assert!(addr + size <= block_base + BLOCK_SIZE);
    }
  }

  #[test]
  fn alloc_spans_multiple_blocks() {
    let mut space = ImmixSpace::new();
    let mut blocks = HashSet::new();
    for _ in 0..(LINES_PER_BLOCK * 3) {
      let size = LINE_SIZE;
      let ptr = space.alloc_old(size, LINE_SIZE).expect("alloc");
      let block_base = (ptr as usize) & !(BLOCK_SIZE - 1);
      blocks.insert(block_base);
    }
    assert!(blocks.len() >= 3);
  }

  #[test]
  fn major_gc_reclaims_lines_and_reuses_blocks() {
    let mut space = ImmixSpace::new();
    let size = LINE_SIZE;

    let mut objs = Vec::new();
    for _ in 0..(LINES_PER_BLOCK * 2) {
      objs.push(space.alloc_old(size, LINE_SIZE).expect("alloc"));
    }

    assert_eq!(space.block_count(), 2);

    let block0 = (objs[0] as usize) & !(BLOCK_SIZE - 1);
    let block1 = (objs[LINES_PER_BLOCK] as usize) & !(BLOCK_SIZE - 1);
    assert_ne!(block0, block1);

    // Simulate a major GC cycle where only the first 10 objects in the first
    // block are live.
    space.clear_all_line_maps();
    for obj in objs.iter().take(10) {
      space.set_lines_for_live_object(*obj, size);
    }
    space.finalize_after_marking();
    assert_eq!(space.block_count(), 2);
    assert_eq!(space.free_block_count(), 1);

    // Allocate a full block worth of objects: should reuse the fully-free block
    // without allocating new blocks.
    for _ in 0..LINES_PER_BLOCK {
      let ptr = space.alloc_old(size, LINE_SIZE).expect("alloc");
      assert_eq!((ptr as usize) & !(BLOCK_SIZE - 1), block1);
    }

    // Next allocation should land in the first block's hole (line 10), reusing
    // the dead object's address.
    let ptr = space.alloc_old(size, LINE_SIZE).expect("alloc");
    assert_eq!(ptr, objs[10]);
    assert_eq!((ptr as usize) & !(BLOCK_SIZE - 1), block0);
    assert_eq!(space.block_count(), 2);
  }

  #[test]
  fn multiple_cursors_do_not_produce_overlapping_allocations() {
    let mut space = ImmixSpace::new();
    let mut a = BumpCursor::new();
    let mut b = BumpCursor::new();

    let _a1 = space
      .alloc_old_with_cursor(&mut a, 24, 8)
      .expect("alloc a1");
    let b1 = space
      .alloc_old_with_cursor(&mut b, 100, 8)
      .expect("alloc b1");

    let a2 = space
      .alloc_old_with_cursor(&mut a, 200, 8)
      .expect("alloc a2");

    let a2_start = a2 as usize;
    let a2_end = a2_start + 200;
    let b1_start = b1 as usize;
    let b1_end = b1_start + 100;
    assert!(
      a2_end <= b1_start || b1_end <= a2_start,
      "allocations overlap: a2={a2_start:#x}..{a2_end:#x} b1={b1_start:#x}..{b1_end:#x}"
    );
  }
}
