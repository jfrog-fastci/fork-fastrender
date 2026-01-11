//! Register-context helpers for interpreting LLVM stackmap locations.
//!
//! LLVM stackmaps describe register and stack locations using **DWARF register
//! numbers**. A precise GC scanner needs to map those numbers to the stopped
//! thread's actual register values.
//!
//! This crate provides:
//! - [`ThreadContext`], a captured register set for the current architecture.
//! - [`StackMapLocation`], a tiny evaluator for `Register(regno)` and
//!   `Indirect { base_reg, offset }` locations.
//!
//! # Supported DWARF register numbers
//!
//! ## `x86_64` (System V)
//! | DWARF reg | Register |
//! |----------|----------|
//! | 0 | RAX |
//! | 1 | RDX |
//! | 2 | RCX |
//! | 3 | RBX |
//! | 4 | RSI |
//! | 5 | RDI |
//! | 6 | RBP |
//! | 7 | RSP |
//! | 8..15 | R8..R15 |
//! | 16 | RIP |
//!
//! ## `aarch64`
//! | DWARF reg | Register |
//! |----------|----------|
//! | 0..30 | X0..X30 |
//! | 31 | SP |
//! | 32 | PC |
//!
//! SIMD / vector registers (x86 XMM/YMM, AArch64 V*) are currently unsupported
//! and will return `None` from [`ThreadContext::get_dwarf_reg_u64`].

mod context;
mod location;

pub use context::{ThreadContext, UnsupportedDwarfRegister, DWARF_REG_IP, DWARF_REG_SP};
pub use location::StackMapLocation;
