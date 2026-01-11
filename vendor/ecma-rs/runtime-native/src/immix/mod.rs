//! Immix-inspired heap space.
//!
//! This module implements the old-generation allocator core (block/line based
//! allocation with line maps) but does **not** include stack maps, safepoints, a
//! remembered set, or a full GC.

mod bitmap;
mod block;
mod space;

pub use block::Block;
pub use block::BlockMetrics;
pub use space::BumpCursor;
pub use space::ImmixSpace;

/// Size of an Immix block in bytes.
pub const BLOCK_SIZE: usize = 32 * 1024;

/// Size of a line within a block in bytes.
pub const LINE_SIZE: usize = 128;

/// Number of lines in a block.
pub const LINES_PER_BLOCK: usize = BLOCK_SIZE / LINE_SIZE;

const LINE_MAP_BYTES: usize = LINES_PER_BLOCK / 8;
