use std::fmt;

/// StackMap location kind (LLVM StackMap v3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocationKind {
    Register,
    Direct,
    Indirect,
    Constant,
    ConstantIndex,
}

/// A single location entry from a StackMap v3 record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    /// A value is held in a register.
    Register { size: u16, dwarf_reg: u16 },
    /// A value is held at a register + offset address.
    Direct { size: u16, dwarf_reg: u16, offset: i32 },
    /// A value is held at the memory address `[register + offset]`.
    ///
    /// For statepoints, stack slots are typically encoded as `Indirect` relative to SP.
    Indirect { size: u16, dwarf_reg: u16, offset: i32 },
    /// A small (signed) constant stored inline as an `i32`.
    Constant { size: u16, value: i64 },
    /// A 64-bit constant stored in the constants table.
    ConstantIndex { size: u16, index: u32, value: u64 },
}

impl Location {
    pub fn kind(&self) -> LocationKind {
        match self {
            Location::Register { .. } => LocationKind::Register,
            Location::Direct { .. } => LocationKind::Direct,
            Location::Indirect { .. } => LocationKind::Indirect,
            Location::Constant { .. } => LocationKind::Constant,
            Location::ConstantIndex { .. } => LocationKind::ConstantIndex,
        }
    }

    pub fn size(&self) -> u16 {
        match self {
            Location::Register { size, .. }
            | Location::Direct { size, .. }
            | Location::Indirect { size, .. }
            | Location::Constant { size, .. }
            | Location::ConstantIndex { size, .. } => *size,
        }
    }

    pub fn dwarf_reg(&self) -> Option<u16> {
        match self {
            Location::Register { dwarf_reg, .. }
            | Location::Direct { dwarf_reg, .. }
            | Location::Indirect { dwarf_reg, .. } => Some(*dwarf_reg),
            Location::Constant { .. } | Location::ConstantIndex { .. } => None,
        }
    }

    pub fn offset(&self) -> Option<i32> {
        match self {
            Location::Direct { offset, .. } | Location::Indirect { offset, .. } => Some(*offset),
            _ => None,
        }
    }

    /// Return the constant value (if this is a constant location).
    ///
    /// `Location::Constant` is restricted to 32-bit signed immediates in the
    /// binary format; `Location::ConstantIndex` can reference a full 64-bit
    /// constant from the constants table.
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Location::Constant { value, .. } => u64::try_from(*value).ok(),
            Location::ConstantIndex { value, .. } => Some(*value),
            _ => None,
        }
    }
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Location::Register { dwarf_reg, .. } => write!(f, "Register[R#{dwarf_reg}]"),
            Location::Direct {
                dwarf_reg, offset, ..
            } => write!(f, "Direct[R#{dwarf_reg} + {offset}]"),
            Location::Indirect {
                dwarf_reg, offset, ..
            } => write!(f, "Indirect[R#{dwarf_reg} + {offset}]"),
            Location::Constant { value, .. } => write!(f, "Constant({value})"),
            Location::ConstantIndex { index, value, .. } => {
                write!(f, "ConstantIndex({index} => {value})")
            }
        }
    }
}

/// A live-out register entry from a StackMap v3 record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveOut {
    pub dwarf_reg: u16,
    pub size: u8,
}

/// A decoded StackMap v3 record.
#[derive(Debug, Clone)]
pub struct StackMapRecord {
    /// StackMap record / patchpoint ID.
    pub id: u64,
    /// Byte offset from function start.
    pub instruction_offset: u32,
    /// Absolute callsite address (return address) = `function_address + instruction_offset`.
    pub callsite_pc: u64,
    pub locations: Vec<Location>,
    pub live_outs: Vec<LiveOut>,
}

impl StackMapRecord {
    pub fn locations(&self) -> &[Location] {
        &self.locations
    }

    pub fn live_outs(&self) -> &[LiveOut] {
        &self.live_outs
    }
}

/// An index entry mapping callsite PC to a record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Callsite {
    pub pc: u64,
    pub record_index: usize,
}

