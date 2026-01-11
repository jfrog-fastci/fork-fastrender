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
}

impl BumpCursor {
  pub fn new() -> Self {
    Self {
      cursor: std::ptr::null_mut(),
      limit: std::ptr::null_mut(),
      block_id: None,
    }
  }

  pub fn reset(&mut self) {
    *self = Self::new();
  }
}

pub struct ImmixSpace {
  blocks: Vec<Block>,
  block_by_start: HashMap<usize, usize>,
  free_blocks: Vec<usize>,
  bump: BumpCursor,
}

impl ImmixSpace {
  pub fn new() -> Self {
    Self {
      blocks: Vec::new(),
      block_by_start: HashMap::new(),
      free_blocks: Vec::new(),
      bump: BumpCursor::new(),
    }
  }

  #[inline]
  pub fn block_count(&self) -> usize {
    self.blocks.len()
  }

  #[inline]
  pub fn free_block_count(&self) -> usize {
    self.free_blocks.len()
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
      free_blocks,
      bump,
    } = self;
    alloc_old_with_cursor(blocks, block_by_start, free_blocks, bump, size, align)
  }

  pub fn alloc_old_with_cursor(&mut self, bump: &mut BumpCursor, size: usize, align: usize) -> Option<*mut u8> {
    let Self {
      blocks,
      block_by_start,
      free_blocks,
      bump: _,
    } = self;
    alloc_old_with_cursor(blocks, block_by_start, free_blocks, bump, size, align)
  }

  /// Clear all line maps in preparation for a full-heap marking pass.
  pub fn clear_all_line_maps(&mut self) {
    for block in &mut self.blocks {
      block.clear_line_map();
    }
    self.free_blocks.clear();
    self.bump.reset();
  }

  /// Mark the lines spanned by a live object.
  pub fn set_lines_for_live_object(&mut self, obj_start: *mut u8, obj_size: usize) {
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

    let block = &mut self.blocks[block_id];
    debug_assert!(block.contains_addr(obj_addr));
    debug_assert!(obj_end <= block.end_addr(), "object crosses block boundary");
    block.mark_addr_range(obj_addr, obj_end);
  }

  /// Finalize marking: identify fully free blocks and rebuild the free list.
  pub fn finalize_after_marking(&mut self) {
    self.free_blocks.clear();
    for (i, block) in self.blocks.iter().enumerate() {
      if block.is_empty() {
        self.free_blocks.push(i);
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
  free_blocks: &mut Vec<usize>,
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
      acquire_hole(blocks, block_by_start, free_blocks, bump, min_lines)?;
    }

    let cursor_addr = bump.cursor as usize;
    let aligned_addr = align_up(cursor_addr, align);
    let end_addr = aligned_addr.checked_add(size)?;

    if end_addr <= bump.limit as usize {
      let block_id = bump.block_id.expect("cursor has a block id");
      let block = &mut blocks[block_id];

      // Mark all lines consumed by the allocation, including alignment padding.
      block.mark_addr_range(cursor_addr, end_addr);

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
    } else {
      bump.reset();
    }
  }
}

fn acquire_hole(
  blocks: &mut Vec<Block>,
  block_by_start: &mut HashMap<usize, usize>,
  free_blocks: &mut Vec<usize>,
  bump: &mut BumpCursor,
  min_lines: usize,
) -> Option<()> {
  if let Some(block_id) = free_blocks.pop() {
    let start = blocks[block_id].start_addr();
    bump.block_id = Some(block_id);
    bump.cursor = start as *mut u8;
    bump.limit = (start + BLOCK_SIZE) as *mut u8;
    return Some(());
  }

  for block_id in 0..blocks.len() {
    let block = &blocks[block_id];
    if let Some((hole_start, hole_end)) = bitmap::find_hole(&block.line_map, 0, min_lines) {
      let start = block.start_addr() + (hole_start * LINE_SIZE);
      let limit = block.start_addr() + (hole_end * LINE_SIZE);
      bump.block_id = Some(block_id);
      bump.cursor = start as *mut u8;
      bump.limit = limit as *mut u8;
      return Some(());
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
  Some(())
}

#[cfg(test)]
mod tests {
  use std::collections::HashSet;

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
}
